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
use crate::dsd::metrics::{
    detect_baseband_cutoff, detect_cd_wall, hf_energy_ratio, noise_shaping_slope, PowerSpectrum,
};
use crate::dsd::unpack::unpack_block;
use crate::dsd::welch::{FrameWorker, WelchPlan};
use crate::dsd::{DsdContainer, DsdError, DsdMeta, DsdStream};
use crate::i18n::Lang;
use crate::report::now_string;

/// Minimum number of FFT frames (per channel) for the metrics to be trustworthy. Below this the
/// file is too short to fit a reliable slope, so it is reported as Unsupported rather than judged.
const MIN_FFT_FRAMES: u64 = 8;

/// Per-channel sample batch (≈ this many) unpacked before a parallel FFT flush. The 1-bit stream
/// expands to 32× its size as `f32`, so the whole file is never held in memory; this caps the
/// working set (~16 MB/channel at f32) while still giving each flush enough frames to parallelize.
const FLUSH_SAMPLES_PER_CHANNEL: usize = 4 * 1024 * 1024;

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

/// Decode every block group and return each channel's mean power spectrum + frame count along with
/// the stream metadata. Shared by the verdict path and `export-spectrum`.
///
/// Reading stays sequential (the container is a stream), but the expensive part — windowing + FFT —
/// runs in parallel: samples are unpacked into per-channel batches, and each full batch is FFT'd
/// frame-by-frame across rayon's pool ([`WelchPlan::power_sum`]). Each channel carries its trailing
/// partial frame into the next batch, so frames never straddle a batch boundary and the result is
/// identical to a single-threaded sweep.
pub(crate) fn accumulate(
    reader: &mut dyn DsdStream,
    fft_size: usize,
) -> Result<(DsdMeta, ChannelSpectra), DsdError> {
    let meta = reader.meta().clone();
    let channels = meta.channels.max(1) as usize;
    let half = fft_size / 2;

    let plan = WelchPlan::new(fft_size);
    let mut workers = plan.worker_pool(); // allocated once, reused across every flush
    // Per channel: running (power sum, frame count) and one persistent pending-sample buffer. Whole
    // frames are FFT'd on flush and drained, leaving only the (<fft_size) partial-frame tail — which
    // keeps the next batch's frames contiguous with this one's.
    let mut totals: ChannelSpectra = (0..channels).map(|_| (vec![0.0; half], 0u64)).collect();
    let mut pending: Vec<Vec<f32>> = (0..channels)
        .map(|_| Vec::with_capacity(FLUSH_SAMPLES_PER_CHANNEL + fft_size))
        .collect();
    let mut scratch: Vec<f32> = Vec::new();

    while let Some(group) = reader.next_block_group()? {
        for (c, bytes) in group.channels.iter().enumerate() {
            if c >= channels {
                break;
            }
            unpack_block(bytes, meta.bit_order, &mut scratch);
            pending[c].extend_from_slice(&scratch);
        }
        if pending[0].len() >= FLUSH_SAMPLES_PER_CHANNEL {
            flush_pending(&plan, &mut workers, &mut pending, &mut totals);
        }
    }
    flush_pending(&plan, &mut workers, &mut pending, &mut totals);

    // Convert the per-channel power *sums* to means now that every frame has been counted.
    let per_channel = totals
        .into_iter()
        .map(|(sum, count)| {
            let mean = if count > 0 {
                sum.iter().map(|s| s / count as f64).collect()
            } else {
                sum
            };
            (mean, count)
        })
        .collect();
    Ok((meta, per_channel))
}

