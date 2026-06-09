# lossless-checker

[中文说明](./README.zh-CN.md)

A heuristic detector for **fake lossless** audio — files that were transcoded from a lossy
source (e.g. 320k MP3) and then re-wrapped as FLAC/ALAC to look lossless.

## How it works

Genuine lossless audio (e.g. 16bit/44.1kHz) carries real high-frequency energy that extends
naturally toward ~20–22 kHz. When audio is encoded with a lossy codec, the encoder applies a
low-pass filter at a fixed frequency, leaving an **energy cliff** in the spectrum:

| Source            | Typical cutoff |
|-------------------|----------------|
| Genuine lossless  | ~19–23 kHz     |
| 256k transcode    | ~18–19 kHz     |
| 128k transcode    | ~16 kHz        |
| lower bitrate     | ~12–15 kHz     |

The tool decodes the audio (via [symphonia](https://github.com/pdeljanov/Symphonia), pure Rust —
FLAC / ALAC / WAV / MP3, no system deps), runs a windowed FFT ([rustfft](https://github.com/ejmahler/RustFFT)),
averages the power spectrum, and finds the frequency where energy rises above the noise floor.
That cutoff frequency is the verdict signal.

## Usage

```bash
cargo run --release -- "path/to/song.flac"
```

Example output:

```
文件: song.flac
采样率: 48000 Hz
采样总数: 12582912
奈奎斯特频率: 24000 Hz
估计高频截止: 20795 Hz (86.6% of Nyquist)

判断: ✅ 高频延伸正常，像真无损
```

### Options

| Flag          | Default | Description |
|---------------|---------|-------------|
| `--threshold` | `10.0`  | Noise-floor multiplier: how many times above the noise floor a bin must be to count as real signal. Calibrated; override only for debugging. |

### Verdict tiers

The verdict is based on the **absolute cutoff frequency in Hz** (not a ratio of Nyquist), because
lossy codecs low-pass at a fixed Hz regardless of the container's sample rate. Using a
ratio-of-Nyquist would wrongly flag perfectly good 48 kHz files (whose real content also stops at
~21–22 kHz, only ~88% of their 24 kHz Nyquist).

| Cutoff            | Verdict |
|-------------------|---------|
| `≥ 19 kHz`        | ✅ Looks like real lossless |
| `16.5 – 19 kHz`   | ⚠️ Narrowed — possible high-bitrate transcode, check manually |
| `< 16.5 kHz`      | 🚩 Clear cliff — highly suspect fake lossless |

### Calibration

Thresholds were calibrated against a real library of ~2786 FLAC files:

- **Noise-floor multiplier (10.0):** sweeping 4→40 showed real-lossless cutoffs stay pinned at
  ~22.5 kHz regardless of the multiplier, while fakes only get exposed at ≥6 (below ~5 the noise
  floor lifts them to a false 20–22 kHz). 10.0 sits on the stable plateau with margin.
- **Verdict cutoffs:** genuine files clustered at 19–23 kHz; confirmed transcodes sat at 12–17 kHz.

## Caveats

This is a **heuristic**, not a verdict of certainty:

- **False positives:** classical, acoustic, vocal, and old recordings naturally have little
  high-frequency energy and may look "narrowed."
- **False negatives:** high-bitrate lossy (e.g. 320k MP3) cuts near ~20 kHz and can be hard to
  distinguish from real lossless by cutoff alone.

Its value is **batch-flagging the highly suspicious files** in a large library. Always confirm
a flagged file by eyeballing the spectrogram in a tool like [Spek](https://www.spek.cc/) before
drawing conclusions.

## License

GPL-3.0 — see [LICENSE](./LICENSE).
