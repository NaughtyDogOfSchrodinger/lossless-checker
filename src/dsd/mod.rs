//! Native DSD authenticity checker.
//!
//! Unlike the PCM path (which decodes via ffmpeg to 88.2k and only sees the audible band), this
//! module parses the DSD container itself, pulls the raw 1-bit stream, and measures the full-band
//! power spectrum up to the DSD Nyquist (~1.41 MHz for DSD64). That exposes the one fingerprint a
//! transcoder cannot cheaply fake: **noise shaping** — the Sigma-Delta quantization noise pushed
//! up into 50–100 kHz. A genuine DSD master shows a strong positive slope there; a PCM/lossy source
//! "washed" into DSD does not, and often still carries a CD/lossy cutoff in the baseband.
//!
//! The bitstream is never expanded to memory in full (DSD64 mono is ~11 MB/s as f32). Every channel
//! is fed frame-by-frame into a streaming Welch accumulator and discarded.

mod dff;
mod dsf;
pub mod judge;
mod metrics;
mod run;
mod unpack;
mod welch;

pub use run::{run_check_dsd, DsdCheckArgs};

use std::fmt;

/// Unified DSD stream metadata, abstracting over the container differences.
#[derive(Debug, Clone)]
pub struct DsdMeta {
    pub format: DsdContainer,
    pub channels: u32,
    /// 2_822_400 = DSD64, 5_644_800 = DSD128, …
    pub sample_rate: u64,
    pub bit_order: BitOrder,
    /// Samples per channel, when the container records it (DFF may not). Parsed now; used by the
    /// second batch for exact tail trimming of the final (zero-padded) block.
    #[allow(dead_code)]
    pub total_samples_per_channel: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DsdContainer {
    Dsf,
    Dff,
}

impl DsdContainer {
    pub fn label(self) -> &'static str {
        match self {
            DsdContainer::Dsf => "DSF",
            DsdContainer::Dff => "DFF",
        }
    }
}

/// Bit order within a byte: which bit carries the earliest sample.
/// DSF = LSB-first (bit0 earliest); DFF = MSB-first (bit7 earliest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOrder {
    Lsb,
    /// Constructed by the DFF reader (second batch); already handled by `unpack_block`.
    #[allow(dead_code)]
    Msb,
}

/// One block group: each channel's raw DSD bytes for this group, already de-interleaved.
/// `channels[c]` is channel `c`'s bytes for the group.
pub struct BlockGroup {
    pub channels: Vec<Vec<u8>>,
}

/// Unified streaming read interface over DSF/DFF containers.
pub trait DsdStream {
    fn meta(&self) -> &DsdMeta;
    /// Read the next group (one block per channel); `Ok(None)` at EOF.
    fn next_block_group(&mut self) -> Result<Option<BlockGroup>, DsdError>;
}

/// Errors raised while parsing/reading a DSD container. Distinct from a *verdict* of
/// `Unsupported` (e.g. DFF/DST), which is a normal analysis result, not a failure.
#[derive(Debug)]
pub enum DsdError {
    Io(std::io::Error),
    /// A chunk magic did not match what the format requires.
    BadMagic { want: &'static str },
    /// The file ended before a required field/chunk was fully read.
    Truncated,
    /// Structurally valid but unsupported (non-1-bit, unknown format id, …).
    Unsupported(String),
}

impl fmt::Display for DsdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DsdError::Io(e) => write!(f, "I/O error: {e}"),
            DsdError::BadMagic { want } => write!(f, "bad chunk magic (expected {want:?})"),
            DsdError::Truncated => write!(f, "file truncated"),
            DsdError::Unsupported(why) => write!(f, "unsupported: {why}"),
        }
    }
}

impl std::error::Error for DsdError {}

impl From<std::io::Error> for DsdError {
    fn from(e: std::io::Error) -> Self {
        DsdError::Io(e)
    }
}
