// Fake-lossless detector — step one: analyze a single audio file's high-frequency cutoff.
//
// Principle: genuine lossless audio (e.g. 16bit/44.1kHz) carries real high-frequency energy
// that extends naturally toward ~20kHz. When audio is transcoded from a lossy format into FLAC
// (e.g. 320k MP3 cuts at ~20kHz; 128k at ~16kHz), the spectrum shows an "energy cliff" — almost
// no energy above that frequency. We run an FFT to move the signal into the frequency domain,
// measure how energy is distributed across frequency, and locate that cliff.
//
// Note: this is a heuristic, not 100% accurate (classical music and old recordings naturally have
// little high frequency and may trigger false positives; high-bitrate lossy may slip through). Its
// value is "batch-flagging the highly suspicious files"; the final call still needs manual review.

use std::fs::File;
use std::path::PathBuf;

use clap::Parser;
use rustfft::{num_complex::Complex, FftPlanner};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Parser)]
#[command(name = "lossless-checker")]
#[command(about = "检测音频文件是否为假无损（有损转码而来）", long_about = None)]
struct Cli {
    /// Path to the audio file to analyze
    file: PathBuf,

    /// Noise-floor multiplier: how many times above the noise floor a bin must be to count as
    /// real signal. The default 10.0 is calibrated against real files; override only for debugging.
    #[arg(long, default_value_t = 10.0)]
    threshold: f64,
}

/// Decoded result: mono PCM samples (multi-channel already mixed down) + sample rate.
struct DecodedAudio {
    samples: Vec<f32>,
    sample_rate: u32,
}

fn main() {
    let cli = Cli::parse();

    let decoded = match decode_audio(&cli.file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("解码失败: {e}");
            std::process::exit(1);
        }
    };

    println!("文件: {}", cli.file.display());
    println!("采样率: {} Hz", decoded.sample_rate);
    println!("采样总数: {}", decoded.samples.len());

    if decoded.samples.is_empty() {
        eprintln!("没有解码出任何采样");
        std::process::exit(1);
    }

    let cutoff = analyze_cutoff(&decoded, cli.threshold);
    report(&decoded, cutoff);
}

/// Decode an audio file with symphonia, mixing all channels down to mono.
fn decode_audio(path: &PathBuf) -> Result<DecodedAudio, String> {
    let file = File::open(path).map_err(|e| format!("打不开文件: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    // Use the file extension as a hint to help the probe identify the format
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
        .map_err(|e| format!("无法识别音频格式: {e}"))?;

    let mut format = probed.format;

    // Pick the first audio track
    let track = format
        .default_track()
        .ok_or_else(|| "找不到音频轨".to_string())?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("无法创建解码器: {e}"))?;

    // Only decode the first ~40s — enough for a stable high-frequency spectrum, far faster than
    // decoding full-length tracks.
    const ANALYZE_SECONDS: usize = 40;

    let mut samples: Vec<f32> = Vec::new();
    let mut sample_rate: u32 = track.codec_params.sample_rate.unwrap_or(0);
    let mut channels: usize = 0;

    // Decode packet by packet
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break, // end of file or a read error — stop
        };

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

                // Copy the decoded audio block into interleaved f32 samples
                let mut sample_buf =
                    SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec);
                sample_buf.copy_interleaved_ref(audio_buf);
                samples.extend_from_slice(sample_buf.samples());

                // The high-frequency cutoff is a global property of the track, stable after a
                // short stretch, so there's no need to decode the whole song. Stop once we have
                // enough audio (~ANALYZE_SECONDS); this is the dominant cost, so it's a big speedup.
                if channels > 0
                    && samples.len() >= sample_rate as usize * channels * ANALYZE_SECONDS
                {
                    break;
                }
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue, // skip bad frames
            Err(e) => return Err(format!("解码出错: {e}")),
        }
    }

    if sample_rate == 0 {
        return Err("无法确定采样率".to_string());
    }

    // Mix multi-channel down to mono (simple average) for single-path spectral analysis
    if channels > 1 {
        let mono: Vec<f32> = samples
            .chunks(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect();
        samples = mono;
    }

    Ok(DecodedAudio {
        samples,
        sample_rate,
    })
}

