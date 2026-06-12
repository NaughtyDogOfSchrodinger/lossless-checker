//! DSF (Sony) container parser. All fields little-endian.
//!
//! Layout: `DSD ` chunk (28 B) → `fmt ` chunk (52 B) → `data` chunk (12 B header + samples).
//! Bit order is LSB-first; channels are stored as **whole separated blocks** per group
//! (`[ch0 block][ch1 block]…`), not byte-interleaved.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::dsd::{BitOrder, BlockGroup, DsdContainer, DsdError, DsdMeta, DsdStream};

/// Sentinel meaning "data chunk size was implausible; read until physical EOF instead".
const UNKNOWN_REMAINING: u64 = u64::MAX;

pub struct DsfReader {
    reader: BufReader<File>,
    meta: DsdMeta,
    block_size: usize,
    channels: usize,
    /// Bytes of sample payload still to read, or `UNKNOWN_REMAINING`.
    remaining: u64,
    buf: Vec<u8>,
}

impl DsfReader {
    pub fn open(path: &Path) -> Result<Self, DsdError> {
        let mut reader = BufReader::new(File::open(path)?);

        // --- DSD chunk (28 bytes) ---
        expect_magic(&mut reader, b"DSD ")?;
        let dsd_size = read_u64(&mut reader)?;
        let _total_file_size = read_u64(&mut reader)?;
        let _metadata_ptr = read_u64(&mut reader)?;
        skip(&mut reader, dsd_size.saturating_sub(28))?;

        // --- fmt chunk (52 bytes) ---
        expect_magic(&mut reader, b"fmt ")?;
        let fmt_size = read_u64(&mut reader)?;
        let _version = read_u32(&mut reader)?;
        let format_id = read_u32(&mut reader)?;
        let _channel_type = read_u32(&mut reader)?;
        let channel_num = read_u32(&mut reader)?;
        let sampling_freq = read_u32(&mut reader)?;
        let bits_per_sample = read_u32(&mut reader)?;
        let sample_count = read_u64(&mut reader)?;
        let block_size = read_u32(&mut reader)?;
        let _reserved = read_u32(&mut reader)?;
        skip(&mut reader, fmt_size.saturating_sub(52))?;

        if format_id != 0 {
            return Err(DsdError::Unsupported(format!(
                "DSF format id {format_id} (only 0 = DSD raw)"
            )));
        }
        if bits_per_sample != 1 {
            return Err(DsdError::Unsupported(format!(
                "bits per sample = {bits_per_sample} (expected 1)"
            )));
        }
        if channel_num == 0 || block_size == 0 {
            return Err(DsdError::Unsupported(
                "zero channels or zero block size".to_string(),
            ));
        }

        // --- data chunk header (12 bytes) ---
        expect_magic(&mut reader, b"data")?;
        let data_size = read_u64(&mut reader)?;
        let remaining = if data_size >= 12 {
            data_size - 12
        } else {
            UNKNOWN_REMAINING
        };

        let channels = channel_num as usize;
        let group_bytes = channels * block_size as usize;

        Ok(Self {
            reader,
            meta: DsdMeta {
                format: DsdContainer::Dsf,
                channels: channel_num,
                sample_rate: sampling_freq as u64,
                bit_order: BitOrder::Lsb,
                total_samples_per_channel: Some(sample_count),
            },
            block_size: block_size as usize,
            channels,
            remaining,
            buf: vec![0u8; group_bytes],
        })
    }
}

impl DsdStream for DsfReader {
    fn meta(&self) -> &DsdMeta {
        &self.meta
    }

    fn next_block_group(&mut self) -> Result<Option<BlockGroup>, DsdError> {
        let group_bytes = self.buf.len();
        let want = if self.remaining == UNKNOWN_REMAINING {
            group_bytes
        } else {
            self.remaining.min(group_bytes as u64) as usize
        };
        if want == 0 {
            return Ok(None);
        }

        let n = read_up_to(&mut self.reader, &mut self.buf[..want])?;
        if n == 0 {
            return Ok(None);
        }
        if self.remaining != UNKNOWN_REMAINING {
            self.remaining -= n as u64;
        }

        // Split the group into per-channel whole blocks. The last group may be short; tail
        // channels then receive fewer (or zero) bytes, which is harmless for the spectrum.
        let mut channels = Vec::with_capacity(self.channels);
        for c in 0..self.channels {
            let start = (c * self.block_size).min(n);
            let end = ((c + 1) * self.block_size).min(n);
            channels.push(self.buf[start..end].to_vec());
        }
        Ok(Some(BlockGroup { channels }))
    }
}

/// Read until `buf` is full or EOF; returns the number of bytes actually read.
fn read_up_to<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<usize, DsdError> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(DsdError::Io(e)),
        }
    }
    Ok(filled)
}

