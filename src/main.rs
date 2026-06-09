// Fake-lossless detector — analyze the high-frequency cutoff of one file or a whole library.
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
//
// Pass a single file for a detailed verdict, or a directory to scan the whole library in parallel
// and emit a ranked text report (+ optional JSON). It is strictly read-only: it never moves or
// deletes anything.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::Parser;
use rayon::prelude::*;
use rustfft::{num_complex::Complex, FftPlanner};
use serde::Serialize;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Parser)]
#[command(name = "lossless-checker")]
#[command(about = "检测音频文件是否为假无损（有损转码而来）；可批量扫描整个文件夹", long_about = None)]
struct Cli {
    /// Path to a single audio file, or a directory to scan recursively
    path: PathBuf,

    /// Peak-relative cutoff threshold, in dB below the spectral peak. This is the default detection
    /// method: the cutoff is the highest frequency whose energy stays within this many dB of the
    /// track's own peak. Calibrated to 65.0; override only for debugging.
    #[arg(long, default_value_t = 65.0)]
    peak_db: f64,

    /// Use the legacy noise-floor detection method (with --threshold) instead of peak-relative.
    #[arg(long)]
    noise_floor: bool,

    /// Noise-floor multiplier for the legacy method (only used with --noise-floor).
    #[arg(long, default_value_t = 10.0)]
    threshold: f64,

    /// Write the text report to this file instead of stdout (directory scan only)
    #[arg(long)]
    report: Option<PathBuf>,

    /// Also write a JSON report to this file (directory scan only)
    #[arg(long)]
    json: Option<PathBuf>,

    /// Comma-separated extensions to scan (lossless containers only; mp3 etc. are pointless here)
    #[arg(long, default_value = "flac,wav,m4a,aif,aiff")]
    ext: String,

    /// Number of parallel worker threads (default: number of CPU cores)
    #[arg(long)]
    jobs: Option<usize>,
}

/// Decoded result: mono PCM samples (multi-channel already mixed down) + sample rate.
struct DecodedAudio {
    samples: Vec<f32>,
    sample_rate: u32,
}

// Verdict cutoffs in absolute Hz. Lossy encoders low-pass at a fixed Hz (independent of the
// container sample rate), so an absolute threshold is correct; a fraction-of-Nyquist would wrongly
// flag 48kHz files whose genuine content also stops at ~21-22kHz. Calibrated for the default
// peak-relative detector against a real library plus known-answer round-trip fakes (a real FLAC
// re-encoded through 128k/320k MP3 and back):
//   genuine lossless -> cutoff ~19-22kHz   128k transcode -> ~16.0-16.7kHz (caught as 🚩)
//   320k transcode   -> ~20kHz, overlaps genuine roll-off and is largely undetectable by cutoff.
const CUTOFF_CLEAN: f32 = 19000.0;
const CUTOFF_NARROW: f32 = 16800.0;

/// Three-tier verdict, shared by the console output, the text report and the JSON.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Verdict {
    Clean,
    Narrowed,
    Suspect,
}

impl Verdict {
    fn icon(self) -> &'static str {
        match self {
            Verdict::Clean => "✅",
            Verdict::Narrowed => "⚠️",
            Verdict::Suspect => "🚩",
        }
    }

    /// Full Chinese sentence used in the single-file detailed output.
    fn sentence(self) -> &'static str {
        match self {
            Verdict::Clean => "✅ 高频延伸正常，像真无损",
            Verdict::Narrowed => "⚠️  高频有收窄（截止约 16.5-19kHz），可能是高码率有损转码，建议人工复核频谱",
            Verdict::Suspect => "🚩 高频明显截断（截止 < 16.5kHz），高度疑似假无损（有损转码）",
        }
    }
}

/// Classify a cutoff frequency into a verdict tier.
fn classify(cutoff_hz: f32) -> Verdict {
    if cutoff_hz >= CUTOFF_CLEAN {
        Verdict::Clean
    } else if cutoff_hz >= CUTOFF_NARROW {
        Verdict::Narrowed
    } else {
        Verdict::Suspect
    }
}

/// Per-file analysis summary (no raw samples — just what the reports need).
struct Analysis {
    sample_rate: u32,
    cutoff_hz: f32,
    ratio: f32,
    verdict: Verdict,
}

/// One file's outcome: either an analysis or a decode error (errors are surfaced, never dropped).
struct Outcome {
    path: PathBuf,
    result: Result<Analysis, String>,
}

fn main() {
    let cli = Cli::parse();

    if let Some(jobs) = cli.jobs {
        // Best-effort: ignore the error if a global pool was already built.
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global();
    }

    // None = legacy noise-floor method (with --threshold); Some(db) = peak-relative (default).
    let peak_db = if cli.noise_floor { None } else { Some(cli.peak_db) };

    if cli.path.is_file() {
        run_single(&cli.path, cli.threshold, peak_db);
    } else if cli.path.is_dir() {
        run_batch(&cli);
    } else {
        eprintln!("路径不存在或无法访问: {}", cli.path.display());
        std::process::exit(1);
    }
}

