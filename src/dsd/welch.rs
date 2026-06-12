//! Streaming Welch power-spectrum accumulator.
//!
//! Samples are fed in continuously (across block-group boundaries); whenever a full `fft_size`
//! frame is buffered it is windowed, transformed, and its magnitude-squared accumulated. The
//! raw samples are never retained, so peak memory is decoupled from file size (≈ the FFT buffers).
//!
//! One accumulator is used per channel so each channel's frames stay temporally contiguous; the
//! per-channel mean power spectra are averaged afterwards.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::spectrum::blackman_harris;

/// Build one FFT plan + window, shared (cloned `Arc`) across a file's per-channel accumulators.
pub struct WelchPlan {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    fft_size: usize,
}

impl WelchPlan {
    pub fn new(fft_size: usize) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(fft_size);
        // Blackman-Harris (reused from the PCM side): ~-92 dB sidelobes keep strong baseband
        // energy from leaking up and corrupting the noise-shaping slope fit at 30–100 kHz.
        let window = blackman_harris(fft_size);
        Self { fft, window, fft_size }
    }

    pub fn accumulator(&self) -> WelchAccumulator {
        WelchAccumulator::new(self.fft.clone(), self.window.clone(), self.fft_size)
    }
}

pub struct WelchAccumulator {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    input: Vec<f32>,            // make_input_vec, len fft_size
    spectrum: Vec<Complex<f32>>, // make_output_vec, len fft_size/2 + 1
    fill: Vec<f32>,
    fill_len: usize,
    fft_size: usize,
    accum: Vec<f64>, // len fft_size/2
    blocks: u64,
}

impl WelchAccumulator {
    fn new(fft: Arc<dyn RealToComplex<f32>>, window: Vec<f32>, fft_size: usize) -> Self {
        let input = fft.make_input_vec();
        let spectrum = fft.make_output_vec();
        Self {
            fft,
            window,
            input,
            spectrum,
            fill: vec![0.0; fft_size],
            fill_len: 0,
            fft_size,
            accum: vec![0.0; fft_size / 2],
            blocks: 0,
        }
    }

    /// Feed ±1 samples; full frames are transformed and accumulated automatically.
    pub fn feed(&mut self, samples: &[f32]) {
        for &s in samples {
            self.fill[self.fill_len] = s;
            self.fill_len += 1;
            if self.fill_len == self.fft_size {
                self.process_frame();
                self.fill_len = 0;
            }
        }
    }

    fn process_frame(&mut self) {
        for (dst, (&s, &w)) in self.input.iter_mut().zip(self.fill.iter().zip(&self.window)) {
            *dst = s * w;
        }
        self.fft
            .process(&mut self.input, &mut self.spectrum)
            .expect("FFT input/output sizes match");
        for (a, c) in self.accum.iter_mut().zip(self.spectrum.iter().take(self.fft_size / 2)) {
            *a += c.norm_sqr() as f64;
        }
        self.blocks += 1;
    }

    /// Mean linear power per bin (len `fft_size/2`) and the number of frames averaged.
    /// Relative metrics (slope, energy ratio, peak-relative cutoff) need no window-gain
    /// normalization, so none is applied.
    pub fn finalize(mut self) -> (Vec<f64>, u64) {
        if self.blocks > 0 {
            let n = self.blocks as f64;
            for a in self.accum.iter_mut() {
                *a /= n;
            }
        }
        (self.accum, self.blocks)
    }
}
