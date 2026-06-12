//! `export-spectrum`: dump a single file's averaged power spectrum as CSV for plotting
//! (the genuine-vs-fake DSD spectrum comparison that drives threshold calibration).

use std::ffi::OsString;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::dsd::metrics::PowerSpectrum;
use crate::dsd::run::{accumulate, mix_power, open_reader};
use crate::i18n::Lang;

/// Power floor (linear) so empty bins map to a finite dB instead of -inf.
const POWER_FLOOR: f64 = 1e-30;

/// Which channel to export.
pub enum ChannelSel {
    /// Average of all channels (default — most representative for a comparison plot).
    Mix,
    /// A specific 0-based channel index.
    Index(usize),
}

pub struct ExportArgs {
    pub file: PathBuf,
    pub fft_size: usize,
    pub output: Option<PathBuf>,
    pub channel: ChannelSel,
    pub lang: Lang,
}

/// Entry point for `export-spectrum`. Returns the process exit code.
pub fn run_export_spectrum(args: ExportArgs) -> i32 {
    let lang = args.lang;

    let mut reader = match open_reader(&args.file) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}: {e}", lang.pick("打开失败", "failed to open"));
            return 1;
        }
    };

    // Single-file export: parallelize this file's frames across the whole pool.
    let (meta, per_channel) = match accumulate(reader.as_mut(), args.fft_size, true) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{}: {e}", lang.pick("读取失败", "failed to read"));
            return 1;
        }
    };

    let power = match select_power(&args.channel, &per_channel, args.fft_size) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("{}: {msg}", lang.pick("无法导出", "cannot export"));
            return 1;
        }
    };

    let ps = PowerSpectrum::new(power, meta.sample_rate, args.fft_size);
    let out = args.output.unwrap_or_else(|| default_output(&args.file));

    match write_csv(&out, &ps) {
        Ok(rows) => {
            eprintln!(
                "{}: {} ({} {})",
                lang.pick("已写入频谱 CSV", "Spectrum CSV written"),
                out.display(),
                rows,
                lang.pick("行", "rows")
            );
            0
        }
        Err(e) => {
            eprintln!("{} ({}): {e}", lang.pick("写 CSV 失败", "failed to write CSV"), out.display());
            1
        }
    }
}

/// Resolve the requested channel selection into a single power spectrum.
fn select_power(
    sel: &ChannelSel,
    per_channel: &[(Vec<f64>, u64)],
    fft_size: usize,
) -> Result<Vec<f64>, String> {
    match sel {
        ChannelSel::Mix => {
            mix_power(per_channel, fft_size).ok_or_else(|| "no usable audio / file too short".into())
        }
        ChannelSel::Index(i) => {
            let (power, frames) = per_channel
                .get(*i)
                .ok_or_else(|| format!("channel {i} out of range (have {})", per_channel.len()))?;
            if *frames == 0 {
                return Err(format!("channel {i} produced no FFT frames"));
            }
            Ok(power.clone())
        }
    }
}

fn default_output(file: &Path) -> PathBuf {
    let mut s: OsString = file.into();
    s.push(".spectrum.csv");
    PathBuf::from(s)
}

/// Write `frequency_hz,power_db` rows. Returns the number of data rows written.
fn write_csv(out: &Path, ps: &PowerSpectrum) -> std::io::Result<usize> {
    let mut w = BufWriter::new(std::fs::File::create(out)?);
    writeln!(w, "frequency_hz,power_db")?;
    let mut rows = 0;
    for (i, &p) in ps.power.iter().enumerate() {
        let freq = i as f64 * ps.bin_hz;
        let db = 10.0 * p.max(POWER_FLOOR).log10();
        writeln!(w, "{freq:.2},{db:.4}")?;
        rows += 1;
    }
    w.flush()?;
    Ok(rows)
}
