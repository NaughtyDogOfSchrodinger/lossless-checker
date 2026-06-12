//! DFF (DSDIFF, Philips) container parser. IFF-style nested chunks, all fields big-endian.
//!
//! Layout: top-level `FRM8` form (formType `DSD `) holding `FVER`, `PROP` (with `FS `/`CHNL`/`CMPR`
//! sub-chunks), and the `DSD ` sound-data chunk (or `DST ` for compressed, which we don't decode).
//! Bit order is MSB-first; channels are **byte-interleaved** (`[ch0 1B][ch1 1B][ch0 1B]…`), unlike
//! DSF's whole-block separation. Each IFF chunk's data is padded to an even byte boundary.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use crate::dsd::{BitOrder, BlockGroup, DsdContainer, DsdError, DsdMeta, DsdStream};

/// Target read size per block group (bytes), rounded down to a whole number of channel frames.
const READ_TARGET: usize = 65_536;

pub struct DffReader {
    reader: BufReader<File>,
    meta: DsdMeta,
    channels: usize,
    /// Bytes of sound-data payload still to read (the `DSD ` chunk length).
    remaining: u64,
    /// Read-buffer length: a multiple of `channels`.
    group_bytes: usize,
    buf: Vec<u8>,
}

impl DffReader {
    pub fn open(path: &Path) -> Result<Self, DsdError> {
        let mut reader = BufReader::new(File::open(path)?);

        // --- FRM8 form header ---
        expect_id(&mut reader, b"FRM8")?;
        let _form_size = read_u64(&mut reader)?;
        let mut form_type = [0u8; 4];
        read_exact(&mut reader, &mut form_type)?;
        if &form_type != b"DSD " {
            return Err(DsdError::BadMagic { want: "FRM8/DSD " });
        }

        let mut sample_rate: Option<u32> = None;
        let mut channels: Option<u16> = None;
        let mut data_size: Option<u64> = None;

        // --- walk top-level chunks until the sound-data chunk (or clean EOF) ---
        while let Some(id) = read_id_opt(&mut reader)? {
            let size = read_u64(&mut reader)?;
            match &id {
                b"PROP" => {
                    let (sr, ch) = parse_prop(&mut reader, size)?;
                    sample_rate = sr.or(sample_rate);
                    channels = ch.or(channels);
                    skip_pad(&mut reader, size)?;
                }
                b"DSD " => {
                    // Sound data starts here; leave the reader positioned at it.
                    data_size = Some(size);
                    break;
                }
                b"DST " => {
                    return Err(DsdError::Unsupported("DST-compressed DSD".to_string()));
                }
                _ => {
                    skip(&mut reader, size)?;
                    skip_pad(&mut reader, size)?;
                }
            }
        }

        let sample_rate = sample_rate
            .ok_or_else(|| DsdError::Unsupported("DFF missing sample rate (FS )".to_string()))?;
        let channels = channels
            .ok_or_else(|| DsdError::Unsupported("DFF missing channel count (CHNL)".to_string()))?
            as usize;
        let remaining = data_size
            .ok_or_else(|| DsdError::Unsupported("DFF has no DSD sound-data chunk".to_string()))?;

        if channels == 0 {
            return Err(DsdError::Unsupported("zero channels".to_string()));
        }

        let group_bytes = (READ_TARGET / channels).max(1) * channels;
        let total_samples_per_channel = Some(remaining / channels as u64 * 8);

        Ok(Self {
            reader,
            meta: DsdMeta {
                format: DsdContainer::Dff,
                channels: channels as u32,
                sample_rate: sample_rate as u64,
                bit_order: BitOrder::Msb,
                total_samples_per_channel,
            },
            channels,
            remaining,
            group_bytes,
            buf: vec![0u8; group_bytes],
        })
    }
}

impl DsdStream for DffReader {
    fn meta(&self) -> &DsdMeta {
        &self.meta
    }

