//! Verdict tiers and the sample-rate-aware classifier.

use serde::Serialize;

use crate::i18n::Lang;
use crate::spectrum::SpectralFeatures;

// CD-rate cutoffs in absolute Hz. Lossy encoders low-pass at a fixed Hz (independent of the
// container sample rate), so an absolute threshold is correct; a fraction-of-Nyquist would
// wrongly flag 48kHz files whose genuine content also stops at ~21-22kHz. Calibrated for the
// peak-relative detector against a real library plus known-answer round-trip fakes:
//   genuine lossless -> ~19-22kHz   128k transcode -> ~16.0-16.7kHz (caught)
//   320k transcode   -> ~20kHz, overlaps genuine roll-off and is largely undetectable.
const CUTOFF_CLEAN: f32 = 19_000.0;
const CUTOFF_NARROW: f32 = 16_800.0;

// Hi-res sample-rate-authenticity thresholds. Reasoned defaults, pending calibration against a
// labelled fixture set (none exists yet). A genuine hi-res master extends well past the CD
// wall; CD/lossy content upsampled into a hi-res container walls at ~22kHz with an empty band
// above it.
const HIRES_MIN_EXT: f32 = 28_000.0; // real content below this on a >48k file => upsampled
const HIRES_EMPTY_DB: f32 = -70.0; // [26k..nyquist] this far below peak => empty band

/// Four-tier verdict, shared by the console output, the text report and the JSON.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Clean,
    Narrowed,
    Suspect,
    Upsampled,
}

impl Verdict {
    pub fn icon(self) -> &'static str {
        match self {
            Verdict::Clean => "✅",
            Verdict::Narrowed => "⚠️",
            Verdict::Suspect => "🚩",
            Verdict::Upsampled => "🔼",
        }
    }

    /// Full sentence used in the single-file detailed output.
    pub fn sentence(self, lang: Lang) -> &'static str {
        match self {
            Verdict::Clean => lang.pick(
                "✅ 高频延伸正常，像真无损",
                "✅ High frequencies extend naturally — looks like genuine lossless",
            ),
            Verdict::Narrowed => lang.pick(
                "⚠️  高频有收窄（截止约 16.5-19kHz），可能是高码率有损转码，建议人工复核频谱",
                "⚠️  High frequencies are narrowed (cutoff ~16.5-19kHz) — possibly a high-bitrate lossy transcode; review the spectrum manually",
            ),
            Verdict::Suspect => lang.pick(
                "🚩 高频明显截断（截止 < 16.5kHz），高度疑似假无损（有损转码）",
                "🚩 High frequencies are clearly cut (cutoff < 16.5kHz) — highly likely fake lossless (lossy transcode)",
            ),
            Verdict::Upsampled => lang.pick(
                "🔼 声称为 Hi-Res，但真实内容止于 ~CD 频段，疑似上采样/有损转制的假 Hi-Res",
                "🔼 Declared as Hi-Res, but real content stops at the ~CD band — likely upsampled / lossy-sourced fake Hi-Res",
            ),
        }
    }
}

/// Per-file analysis summary (no raw samples — just what the reports need).
pub struct Analysis {
    pub sample_rate: u32,
    pub format_label: String,
    pub cutoff_hz: f32,
    pub ratio: f32,
    pub hole_count: usize,
    pub hires_ext_db: Option<f32>,
    pub verdict: Verdict,
}

/// Classify spectral features into a verdict, taking the declared sample rate into account.
///
/// Hi-res PCM (> 48k): real content must extend past the CD wall, otherwise the file is CD/lossy
/// content upsampled into a hi-res container (`Upsampled`). CD-rate: the absolute-Hz cutoff
/// thresholds on the audible band. Holes are intentionally NOT consulted here (report-only).
pub fn classify(f: &SpectralFeatures, sample_rate: u32) -> Verdict {
    if sample_rate > 48_000 {
        // `map_or(true, …)` instead of `is_none_or` so the crate builds on Rust < 1.82.
        let empty = f.hires_ext_db.map_or(true, |db| db < HIRES_EMPTY_DB);
        if f.cutoff_hz < HIRES_MIN_EXT || empty {
            return Verdict::Upsampled;
        }
        return Verdict::Clean;
    }

    if f.cutoff_hz >= CUTOFF_CLEAN {
        Verdict::Clean
    } else if f.cutoff_hz >= CUTOFF_NARROW {
        Verdict::Narrowed
    } else {
        Verdict::Suspect
    }
}
