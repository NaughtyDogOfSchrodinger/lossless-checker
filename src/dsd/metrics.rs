//! Spectral metrics derived from an averaged DSD power spectrum.

/// Peak-relative threshold for baseband cutoff detection: the cutoff is the highest baseband
/// frequency whose power stays within this many dB of the baseband peak. Mirrors the PCM side's
/// calibrated `detect_cutoff` (`crate::spectrum`); a shared helper could be factored out later.
const BASEBAND_PEAK_DB: f64 = 65.0;

/// An averaged power spectrum: linear power per FFT bin, with the bin→Hz mapping.
pub struct PowerSpectrum {
    /// Linear (mean) power per bin, index 0 = DC. Length = fft_size/2.
    pub power: Vec<f64>,
    pub bin_hz: f64,
}

impl PowerSpectrum {
    pub fn new(power: Vec<f64>, sample_rate: u64, fft_size: usize) -> Self {
        let bin_hz = sample_rate as f64 / fft_size as f64;
        Self { power, bin_hz }
    }

    #[inline]
    fn freq(&self, i: usize) -> f64 {
        i as f64 * self.bin_hz
    }

    /// Highest bin index whose center frequency is <= `hz` (clamped to the spectrum length).
    fn bin_at(&self, hz: f64) -> usize {
        ((hz / self.bin_hz).floor() as usize).min(self.power.len())
    }
}

/// Noise-shaping slope in dB/oct: least-squares fit of (log2 freq, power dB) over `[f_lo, f_hi]`.
/// A genuine DSD master shapes quantization noise upward here (positive slope ~+18…+24 dB/oct).
pub fn noise_shaping_slope(ps: &PowerSpectrum, f_lo: f64, f_hi: f64) -> f64 {
    let lo = ps.bin_at(f_lo).max(1); // skip DC
    let hi = ps.bin_at(f_hi);
    if lo + 2 >= hi {
        return 0.0;
    }

    let (mut sx, mut sy, mut sxy, mut sxx, mut n) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for i in lo..hi {
        let p = ps.power[i];
        if p <= 0.0 {
            continue;
        }
        let x = ps.freq(i).log2();
        let y = 10.0 * p.log10();
        sx += x;
        sy += y;
        sxy += x * y;
        sxx += x * x;
        n += 1.0;
    }
    if n < 3.0 {
        return 0.0;
    }
    let denom = n * sxx - sx * sx;
    if denom.abs() < f64::EPSILON {
        return 0.0;
    }
    (n * sxy - sx * sy) / denom
}

/// Fraction of total linear power that sits above `threshold_hz` (0.0..=1.0).
pub fn hf_energy_ratio(ps: &PowerSpectrum, threshold_hz: f64) -> f64 {
    let mut total = 0.0f64;
    let mut hf = 0.0f64;
    let thr_bin = ps.bin_at(threshold_hz);
    for (i, &p) in ps.power.iter().enumerate() {
        total += p;
        if i >= thr_bin {
            hf += p;
        }
    }
    if total <= 0.0 {
        0.0
    } else {
        hf / total
    }
}

/// Detect a baseband cutoff cliff within `[0, max_hz]`. Returns the cutoff frequency when the
/// baseband energy clearly stops below the band ceiling (e.g. a 22.05 kHz CD wall), else `None`
/// (genuine DSD rolls off naturally and reaches the ceiling). Only the audible band is inspected,
/// so a DXD workflow's 176 kHz corner is never mistaken for a CD/lossy cutoff.
pub fn detect_baseband_cutoff(ps: &PowerSpectrum, max_hz: f64) -> Option<f64> {
    let top = ps.bin_at(max_hz).min(ps.power.len());
    if top < 4 {
        return None;
    }
    let peak = ps.power[..top].iter().cloned().fold(0.0f64, f64::max);
    if peak <= 0.0 {
        return None;
    }
    let thresh = peak * 10f64.powf(-BASEBAND_PEAK_DB / 10.0);

    let mut cutoff_bin = 0usize;
    for i in (0..top).rev() {
        if ps.power[i] > thresh {
            cutoff_bin = i;
            break;
        }
    }
    let cutoff = ps.freq(cutoff_bin);
    // Only a genuine cliff counts: energy must die well before the band ceiling.
    if cutoff < max_hz - 2.0 * ps.bin_hz {
        Some(cutoff)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A spectrum whose dB rises with a known slope must fit back to that slope.
    #[test]
    fn slope_fit_recovers_known_slope() {
        const SR: u64 = 2_822_400;
        const FFT: usize = 65_536;
        let bin_hz = SR as f64 / FFT as f64;
        let half = FFT / 2;
        let target = 18.0; // dB/oct
        let mut power = vec![0.0f64; half];
        for (i, p) in power.iter_mut().enumerate().skip(1) {
            let f = i as f64 * bin_hz;
            let db = target * f.log2();
            *p = 10f64.powf(db / 10.0);
        }
        let ps = PowerSpectrum { power, bin_hz };
        let slope = noise_shaping_slope(&ps, 30_000.0, 100_000.0);
        assert!((slope - target).abs() < 0.1, "slope = {slope}");
    }

    #[test]
    fn hf_ratio_is_one_for_pure_high_band() {
        let bin_hz = 43.0;
        let mut power = vec![0.0f64; 1000];
        // Put all energy near the top.
        for p in power.iter_mut().skip(900) {
            *p = 1.0;
        }
        let ps = PowerSpectrum { power, bin_hz };
        let r = hf_energy_ratio(&ps, 800.0 * bin_hz);
        assert!(r > 0.99, "ratio = {r}");
    }

    #[test]
    fn hf_ratio_is_zero_for_pure_low_band() {
        let bin_hz = 43.0;
        let mut power = vec![0.0f64; 1000];
        for p in power.iter_mut().take(50) {
            *p = 1.0;
        }
        let ps = PowerSpectrum { power, bin_hz };
        let r = hf_energy_ratio(&ps, 800.0 * bin_hz);
        assert!(r < 0.01, "ratio = {r}");
    }
}