    fn next_block_group(&mut self) -> Result<Option<BlockGroup>, DsdError> {
        let want = self.remaining.min(self.group_bytes as u64) as usize;
        if want == 0 {
            return Ok(None);
        }
        let n = read_up_to(&mut self.reader, &mut self.buf[..want])?;
        if n == 0 {
            return Ok(None);
        }
        self.remaining -= n as u64;

        // De-interleave byte-wise: byte i belongs to channel (i % channels).
        let mut channels: Vec<Vec<u8>> = (0..self.channels)
            .map(|_| Vec::with_capacity(n / self.channels + 1))
            .collect();
        for (i, &b) in self.buf[..n].iter().enumerate() {
            channels[i % self.channels].push(b);
        }
        Ok(Some(BlockGroup { channels }))
    }
}

/// Parse a PROP chunk (read its `size` bytes whole — PROP is small) for FS and CHNL.
fn parse_prop<R: Read>(r: &mut R, size: u64) -> Result<(Option<u32>, Option<u16>), DsdError> {
    let mut body = vec![0u8; size as usize];
    read_exact(r, &mut body)?;
    if body.len() < 4 || &body[..4] != b"SND " {
        return Err(DsdError::Unsupported("DFF PROP is not a sound property chunk".to_string()));
    }

    let mut sample_rate = None;
    let mut channels = None;
    let mut pos = 4;
    while pos + 12 <= body.len() {
        let id = &body[pos..pos + 4];
        let sz = u64::from_be_bytes(body[pos + 4..pos + 12].try_into().unwrap()) as usize;
        pos += 12;
        let end = (pos + sz).min(body.len());
        let data = &body[pos..end];
        match id {
            b"FS  " if data.len() >= 4 => {
                sample_rate = Some(u32::from_be_bytes(data[..4].try_into().unwrap()));
            }
            b"CHNL" if data.len() >= 2 => {
                channels = Some(u16::from_be_bytes(data[..2].try_into().unwrap()));
            }
            b"CMPR" if data.len() >= 4 && &data[..4] == b"DST " => {
                return Err(DsdError::Unsupported("DST-compressed DSD".to_string()));
            }
            _ => {}
        }
        pos += sz;
        if sz % 2 == 1 {
            pos += 1; // chunk data is padded to an even length
        }
    }
    Ok((sample_rate, channels))
}

// --- byte helpers (big-endian) ---

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

fn expect_id<R: Read>(r: &mut R, want: &'static [u8; 4]) -> Result<(), DsdError> {
    let mut m = [0u8; 4];
    read_exact(r, &mut m)?;
    if &m != want {
        return Err(DsdError::BadMagic {
            want: std::str::from_utf8(want).unwrap_or("????"),
        });
    }
    Ok(())
}

/// Read a 4-byte chunk id, returning `None` on a clean EOF (no more chunks).
fn read_id_opt<R: Read>(r: &mut R) -> Result<Option<[u8; 4]>, DsdError> {
    let mut id = [0u8; 4];
    let mut filled = 0;
    while filled < 4 {
        match r.read(&mut id[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(None);
                }
                return Err(DsdError::Truncated);
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(DsdError::Io(e)),
        }
    }
    Ok(Some(id))
}

fn read_u64<R: Read>(r: &mut R) -> Result<u64, DsdError> {
    let mut b = [0u8; 8];
    read_exact(r, &mut b)?;
    Ok(u64::from_be_bytes(b))
}

fn read_exact<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<(), DsdError> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(DsdError::Truncated),
        Err(e) => Err(DsdError::Io(e)),
    }
}

fn skip<R: Read>(r: &mut R, n: u64) -> Result<(), DsdError> {
    if n == 0 {
        return Ok(());
    }
    let mut left = n;
    let mut scratch = [0u8; 4096];
    while left > 0 {
        let want = left.min(scratch.len() as u64) as usize;
        read_exact(r, &mut scratch[..want])?;
        left -= want as u64;
    }
    Ok(())
}