/// FFT each channel's whole pending frames in parallel, add the per-bin power sums into `totals`,
/// then drain the consumed prefix so only the partial-frame tail carries forward (contiguity).
fn flush_pending(
    plan: &WelchPlan,
    workers: &mut [FrameWorker<'_>],
    pending: &mut [Vec<f32>],
    totals: &mut ChannelSpectra,
) {
    let fft_size = plan.fft_size();
    for (c, buf) in pending.iter_mut().enumerate() {
        let whole = (buf.len() / fft_size) * fft_size;
        if whole == 0 {
            continue;
        }
        totals[c].1 += plan.power_sum_into(&buf[..whole], workers, &mut totals[c].0);
        buf.drain(..whole); // shift the <fft_size remainder to the front, keep capacity
    }
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
    let cd_wall = detect_cd_wall(&ps, th.cd_cutoff_hz, th.cd_wall_step_db, th.cd_wall_floor_db);
    let (flags, status) = judge(slope, hf_ratio, cutoff, cd_wall, th);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsd::judge::DsdFlag;
    use std::f32::consts::PI;
    use std::io::Write as _;

    const SR: u32 = 2_822_400; // DSD64
    const BLOCK: u32 = 4096; // bytes/channel/group
    const N_SAMPLES: usize = 262_144; // = 8 groups of 32768 samples (mono), 32 frames @ fft 8192
    const FFT: usize = 8192;

    /// Sum of equal-amplitude tones from 100 Hz up to `f_hi`, decorrelated by a per-tone phase and
    /// normalized to 0.4 peak — a band-limited broadband source. Above `f_hi` the only energy in
    /// the modulated output is the SDM's shaped quantization noise.
    fn band_limited(f_hi: f32) -> Vec<f32> {
        let mut sig = vec![0f32; N_SAMPLES];
        let mut f = 100.0;
        while f < f_hi {
            let w = 2.0 * PI * f / SR as f32;
            for (i, s) in sig.iter_mut().enumerate() {
                *s += (i as f32 * w + f).sin(); // `+ f` = per-tone phase offset
            }
            f += 200.0;
        }
        // Normalize to a fairly hot 0.6 peak: the ±1 quantizer noise power is fixed, so a louder
        // signal sits further above the shaped noise floor, giving the baseband cutoff a clean step
        // to detect (mirroring a real, well-modulated CD→DSD).
        let peak = sig.iter().fold(0f32, |m, &x| m.max(x.abs()));
        if peak > 0.0 {
            let g = 0.6 / peak;
            for s in &mut sig {
                *s *= g;
            }
        }
        sig
    }

    /// Second-order error-feedback sigma-delta modulator (noise transfer (1-z^-1)^2): shapes
    /// quantization noise upward at ~+12 dB/oct with low in-band noise — the genuine-DSD
    /// fingerprint, in miniature. Stable for the modest (0.4-peak) inputs used here.
    fn sdm2(signal: &[f32]) -> Vec<bool> {
        let mut e1 = 0.0f64;
        let mut e2 = 0.0f64;
        signal
            .iter()
            .map(|&x| {
                let v = x as f64 + 2.0 * e1 - e2;
                let y = if v >= 0.0 { 1.0 } else { -1.0 };
                e2 = e1;
                e1 = v - y;
                y > 0.0
            })
            .collect()
    }

    /// Pack ±1 bits LSB-first (DSF order): bit i of sample 8k+i goes into byte k.
    fn pack_lsb(bits: &[bool]) -> Vec<u8> {
        bits.chunks(8)
            .map(|c| c.iter().enumerate().fold(0u8, |b, (i, &on)| b | ((on as u8) << i)))
            .collect()
    }

    /// Wrap mono DSD bytes in a minimal valid DSF container.
    fn build_dsf_mono(data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"DSD ");
        v.extend_from_slice(&28u64.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(&0u64.to_le_bytes());
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&52u64.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // version
        v.extend_from_slice(&0u32.to_le_bytes()); // format id (DSD raw)
        v.extend_from_slice(&0u32.to_le_bytes()); // channel type
        v.extend_from_slice(&1u32.to_le_bytes()); // 1 channel
        v.extend_from_slice(&SR.to_le_bytes());
        v.extend_from_slice(&1u32.to_le_bytes()); // bits
        v.extend_from_slice(&((data.len() * 8) as u64).to_le_bytes());
        v.extend_from_slice(&BLOCK.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&((12 + data.len()) as u64).to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    fn write_temp(bytes: &[u8]) -> PathBuf {
        use std::sync::atomic::AtomicU64;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("lc_dsd_integ_{}_{id}.dsf", std::process::id()));
        std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        p
    }

    /// Lower the slope gate so the 1st-order modulator's ~+6 dB/oct counts as genuine SDM.
    fn test_thresholds() -> DsdThresholds {
        DsdThresholds { min_noise_shaping_slope: 3.0, ..Default::default() }
    }

    /// Naive 1-bit conversion with NO noise shaping: just the sign of each sample. The quantization
    /// noise stays white (flat), so the noise-shaping slope is ~0 — a "PCM crudely dumped to 1-bit"
    /// fake that lacks the DSD fingerprint.
    fn sign_quantize(signal: &[f32]) -> Vec<bool> {
        signal.iter().map(|&x| x >= 0.0).collect()
    }

    /// Run a ready-made bitstream through the full container → analysis → verdict pipeline.
    fn verdict_of(bits: &[bool]) -> DsdVerdict {
        let data = pack_lsb(bits);
        let path = write_temp(&build_dsf_mono(&data));
        let v = analyze_one(&path, FFT, &test_thresholds()).unwrap();
        let _ = std::fs::remove_file(&path);
        v
    }

    /// A genuine SDM bitstream of a 26 kHz-band source: noise shaping present, no baseband cutoff,
    /// judged Pass through the full container → analysis → verdict pipeline.
    #[test]
    fn synthetic_genuine_dsd_passes() {
        let v = verdict_of(&sdm2(&band_limited(26_000.0)));
        assert!(v.noise_shaping_slope > 3.0, "slope = {}", v.noise_shaping_slope);
        assert_eq!(v.baseband_cutoff_hz, None, "unexpected cutoff: {:?}", v.baseband_cutoff_hz);
        assert_eq!(v.status, VerdictStatus::Pass, "flags = {:?}", v.flags);
    }

    /// The same source dumped to 1-bit WITHOUT noise shaping: the slope is flat, so the file lacks
    /// the DSD fingerprint and is judged Suspicious. (The complementary "CD→DSD with a 22 kHz wall
    /// but genuine slope" case is covered by the judge unit tests.)
    #[test]
    fn synthetic_unshaped_fake_is_suspicious() {
        let v = verdict_of(&sign_quantize(&band_limited(12_000.0)));
        assert!(v.noise_shaping_slope < 3.0, "slope unexpectedly steep: {}", v.noise_shaping_slope);
        assert_eq!(v.status, VerdictStatus::Suspicious);
        assert!(v.flags.contains(&DsdFlag::WeakNoiseShaping), "flags = {:?}", v.flags);
    }
}
