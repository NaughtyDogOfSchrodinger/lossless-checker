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
    pub lossy_cutoff_max_hz: f64,
    pub slope_fit_lo_hz: f64,
    pub slope_fit_hi_hz: f64,
    pub hf_threshold_hz: f64,
    /// Audible-band ceiling for baseband cutoff detection (only ≤ this is inspected).
    pub baseband_max_hz: f64,
}

impl Default for DsdThresholds {
    fn default() -> Self {
        Self {
            min_noise_shaping_slope: 6.0,
            min_hf_ratio: 0.05,
            cd_cutoff_hz: 22_050.0,
            cd_cutoff_tol_hz: 1_000.0,
            lossy_cutoff_max_hz: 20_000.0,
            slope_fit_lo_hz: 30_000.0,
            slope_fit_hi_hz: 100_000.0,
            hf_threshold_hz: 50_000.0,
            baseband_max_hz: 24_000.0,
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
pub fn judge(
    slope: f64,
    hf_ratio: f64,
    cutoff: Option<f64>,
    th: &DsdThresholds,
) -> (Vec<DsdFlag>, VerdictStatus) {
    let mut flags = Vec::new();

    if slope < th.min_noise_shaping_slope {
        flags.push(DsdFlag::WeakNoiseShaping);
    }
    if hf_ratio < th.min_hf_ratio {
        flags.push(DsdFlag::LowHfEnergy);
    }
    if let Some(c) = cutoff {
        if (c - th.cd_cutoff_hz).abs() < th.cd_cutoff_tol_hz {
            flags.push(DsdFlag::CdCutoff);
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
        let (flags, status) = judge(20.0, 0.30, None, &th);
        assert!(flags.is_empty());
        assert_eq!(status, VerdictStatus::Pass);
    }

    #[test]
    fn flat_slope_and_cd_wall_is_suspicious() {
        let th = DsdThresholds::default();
        let (flags, status) = judge(1.0, 0.30, Some(22_050.0), &th);
        assert_eq!(status, VerdictStatus::Suspicious);
        assert!(flags.contains(&DsdFlag::WeakNoiseShaping));
        assert!(flags.contains(&DsdFlag::CdCutoff));
    }
}
