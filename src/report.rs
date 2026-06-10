//! Report aggregation: human-readable text report and machine-readable JSON.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::i18n::Lang;
use crate::verdict::{Analysis, Verdict};

/// One file's outcome: either an analysis or a decode error (errors are surfaced, never dropped).
pub struct Outcome {
    pub path: PathBuf,
    pub result: Result<Analysis, String>,
}

/// First path component below `root` — the "album" bucket for aggregation. Files directly
/// under root are grouped under a placeholder.
fn album_of(root: &Path, path: &Path, lang: Lang) -> String {
    let rel = path.strip_prefix(root).unwrap_or(path);
    let comps: Vec<_> = rel.components().collect();
    if comps.len() > 1 {
        comps[0].as_os_str().to_string_lossy().into_owned()
    } else {
        lang.pick("(根目录直属文件)", "(files directly under root)")
            .to_string()
    }
}

/// Path shown in the report, relative to the scan root when possible.
fn rel_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

/// True for verdicts that belong on the flagged list (anything not Clean).
fn is_flagged(v: Verdict) -> bool {
    !matches!(v, Verdict::Clean)
}

/// Build the human-readable text report.
pub fn build_text_report(root: &Path, outcomes: &[Outcome], lang: Lang) -> String {
    use std::fmt::Write as _;

    let mut clean = 0usize;
    let mut narrowed = 0usize;
    let mut suspect = 0usize;
    let mut upsampled = 0usize;
    let mut errors: Vec<(&PathBuf, &str)> = Vec::new();
    // (cutoff, verdict, path) for every flagged track.
    let mut flagged: Vec<(f32, Verdict, &PathBuf)> = Vec::new();
    // album -> [bad_count (suspect + upsampled), narrowed_count]
    let mut albums: HashMap<String, [usize; 2]> = HashMap::new();

    for o in outcomes {
        match &o.result {
            Ok(a) => {
                match a.verdict {
                    Verdict::Clean => clean += 1,
                    Verdict::Narrowed => narrowed += 1,
                    Verdict::Suspect => suspect += 1,
                    Verdict::Upsampled => upsampled += 1,
                }
                if is_flagged(a.verdict) {
                    flagged.push((a.cutoff_hz, a.verdict, &o.path));
                    let slot = albums.entry(album_of(root, &o.path, lang)).or_insert([0, 0]);
                    match a.verdict {
                        Verdict::Narrowed => slot[1] += 1,
                        _ => slot[0] += 1, // suspect + upsampled
                    }
                }
            }
            Err(e) => errors.push((&o.path, e)),
        }
    }

    // Worst first (suspect/upsampled rank above narrowed); within a tier by cutoff ascending.
    flagged.sort_by(|a, b| {
        let rank = |v: Verdict| if matches!(v, Verdict::Narrowed) { 1 } else { 0 };
        rank(a.1)
            .cmp(&rank(b.1))
            .then(a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    });

    let mut album_rank: Vec<(&String, &[usize; 2])> = albums.iter().collect();
    album_rank.sort_by(|a, b| b.1[0].cmp(&a.1[0]).then(b.1[1].cmp(&a.1[1])).then(a.0.cmp(b.0)));

    let mut s = String::new();
    let _ = writeln!(s, "{}", lang.pick("假无损批量扫描报告", "Fake-lossless batch scan report"));
    let _ = writeln!(s, "{}: {}", lang.pick("生成时间", "Generated"), now_string());
    let _ = writeln!(s, "{}: {}", lang.pick("扫描根目录", "Scan root"), root.display());
    let _ = writeln!(s, "{}: {}", lang.pick("文件总数", "Total files"), outcomes.len());
    let _ = writeln!(s);
    let _ = writeln!(s, "== {} ==", lang.pick("汇总", "Summary"));
    let _ = writeln!(s, "  {}  {clean}", lang.pick("✅ 像真无损 (≥19kHz)        ", "✅ Likely lossless (≥19kHz)    "));
    let _ = writeln!(s, "  {}  {narrowed}", lang.pick("⚠️  高频收窄 (16.5-19kHz)    ", "⚠️  Narrowed HF (16.5-19kHz)    "));
    let _ = writeln!(s, "  {}  {suspect}", lang.pick("🚩 高度可疑 (<16.5kHz)      ", "🚩 Highly suspect (<16.5kHz)   "));
    let _ = writeln!(s, "  {}  {upsampled}", lang.pick("🔼 假 Hi-Res (上采样)        ", "🔼 Fake Hi-Res (upsampled)     "));
    let _ = writeln!(s, "  {}  {}", lang.pick("✖  解码失败                 ", "✖  Decode failed               "), errors.len());
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{}",
        lang.pick(
            "启发式判断：古典/人声/老录音/间奏(interlude)/skit 本身高频就少，可能误报；",
            "Heuristic: classical/vocal/old recordings/interludes/skits naturally have little HF and may false-positive;"
        )
    );
    let _ = writeln!(
        s,
        "{}",
        lang.pick(
            "整张专辑同档位低截止＝来源八成有损，这是最强信号。可疑文件请用 Spek 复核。",
            "a whole album sharing the same low cutoff is the strongest signal of a lossy source. Verify suspects with Spek."
        )
    );

    let _ = writeln!(s);
    let _ = writeln!(s, "== {} ==", lang.pick("按专辑排行（🚩/🔼 数量降序）", "Albums ranked (by 🚩/🔼 count, descending)"));
    if album_rank.iter().all(|(_, c)| c[0] == 0) {
        let _ = writeln!(s, "  {}", lang.pick("（无 🚩/🔼 文件）", "(no 🚩/🔼 files)"));
    }
    for (name, c) in album_rank.iter().filter(|(_, c)| c[0] > 0) {
        let _ = writeln!(s, "  🚩/🔼{:>3}  ⚠️{:>3}  {}", c[0], c[1], name);
    }

    let _ = writeln!(s);
    let _ = writeln!(s, "== {} ==", lang.pick("可疑文件清单（🚩/🔼 在前，各按截止频率升序）", "Flagged files (🚩/🔼 first, each by cutoff ascending)"));
    if flagged.is_empty() {
        let _ = writeln!(s, "  {}", lang.pick("（无）", "(none)"));
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
        let _ = writeln!(s, "== {} ==", lang.pick("解码失败（未参与判定，需人工检查）", "Decode failures (not classified; check manually)"));
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
    upsampled: usize,
    error: usize,
}

#[derive(Serialize)]
struct ResultJson {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    format_label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cutoff_hz: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ratio: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hires_ext_db: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    holes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<Verdict>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
pub struct ReportJson {
    root: String,
    scanned: usize,
    summary: SummaryJson,
    results: Vec<ResultJson>,
}

/// Build the JSON report structure.
pub fn build_json(root: &Path, outcomes: &[Outcome]) -> ReportJson {
    let mut summary = SummaryJson {
        clean: 0,
        narrowed: 0,
        suspect: 0,
        upsampled: 0,
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
                    Verdict::Upsampled => summary.upsampled += 1,
                }
                results.push(ResultJson {
                    path,
                    format_label: Some(a.format_label.clone()),
                    sample_rate: Some(a.sample_rate),
                    cutoff_hz: Some(a.cutoff_hz.round()),
                    ratio: Some((a.ratio * 10000.0).round() / 10000.0),
                    hires_ext_db: a.hires_ext_db.map(|db| (db * 10.0).round() / 10.0),
                    holes: Some(a.hole_count),
                    verdict: Some(a.verdict),
                    error: None,
                });
            }
            Err(e) => {
                summary.error += 1;
                results.push(ResultJson {
                    path,
                    format_label: None,
                    sample_rate: None,
                    cutoff_hz: None,
                    ratio: None,
                    hires_ext_db: None,
                    holes: None,
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
