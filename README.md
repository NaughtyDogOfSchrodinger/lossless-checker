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
the power spectrum, and finds the highest frequency where energy clearly rises above the noise
floor. That cutoff frequency is the verdict signal. (It analyzes the full track on purpose: many
songs open with a quiet intro and only bring in cymbals/percussion later, so sampling just the
head would underestimate the cutoff and cause false positives.)

## Build

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
  "summary": { "clean": 2086, "narrowed": 515, "suspect": 185, "error": 0 },
  "results": [
    { "path": "Album/track.flac", "sample_rate": 44100, "cutoff_hz": 12672.0, "ratio": 0.5747, "verdict": "suspect" }
  ]
}
```

### Options

| Flag          | Default                 | Description |
|---------------|-------------------------|-------------|
| `--threshold` | `10.0`                  | Noise-floor multiplier: how many times above the noise floor a bin must be to count as real signal. Calibrated; override only for debugging. |
| `--report`    | stdout                  | Write the text report to this file (directory scan only). |
| `--json`      | —                       | Also write a JSON report to this file (directory scan only). |
| `--ext`       | `flac,wav,m4a,aif,aiff` | Comma-separated extensions to scan. Lossless containers only — scanning mp3 etc. is pointless for fake-lossless detection. |
| `--jobs`      | CPU cores               | Number of parallel worker threads. |

### Verdict tiers

The verdict is based on the **absolute cutoff frequency in Hz**, not a ratio of Nyquist, because
lossy codecs low-pass at a fixed Hz regardless of the container's sample rate. A ratio-of-Nyquist
would wrongly flag perfectly good 48 kHz files (whose real content also stops at ~21–22 kHz — only
~88% of their 24 kHz Nyquist).

| Cutoff          | Verdict |
|-----------------|---------|
| `≥ 19 kHz`      | ✅ Looks like real lossless |
| `16.5 – 19 kHz` | ⚠️ Narrowed — possible high-bitrate transcode, check manually |
| `< 16.5 kHz`    | 🚩 Clear cliff — highly suspect fake lossless |

### Calibration

Thresholds were calibrated against a real library of ~2786 FLAC files:

- **Noise-floor multiplier (10.0):** sweeping 4→40 showed real-lossless cutoffs stay pinned at
  ~22.5 kHz regardless of the multiplier, while fakes only get exposed at ≥6 (below ~5 the noise
  floor lifts them to a false 20–22 kHz). 10.0 sits on the stable plateau with margin.
- **Verdict cutoffs:** genuine files clustered at 19–23 kHz; confirmed transcodes sat at 12–17 kHz.

## Limitations

This is a **heuristic**, not proof:

- **False positives:** classical, acoustic, vocal, ambient, and old recordings naturally have
  little high-frequency energy and may show up as ⚠️ or 🚩. Interludes, skits, and solo-piano
  tracks routinely do. Treat **album-wide** patterns as the real signal, not isolated tracks.
- **False negatives:** high-bitrate lossy (e.g. 320k MP3) cuts near ~20 kHz and is hard to
  distinguish from genuine lossless by cutoff alone.
- **Performance:** it decodes every track in full, so a large library takes a few minutes (it runs
  on all CPU cores). Single-file checks are near-instant.

Its job is to **batch-flag the highly suspicious files** in a large library. Always confirm a
flagged file by eyeballing its spectrogram in a tool like [Spek](https://www.spek.cc/) before
drawing conclusions.

## License

GPL-3.0 — see [LICENSE](./LICENSE).
