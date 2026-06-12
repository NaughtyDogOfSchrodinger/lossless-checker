//! DSD authenticity thresholds, flags, and the verdict assembler.

use serde::Serialize;

use crate::i18n::Lang;

/// Tunable thresholds for the DSD verdict. Defaults are *starting* values from the design doc and
/// must be calibrated against labelled real/fake/DXD samples before they are trustworthy.
#[derive(Debug, Clone)]
pub struct DsdThresholds {
    pub min_noise_shaping_slope: f64, // dB/oct
    pub min_hf_ratio: f64,
    pub cd_cutoff_hz: f64,
    pub cd_cutoff_tol_hz: f64,
    /// Strict ceiling: a baseband cutoff below this is treated as a lossy cliff *when the noise
    /// shaping is flat* (the "washed PCM" case, where the cutoff corroborates a fake).
    pub lossy_cutoff_max_hz: f64,
    /// Hard low cliff: a cutoff below this convicts *even when the slope confirms genuine DSD*.
    /// Above it (up to the CD wall) is treated as natural master roll-off and left alone.
    pub hard_lossy_cutoff_hz: f64,
    pub slope_fit_lo_hz: f64,
    pub slope_fit_hi_hz: f64,
    pub hf_threshold_hz: f64,
    /// Audible-band ceiling for baseband cutoff detection (only ≤ this is inspected).
    pub baseband_max_hz: f64,
    /// Minimum power drop (dB) across the CD Nyquist to count as a brick-wall step.
    pub cd_wall_step_db: f64,
    /// The band just below the wall must stay within this many dB of the baseband peak for the
    /// step to count as a real wall (i.e. music actually reaches it).
    pub cd_wall_floor_db: f64,
}

impl Default for DsdThresholds {
    fn default() -> Self {
        Self {
            min_noise_shaping_slope: 6.0,
            min_hf_ratio: 0.05,
            cd_cutoff_hz: 22_050.0,
            cd_cutoff_tol_hz: 1_000.0,
            lossy_cutoff_max_hz: 20_000.0,
            hard_lossy_cutoff_hz: 16_500.0,
            slope_fit_lo_hz: 30_000.0,
            slope_fit_hi_hz: 100_000.0,
            hf_threshold_hz: 50_000.0,
            baseband_max_hz: 24_000.0,
            cd_wall_step_db: 20.0,
            cd_wall_floor_db: 70.0,
        }
    }
}

/// A single reason a file looks suspicious. Serialized as a stable machine key; the human message
/// is localized separately so JSON stays language-neutral (matching the PCM side).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DsdFlag {
    WeakNoiseShaping,
    LowHfEnergy,
    CdCutoff,
    /// A sharp brick-wall step at the CD Nyquist (~22.05 kHz) — distinct from a roll-off that
    /// merely lands near 22 kHz. Strong evidence of a CD/digital master source.
    CdWall,
    LossyCutoff,
    SuspiciousCutoff,
}

