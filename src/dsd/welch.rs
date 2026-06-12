//! Welch power-spectrum primitives.
//!
//! A file's samples are unpacked in bounded batches (the 1-bit stream expands to 32× its size as
//! `f32`, so it is never held whole) and handed to [`WelchPlan::power_sum`], which transforms every
//! whole `fft_size` frame and sums the magnitude-squared per bin. Frames are independent, so within
//! one batch they are FFT'd in parallel — that is the per-file speedup. Frame contiguity across
//! batch boundaries is preserved by the caller (it carries the trailing partial frame forward), so
//! the result is bit-for-bit identical to a single-threaded sweep.

use std::sync::Arc;

use rayon::prelude::*;
use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

use crate::spectrum::blackman_harris;

/// One FFT plan + window, shared across a file's batches and frames.
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

    pub fn fft_size(&self) -> usize {
        self.fft_size
    }

    /// A pool of reusable per-thread FFT workers, sized to the rayon pool. Allocate once per file
    /// and reuse across every batch so the (large, FFT-sized) scratch buffers are not re-allocated
    /// per flush.
    pub fn worker_pool(&self) -> Vec<FrameWorker<'_>> {
        (0..rayon::current_num_threads().max(1))
            .map(|_| FrameWorker::new(self))
            .collect()
    }

    /// Sum `|FFT(frame · window)|²` over every whole `fft_size` frame in `samples`, **adding** the
    /// per-bin power into `out_sum` (length `fft_size/2`) and returning the number of frames summed.
    /// A trailing partial frame (`samples.len() % fft_size`) is ignored; the caller carries it into
    /// the next batch so no frame straddles a boundary. Frames are split into one contiguous range
    /// per pooled worker and transformed in parallel.
    pub fn power_sum_into(
        &self,
        samples: &[f32],
        workers: &mut [FrameWorker<'_>],
        out_sum: &mut [f64],
    ) -> u64 {
        let frames = samples.len() / self.fft_size;
        if frames == 0 {
            return 0;
        }
        let per_worker = frames.div_ceil(workers.len());

        workers.par_iter_mut().enumerate().for_each(|(i, w)| {
            w.reset();
            let lo = (i * per_worker).min(frames) * self.fft_size;
            let hi = ((i + 1) * per_worker).min(frames) * self.fft_size;
            for frame in samples[lo..hi].chunks_exact(self.fft_size) {
                w.accumulate(frame);
            }
        });

        let mut count = 0;
        for w in workers.iter() {
            for (o, s) in out_sum.iter_mut().zip(&w.sum) {
                *o += s;
            }
            count += w.count;
        }
        count
    }
}

/// Reusable per-thread scratch + running power sum. One per pooled worker; [`FrameWorker::reset`]
/// clears it for the next batch so the buffers (notably the FFT scratch) are allocated only once.
pub struct FrameWorker<'a> {
    plan: &'a WelchPlan,
    input: Vec<f32>,
    output: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    sum: Vec<f64>,
    count: u64,
}

impl<'a> FrameWorker<'a> {
    fn new(plan: &'a WelchPlan) -> Self {
        Self {
            input: plan.fft.make_input_vec(),
            output: plan.fft.make_output_vec(),
            scratch: plan.fft.make_scratch_vec(),
            sum: vec![0.0; plan.fft_size / 2],
            count: 0,
            plan,
        }
    }

    fn reset(&mut self) {
        self.sum.iter_mut().for_each(|s| *s = 0.0);
        self.count = 0;
    }

    fn accumulate(&mut self, frame: &[f32]) {
        for (dst, (&s, &w)) in self.input.iter_mut().zip(frame.iter().zip(&self.plan.window)) {
            *dst = s * w;
        }
        self.plan
            .fft
            .process_with_scratch(&mut self.input, &mut self.output, &mut self.scratch)
            .expect("FFT input/output sizes match");
        for (a, c) in self.sum.iter_mut().zip(self.output.iter().take(self.plan.fft_size / 2)) {
            *a += c.norm_sqr() as f64;
        }
        self.count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn power_sum(plan: &WelchPlan, samples: &[f32]) -> (Vec<f64>, u64) {
        let mut workers = plan.worker_pool();
        let mut sum = vec![0.0f64; plan.fft_size() / 2];
        let count = plan.power_sum_into(samples, &mut workers, &mut sum);
        (sum, count)
    }

    /// `power_sum_into` on the whole buffer must equal summing it in arbitrary contiguous splits —
    /// the property the batched caller relies on for losslessness.
    #[test]
    fn power_sum_is_split_invariant() {
        const FFT: usize = 1024;
        let plan = WelchPlan::new(FFT);
        let samples: Vec<f32> = (0..FFT * 10).map(|i| ((i as f32) * 0.001).sin()).collect();

        let (whole, n_whole) = power_sum(&plan, &samples);
        // Split at a frame boundary (4 frames + 6 frames) and sum the parts.
        let (a, na) = power_sum(&plan, &samples[..FFT * 4]);
        let (b, nb) = power_sum(&plan, &samples[FFT * 4..]);
        assert_eq!(n_whole, na + nb);
        for ((w, x), y) in whole.iter().zip(&a).zip(&b) {
            assert!((w - (x + y)).abs() < 1e-6, "{w} vs {}", x + y);
        }
    }

    #[test]
    fn power_sum_ignores_trailing_partial_frame() {
        const FFT: usize = 512;
        let plan = WelchPlan::new(FFT);
        let samples = vec![0.5f32; FFT * 3 + 100]; // 3 whole frames + a partial
        let (_, count) = power_sum(&plan, &samples);
        assert_eq!(count, 3);
    }
}
