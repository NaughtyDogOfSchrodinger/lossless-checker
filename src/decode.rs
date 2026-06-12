//! Decoding to mono f32 PCM.
//!
//! symphonia handles the lossless/PCM containers (FLAC, ALAC, WAV, AIFF, CAF, …) with no system
//! dependencies. DSD (.dsf/.dff) has no symphonia codec and is handled by the separate `check-dsd`
//! subcommand (native 1-bit analysis, see `crate::dsd`), not here.

use std::fs::File;
use std::path::Path;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::i18n::Lang;

/// Decoded result: mono PCM samples (multi-channel already mixed down) + sample rate +
/// a human-readable source-format label for the reports.
pub struct DecodedAudio {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub format_label: String,
}

/// Decode an audio file to mono f32 PCM. DSD is not handled here — point the user at `check-dsd`.
pub fn decode_audio(path: &Path, lang: Lang) -> Result<DecodedAudio, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if matches!(ext.as_deref(), Some("dsf") | Some("dff")) {
        return Err(lang
            .pick(
                "DSD 文件请用 check-dsd 子命令分析（check 仅处理 PCM/无损容器）",
                "DSD files: use the `check-dsd` subcommand (`check` handles PCM/lossless only)",
            )
            .to_string());
    }
    decode_symphonia(path, lang)
}

/// Decode with symphonia, mixing all channels down to mono.
fn decode_symphonia(path: &Path, lang: Lang) -> Result<DecodedAudio, String> {
    let file = File::open(path)
        .map_err(|e| format!("{}: {e}", lang.pick("打不开文件", "cannot open file")))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Use the file extension as a hint to help the probe identify the format.
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("{}: {e}", lang.pick("无法识别音频格式", "unrecognized audio format")))?;

    let mut format = probed.format;

    let track = format
        .default_track()
        .ok_or_else(|| lang.pick("找不到音频轨", "no audio track found").to_string())?;
    let track_id = track.id;

    let format_label = symphonia::default::get_codecs()
        .get_codec(track.codec_params.codec)
        .map(|d| d.short_name.to_uppercase())
        .unwrap_or_else(|| "PCM".to_string());

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("{}: {e}", lang.pick("无法创建解码器", "cannot create decoder")))?;

    // Decode the whole track (no early stop — see spectrum::avg_power_spectrum for why).
    let mut samples: Vec<f32> = Vec::new();
    let mut sample_rate: u32 = track.codec_params.sample_rate.unwrap_or(0);
    let mut channels: usize = 0;

    // next_packet returns Err at end of file (or on a read error) — either way we stop.
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = *audio_buf.spec();
                if sample_rate == 0 {
                    sample_rate = spec.rate;
                }
                channels = spec.channels.count();

                let mut sample_buf = SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec);
                sample_buf.copy_interleaved_ref(audio_buf);
                samples.extend_from_slice(sample_buf.samples());
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue, // skip bad frames
            Err(e) => return Err(format!("{}: {e}", lang.pick("解码出错", "decode error"))),
        }
    }

    if sample_rate == 0 {
        return Err(lang
            .pick("无法确定采样率", "could not determine sample rate")
            .to_string());
    }

    Ok(DecodedAudio {
        samples: mix_to_mono(samples, channels),
        sample_rate,
        format_label,
    })
}

/// Mix interleaved multi-channel samples down to mono (simple average).
fn mix_to_mono(samples: Vec<f32>, channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples;
    }
    samples
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}