/// Single-file mode: print the detailed per-file verdict (backward-compatible behaviour).
fn run_single(path: &Path, threshold: f64, peak_db: Option<f64>) {
    let decoded = match decode_audio(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("解码失败: {e}");
            std::process::exit(1);
        }
    };

    if decoded.samples.is_empty() {
        eprintln!("没有解码出任何采样");
        std::process::exit(1);
    }

    let cutoff = analyze_cutoff(&decoded, threshold, peak_db);
    let nyquist = decoded.sample_rate as f32 / 2.0;
    let ratio = cutoff / nyquist;

    println!("文件: {}", path.display());
    println!("采样率: {} Hz", decoded.sample_rate);
    println!("采样总数: {}", decoded.samples.len());
    println!("奈奎斯特频率: {:.0} Hz", nyquist);
    println!(
        "估计高频截止: {:.0} Hz ({:.1}% of Nyquist)",
        cutoff,
        ratio * 100.0
    );
    println!();
    println!("判断: {}", classify(cutoff).sentence());
    println!();
    println!("（提示：古典乐、人声、老录音本身高频能量就少，可能误报；");
    println!("  建议对可疑文件用 Spek 等工具看一眼频谱图再下结论。）");
}

/// Directory mode: scan in parallel, then write the text report (and optional JSON).
fn run_batch(cli: &Cli) {
    let exts: Vec<String> = cli
        .ext
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    eprintln!("收集音频文件中… (扩展名: {})", exts.join(", "));
    let mut files = collect_audio_files(&cli.path, &exts);
    files.sort();

    if files.is_empty() {
        eprintln!("在 {} 下没有找到匹配的音频文件", cli.path.display());
        std::process::exit(1);
    }

    let total = files.len();
    eprintln!("找到 {total} 个文件，开始扫描…");

    let done = AtomicUsize::new(0);
    let threshold = cli.threshold;
    let peak_db = if cli.noise_floor { None } else { Some(cli.peak_db) };
    let mut outcomes: Vec<Outcome> = files
        .par_iter()
        .map(|p| {
            let o = analyze_file(p, threshold, peak_db);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 25 == 0 || n == total {
                eprint!("\r扫描中… {n}/{total}");
                let _ = io::stderr().flush();
            }
            o
        })
        .collect();
    eprintln!();
    // Deterministic ordering for the report regardless of thread completion order.
    outcomes.sort_by(|a, b| a.path.cmp(&b.path));

    let text = build_text_report(&cli.path, &outcomes);
    match &cli.report {
        Some(p) => match std::fs::write(p, &text) {
            Ok(()) => eprintln!("文本报告已写入: {}", p.display()),
            Err(e) => {
                eprintln!("写文本报告失败 ({}): {e}", p.display());
                std::process::exit(1);
            }
        },
        None => print!("{text}"),
    }

    if let Some(p) = &cli.json {
        let json = build_json(&cli.path, &outcomes);
        let serialized = match serde_json::to_string_pretty(&json) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("序列化 JSON 失败: {e}");
                std::process::exit(1);
            }
        };
        match std::fs::write(p, serialized) {
            Ok(()) => eprintln!("JSON 报告已写入: {}", p.display()),
            Err(e) => {
                eprintln!("写 JSON 报告失败 ({}): {e}", p.display());
                std::process::exit(1);
            }
        }
    }
}

/// Recursively collect files whose extension matches `exts`. Skips symlinked directories to avoid
/// cycles; unreadable entries are silently skipped (they can't be audio we care about).
fn collect_audio_files(root: &Path, exts: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_into(root, exts, &mut out);
    out
}

fn collect_into(dir: &Path, exts: &[String], out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Use symlink-aware metadata so we never follow a symlinked directory into a loop.
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            if !meta.file_type().is_symlink() {
                collect_into(&path, exts, out);
            }
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if exts.iter().any(|want| want == &ext.to_ascii_lowercase()) {
                out.push(path);
            }
        }
    }
}

/// Decode + analyze one file, capturing any failure as an error string (never panics).
fn analyze_file(path: &Path, threshold: f64, peak_db: Option<f64>) -> Outcome {
    let result = (|| {
        let decoded = decode_audio(path)?;
        if decoded.samples.is_empty() {
            return Err("没有解码出任何采样".to_string());
        }
        let cutoff = analyze_cutoff(&decoded, threshold, peak_db);
        let nyquist = decoded.sample_rate as f32 / 2.0;
        Ok(Analysis {
            sample_rate: decoded.sample_rate,
            cutoff_hz: cutoff,
            ratio: if nyquist > 0.0 { cutoff / nyquist } else { 0.0 },
            verdict: classify(cutoff),
        })
    })();
    Outcome {
        path: path.to_path_buf(),
        result,
    }
}

