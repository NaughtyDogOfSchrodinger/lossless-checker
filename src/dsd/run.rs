//! `check-dsd` orchestration: file discovery, parallel per-file analysis, and reporting.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::prelude::*;
use serde::Serialize;

use crate::dsd::dff::DffReader;
use crate::dsd::dsf::DsfReader;
use crate::dsd::judge::{judge, DsdThresholds, DsdVerdict, VerdictStatus};
use crate::dsd::metrics::{detect_baseband_cutoff, hf_energy_ratio, noise_shaping_slope, PowerSpectrum};
use crate::dsd::unpack::unpack_block;
use crate::dsd::welch::WelchPlan;
use crate::dsd::{DsdContainer, DsdError, DsdMeta, DsdStream};
use crate::i18n::Lang;
use crate::report::now_string;

/// Minimum number of FFT frames (per channel) for the metrics to be trustworthy. Below this the
/// file is too short to fit a reliable slope, so it is reported as Unsupported rather than judged.
const MIN_FFT_FRAMES: u64 = 8;

/// Everything `run_check_dsd` needs, assembled by the CLI layer (kept clap-free here).
pub struct DsdCheckArgs {
    pub paths: Vec<PathBuf>,
    pub fft_size: usize,
    pub thresholds: DsdThresholds,
    pub as_json: bool,
    pub album_summary: bool,
    pub verbose: bool,
    pub lang: Lang,
}

/// One file's outcome: a verdict (including the Unsupported status) or a parse failure.
struct DsdOutcome {
    path: PathBuf,
    result: Result<DsdVerdict, DsdError>,
}

/// Entry point for the `check-dsd` subcommand. Returns the process exit code.
pub fn run_check_dsd(args: DsdCheckArgs) -> i32 {
    let lang = args.lang;
    let exts = [String::from("dsf"), String::from("dff")];

    let mut files = Vec::new();
    for p in &args.paths {
        if p.is_dir() {
            files.extend(crate::collect_audio_files(p, &exts));
        } else if p.is_file() && has_dsd_ext(p) {
            files.push(p.clone());
        } else {
            eprintln!(
                "{}: {}",
                lang.pick("跳过（非 DSD 文件或路径不存在）", "skipped (not a DSD file or missing)"),
                p.display()
            );
        }
    }
    files.sort();
    files.dedup();

    if files.is_empty() {
        eprintln!("{}", lang.pick("未找到 .dsf / .dff 文件", "no .dsf / .dff files found"));
        return 1;
    }

    let total = files.len();
    eprintln!(
        "{}",
        lang.pick(
            &format!("找到 {total} 个 DSD 文件，开始分析…"),
            &format!("Found {total} DSD files, analyzing…")
        )
    );

    let done = AtomicUsize::new(0);
    let fft_size = args.fft_size;
    let th = &args.thresholds;
    let mut outcomes: Vec<DsdOutcome> = files
        .par_iter()
        .map(|p| {
            let result = analyze_one(p, fft_size, th);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n % 10 == 0 || n == total {
                eprint!("\r{} {n}/{total}", lang.pick("分析中…", "Analyzing…"));
                let _ = io::stderr().flush();
            }
            DsdOutcome { path: p.clone(), result }
        })
        .collect();
    eprintln!();
    outcomes.sort_by(|a, b| a.path.cmp(&b.path));

    let root = scan_root(&args.paths);
    if args.as_json {
        match serde_json::to_string_pretty(&build_json(&outcomes)) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("{}: {e}", lang.pick("序列化 JSON 失败", "failed to serialize JSON"));
                return 1;
            }
        }
    } else {
        print!("{}", build_text(&root, &outcomes, args.album_summary, args.verbose, lang));
    }
    0
}

fn has_dsd_ext(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("dsf") | Some("dff")
    )
}

/// Pick a display root: the single dir argument if given, else the common parent, else cwd-ish.
fn scan_root(paths: &[PathBuf]) -> PathBuf {
    if paths.len() == 1 {
        let p = &paths[0];
        if p.is_dir() {
            return p.clone();
        }
        if let Some(parent) = p.parent() {
            return parent.to_path_buf();
        }
    }
    PathBuf::from(".")
}

// --- per-file analysis ---