impl DsdFlag {
    pub fn message(self, lang: Lang) -> &'static str {
        match self {
            DsdFlag::WeakNoiseShaping => lang.pick(
                "缺乏自然噪声整形上扬（疑似 PCM 转制）",
                "lacks natural noise-shaping rise (likely PCM-sourced)",
            ),
            DsdFlag::LowHfEnergy => lang.pick(
                "超高频能量异常偏低",
                "abnormally low ultrasonic energy",
            ),
            DsdFlag::CdCutoff => lang.pick(
                "22 kHz 处硬截止（疑似 CD/PCM 来源）",
                "hard cutoff at ~22 kHz (likely CD/PCM source)",
            ),
            DsdFlag::CdWall => lang.pick(
                "22.05 kHz 数字硬墙（疑似 CD/数字母带来源）",
                "sharp 22.05 kHz brick wall (likely CD/digital-master source)",
            ),
            DsdFlag::LossyCutoff => lang.pick(
                "低位截止（疑似有损来源）",
                "low baseband cutoff (likely lossy source)",
            ),
            DsdFlag::SuspiciousCutoff => lang.pick(
                "基带存在可疑截止",
                "suspicious baseband cutoff",
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VerdictStatus {
    Pass,
    Suspicious,
    Unsupported,
}

/// Full per-file verdict (also the JSON shape). Metrics are `None`/0 when `status == Unsupported`.
#[derive(Debug, Clone, Serialize)]
pub struct DsdVerdict {
    pub file: String,
    pub container: String,
    pub sample_rate: u64,
    pub channels: u32,
    pub noise_shaping_slope: f64,
    pub hf_ratio: f64,
    pub baseband_cutoff_hz: Option<f64>,
    pub flags: Vec<DsdFlag>,
    pub status: VerdictStatus,
}

/// Evaluate metrics against thresholds: collect flags and derive the status.
/// Any flag => `Suspicious`; none => `Pass`. (`Unsupported` is decided upstream.)
///
/// The **noise-shaping slope is the primary authenticity signal** — it is the one fingerprint a
/// transcoder cannot cheaply fake. The baseband cutoff is interpreted *relative to it*:
///
/// - **Slope confirms genuine SDM** (`slope >= min`): a baseband cutoff is mostly natural master
///   roll-off (a 2020 vocal/acoustic or analog-sourced master commonly rolls off ~18–21 kHz), so
///   it does **not** convict on its own. Only a sharp **CD wall** (~22.05 kHz — the one position
///   that survives a CD→DSD re-modulation even though SDM re-adds noise shaping) or a **hard low
///   cliff** (`< hard_lossy_cutoff_hz`, ~16.5 kHz) is still flagged.
/// - **Slope is flat** (`slope < min`): the file already lacks the DSD fingerprint, so a baseband
///   cutoff is *corroborating* evidence of a PCM/lossy source and is interpreted strictly (any
///   cutoff below `lossy_cutoff_max_hz`, or near the CD wall, flags).
///
/// `cd_wall` is a dedicated sharp-step detection at ~22.05 kHz (see `metrics::detect_cd_wall`). It
/// convicts even when the slope confirms genuine SDM — a brick wall exactly at the CD Nyquist is
/// the digital-ADC fingerprint that survives a CD→DSD re-modulation, which a gentle analog roll-off
/// (left alone above) does not produce.
pub fn judge(
    slope: f64,
    hf_ratio: f64,
    cutoff: Option<f64>,
    cd_wall: bool,
    th: &DsdThresholds,
) -> (Vec<DsdFlag>, VerdictStatus) {
    let mut flags = Vec::new();

    let slope_ok = slope >= th.min_noise_shaping_slope;
    if !slope_ok {
        flags.push(DsdFlag::WeakNoiseShaping);
    }
    if hf_ratio < th.min_hf_ratio {
        flags.push(DsdFlag::LowHfEnergy);
    }

    // A sharp brick wall at the CD Nyquist convicts regardless of slope.
    if cd_wall {
        flags.push(DsdFlag::CdWall);
    }

    if let Some(c) = cutoff {
        let near_cd_wall = (c - th.cd_cutoff_hz).abs() < th.cd_cutoff_tol_hz;
        if near_cd_wall {
            flags.push(DsdFlag::CdCutoff);
        } else if slope_ok {
            // Genuine DSD confirmed: only a hard low cliff still convicts; everything between
            // ~16.5 kHz and the CD wall (and above it) is treated as natural roll-off.
            if c < th.hard_lossy_cutoff_hz {
                flags.push(DsdFlag::LossyCutoff);
            }
        } else if c < th.lossy_cutoff_max_hz {
            flags.push(DsdFlag::LossyCutoff);
        } else {
            flags.push(DsdFlag::SuspiciousCutoff);
        }
    }

    let status = if flags.is_empty() {
        VerdictStatus::Pass
    } else {
        VerdictStatus::Suspicious
    };
    (flags, status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genuine_metrics_pass() {
        let th = DsdThresholds::default();
        let (flags, status) = judge(20.0, 0.30, None, false, &th);
        assert!(flags.is_empty());
        assert_eq!(status, VerdictStatus::Pass);
    }

    #[test]
    fn flat_slope_and_cd_wall_is_suspicious() {
        let th = DsdThresholds::default();
        let (flags, status) = judge(1.0, 0.30, Some(22_050.0), false, &th);
        assert_eq!(status, VerdictStatus::Suspicious);
        assert!(flags.contains(&DsdFlag::WeakNoiseShaping));
        assert!(flags.contains(&DsdFlag::CdCutoff));
    }

    /// Genuine DSD (strong slope) with a natural ~19 kHz vocal-master roll-off must NOT be flagged.
    /// This is the Emi Fujita DSD128 false-positive case that motivated the slope-gated logic.
    #[test]
    fn genuine_slope_with_natural_rolloff_passes() {
        let th = DsdThresholds::default();
        for cutoff in [17_916.0, 19_466.0, 19_638.0, 23_170.0] {
            let (flags, status) = judge(18.0, 0.99, Some(cutoff), false, &th);
            assert!(flags.is_empty(), "cutoff {cutoff} should not flag: {flags:?}");
            assert_eq!(status, VerdictStatus::Pass);
        }
    }

    /// Even with a genuine slope, a hard low cliff (<16.5 kHz) and a sharp CD wall still convict.
    #[test]
    fn genuine_slope_still_catches_hard_cliff_and_cd_wall() {
        let th = DsdThresholds::default();
        let (flags, status) = judge(18.0, 0.99, Some(15_000.0), false, &th);
        assert_eq!(status, VerdictStatus::Suspicious);
        assert!(flags.contains(&DsdFlag::LossyCutoff));

        let (flags, status) = judge(18.0, 0.99, Some(22_050.0), false, &th);
        assert_eq!(status, VerdictStatus::Suspicious);
        assert!(flags.contains(&DsdFlag::CdCutoff));
    }

    /// A detected sharp CD-Nyquist wall convicts even with a genuine slope and no rolloff cutoff —
    /// the digital brick wall that survives CD→DSD re-modulation and isn't a gentle analog roll-off.
    #[test]
    fn cd_wall_step_convicts_despite_genuine_slope() {
        let th = DsdThresholds::default();
        let (flags, status) = judge(20.0, 0.99, None, true, &th);
        assert_eq!(status, VerdictStatus::Suspicious);
        assert!(flags.contains(&DsdFlag::CdWall));
    }

    /// With a flat slope the mid-band cutoff is corroborating evidence and is judged strictly.
    #[test]
    fn flat_slope_with_mid_cutoff_is_strict() {
        let th = DsdThresholds::default();
        let (flags, status) = judge(1.0, 0.99, Some(19_000.0), false, &th);
        assert_eq!(status, VerdictStatus::Suspicious);
        assert!(flags.contains(&DsdFlag::WeakNoiseShaping));
        assert!(flags.contains(&DsdFlag::LossyCutoff));
    }
}
