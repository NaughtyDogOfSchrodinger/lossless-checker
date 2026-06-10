//! Frequency-domain feature extraction.
//!
//! One averaged power spectrum is computed per file and reused by every detector:
//! `detect_cutoff` (where the HF energy "cliff" sits), `hires_extension_db` (how much real
//! energy lives well above the CD Nyquist — the empty-HF / fake-hi-res signal), and
//! `detect_holes` (AAC/Vorbis-style notches below the cutoff; report-only).

use rustfft::{num_complex::Complex, FftPlanner};

/// FFT window size. Frequency resolution = sample_rate / FFT_SIZE (~5-6 Hz/bin @44.1k).
const FFT_SIZE: usize = 8192;
/// Moving-average window (in bins) applied to the power spectrum so isolated bins don't
/// trip the detectors.
const SMOOTH_WIN: usize = 9;
/// Band floor for the hi-res extension check. Above the CD Nyquist (22.05k) plus a margin
/// for resampler transition bands, so an upsample's soft roll-off just past 22k doesn't read
/// as genuine hi-res energy. Real hi-res content extends comfortably past this.
const HIRES_BAND_LO_HZ: f32 = 26_000.0;

// Hole-detection parameters. Reasoned defaults, pending calibration against a labelled
// fixture set (none exists yet) — kept conservative so natural musical nulls don't trip it.
const HOLE_DEPTH_DB: f32 = 35.0; // dB below the local shoulder envelope to count as a notch
const HOLE_MIN_HZ: f32 = 300.0; // minimum width so narrow natural nulls are ignored
const HOLE_SCAN_LO_HZ: f32 = 1_000.0; // ignore the bass region where wide nulls are normal
const HOLE_ENV_HZ: f32 = 1_000.0; // half-window for the shoulder (running-max) envelope

/// A detected spectral notch: a band that sits far below its surrounding shoulders.
pub struct Hole {
    pub low_hz: f32,
    pub high_hz: f32,
    pub depth_db: f32,
}

/// Everything the verdict and the reports need from the frequency domain.
pub struct SpectralFeatures {
    pub cutoff_hz: f32,
    pub holes: Vec<Hole>,
    /// Only set for declared rate > 48k: loudest energy in [CD_NYQUIST..nyquist] relative
    /// to the spectral peak, in dB. Very negative => the hi-res band is empty.
    pub hires_ext_db: Option<f32>,
}

/// Detection options. `peak_db = Some(db)` selects the default peak-relative cutoff
/// method; `None` selects the legacy noise-floor method (uses `threshold_mult`).
pub struct SpectrumOpts {
    pub peak_db: Option<f64>,
    pub threshold_mult: f64,
    /// True for DSD (decoded via ffmpeg). DSD's ultrasonic region is noise-shaping noise
    /// shaped by the decode filter, so the hi-res extension metric is meaningless and is
    /// skipped (the upsample verdict is likewise suppressed in `verdict::classify`).
    pub is_dsd: bool,
}

/// Analyze mono PCM and extract the spectral features.
pub fn analyze(samples: &[f32], sample_rate: u32, opts: SpectrumOpts) -> SpectralFeatures {
    let nyquist = sample_rate as f32 / 2.0;

    // Too short to analyze meaningfully — report full-band and no anomalies.
    if samples.len() < FFT_SIZE {
        return SpectralFeatures {
            cutoff_hz: nyquist,
            holes: Vec::new(),
            hires_ext_db: None,
        };
    }

    let (energy, window_count) = avg_power_spectrum(samples);
    if window_count == 0 {
        return SpectralFeatures {
            cutoff_hz: nyquist,
            holes: Vec::new(),
            hires_ext_db: None,
        };
    }

    let bin_hz = nyquist / (FFT_SIZE as f32 / 2.0);
    let smooth = smooth_spectrum(&energy);

    let cutoff_hz = detect_cutoff(&energy, &smooth, bin_hz, nyquist, &opts);
    let hires_ext_db = if sample_rate > 48_000 && !opts.is_dsd {
        hires_extension_db(&smooth, bin_hz, nyquist)
    } else {
        None
    };
    let holes = detect_holes(&smooth, bin_hz, cutoff_hz);

    SpectralFeatures {
        cutoff_hz,
        holes,
        hires_ext_db,
    }
}