fn container_of(path: &Path) -> Option<DsdContainer> {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("dsf") => Some(DsdContainer::Dsf),
        Some("dff") => Some(DsdContainer::Dff),
        _ => None,
    }
}

/// Open a `.dsf`/`.dff` file as a boxed `DsdStream`. Shared by the verdict path and export.
pub(crate) fn open_reader(path: &Path) -> Result<Box<dyn DsdStream>, DsdError> {
    match container_of(path) {
        Some(DsdContainer::Dsf) => Ok(Box::new(DsfReader::open(path)?)),
        Some(DsdContainer::Dff) => Ok(Box::new(DffReader::open(path)?)),
        None => Err(DsdError::Unsupported("not a DSD extension".into())),
    }
}

fn analyze_one(path: &Path, fft_size: usize, th: &DsdThresholds) -> Result<DsdVerdict, DsdError> {
    let container = match container_of(path) {
        Some(c) => c,
        None => return Err(DsdError::Unsupported("not a DSD extension".into())),
    };

    let result = open_reader(path)
        .and_then(|mut r| analyze_stream(r.as_mut(), path, fft_size, th));

    // "Structurally valid but unhandled" (non-1-bit DSF, DST-compressed DFF, missing chunks) is a
    // normal Unsupported *verdict*, not a parse failure — only truncation/bad-magic/I/O are errors.
    match result {
        Ok(v) => Ok(v),
        Err(DsdError::Unsupported(_)) => Ok(unsupported(path, container)),
        Err(e) => Err(e),
    }
}

/// Per-channel mean power spectrum + frame count, as returned by `accumulate`.
pub(crate) type ChannelSpectra = Vec<(Vec<f64>, u64)>;

/// Stream every block group through one Welch accumulator per channel, returning each channel's
/// mean power spectrum + frame count along with the stream metadata. Shared by the verdict path
/// and `export-spectrum`.
pub(crate) fn accumulate(
    reader: &mut dyn DsdStream,
    fft_size: usize,
) -> Result<(DsdMeta, ChannelSpectra), DsdError> {
    let meta = reader.meta().clone();
    let channels = meta.channels.max(1) as usize;

    let plan = WelchPlan::new(fft_size);
    let mut accs: Vec<_> = (0..channels).map(|_| plan.accumulator()).collect();
    let mut scratch: Vec<f32> = Vec::new();

    while let Some(group) = reader.next_block_group()? {
        for (c, bytes) in group.channels.iter().enumerate() {
            if c >= accs.len() {
                break;
            }
            unpack_block(bytes, meta.bit_order, &mut scratch);
            accs[c].feed(&scratch);
        }
    }

    let per_channel = accs.into_iter().map(|a| a.finalize()).collect();
    Ok((meta, per_channel))
}

/// Average the per-channel mean power spectra (channels with no full frame are skipped). Returns
/// `None` when nothing usable was decoded or the file is too short for a reliable fit.
pub(crate) fn mix_power(per_channel: &[(Vec<f64>, u64)], fft_size: usize) -> Option<Vec<f64>> {
    let half = fft_size / 2;
    let mut sum = vec![0.0f64; half];
    let mut active = 0u64;
    let mut frames = 0u64;
    for (power, n) in per_channel {
        if *n > 0 {
            for (s, v) in sum.iter_mut().zip(power) {
                *s += v;
            }
            active += 1;
            frames = frames.max(*n);
        }
    }
    if active == 0 || frames < MIN_FFT_FRAMES {
        return None;
    }
    for s in sum.iter_mut() {
        *s /= active as f64;
    }
    Some(sum)
}

fn analyze_stream(
    reader: &mut dyn DsdStream,
    path: &Path,
    fft_size: usize,
    th: &DsdThresholds,
) -> Result<DsdVerdict, DsdError> {
    let (meta, per_channel) = accumulate(reader, fft_size)?;
    let sum = match mix_power(&per_channel, fft_size) {
        Some(s) => s,
        None => return Ok(unsupported(path, meta.format)),
    };

    let ps = PowerSpectrum::new(sum, meta.sample_rate, fft_size);
    let slope = noise_shaping_slope(&ps, th.slope_fit_lo_hz, th.slope_fit_hi_hz);
    let hf_ratio = hf_energy_ratio(&ps, th.hf_threshold_hz);
    let cutoff = detect_baseband_cutoff(&ps, th.baseband_max_hz);
    let (flags, status) = judge(slope, hf_ratio, cutoff, th);

    Ok(DsdVerdict {
        file: path.display().to_string(),
        container: meta.format.label().to_string(),
        sample_rate: meta.sample_rate,
        channels: meta.channels,
        noise_shaping_slope: slope,
        hf_ratio,
        baseband_cutoff_hz: cutoff,
        flags,
        status,
    })
}