/// Decode an audio file with symphonia, mixing all channels down to mono.
fn decode_audio(path: &Path) -> Result<DecodedAudio, String> {
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

    // Decode the whole track. We deliberately do NOT early-stop after the first N seconds: many
    // songs (especially ballads) open with a quiet piano/vocal intro and only bring in the
    // high-frequency content — cymbals, percussion — later. Sampling just the intro underestimates
    // the cutoff and produces false positives. Throughput comes from parallelism instead (rayon).
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
fn analyze_cutoff(audio: &DecodedAudio, threshold_mult: f64, peak_db: Option<f64>) -> f32 {
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

    // Default method: peak-relative detection. Reference the cutoff against the spectrum's own peak
    // (the loudest bin, usually low-mid) rather than the top-band noise floor. The cutoff is the
    // highest frequency whose smoothed energy stays within `db` dB of that peak. This is robust to
    // brickwall cuts: above a hard low-pass the energy collapses far below the strong mid-band
    // reference, so faint residue no longer masquerades as real signal. This fixed the noise-floor
    // method's failure mode where clean transcode cuts on weak-HF content (orchestral/vocal) read
    // as full-band. Calibrated to db=65 against known-answer 128k/320k round-trip fakes.
    if let Some(db) = peak_db {
        // Smooth the power spectrum with a small moving average so isolated bins don't trip it.
        let half = FFT_SIZE / 2;
        let win = 9usize;
        let mut smooth = vec![0.0f64; half];
        for i in 0..half {
            let lo = i.saturating_sub(win / 2);
            let hi = (i + win / 2 + 1).min(half);
            let slice = &energy[lo..hi];
            smooth[i] = slice.iter().sum::<f64>() / slice.len() as f64;
        }
        let peak = smooth.iter().cloned().fold(0.0f64, f64::max);
        if peak <= 0.0 {
            return nyquist;
        }
        let thresh = peak * 10f64.powf(-db / 10.0); // dB below peak, in power
        let mut cutoff_bin = 0usize;
        for i in (0..half).rev() {
            if smooth[i] > thresh {
                cutoff_bin = i;
                break;
            }
        }
        return cutoff_bin as f32 * bin_hz;
    }

    // Find the cutoff: scan from high frequency down to the first bin where energy clearly rises.
    // First estimate a noise floor (average energy of the topmost slice, usually just noise).
    let tail_start = (FFT_SIZE / 2) * 95 / 100; // take the top 5% of bins as a noise reference
    let noise_floor: f64 = {
        let tail = &energy[tail_start..];
        tail.iter().sum::<f64>() / tail.len().max(1) as f64
    };

    // Legacy noise-floor method (--noise-floor). Kept for comparison; the peak-relative method
    // above is the default because this one false-negatives on clean cuts of weak-HF content.
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

/// First path component below `root` — used as the "album" bucket for aggregation. Files sitting
/// directly under root are grouped under a placeholder.
fn album_of(root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let comps: Vec<_> = rel.components().collect();
    if comps.len() > 1 {
        comps[0].as_os_str().to_string_lossy().into_owned()
    } else {
        "(根目录直属文件)".to_string()
    }
}

/// Path shown in the report, relative to the scan root when possible.
fn rel_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// Build the human-readable text report.
fn build_text_report(root: &Path, outcomes: &[Outcome]) -> String {
    use std::fmt::Write as _;

    let mut clean = 0usize;
    let mut narrowed = 0usize;
    let mut suspect = 0usize;
    let mut errors: Vec<(&PathBuf, &str)> = Vec::new();
    // (cutoff, verdict, path) for every flagged track
    let mut flagged: Vec<(f32, Verdict, &PathBuf)> = Vec::new();
    // album -> [suspect_count, narrowed_count]
    let mut albums: HashMap<String, [usize; 2]> = HashMap::new();

    for o in outcomes {
        match &o.result {
            Ok(a) => match a.verdict {
                Verdict::Clean => clean += 1,
                Verdict::Narrowed | Verdict::Suspect => {
                    if a.verdict == Verdict::Suspect {
                        suspect += 1;
                    } else {
                        narrowed += 1;
                    }
                    flagged.push((a.cutoff_hz, a.verdict, &o.path));
                    let slot = albums.entry(album_of(root, &o.path)).or_insert([0, 0]);
                    if a.verdict == Verdict::Suspect {
                        slot[0] += 1;
                    } else {
                        slot[1] += 1;
                    }
                }
            },
            Err(e) => errors.push((&o.path, e)),
        }
    }

    // Suspect first, then narrowed; within each tier by cutoff ascending (worst at the top).
    flagged.sort_by(|a, b| {
        let rank = |v: Verdict| if v == Verdict::Suspect { 0 } else { 1 };
        rank(a.1)
            .cmp(&rank(b.1))
            .then(a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut album_rank: Vec<(&String, &[usize; 2])> = albums.iter().collect();
    album_rank.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(b.1[1].cmp(&a.1[1])).then(a.0.cmp(b.0)));

    let mut s = String::new();
    let _ = writeln!(s, "假无损批量扫描报告");
    let _ = writeln!(s, "生成时间: {}", now_string());
    let _ = writeln!(s, "扫描根目录: {}", root.display());
    let _ = writeln!(s, "文件总数: {}", outcomes.len());
    let _ = writeln!(s);
    let _ = writeln!(s, "== 汇总 ==");
    let _ = writeln!(s, "  ✅ 像真无损 (≥19kHz)        {clean}");
    let _ = writeln!(s, "  ⚠️  高频收窄 (16.5-19kHz)    {narrowed}");
    let _ = writeln!(s, "  🚩 高度可疑 (<16.5kHz)      {suspect}");
    let _ = writeln!(s, "  ✖  解码失败                 {}", errors.len());
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "启发式判断：古典/人声/老录音/间奏(interlude)/skit 本身高频就少，可能误报；"
    );
    let _ = writeln!(s, "整张专辑同档位低截止＝来源八成有损，这是最强信号。可疑文件请用 Spek 复核。");

    let _ = writeln!(s);
    let _ = writeln!(s, "== 按专辑排行（🚩 数量降序）==");
    if album_rank.iter().all(|(_, c)| c[0] == 0) {
        let _ = writeln!(s, "  （无 🚩 文件）");
    }
    for (name, c) in album_rank.iter().filter(|(_, c)| c[0] > 0) {
        let _ = writeln!(s, "  🚩{:>3}  ⚠️{:>3}  {}", c[0], c[1], name);
    }

    let _ = writeln!(s);
    let _ = writeln!(s, "== 可疑文件清单（🚩 在前，各按截止频率升序）==");
    if flagged.is_empty() {
        let _ = writeln!(s, "  （无）");
    }
    for (cutoff, verdict, path) in &flagged {
        let _ = writeln!(
            s,
            "  {:>6.0} Hz  {}  {}",
            cutoff,
            verdict.icon(),
            rel_display(root, path)
        );
    }

    if !errors.is_empty() {
        let _ = writeln!(s);
        let _ = writeln!(s, "== 解码失败（未参与判定，需人工检查）==");
        for (path, err) in &errors {
            let _ = writeln!(s, "  ✖  {}  — {}", rel_display(root, path), err);
        }
    }

    s
}

/// JSON shapes (serialized with serde — handles paths containing CJK/quotes correctly).
#[derive(Serialize)]
struct SummaryJson {
    clean: usize,
    narrowed: usize,
    suspect: usize,
    error: usize,
}

#[derive(Serialize)]
struct ResultJson {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cutoff_hz: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ratio: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<Verdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct ReportJson {
    root: String,
    scanned: usize,
    summary: SummaryJson,
    results: Vec<ResultJson>,
}

/// Build the JSON report structure.
fn build_json(root: &Path, outcomes: &[Outcome]) -> ReportJson {
    let mut summary = SummaryJson {
        clean: 0,
        narrowed: 0,
        suspect: 0,
        error: 0,
    };
    let mut results = Vec::with_capacity(outcomes.len());

    for o in outcomes {
        let path = rel_display(root, &o.path);
        match &o.result {
            Ok(a) => {
                match a.verdict {
                    Verdict::Clean => summary.clean += 1,
                    Verdict::Narrowed => summary.narrowed += 1,
                    Verdict::Suspect => summary.suspect += 1,
                }
                results.push(ResultJson {
                    path,
                    sample_rate: Some(a.sample_rate),
                    cutoff_hz: Some(a.cutoff_hz.round()),
                    ratio: Some((a.ratio * 10000.0).round() / 10000.0),
                    verdict: Some(a.verdict),
                    error: None,
                });
            }
            Err(e) => {
                summary.error += 1;
                results.push(ResultJson {
                    path,
                    sample_rate: None,
                    cutoff_hz: None,
                    ratio: None,
                    verdict: None,
                    error: Some(e.clone()),
                });
            }
        }
    }

    ReportJson {
        root: root.to_string_lossy().into_owned(),
        scanned: outcomes.len(),
        summary,
        results,
    }
}

/// Local timestamp via the `date` command; empty string if it isn't available.
fn now_string() -> String {
    std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}