fn expect_magic<R: Read>(r: &mut R, want: &'static [u8; 4]) -> Result<(), DsdError> {
    let mut m = [0u8; 4];
    read_exact(r, &mut m)?;
    if &m != want {
        return Err(DsdError::BadMagic {
            want: std::str::from_utf8(want).unwrap_or("????"),
        });
    }
    Ok(())
}

fn read_u32<R: Read>(r: &mut R) -> Result<u32, DsdError> {
    let mut b = [0u8; 4];
    read_exact(r, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64, DsdError> {
    let mut b = [0u8; 8];
    read_exact(r, &mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// `read_exact` mapping a clean EOF to `Truncated` rather than a raw I/O error.
fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), DsdError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(DsdError::Truncated),
        Err(e) => Err(DsdError::Io(e)),
    }
}

/// Skip `n` bytes by reading and discarding (BufReader-friendly for the small inter-chunk gaps).
fn skip<R: Read>(r: &mut R, n: u64) -> Result<(), DsdError> {
    if n == 0 {
        return Ok(());
    }
    let mut left = n;
    let mut scratch = [0u8; 512];
    while left > 0 {
        let want = left.min(scratch.len() as u64) as usize;
        read_exact(r, &mut scratch[..want])?;
        left -= want as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal valid DSF: header chunks + `frames` block-groups of `block_size` per
    /// channel, all bytes = `fill`. Returns the file bytes.
    pub(crate) fn synth_dsf(
        channels: u32,
        sample_rate: u32,
        block_size: u32,
        frames: u32,
        fill: u8,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        let data_payload = (channels * block_size * frames) as u64;

        // DSD chunk
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&28u64.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes()); // total file size (unused by reader)
        v.extend_from_slice(&0u64.to_le_bytes()); // metadata ptr

        // fmt chunk
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&52u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // version
        v.extend_from_slice(&0u32.to_le_bytes()); // format id (DSD raw)
        v.extend_from_slice(&0u32.to_le_bytes()); // channel type
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // bits per sample
        v.extend_from_slice(&((block_size * frames * 8) as u64).to_le_bytes()); // sample count/ch
        v.extend_from_slice(&block_size.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved

        // data chunk
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(12 + data_payload).to_le_bytes());
        v.extend(vec![fill; data_payload as usize]);
        v
    }

    fn write_temp(bytes: &[u8]) -> std::path::PathBuf {
        // Process-wide counter so parallel tests never collide on a filename.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("lc_dsf_test_{}_{id}.dsf", std::process::id()));
        let mut f = File::create(&p).unwrap();
        f.write_all(bytes).unwrap();
        p
    }

    #[test]
    fn parses_header_fields() {
        let bytes = synth_dsf(2, 2_822_400, 4096, 3, 0xAA);
        let p = write_temp(&bytes);
        let reader = DsfReader::open(&p).unwrap();
        let m = reader.meta();
        assert_eq!(m.format, DsdContainer::Dsf);
        assert_eq!(m.channels, 2);
        assert_eq!(m.sample_rate, 2_822_400);
        assert_eq!(m.bit_order, BitOrder::Lsb);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_all_block_groups_then_eof() {
        let bytes = synth_dsf(2, 2_822_400, 4096, 3, 0xFF);
        let p = write_temp(&bytes);
        let mut reader = DsfReader::open(&p).unwrap();
        let mut groups = 0;
        while let Some(g) = reader.next_block_group().unwrap() {
            assert_eq!(g.channels.len(), 2);
            assert_eq!(g.channels[0].len(), 4096);
            groups += 1;
        }
        assert_eq!(groups, 3);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut bytes = synth_dsf(2, 2_822_400, 4096, 1, 0);
        bytes[0] = b'X';
        let p = write_temp(&bytes);
        assert!(matches!(DsfReader::open(&p), Err(DsdError::BadMagic { .. })));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn truncated_header_is_rejected() {
        let bytes = synth_dsf(2, 2_822_400, 4096, 1, 0);
        let p = write_temp(&bytes[..20]); // cut off mid-header
        assert!(matches!(DsfReader::open(&p), Err(DsdError::Truncated)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn non_one_bit_is_unsupported() {
        let mut bytes = synth_dsf(2, 2_822_400, 4096, 1, 0);
        // bits_per_sample sits right after sampling_freq in the fmt chunk. Offset:
        // DSD(28) + "fmt "(4) + size(8) + version(4) + fmt_id(4) + ch_type(4) + ch_num(4)
        // + samp_freq(4) = 60; bits field at 60..64.
        bytes[60..64].copy_from_slice(&2u32.to_le_bytes());
        let p = write_temp(&bytes);
        assert!(matches!(DsfReader::open(&p), Err(DsdError::Unsupported(_))));
        let _ = std::fs::remove_file(&p);
    }
}