/// Run a windowed FFT over the audio, accumulate per-band energy, and find the cutoff frequency
/// where the high-frequency energy "cliff" sits. Returns the estimated cutoff frequency (Hz).
fn analyze_cutoff(audio: &DecodedAudio, threshold_mult: f64) -> f32 {
    const FFT_SIZE: usize = 8192; // freq resolution = sample_rate / FFT_SIZE, ~5-6 Hz/bin @44.1k

    if audio.samples.len() < FFT_SIZE {
        // File too short — just use the whole thing
        return audio.sample_rate as f32 / 2.0;
    }

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);

    // Accumulate per-bin energy (power spectrum), averaged over many windows for a stable result
    let mut energy = vec![0.0f64; FFT_SIZE / 2];
    let mut window_count = 0u64;

    // Take a window every so often (no need to process every sample — keeps the scan fast)
    let hop = FFT_SIZE; // no overlap; good enough and fast
    let mut pos = 0;
    while pos + FFT_SIZE <= audio.samples.len() {
        let mut buffer: Vec<Complex<f32>> = audio.samples[pos..pos + FFT_SIZE]
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                // Apply a Hann window to reduce spectral leakage
                let w = 0.5
                    - 0.5
                        * (2.0 * std::f32::consts::PI * i as f32 / FFT_SIZE as f32).cos();
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

    if window_count == 0 {
        return audio.sample_rate as f32 / 2.0;
    }

    // Average
    for e in energy.iter_mut() {
        *e /= window_count as f64;
    }

    let nyquist = audio.sample_rate as f32 / 2.0;
    let bin_hz = nyquist / (FFT_SIZE as f32 / 2.0);

    // Find the cutoff: scan from high frequency down to the first bin where energy clearly rises.
    // First estimate a noise floor (average energy of the topmost slice, usually just noise).
    let tail_start = (FFT_SIZE / 2) * 95 / 100; // take the top 5% of bins as a noise reference
    let noise_floor: f64 = {
        let tail = &energy[tail_start..];
        tail.iter().sum::<f64>() / tail.len().max(1) as f64
    };

    // This multiplier is the "how many times above the noise floor counts as real signal" threshold.
    // Too small treats noise as signal (overestimates the cutoff); too large misses weak highs.
    // Calibrated (multiplier swept 4->40 over real lossless vs. fake files):
    //   genuine-lossless cutoffs barely move with the multiplier, always topping out at ~22.5kHz
    //   (there is real high-frequency signal); fakes get lifted to a false 20-22kHz below ~5 and
    //   only reveal their 12-17kHz cliff at >=6. At >=10 it enters a stable plateau with clear
    //   separation and margin, so the default is 10.0 — override with --threshold.
    let threshold = noise_floor * threshold_mult;

    // Scan from the highest bin downward; the first bin above the threshold is the cutoff
    let mut cutoff_bin = FFT_SIZE / 2 - 1;
    for i in (0..FFT_SIZE / 2).rev() {
        if energy[i] > threshold {
            cutoff_bin = i;
            break;
        }
    }

    cutoff_bin as f32 * bin_hz
}

/// Turn the cutoff frequency and sample rate into a human-readable verdict.
fn report(audio: &DecodedAudio, cutoff_hz: f32) {
    let nyquist = audio.sample_rate as f32 / 2.0;
    let ratio = cutoff_hz / nyquist; // cutoff as a fraction of the Nyquist frequency

    println!("奈奎斯特频率: {:.0} Hz", nyquist);
    println!("估计高频截止: {:.0} Hz ({:.1}% of Nyquist)", cutoff_hz, ratio * 100.0);
    println!();

    // The verdict uses the absolute cutoff frequency (Hz) rather than a fraction of Nyquist:
    // lossy encoders low-pass at a fixed Hz (independent of the container sample rate), so a ratio
    // would wrongly flag 48kHz files (Nyquist 24kHz; genuine lossless also stops at ~21-22kHz, a
    // mere ~88% that looks suspicious). The thresholds below come from calibration on a real library:
    //   genuine lossless -> cutoff 19-23kHz (320k transcodes also often sit ~20kHz, hard to tell apart)
    //   256k transcode   -> cutoff ~18-19kHz
    //   128k transcode   -> cutoff ~16kHz  -- strongly suspect
    //   lower bitrate    -> cutoff 12-15kHz -- almost certainly fake lossless
    let _ = ratio; // ratio is shown for info only; it does not drive the verdict
    let verdict = if cutoff_hz >= 19000.0 {
        "✅ 高频延伸正常，像真无损"
    } else if cutoff_hz >= 16500.0 {
        "⚠️  高频有收窄（截止约 16.5-19kHz），可能是高码率有损转码，建议人工复核频谱"
    } else {
        "🚩 高频明显截断（截止 < 16.5kHz），高度疑似假无损（有损转码）"
    };

    println!("判断: {verdict}");
    println!();
    println!("（提示：古典乐、人声、老录音本身高频能量就少，可能误报；");
    println!("  建议对可疑文件用 Spek 等工具看一眼频谱图再下结论。）");
}
