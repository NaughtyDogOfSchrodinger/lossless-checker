# Roadmap

Planned mitigations for the [Limitations](./README.md#limitations). This is a contributor-facing
reference, not a commitment — items are ordered by return on effort, and each one names the honest
trade-off. The headline limitation is that the current verdict rides almost entirely on the
**high-frequency cutoff cliff**, so any codec that doesn't leave a low, hard lowpass is invisible.

## P1 — Stereo-correlation / joint-stereo detector

**Targets:** the 320k MP3 and 256k+ AAC blind spot — files that pass the cutoff test today.

MP3 and AAC use joint/intensity stereo: above a crossover frequency they collapse L/R into a shared
representation, driving the two channels to near-perfect correlation in the treble. Genuine stereo
lossless keeps L/R **decorrelated** up high. Measure per-band L/R correlation (or mid/side energy
ratio) above ~10 kHz; correlation pinned near 1.0 across a wide HF band is a strong lossy tell that
survives even at 320k, where the cutoff cliff is gone.

- **Enabler:** the stereo signal already exists at `src/decode.rs:108` (interleaved copy) and is only
  collapsed at `src/decode.rs:123` (`mix_to_mono`). A second analysis path can tap it before the
  downmix — no re-decode, the mono spectrum path stays unchanged.
- **Effort:** medium. No new dependencies, no training data.
- **Risk / guards:** mono and true-mono recordings have no stereo to analyze (skip them); mid/side-
  mastered or heavily-panned material can read as correlated and needs a guard band.
- **Honest limit:** does **not** catch Opus (more adaptive stereo coding), and is inapplicable to
  mono sources.

## P1 — Per-album consensus scoring

**Targets:** false positives on classical, acoustic, vocal, ambient, and old recordings.

Sparse-HF genres trip the cutoff heuristic on isolated tracks. The strongest real signal is already
album-wide: a whole album sharing one low cutoff is far more telling than a single quiet track.
Strengthen the existing "Albums ranked" aggregation in `src/report.rs` so a track is only escalated
to 🚩 when a threshold share of its album shares the same low cutoff.

- **Effort:** low — pure heuristic refinement of existing aggregation.
- **Risk:** low. Mostly an exercise in picking the consensus fraction.

## P2 — MDCT frame-grid / quantization-hole detection

**Targets:** high-bitrate MP3/AAC that survive both the cutoff and stereo tests.

Lossy codecs quantize on a fixed block grid (MP3: 576-sample granule; AAC: 1024/128-sample blocks),
leaving periodic frame-boundary discontinuities and zeroed scalefactor bands. Detect via
cepstrum/autocorrelation at the codec frame period, plus counting zeroed-band "stair-steps" in the
power spectrum. This is the signal dedicated tools (Lossless Audio Checker, *fakin' the funk?*) lean
on.

- **Reuse:** overlaps the existing `detect_holes` (`src/spectrum.rs`), which already finds notches but
  is report-only (never consulted by `classify`, see `src/verdict.rs:82`). This work could promote a
  refined version of it into the verdict.
- **Effort:** high — real DSP.
- **Risk:** higher false positives; natural musical nulls and percussive transients mimic some of
  these patterns, which is exactly why holes are report-only today.

## P3 — ML spectrogram classifier

**Targets:** everything above, and the only realistic shot at Opus.

Train a CNN on (genuine, transcoded) spectrogram pairs. The training corpus already exists — the
~2786 real FLAC plus round-trip 128k/320k fakes used for calibration. Highest accuracy of any
approach here.

- **Effort:** high. Adds a model artifact and inference path (a Rust ML crate or a Python sidecar),
  departing from the current zero-dependency design — a deliberate architectural choice, not a
  drop-in.
- **Risk:** distribution drift across encoders/genres; needs a held-out eval set to stay honest.
- **Honest limit:** even an ML model sits near the detectability floor for high-bitrate Opus.

## Data and engineering tasks

- **Hi-Res threshold calibration.** `HIRES_MIN_EXT` / `HIRES_EMPTY_DB` in `src/verdict.rs` ship as
  reasoned defaults. Tuning them needs a labelled Hi-Res fixture set (genuine vs upsampled) — a data
  problem, not an algorithm one.
- **DSD `check-dsd` follow-ups.** DST-compressed DSD decompression (currently `Unsupported`);
  per-sample-rate threshold calibration (DSD64/128/256); more labelled CD→DSD samples to calibrate
  the `cd_wall` step detector; a confidence score beyond the binary Pass/Suspicious. Thresholds and
  method live in [`docs/calibration.md`](docs/calibration.md). (DSD is now analyzed entirely
  natively from the 1-bit stream — the old ffmpeg decode path has been removed.)
- **Optional DSD→PCM decimation.** If a future feature needs DSD as PCM (e.g. to run the PCM
  detectors on it), reuse the `src/dsd/` container readers plus a decimating low-pass to ~88.2 kHz —
  still no external dependency.

## Known-unsolvable (be honest)

High-bitrate **Opus**, and near-transparent **320k MP3/AAC**, may be genuinely indistinguishable from
true lossless without the original file. These codecs are transparent by design. The roadmap above
improves **recall on codecs that leave artifacts** — joint-stereo collapse, MDCT-grid imprints — it
does **not** claim to close the transparent case. A ✅ from this tool means "no detected lossy
artifact," never a guarantee of provenance.