/// Windowed FFT over the whole signal, accumulating an averaged power spectrum.
///
/// We deliberately process the entire track (no early stop): many songs open with a quiet
/// intro and only bring in high-frequency content — cymbals, percussion — later. Sampling
/// just the intro underestimates the cutoff and produces false positives. Throughput comes
/// from parallelism across files instead.
fn avg_power_spectrum(samples: &[f32]) -> (Vec<f64>, u64) {
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);

    let mut energy = vec![0.0f64; FFT_SIZE / 2];
    let mut window_count = 0u64;

    let hop = FFT_SIZE; // no overlap; good enough and fast
    let mut pos = 0;
    while pos + FFT_SIZE <= samples.len() {
        let mut buffer: Vec<Complex<f32>> = samples[pos..pos + FFT_SIZE]
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                // Hann window to reduce spectral leakage.
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos();
                Complex::new(s * w, 0.0)
            })
            .collect();

        fft.process(&mut buffer);

        for (i, c) in buffer.iter().take(FFT_SIZE / 2).enumerate() {
            energy[i] += c.norm_sqr() as f64;
        }
        window_count += 1;
        pos += hop;
    }

    if window_count > 0 {
        for e in energy.iter_mut() {
            *e /= window_count as f64;
        }
    }
    (energy, window_count)
}

/// Small centered moving average over the power spectrum.
fn smooth_spectrum(energy: &[f64]) -> Vec<f64> {
    let half = energy.len();
    let mut smooth = vec![0.0f64; half];
    for (i, sm) in smooth.iter_mut().enumerate() {
        let lo = i.saturating_sub(SMOOTH_WIN / 2);
        let hi = (i + SMOOTH_WIN / 2 + 1).min(half);
        let slice = &energy[lo..hi];
        *sm = slice.iter().sum::<f64>() / slice.len() as f64;
    }
    smooth
}

/// Locate the high-frequency cutoff in Hz.
///
/// Default (peak-relative): the cutoff is the highest frequency whose smoothed energy stays
/// within `db` dB of the spectrum's own peak (the loudest, usually low-mid, bin). This is
/// robust to brickwall cuts: above a hard low-pass the energy collapses far below the strong
/// mid-band reference, so faint residue no longer masquerades as real signal. Calibrated to
/// db=65 against known-answer 128k/320k round-trip fakes.
///
/// Legacy (noise-floor, `--noise-floor`): references the cutoff against the top-band noise
/// floor times a multiplier. Kept for comparison; it false-negatives on clean cuts of
/// weak-HF (orchestral/vocal) content.
fn detect_cutoff(
    energy: &[f64],
    smooth: &[f64],
    bin_hz: f32,
    nyquist: f32,
    opts: &SpectrumOpts,
) -> f32 {
    let half = energy.len();

    if let Some(db) = opts.peak_db {
        let peak = smooth.iter().cloned().fold(0.0f64, f64::max);
        if peak <= 0.0 {
            return nyquist;
        }
        let thresh = peak * 10f64.powf(-db / 10.0); // dB below peak, in power
        let mut cutoff_bin = 0usize;
        for i in (0..half).rev() {
            if smooth[i] > thresh {
                cutoff_bin = i;
                break;
            }
        }
        return cutoff_bin as f32 * bin_hz;
    }

    // Legacy noise-floor method: estimate the floor from the top 5% of bins.
    let tail_start = half * 95 / 100;
    let noise_floor: f64 = {
        let tail = &energy[tail_start..];
        tail.iter().sum::<f64>() / tail.len().max(1) as f64
    };
    let threshold = noise_floor * opts.threshold_mult;

    let mut cutoff_bin = half - 1;
    for i in (0..half).rev() {
        if energy[i] > threshold {
            cutoff_bin = i;
            break;
        }
    }
    cutoff_bin as f32 * bin_hz
}