fn unsupported(path: &Path, container: DsdContainer) -> DsdVerdict {
    DsdVerdict {
        file: path.display().to_string(),
        container: container.label().to_string(),
        sample_rate: 0,
        channels: 0,
        noise_shaping_slope: 0.0,
        hf_ratio: 0.0,
        baseband_cutoff_hz: None,
        flags: Vec::new(),
        status: VerdictStatus::Unsupported,
    }
}

// --- text report ---

fn status_icon(s: VerdictStatus) -> &'static str {
    match s {
        VerdictStatus::Pass => "✅",
        VerdictStatus::Suspicious => "🚩",
        VerdictStatus::Unsupported => "⛔",
    }
}

fn album_key(root: &Path, path: &Path, lang: Lang) -> String {
    crate::report::album_of(root, path, lang)
}

fn rel_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

fn build_text(
    root: &Path,
    outcomes: &[DsdOutcome],
    album_summary: bool,
    verbose: bool,
    lang: Lang,
) -> String {
    use std::fmt::Write as _;

    let (mut pass, mut susp, mut unsup) = (0usize, 0usize, 0usize);
    let mut errors: Vec<(&PathBuf, String)> = Vec::new();
    let mut suspicious: Vec<&DsdVerdict> = Vec::new();
    let mut all: Vec<&DsdVerdict> = Vec::new();
    // album -> suspicious count
    let mut albums: HashMap<String, usize> = HashMap::new();

    for o in outcomes {
        match &o.result {
            Ok(v) => {
                all.push(v);
                match v.status {
                    VerdictStatus::Pass => pass += 1,
                    VerdictStatus::Suspicious => {
                        susp += 1;
                        suspicious.push(v);
                        *albums.entry(album_key(root, &o.path, lang)).or_insert(0) += 1;
                    }
                    VerdictStatus::Unsupported => unsup += 1,
                }
            }
            Err(e) => errors.push((&o.path, e.to_string())),
        }
    }

    let mut s = String::new();
    let _ = writeln!(s, "{}", lang.pick("DSD 真伪检测报告", "DSD authenticity report"));
    let _ = writeln!(s, "{}: {}", lang.pick("生成时间", "Generated"), now_string());
    let _ = writeln!(s, "{}: {}", lang.pick("扫描根目录", "Scan root"), root.display());
    let _ = writeln!(s, "{}: {}", lang.pick("文件总数", "Total files"), outcomes.len());
    let _ = writeln!(s);
    let _ = writeln!(s, "== {} ==", lang.pick("汇总", "Summary"));
    let _ = writeln!(s, "  ✅ {}  {pass}", lang.pick("通过", "Pass        "));
    let _ = writeln!(s, "  🚩 {}  {susp}", lang.pick("可疑", "Suspicious  "));
    let _ = writeln!(s, "  ⛔ {}  {unsup}", lang.pick("暂不支持", "Unsupported "));
    let _ = writeln!(s, "  ✖  {}  {}", lang.pick("解析失败", "Parse error "), errors.len());
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{}",
        lang.pick(
            "判据：真 DSD 有强噪声整形上扬(>50kHz)；缺斜率/低超高频能量/基带硬截止＝疑似 PCM/有损转制。",
            "Genuine DSD shows strong noise-shaping rise (>50kHz); missing slope / low ultrasonic energy / a baseband cutoff => likely PCM/lossy-sourced."
        )
    );
    let _ = writeln!(
        s,
        "{}",
        lang.pick(
            "注意：阈值为起步经验值，需用真/假/DXD 样本标定后才可信。",
            "Note: thresholds are unstarted defaults; calibrate against real/fake/DXD samples before trusting them."
        )
    );

    if album_summary {
        let _ = writeln!(s);
        let _ = writeln!(s, "== {} ==", lang.pick("按专辑（🚩 数量降序）", "Albums (by 🚩 count, descending)"));
        if albums.is_empty() {
            let _ = writeln!(s, "  {}", lang.pick("（无可疑文件）", "(no suspicious files)"));
        } else {
            let mut ranked: Vec<(&String, &usize)> = albums.iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            for (name, count) in ranked {
                let _ = writeln!(s, "  🚩{count:>3}  {name}");
            }
        }
    }

    let _ = writeln!(s);
    let _ = writeln!(s, "== {} ==", lang.pick("可疑文件清单", "Suspicious files"));
    if suspicious.is_empty() {
        let _ = writeln!(s, "  {}", lang.pick("（无）", "(none)"));
    } else {
        // Worst first: lowest noise-shaping slope on top.
        suspicious.sort_by(|a, b| {
            a.noise_shaping_slope
                .partial_cmp(&b.noise_shaping_slope)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for v in &suspicious {
            let flags = v
                .flags
                .iter()
                .map(|f| f.message(lang))
                .collect::<Vec<_>>()
                .join(lang.pick("；", "; "));
            let _ = writeln!(
                s,
                "  🚩 slope={:+.1} dB/oct  HF={:.3}  {}  — {}",
                v.noise_shaping_slope,
                v.hf_ratio,
                cutoff_label(v.baseband_cutoff_hz, lang),
                rel_display_file(root, &v.file),
            );
            let _ = writeln!(s, "       {flags}");
        }
    }

    if verbose {
        let _ = writeln!(s);
        let _ = writeln!(s, "== {} ==", lang.pick("全部文件指标", "All file metrics"));
        all.sort_by(|a, b| a.file.cmp(&b.file));
        for v in &all {
            let _ = writeln!(
                s,
                "  {} slope={:+.1} HF={:.3} {} {}",
                status_icon(v.status),
                v.noise_shaping_slope,
                v.hf_ratio,
                cutoff_label(v.baseband_cutoff_hz, lang),
                rel_display_file(root, &v.file),
            );
        }
    }

    if !errors.is_empty() {
        let _ = writeln!(s);
        let _ = writeln!(s, "== {} ==", lang.pick("解析失败（需人工检查）", "Parse failures (check manually)"));
        for (path, err) in &errors {
            let _ = writeln!(s, "  ✖  {}  — {}", rel_display(root, path), err);
        }
    }

    s
}

fn cutoff_label(cutoff: Option<f64>, lang: Lang) -> String {
    match cutoff {
        Some(hz) => format!("{}={:.0}Hz", lang.pick("基带截止", "cutoff"), hz),
        None => lang.pick("无基带截止", "no cutoff").to_string(),
    }
}

/// `DsdVerdict.file` is already a display string; strip the root prefix for readability.
fn rel_display_file(root: &Path, file: &str) -> String {
    let p = Path::new(file);
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

// --- JSON report ---

#[derive(Serialize)]
struct DsdSummary {
    pass: usize,
    suspicious: usize,
    unsupported: usize,
    error: usize,
}

#[derive(Serialize)]
struct DsdErrorJson {
    file: String,
    error: String,
}

#[derive(Serialize)]
struct DsdReportJson<'a> {
    scanned: usize,
    summary: DsdSummary,
    results: Vec<&'a DsdVerdict>,
    errors: Vec<DsdErrorJson>,
}

fn build_json(outcomes: &[DsdOutcome]) -> DsdReportJson<'_> {
    let mut summary = DsdSummary { pass: 0, suspicious: 0, unsupported: 0, error: 0 };
    let mut results = Vec::new();
    let mut errors = Vec::new();

    for o in outcomes {
        match &o.result {
            Ok(v) => {
                match v.status {
                    VerdictStatus::Pass => summary.pass += 1,
                    VerdictStatus::Suspicious => summary.suspicious += 1,
                    VerdictStatus::Unsupported => summary.unsupported += 1,
                }
                results.push(v);
            }
            Err(e) => {
                summary.error += 1;
                errors.push(DsdErrorJson {
                    file: o.path.display().to_string(),
                    error: e.to_string(),
                });
            }
        }
    }

    DsdReportJson { scanned: outcomes.len(), summary, results, errors }
}
