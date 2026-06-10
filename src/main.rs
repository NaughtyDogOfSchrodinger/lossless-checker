// Fake-lossless detector — analyze one file or a whole library to flag audio that is not the
// genuine lossless it claims to be.
//
// Three signals, all derived from the decoded PCM (the file's container/metadata is never
// trusted):
//   1. High-frequency cutoff cliff — lossy transcodes (MP3/AAC) low-pass at a fixed Hz, leaving
//      almost no energy above it. Genuine lossless extends naturally toward ~20kHz.
//   2. Sample-rate authenticity — a hi-res (>48k) file whose real content walls at the CD
//      Nyquist (~22kHz) is CD/lossy material upsampled into a hi-res container.
//   3. Spectral holes — AAC/Vorbis-style notches (report-only; too false-positive-prone to judge on).
//
// This is a heuristic (classical/vocal/old recordings naturally have little HF and may false-
// positive); its value is batch-flagging the highly suspicious. It is strictly read-only.
//
// DSD (.dsf/.dff) is decoded via an ffmpeg subprocess; all other formats via symphonia.

mod decode;
mod i18n;
mod report;
mod spectrum;
mod verdict;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicUsize, Ordering};

use clap::Parser;
use rayon::prelude::*;

use decode::decode_audio;
use i18n::Lang;
use report::{build_json, build_text_report, Outcome};
use spectrum::SpectrumOpts;
use verdict::{classify, Analysis};

#[derive(Parser)]
#[command(name = "lossless-checker")]
#[command(about = "Detect fake-lossless audio (lossy transcodes / upsampled hi-res); can scan a whole folder", long_about = None)]
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

    /// Comma-separated extensions to scan (lossless containers + DSD; mp3 etc. are pointless here)
    #[arg(long, default_value = "flac,wav,m4a,aif,aiff,caf,alac,dsf,dff")]
    ext: String,

    /// Number of parallel worker threads (default: number of CPU cores)
    #[arg(long)]
    jobs: Option<usize>,

    /// Output language for logs and reports: zh (中文, default) or en. JSON is unaffected.
    #[arg(long, value_enum, default_value = "zh")]
    lang: Lang,
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
        run_single(&cli.path, cli.threshold, peak_db, cli.lang);
    } else if cli.path.is_dir() {
        run_batch(&cli);
    } else {
        eprintln!(
            "{}: {}",
            cli.lang.pick("路径不存在或无法访问", "path does not exist or is not accessible"),
            cli.path.display()
        );
        exit(1);
    }
}

/// Build the detection options from CLI knobs.
fn opts(threshold: f64, peak_db: Option<f64>) -> SpectrumOpts {
    SpectrumOpts {
        peak_db,
        threshold_mult: threshold,
    }
}

/// Single-file mode: print the detailed per-file verdict.
fn run_single(path: &Path, threshold: f64, peak_db: Option<f64>, lang: Lang) {
    let decoded = match decode_audio(path, lang) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{}: {e}", lang.pick("解码失败", "decode failed"));
            exit(1);
        }
    };

    if decoded.samples.is_empty() {
        eprintln!("{}", lang.pick("没有解码出任何采样", "no samples were decoded"));
        exit(1);
    }

    let features = spectrum::analyze(&decoded.samples, decoded.sample_rate, opts(threshold, peak_db));
    let nyquist = decoded.sample_rate as f32 / 2.0;
    let ratio = if nyquist > 0.0 {
        features.cutoff_hz / nyquist
    } else {
        0.0
    };
    let verdict = classify(&features, decoded.sample_rate);

    println!("{}: {}", lang.pick("文件", "File"), path.display());
    println!("{}: {}", lang.pick("格式", "Format"), decoded.format_label);
    println!("{}: {} Hz", lang.pick("采样率", "Sample rate"), decoded.sample_rate);
    println!("{}: {}", lang.pick("采样总数", "Total samples"), decoded.samples.len());
    println!("{}: {:.0} Hz", lang.pick("奈奎斯特频率", "Nyquist frequency"), nyquist);
    println!(
        "{}: {:.0} Hz ({:.1}% of Nyquist)",
        lang.pick("估计高频截止", "Estimated HF cutoff"),
        features.cutoff_hz,
        ratio * 100.0
    );
    if let Some(db) = features.hires_ext_db {
        println!(
            "{}: {db:.1} dB ({})",
            lang.pick("高频延伸(>26kHz)", "HF extension (>26kHz)"),
            lang.pick("相对频谱峰值；越低代表高频越空", "relative to spectral peak; lower = emptier highs")
        );
    }
    if features.holes.is_empty() {
        println!("{}", lang.pick("频谱空洞: 无明显空洞", "Spectral holes: none significant"));
    } else {
        println!(
            "{}: {} {}",
            lang.pick("频谱空洞", "Spectral holes"),
            features.holes.len(),
            lang.pick("处（仅供参考，不影响判定）", "(informational only, does not affect the verdict)")
        );
        for h in &features.holes {
            match lang {
                Lang::Zh => {
                    println!("  - {:.0}-{:.0} Hz（深约 {:.0} dB）", h.low_hz, h.high_hz, h.depth_db)
                }
                Lang::En => {
                    println!("  - {:.0}-{:.0} Hz (~{:.0} dB deep)", h.low_hz, h.high_hz, h.depth_db)
                }
            }
        }
    }
    println!();
    println!("{}: {}", lang.pick("判断", "Verdict"), verdict.sentence(lang));
    println!();
    println!(
        "{}",
        lang.pick(
            "（提示：古典乐、人声、老录音本身高频能量就少，可能误报；",
            "(Note: classical, vocal and old recordings naturally have little HF and may false-positive;"
        )
    );
    println!(
        "{}",
        lang.pick(
            "  建议对可疑文件用 Spek 等工具看一眼频谱图再下结论。）",
            "  for suspect files, eyeball the spectrum in a tool like Spek before concluding.)"
        )
    );
}

