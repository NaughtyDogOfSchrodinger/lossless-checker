//! Spectral metrics derived from an averaged DSD power spectrum.

/// Peak-relative threshold for baseband cutoff detection: the cutoff is the highest baseband
/// frequency whose power stays within this many dB of the baseband peak. Mirrors the PCM side's
/// calibrated `detect_cutoff` (`crate::spectrum`); a shared helper could be factored out later.
const BASEBAND_PEAK_DB: f64 = 65.0;

/// CD-wall step detector geometry: compare a band this far below the wall against one this far
/// above it, each `CD_WALL_SPAN_HZ` wide, skipping `CD_WALL_GAP_HZ` of transition on each side.
const CD_WALL_GAP_HZ: f64 = 500.0;
const CD_WALL_SPAN_HZ: f64 = 2_000.0;

/// Baseband-cutoff "music reaches the edge" gate. A real lossy/CD cutoff is an *edge*: full-band
/// music right up to it, then a drop. A genuine dark or quiet passage instead tapers gradually and
/// merely crosses the -65 dB line low in the band, the energy already far below the peak by then.
/// Require the `CUTOFF_EDGE_SPAN_HZ`-wide band just below the cutoff to stay within
/// `CUTOFF_EDGE_FLOOR_DB` of the baseband peak; otherwise it is natural roll-off, not a cliff.
/// Cross-validated against ffmpeg PCM decodes: genuine dark DSD masters sit ≤ -61 dB there, real
/// brick walls ≥ -40 dB.
const CUTOFF_EDGE_FLOOR_DB: f64 = 50.0;
const CUTOFF_EDGE_SPAN_HZ: f64 = 2_000.0;

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
    // Energy must die well before the band ceiling — otherwise it reaches the top, no cutoff.
    if cutoff >= max_hz - 2.0 * ps.bin_hz {
        return None;
    }
    // ...and the music must actually reach the cutoff (an edge), not have already faded into it (a
    // gradual dark/quiet roll-off). The band just below must stay within CUTOFF_EDGE_FLOOR_DB of the
    // baseband peak; a gradual taper that merely crosses -65 dB low in the band does not count.
    let edge = mean_power_band(ps, cutoff - CUTOFF_EDGE_SPAN_HZ, cutoff);
    if edge < peak * 10f64.powf(-CUTOFF_EDGE_FLOOR_DB / 10.0) {
        return None;
    }
    Some(cutoff)
}

/// Mean linear power over the bins whose center frequency falls in `[lo, hi]`.
fn mean_power_band(ps: &PowerSpectrum, lo: f64, hi: f64) -> f64 {
    let lo_bin = ps.bin_at(lo).min(ps.power.len());
    let hi_bin = ps.bin_at(hi).min(ps.power.len());
    if lo_bin >= hi_bin {
        return 0.0;
    }
    let band = &ps.power[lo_bin..hi_bin];
    band.iter().sum::<f64>() / band.len() as f64
}

