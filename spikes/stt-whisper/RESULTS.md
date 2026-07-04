# STT whisper.cpp Rust-side spike — RESULTS

**The deliverable of Plan 06-spike.** Decides dam's stated preference — *"go straight to
whisper.cpp Rust-side only"* (Option B) — against measured evidence, vs. the staged-hybrid
fallback (Option C: Apple `SpeechAnalyzer` for v1).

- **Host:** Apple Silicon Mac (dam's dev machine), macOS. Metal backend.
- **Engine:** `whisper-rs =0.16.0` (pinned) → `whisper-rs-sys 0.15.0` → vendored whisper.cpp.
- **Status:** Mac tiers (T0–T4, T6) executed by the spike worker. iPhone tier (T5) **pending — needs dam's device.**

---

## Table 1 — Feasibility & performance (Mac, Apple Silicon, Metal backend)

Host: **Apple M4 Max**, macOS 26.2, Metal backend (`use gpu = 1`, `Metal total size` confirmed
in whisper.cpp stderr for every model — no CPU fallback). Audio: `jargon1.wav`, 59.8 s, 16 kHz
mono. Each model measured in its own process (peak RSS is that model's own high-water mark).

| Model | Quant | Size (MB) | Load (s) | RTF | Peak RSS (MB) | Backend | Notes |
|-------|-------|-----------|----------|-----|---------------|--------|-------|
| tiny.en | q5_1 | 32 | 0.08 | **0.006** | 161 | metal | decode 0.36 s / 59.8 s |
| base.en | q5_1 | 60 | 0.09 | **0.009** | 205 | metal | decode 0.51 s / 59.8 s |
| small.en | q5_1 | 190 | 0.13 | **0.021** | 392 | metal | decode 1.25 s / 59.8 s |
| large-v3-turbo | q5_0 | 574 | 0.27 | **0.041** | 786 | metal | decode 2.47 s / 59.8 s |
| distil-large-v3 | **f16** | 1520 | 0.66 | **0.029** | 1703 | metal | decode 1.72 s; f16 (no q5 ggml published) |

> RTF = wall-clock decode time ÷ audio duration, measured on the **second** decode (first is a
> discarded Metal-shader-JIT warm-up). RTF < 1.0 = faster than real-time. Peak RSS from
> `getrusage` `ru_maxrss` (**bytes** on macOS — conversion baked into `peak_rss_mb()`).
>
> **Load-time note:** the first whisper.cpp process on this machine paid a one-time
> `ggml_metal_library_init: loaded in 7.35 sec` (embedded Metal shader library compile). That
> shader cache is OS-level and warm for subsequent processes, so the load times above (0.08–0.66 s)
> are **steady-state** (cache-warm). First-ever cold launch on a fresh machine adds ~7 s once.
>
> **Result:** every model — including the largest — is **far under RTF 0.5** on this Mac
> (fastest usable model `base.en` at RTF 0.009, ~55× faster than real-time). The Mac is the
> optimistic proxy; even a pessimistic 5–10× iPhone slowdown keeps `base`/`small` comfortably
> real-time. Feasibility + performance bars: **cleared with large margin.**

## Table 2 — Streaming / append-only (chosen model from Table 1)

| Chunk (s) | Overlap (s) | Boundary re-transcription % | Finalize latency (s) | Append-only derivable? | Notes |
|-----------|-------------|-----------------------------|----------------------|------------------------|-------|

## Table 3 — Accuracy & biasing (per model × condition)

| Model | Audio clip | Noise cond. | WER % | Target-term recall (no bias) | Target-term recall (initial_prompt) | Recall Δ (pp) | Hallucination flag | Notes |
|-------|-----------|-------------|-------|------------------------------|-------------------------------------|---------------|--------------------|-------|

## Table 4 — iPhone tier (optional, real device)

**PENDING — not run.** Requires dam's physical iPhone (T5, hardware-gated). The iOS simulator
is explicitly insufficient (no Metal/ANE, no real battery/thermal). See `ios/README.md` for the
build recipe (whisper.cpp's bundled `examples/whisper.swiftui`, path B — no UniFFI).

| Device | iOS | Model | RTF | Battery Δ (%/10 min) | Thermal state @ 10 min | Killed in background? | Notes |
|--------|-----|-------|-----|----------------------|------------------------|-----------------------|-------|
| — | — | — | — | — | — | — | pending device |

---

## Feasibility (kill-question 1)

**PASS — `whisper-rs =0.16.0` with the `metal` feature builds and runs on this Apple Silicon Mac.**

- `nix-shell` (spike-local `shell.nix`: `cargo rustc cmake clang` + `LIBCLANG_PATH`) built the
  full native stack cleanly: `whisper-rs-sys 0.15.0` compiled vendored whisper.cpp via cmake +
  bindgen; `stt-whisper-spike` linked and ran. Release build: ~32 s cold.
- **Environment note (not KILL evidence):** the plan's `shell.nix` uses `import <nixpkgs>`, but
  this machine is a channel-less flake system — `<nixpkgs>` is not on `NIX_PATH`. Bare
  `nix-shell` fails with *"file 'nixpkgs' was not found in the Nix search path."* Resolved by
  invoking `nix-shell -I nixpkgs=flake:nixpkgs` (resolves nixpkgs via the flake registry). The
  system Xcode CLI-tools fallback was therefore **not needed** — the nix path works. Recorded
  because it's a real friction for reproducing the spike shell on this host.

---

## Decision

_(Filled in Task 6 against the exit criteria.)_

---

## Attribution

- **whisper.cpp** — MIT. Vendored by `whisper-rs-sys` as a git submodule.
- **whisper-rs** (tazz4843) `=0.16.0` — MIT. https://crates.io/crates/whisper-rs
- **whisper-rs-sys** `0.15.0` — MIT.
- **hound** `3.5.1` — MIT/Apache-2.0.
- **ggml Whisper models** — MIT (OpenAI Whisper weights). Fetched by `download-models.sh` from
  `https://huggingface.co/ggerganov/whisper.cpp` (see script note: the plan named `ggml-org`,
  which returns 401 today; ggerganov serves the same MIT weights directly):
  - `ggml-tiny.en-q5_1.bin` — q5_1, 31 MB
  - `ggml-base.en-q5_1.bin` — q5_1, 57 MB
  - `ggml-small.en-q5_1.bin` — q5_1, 182 MB
  - `ggml-large-v3-turbo-q5_0.bin` — q5_0, 548 MB
- **distil-large-v3 ggml** — MIT (HuggingFace distil-whisper).
  `https://huggingface.co/distil-whisper/distil-large-v3-ggml` → `ggml-distil-large-v3.bin`,
  1.5 GB. **Note:** this is the **f16 (unquantized)** ggml conversion — distil-whisper does not
  publish a q5_0 ggml, so Table 1's "Quant" for this row is f16, not q5_0 as the plan template
  assumed. Recorded as a deviation.