/// Skip the IFF pad byte that follows an odd-length chunk.
fn skip_pad<R: Read>(r: &mut R, size: u64) -> Result<(), DsdError> {
    if size % 2 == 1 {
        skip(r, 1)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn be64(n: u64) -> [u8; 8] {
        n.to_be_bytes()
    }

    /// Build a minimal valid stereo DFF. `compression` is the CMPR type (`b"DSD "` or `b"DST "`).
    /// Sound data is `frames` bytes/channel, byte-interleaved; ch0 bytes = 0xF0, ch1 = 0x0F.
    fn synth_dff(sample_rate: u32, compression: &[u8; 4], frames: usize) -> Vec<u8> {
        // PROP body: "SND " + FS + CHNL + CMPR
        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(b"FS  ");
        prop.extend_from_slice(&be64(4));
        prop.extend_from_slice(&sample_rate.to_be_bytes());
        prop.extend_from_slice(b"CHNL");
        prop.extend_from_slice(&be64(2 + 8)); // numChannels(2) + 2×channelId(4)
        prop.extend_from_slice(&2u16.to_be_bytes());
        prop.extend_from_slice(b"SLFT");
        prop.extend_from_slice(b"SRGT");
        prop.extend_from_slice(b"CMPR");
        prop.extend_from_slice(&be64(4 + 1 + 1)); // type(4) + count(1) + pad name len(1) -> even
        prop.extend_from_slice(compression);
        prop.extend_from_slice(&[0u8, 0u8]); // count + 1-byte pascal name (empty), already even

        // Sound data: interleaved [0xF0, 0x0F] × frames
        let mut sound = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            sound.push(0xF0);
            sound.push(0x0F);
        }

        let mut body = Vec::new();
        // FVER
        body.extend_from_slice(b"FVER");
        body.extend_from_slice(&be64(4));
        body.extend_from_slice(&[0x01, 0x05, 0x00, 0x00]);
        // PROP
        body.extend_from_slice(b"PROP");
        body.extend_from_slice(&be64(prop.len() as u64));
        body.extend_from_slice(&prop);
        // DSD sound data
        body.extend_from_slice(b"DSD ");
        body.extend_from_slice(&be64(sound.len() as u64));
        body.extend_from_slice(&sound);

        let mut v = Vec::new();
        v.extend_from_slice(b"FRM8");
        v.extend_from_slice(&be64((4 + body.len()) as u64));
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&body);
        v
    }

    fn write_temp(bytes: &[u8]) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("lc_dff_test_{}_{id}.dff", std::process::id()));
        File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    #[test]
    fn parses_header_fields() {
        let p = write_temp(&synth_dff(5_644_800, b"DSD ", 100));
        let reader = DffReader::open(&p).unwrap();
        let m = reader.meta();
        assert_eq!(m.format, DsdContainer::Dff);
        assert_eq!(m.channels, 2);
        assert_eq!(m.sample_rate, 5_644_800);
        assert_eq!(m.bit_order, BitOrder::Msb);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn deinterleaves_channels_byte_wise() {
        let p = write_temp(&synth_dff(2_822_400, b"DSD ", 5_000));
        let mut reader = DffReader::open(&p).unwrap();
        let mut ch0 = 0usize;
        let mut ch1 = 0usize;
        while let Some(g) = reader.next_block_group().unwrap() {
            assert_eq!(g.channels.len(), 2);
            assert!(g.channels[0].iter().all(|&b| b == 0xF0));
            assert!(g.channels[1].iter().all(|&b| b == 0x0F));
            ch0 += g.channels[0].len();
            ch1 += g.channels[1].len();
        }
        assert_eq!(ch0, 5_000);
        assert_eq!(ch1, 5_000);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn dst_compression_is_unsupported() {
        let p = write_temp(&synth_dff(2_822_400, b"DST ", 100));
        assert!(matches!(DffReader::open(&p), Err(DsdError::Unsupported(_))));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_form_type_is_rejected() {
        let mut bytes = synth_dff(2_822_400, b"DSD ", 10);
        bytes[0] = b'X'; // corrupt FRM8
        let p = write_temp(&bytes);
        assert!(matches!(DffReader::open(&p), Err(DsdError::BadMagic { .. })));
        let _ = std::fs::remove_file(&p);
    }
}