/// Directory mode: scan in parallel, then write the text report (and optional JSON).
fn run_batch(cli: &Cli) {
    let exts: Vec<String> = cli
        .ext
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    let lang = cli.lang;
    eprintln!(
        "{} ({}: {})",
        lang.pick("收集音频文件中…", "Collecting audio files…"),
        lang.pick("扩展名", "extensions"),
        exts.join(", ")
    );
    let mut files = collect_audio_files(&cli.path, &exts);
    files.sort();

    if files.is_empty() {
        eprintln!(
            "{} {}",
            lang.pick("在以下目录未找到匹配的音频文件:", "no matching audio files found under"),
            cli.path.display()
        );
        exit(1);
    }

    let total = files.len();
    eprintln!("{}", lang.pick(&format!("找到 {total} 个文件，开始扫描…"), &format!("Found {total} files, scanning…")));

    let done = AtomicUsize::new(0);
    let threshold = cli.threshold;
    let peak_db = if cli.noise_floor { None } else { Some(cli.peak_db) };
    let mut outcomes: Vec<Outcome> = files
        .par_iter()
        .map(|p| {
            let o = analyze_file(p, threshold, peak_db, lang);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 25 == 0 || n == total {
                eprint!("\r{} {n}/{total}", lang.pick("扫描中…", "Scanning…"));
                let _ = io::stderr().flush();
            }
            o
        })
        .collect();
    eprintln!();
    // Deterministic ordering for the report regardless of thread completion order.
    outcomes.sort_by(|a, b| a.path.cmp(&b.path));

    let text = build_text_report(&cli.path, &outcomes, lang);
    match &cli.report {
        Some(p) => match std::fs::write(p, &text) {
            Ok(()) => eprintln!("{}: {}", lang.pick("文本报告已写入", "Text report written to"), p.display()),
            Err(e) => {
                eprintln!("{} ({}): {e}", lang.pick("写文本报告失败", "failed to write text report"), p.display());
                exit(1);
            }
        },
        None => print!("{text}"),
    }

    if let Some(p) = &cli.json {
        let json = build_json(&cli.path, &outcomes);
        let serialized = match serde_json::to_string_pretty(&json) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {e}", lang.pick("序列化 JSON 失败", "failed to serialize JSON"));
                exit(1);
            }
        };
        match std::fs::write(p, serialized) {
            Ok(()) => eprintln!("{}: {}", lang.pick("JSON 报告已写入", "JSON report written to"), p.display()),
            Err(e) => {
                eprintln!("{} ({}): {e}", lang.pick("写 JSON 报告失败", "failed to write JSON report"), p.display());
                exit(1);
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
fn analyze_file(path: &Path, threshold: f64, peak_db: Option<f64>, lang: Lang) -> Outcome {
    let result = (|| {
        let decoded = decode_audio(path, lang)?;
        if decoded.samples.is_empty() {
            return Err(lang.pick("没有解码出任何采样", "no samples were decoded").to_string());
        }
        let features =
            spectrum::analyze(&decoded.samples, decoded.sample_rate, opts(threshold, peak_db));
        let nyquist = decoded.sample_rate as f32 / 2.0;
        Ok(Analysis {
            sample_rate: decoded.sample_rate,
            format_label: decoded.format_label,
            cutoff_hz: features.cutoff_hz,
            ratio: if nyquist > 0.0 {
                features.cutoff_hz / nyquist
            } else {
                0.0
            },
            hole_count: features.holes.len(),
            hires_ext_db: features.hires_ext_db,
            verdict: classify(&features, decoded.sample_rate),
        })
    })();
    Outcome {
        path: path.to_path_buf(),
        result,
    }
}
