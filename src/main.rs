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
        eprintln!("path does not exist or is not accessible: {}", cli.path.display());
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
fn run_single(path: &Path, threshold: f64, peak_db: Option<f64>) {
    let decoded = match decode_audio(path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("decode failed: {e}");
            exit(1);
        }
    };

    if decoded.samples.is_empty() {
        eprintln!("no samples were decoded");
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

    println!("File: {}", path.display());
    println!("Format: {}", decoded.format_label);
    println!("Sample rate: {} Hz", decoded.sample_rate);
    println!("Total samples: {}", decoded.samples.len());
    println!("Nyquist frequency: {:.0} Hz", nyquist);
    println!(
        "Estimated HF cutoff: {:.0} Hz ({:.1}% of Nyquist)",
        features.cutoff_hz,
        ratio * 100.0
    );
    if let Some(db) = features.hires_ext_db {
        println!("HF extension (>26kHz): {db:.1} dB (relative to spectral peak; lower = emptier highs)");
    }
    if features.holes.is_empty() {
        println!("Spectral holes: none significant");
    } else {
        println!(
            "Spectral holes: {} (informational only, does not affect the verdict)",
            features.holes.len()
        );
        for h in &features.holes {
            println!("  - {:.0}-{:.0} Hz (~{:.0} dB deep)", h.low_hz, h.high_hz, h.depth_db);
        }
    }
    println!();
    println!("Verdict: {}", verdict.sentence());
    println!();
    println!("(Note: classical, vocal and old recordings naturally have little HF and may false-positive;");
    println!("  for suspect files, eyeball the spectrum in a tool like Spek before concluding.)");
}

/// Directory mode: scan in parallel, then write the text report (and optional JSON).
fn run_batch(cli: &Cli) {
    let exts: Vec<String> = cli
        .ext
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    eprintln!("Collecting audio files… (extensions: {})", exts.join(", "));
    let mut files = collect_audio_files(&cli.path, &exts);
    files.sort();

    if files.is_empty() {
        eprintln!("no matching audio files found under {}", cli.path.display());
        exit(1);
    }

    let total = files.len();
    eprintln!("Found {total} files, scanning…");

    let done = AtomicUsize::new(0);
    let threshold = cli.threshold;
    let peak_db = if cli.noise_floor { None } else { Some(cli.peak_db) };
    let mut outcomes: Vec<Outcome> = files
        .par_iter()
        .map(|p| {
            let o = analyze_file(p, threshold, peak_db);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 25 == 0 || n == total {
                eprint!("\rScanning… {n}/{total}");
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
            Ok(()) => eprintln!("Text report written to: {}", p.display()),
            Err(e) => {
                eprintln!("failed to write text report ({}): {e}", p.display());
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
                eprintln!("failed to serialize JSON: {e}");
                exit(1);
            }
        };
        match std::fs::write(p, serialized) {
            Ok(()) => eprintln!("JSON report written to: {}", p.display()),
            Err(e) => {
                eprintln!("failed to write JSON report ({}): {e}", p.display());
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
fn analyze_file(path: &Path, threshold: f64, peak_db: Option<f64>) -> Outcome {
    let result = (|| {
        let decoded = decode_audio(path)?;
        if decoded.samples.is_empty() {
            return Err("no samples were decoded".to_string());
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