/// Detect a sharp brick-wall **step** right at the CD Nyquist (~22.05 kHz) — the digital-ADC
/// signature of CD-sourced material, distinct from a genuine master's gentle analog roll-off.
///
/// Unlike `detect_baseband_cutoff` (a global "65 dB below the baseband peak" test, which a high
/// modulator noise floor above the wall can bury), this compares a narrow band just *below* the
/// wall against one just *above* it. It fires only when (a) real music reaches the wall — the
/// below-band stays within `floor_db` of the baseband peak — and (b) the power drops by at least
/// `step_db` across it. A gentle analog roll-off has no such sharp step, so it is not flagged.
pub fn detect_cd_wall(ps: &PowerSpectrum, cd_hz: f64, step_db: f64, floor_db: f64) -> bool {
    let below = mean_power_band(ps, cd_hz - CD_WALL_GAP_HZ - CD_WALL_SPAN_HZ, cd_hz - CD_WALL_GAP_HZ);
    let above = mean_power_band(ps, cd_hz + CD_WALL_GAP_HZ, cd_hz + CD_WALL_GAP_HZ + CD_WALL_SPAN_HZ);
    if below <= 0.0 || above <= 0.0 {
        return false;
    }

    // Music must actually reach the wall — otherwise a quiet region's noise-vs-noise step is
    // meaningless (e.g. a track that already rolled off at 15 kHz).
    let peak = ps.power[..ps.bin_at(cd_hz).max(1)].iter().cloned().fold(0.0f64, f64::max);
    if peak <= 0.0 || below < peak * 10f64.powf(-floor_db / 10.0) {
        return false;
    }

    let drop_db = 10.0 * (below / above).log10();
    drop_db >= step_db
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

    /// Build a DSD64-scale spectrum: full power (`band_db` dB rel. peak) up to `wall_hz`, then a
    /// floor (`above_db`) above it. Peak (0 dB) sits at a low bin.
    fn wall_spectrum(wall_hz: f64, band_db: f64, above_db: f64) -> PowerSpectrum {
        let bin_hz = 2_822_400.0 / 65_536.0; // ~43 Hz
        let half = 65_536 / 2;
        let mut power = vec![10f64.powf(above_db / 10.0); half];
        let wall_bin = (wall_hz / bin_hz) as usize;
        for p in power.iter_mut().take(wall_bin) {
            *p = 10f64.powf(band_db / 10.0);
        }
        power[10] = 1.0; // peak = 0 dB at a low bin
        PowerSpectrum { power, bin_hz }
    }

    #[test]
    fn cd_wall_detects_sharp_step_above_the_noise_floor() {
        // Music to 22.05k at -30 dB, then a -55 dB floor above — a 25 dB step the global
        // "65 dB below peak" test would miss (the floor sits within 65 dB of the peak).
        let ps = wall_spectrum(22_050.0, -30.0, -55.0);
        assert!(detect_cd_wall(&ps, 22_050.0, 20.0, 50.0));
        // The global cutoff detector misses it (floor is within 65 dB → no cutoff before ceiling).
        assert_eq!(detect_baseband_cutoff(&ps, 24_000.0), None);
    }

    #[test]
    fn cd_wall_ignores_gentle_rolloff() {
        // Only a ~6 dB difference across the wall — a gentle analog roll-off, not a brick wall.
        let ps = wall_spectrum(22_050.0, -30.0, -36.0);
        assert!(!detect_cd_wall(&ps, 22_050.0, 20.0, 50.0));
    }

    #[test]
    fn cd_wall_ignores_noise_buried_wall() {
        // Music below the wall is weaker than the noise above it (the crude-modulator case): no
        // visible step, so nothing to detect.
        let ps = wall_spectrum(22_050.0, -60.0, -55.0);
        assert!(!detect_cd_wall(&ps, 22_050.0, 20.0, 50.0));
    }

    #[test]
    fn cd_wall_ignores_silence_at_the_wall() {
        // A track that already rolled off far below the wall: the below-band is deep noise, well
        // past `floor_db` from the peak, so a noise-vs-noise step does not count.
        let ps = wall_spectrum(15_000.0, -30.0, -90.0);
        assert!(!detect_cd_wall(&ps, 22_050.0, 20.0, 50.0));
    }

    /// A real brick wall (full music to the cutoff, then a deep floor) is reported: the band just
    /// below the cutoff still carries music, so the "music reaches the edge" gate passes.
    #[test]
    fn baseband_cutoff_reports_brick_wall() {
        let ps = wall_spectrum(16_000.0, -12.0, -90.0);
        let co = detect_baseband_cutoff(&ps, 24_000.0).expect("brick wall should be detected");
        assert!((co - 16_000.0).abs() < 200.0, "cutoff = {co}");
    }

    /// A gradual dark/quiet roll-off (energy already ~ -60 dB by the time it crosses -65 dB) must
    /// NOT be reported — it is natural master roll-off, not a lossy/CD cliff. This is the genuine
    /// dark-DSD false-positive case (e.g. a soft classical-guitar passage) the gate fixes.
    #[test]
    fn baseband_cutoff_ignores_gradual_dark_rolloff() {
        let bin_hz = 2_822_400.0 / 65_536.0;
        let half = 65_536 / 2;
        // dB(f) declines linearly from 0 dB at DC to -90 dB at 12 kHz — crosses -65 dB near 8.7 kHz,
        // with the 2 kHz below it already around -55..-65 dB (well past the -50 dB edge floor).
        let mut power = vec![0.0f64; half];
        for (i, p) in power.iter_mut().enumerate() {
            let f = i as f64 * bin_hz;
            let db = (-90.0 / 12_000.0) * f; // -7.5 dB per kHz
            *p = 10f64.powf(db / 10.0);
        }
        let ps = PowerSpectrum { power, bin_hz };
        assert_eq!(detect_baseband_cutoff(&ps, 24_000.0), None);
    }
}
