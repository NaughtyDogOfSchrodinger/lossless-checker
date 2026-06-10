# lossless-checker

[中文说明](./README.zh-CN.md)

A heuristic detector for **fake lossless** audio — files transcoded from a lossy source
(e.g. 320k MP3) and then re-wrapped as FLAC/ALAC to masquerade as lossless. Point it at one
file for a verdict, or at a whole library for a ranked report of the suspects.

- **Read-only** — it analyzes and reports; it never moves, renames, or deletes anything.
- **Self-contained** — pure-Rust audio decoding, no system dependencies (ffmpeg etc.).
- **Parallel** — a multi-thousand-file library scans in a few minutes on a modern machine.

## How it works

Genuine lossless audio (e.g. 16bit/44.1kHz) carries real high-frequency energy that extends
naturally toward ~20–22 kHz. A lossy codec applies a low-pass filter at a fixed frequency, leaving
an **energy cliff** in the spectrum — and that cliff frequency betrays the original bitrate:

| Source            | Typical cutoff |
|-------------------|----------------|
| Genuine lossless  | ~19–23 kHz     |
| 256k transcode    | ~18–19 kHz     |
| 128k transcode    | ~16 kHz        |
| lower bitrate     | ~12–15 kHz     |

The tool decodes the audio (via [symphonia](https://github.com/pdeljanov/Symphonia)), runs a
windowed FFT ([rustfft](https://github.com/ejmahler/RustFFT)) across the **whole track**, averages
the power spectrum, and finds the cutoff as the highest frequency whose energy still stays within
~65 dB of the track's own spectral peak (a **peak-relative** threshold). That cutoff frequency is
the verdict signal.

Two deliberate design points:

- **Peak-relative, not noise-floor-relative.** Referencing the loud mid-band peak (instead of the
  near-silent top of the spectrum) means a hard low-pass shows up as a true cliff even when the
  track has little high-frequency energy to begin with — orchestral and vocal transcodes that a
  noise-floor method reads as "full-band" are caught correctly.
- **Whole-track analysis.** Many songs open with a quiet intro and only bring in cymbals/percussion
  later, so sampling just the head would underestimate the cutoff and cause false positives.

## Install

Grab a prebuilt binary from the [latest release](https://github.com/NaughtyDogOfSchrodinger/lossless-checker/releases/latest) — no toolchain required. Pick the asset for your platform:

| Platform                 | Asset                                                  |
|--------------------------|--------------------------------------------------------|
| macOS (Apple Silicon)    | `lossless-checker-<ver>-aarch64-apple-darwin.tar.gz`   |
| macOS (Intel)            | `lossless-checker-<ver>-x86_64-apple-darwin.tar.gz`    |
| Linux (most distros)     | `lossless-checker-<ver>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (static / musl)    | `lossless-checker-<ver>-x86_64-unknown-linux-musl.tar.gz` |
| Windows (x64)            | `lossless-checker-<ver>-x86_64-pc-windows-msvc.zip`    |

**macOS / Linux:**

```bash
tar xzf lossless-checker-*.tar.gz
cd lossless-checker-*/
./lossless-checker "path/to/song.flac"
```

On macOS, Gatekeeper quarantines binaries downloaded from the web. If you see *"cannot be opened because the developer cannot be verified"*, clear the quarantine flag once:

```bash
xattr -d com.apple.quarantine ./lossless-checker
```

To run it from anywhere, move it onto your `PATH`, e.g. `sudo mv lossless-checker /usr/local/bin/`.

**Windows:**

Unzip the archive and run `lossless-checker.exe` from PowerShell or `cmd`:

```powershell
.\lossless-checker.exe "path\to\song.flac"
```

If SmartScreen warns about an unrecognized app, choose **More info → Run anyway**.

> The `<ver>` placeholder matches the release tag, e.g. `v0.1.0`. In the [Usage](#usage) examples below, substitute the binary path (`./lossless-checker`) wherever a command shows `cargo run --release --`.

## Build

To build from source instead (requires a [Rust toolchain](https://rustup.rs/)):

```bash
cargo build --release
# binary at ./target/release/lossless-checker
```

## Usage

**Single file** — detailed verdict:

```bash
cargo run --release -- "path/to/song.flac"
```

```
文件: song.flac
采样率: 48000 Hz
采样总数: 12582912
奈奎斯特频率: 24000 Hz
估计高频截止: 20795 Hz (86.6% of Nyquist)

判断: ✅ 高频延伸正常，像真无损
```

**Whole library** — pass a directory to scan it recursively, in parallel, and emit a ranked report:

```bash
cargo run --release -- ~/Music --report scan.txt --json scan.json
```

The text report has a summary, an **album ranking** (by 🚩 count — a whole album cut at the same
low frequency is the strongest signal of a lossy source), the full suspect list (🚩 then ⚠️,
sorted by cutoff), and a **decode-failure list** (surfaced, never silently dropped):

```
== 汇总 ==
  ✅ 像真无损 (≥19kHz)        2086
  ⚠️  高频收窄 (16.5-19kHz)    515
  🚩 高度可疑 (<16.5kHz)      185
  ✖  解码失败                 0

== 按专辑排行（🚩 数量降序）==
  🚩 15  ⚠️  0  Some Artist - Debut Album (2006)
  ...

== 可疑文件清单（🚩 在前，各按截止频率升序）==
   12672 Hz  🚩  Some Artist - Album/03. track.flac
   17800 Hz  ⚠️  Other Artist - Album/05. track.flac
   ...
```

With `--json` you also get machine-readable output:

```json
{
  "root": "/Users/you/Music",
  "scanned": 2786,
  "summary": { "clean": 2469, "narrowed": 234, "suspect": 83, "error": 0 },
  "results": [
    { "path": "Album/track.flac", "sample_rate": 44100, "cutoff_hz": 12672.0, "ratio": 0.5747, "verdict": "suspect" }
  ]
}
```

### Options

| Flag           | Default                 | Description |
|----------------|-------------------------|-------------|
| `--peak-db`    | `65`                    | Peak-relative threshold (dB below the spectral peak) for the default detector. Calibrated; override only for debugging. |
| `--noise-floor`| off                     | Use the legacy noise-floor detector (with `--threshold`) instead of peak-relative. Kept for comparison. |
| `--threshold`  | `10.0`                  | Noise-floor multiplier — only used with `--noise-floor`. |
| `--report`     | stdout                  | Write the text report to this file (directory scan only). |
| `--json`       | —                       | Also write a JSON report to this file (directory scan only). |
| `--ext`        | `flac,wav,m4a,aif,aiff` | Comma-separated extensions to scan. Lossless containers only — scanning mp3 etc. is pointless for fake-lossless detection. |
| `--jobs`       | CPU cores               | Number of parallel worker threads. |

### Verdict tiers

The verdict is based on the **absolute cutoff frequency in Hz**, not a ratio of Nyquist, because
lossy codecs low-pass at a fixed Hz regardless of the container's sample rate. A ratio-of-Nyquist
would wrongly flag perfectly good 48 kHz files (whose real content also stops at ~21–22 kHz — only
~88% of their 24 kHz Nyquist).

| Cutoff           | Verdict |
|------------------|---------|
| `≥ 19 kHz`       | ✅ Looks like real lossless |
| `16.8 – 19 kHz`  | ⚠️ Narrowed — possible high-bitrate transcode, check manually |
| `< 16.8 kHz`     | 🚩 Clear cliff — highly suspect fake lossless |

### Calibration

Tuned against a real library of ~2786 FLAC files **plus known-answer round-trip fakes** — a real
FLAC re-encoded through 128k/320k MP3 and back, which is a perfect "fake lossless" with a known
original bitrate:

- **Peak-db (65):** swept 45→75. Too low collapses even genuine weak-HF tracks; too high lets 128k
  residue read as full-band again. At 65, every 128k fake lands at **16.0–16.7 kHz** (caught) while
  genuine lossless clusters at **21–22 kHz**.
- **Verdict cutoffs:** genuine library files cluster at 19–22 kHz; 128k round-trips sit just under
  16.8 kHz, so that's the 🚩 line.

## Limitations

This is a **heuristic**, not proof:

- **False positives:** classical, acoustic, vocal, ambient, and old recordings naturally have
  little high-frequency energy and may show up as ⚠️ or 🚩. Interludes, skits, and solo-piano
  tracks routinely do. Treat **album-wide** patterns as the real signal, not isolated tracks.
- **False negatives — 320k is the blind spot.** Round-trip tests show 128k fakes are caught
  reliably (~16.5 kHz), but 320k MP3 low-passes at ~20 kHz, which overlaps where plenty of genuine
  lossless naturally rolls off. No cutoff-based metric can separate those, so a 320k transcode will
  usually read ✅. This tool catches obvious fakes, not the high-bitrate ones.
- **Performance:** it decodes every track in full, so a large library takes a few minutes (it runs
  on all CPU cores). Single-file checks are near-instant.

Its job is to **batch-flag the highly suspicious files** in a large library. Always confirm a
flagged file by eyeballing its spectrogram in a tool like [Spek](https://www.spek.cc/) before
drawing conclusions.

## License

GPL-3.0 — see [LICENSE](./LICENSE).