/// For hi-res files: the loudest smoothed energy in [HIRES_BAND_LO_HZ..nyquist], in dB
/// relative to the spectral peak. A genuine hi-res master has real content here
/// (e.g. -20..-50 dB); CD/lossy content upsampled into a hi-res container leaves only the
/// reconstruction floor (≪ -70 dB). The band starts above 22k so a resampler's transition
/// roll-off just past the CD wall isn't mistaken for real hi-res energy.
fn hires_extension_db(smooth: &[f64], bin_hz: f32, nyquist: f32) -> Option<f32> {
    let peak = smooth.iter().cloned().fold(0.0f64, f64::max);
    if peak <= 0.0 {
        return None;
    }
    let lo_bin = (HIRES_BAND_LO_HZ / bin_hz).ceil() as usize;
    let hi_bin = ((nyquist / bin_hz) as usize).min(smooth.len());
    if lo_bin >= hi_bin {
        return None;
    }
    let band_max = smooth[lo_bin..hi_bin].iter().cloned().fold(0.0f64, f64::max);
    if band_max <= 0.0 {
        return Some(-120.0);
    }
    Some((10.0 * (band_max / peak).log10()) as f32)
}

/// Find spectral notches between `HOLE_SCAN_LO_HZ` and the cutoff: contiguous bands sitting
/// `HOLE_DEPTH_DB` below the surrounding shoulder envelope and wider than `HOLE_MIN_HZ`.
/// Report-only; never feeds the verdict (too false-positive-prone on real music).
fn detect_holes(smooth: &[f64], bin_hz: f32, cutoff_hz: f32) -> Vec<Hole> {
    let half = smooth.len();
    let lo = (HOLE_SCAN_LO_HZ / bin_hz) as usize;
    let hi = ((cutoff_hz / bin_hz) as usize).min(half);
    if lo + 2 >= hi {
        return Vec::new();
    }

    // Shoulder envelope = running max over a ~1kHz half-window, so the hole itself doesn't
    // drag the reference down.
    let env_w = ((HOLE_ENV_HZ / bin_hz) as usize).max(1);

    let mut holes = Vec::new();
    let mut run_start: Option<usize> = None;
    let mut run_depth = 0.0f32;

    for i in lo..hi {
        let elo = i.saturating_sub(env_w);
        let ehi = (i + env_w + 1).min(half);
        let env = smooth[elo..ehi].iter().cloned().fold(0.0f64, f64::max);
        let here = smooth[i];

        let below_db = if env > 0.0 && here > 0.0 {
            (10.0 * (env / here).log10()) as f32 // positive = how far below the shoulder
        } else if env > 0.0 {
            120.0
        } else {
            0.0
        };

        if below_db >= HOLE_DEPTH_DB {
            if run_start.is_none() {
                run_start = Some(i);
                run_depth = 0.0;
            }
            run_depth = run_depth.max(below_db);
        } else if let Some(start) = run_start.take() {
            push_hole(&mut holes, start, i, run_depth, bin_hz);
        }
    }
    if let Some(start) = run_start.take() {
        push_hole(&mut holes, start, hi, run_depth, bin_hz);
    }
    holes
}

fn push_hole(holes: &mut Vec<Hole>, start_bin: usize, end_bin: usize, depth_db: f32, bin_hz: f32) {
    let low_hz = start_bin as f32 * bin_hz;
    let high_hz = end_bin as f32 * bin_hz;
    if high_hz - low_hz >= HOLE_MIN_HZ {
        holes.push(Hole {
            low_hz,
            high_hz,
            depth_db,
        });
    }
}
