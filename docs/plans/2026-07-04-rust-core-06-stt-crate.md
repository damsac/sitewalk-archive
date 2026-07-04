# Murmur Rust Core — Plan 06: The STT Crate (`crates/stt`)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `crates/stt` — a real, on-device speech-to-text crate wrapping whisper.cpp (via `whisper-rs`) behind a clean, testable seam. It turns a stream of 16 kHz mono f32 PCM buffers (captured by the platform shell — Rust never touches the mic) into an **append-only stream of finalized transcript segments** plus a **volatile preview tail** for UI, biased by the user's ≤100-term vocabulary. It is the STT half of spec Rev 2 §2 (live in-session extraction, offline, on-device) and the direct feeder for Plan 05's `LiveExtractor` (finalized segments → `Store::append_transcript` → the append-only char cursor).

This plan **productionizes the spike** (`spikes/stt-whisper/`, GO verdict in `RESULTS.md`). The spike's measured numbers are this plan's design constants; the spike's `stream.rs` finalize logic is the reference for the quality lever (LocalAgreement word-level finalize). We steal ideas, not code — the spike is quarantined (`workspace.exclude = ["spikes"]`); `crates/stt` is a first-class **workspace member**.

**Spike verdict constants (measured on M4 Max, Metal — locked):**
- Engine: `whisper-rs =0.16.0` (exact pin), `metal` feature → `whisper-rs-sys 0.15.0` → vendored whisper.cpp (all MIT).
- Target models: `base.en` q5_1 (RTF 0.009, WER 5.8% clean / 11.7% noisy) and `small.en` q5_1 (RTF 0.021, WER 4.7% / 11.7%). Both clear RTF<0.5 and WER bars from a single model row.
- Chunking: **5 s chunk / 1 s overlap** default (configurable). Naive segment-level time-horizon finalize is lossy (80% streaming WER); **word-level LocalAgreement dedup finalize → 19% WER at ≤3 s finalize latency**. The finalize rule is the quality lever — this plan productionizes it.
- Append-only invariant: a finalized word is never revised. Proven + unit-tested in the spike; the production crate carries equivalent tests.
- Biasing: `initial_prompt` injection of the vocabulary → **+10 to +19 pp** term recall, **zero** hallucination flagged. v1 biasing = initial_prompt injection; trie/logit-bias deferred.
- Live WER (~19%) is ~4× batch WER (~5%). **Implication carried into the design:** the finalized live stream is a *provisional preview*; end-of-session `process()` on the full transcript (Plan 04) remains authoritative. `crates/stt` is not the truth; it feeds the live board.

**The four hard design decisions (justified up front — reviewers read these first):**

1. **The `Decoder` trait is the one seam that touches whisper.** Everything above it — PCM accumulation, chunk cutting, overlap, LocalAgreement finalize, bias-prompt assembly — is **pure Rust with no whisper dependency**, unit-tested against a `ScriptedDecoder` fake that returns canned segments. `whisper-rs` is an **optional dependency behind a `whisper` cargo feature**, off by default. This is what makes requirement 4 (hermetic CI) achievable: `cargo test --workspace` with default features compiles and passes on Linux CI with **no model files, no cmake/clang, no Metal** — because the only thing gated out is the ~40 lines of `WhisperDecoder`. The alternative (a macOS-only workspace) would break the existing Linux CI for `harness`/`murmur-core`/`evals`; rejected.

2. **Caller-driven pump, not an internal worker thread.** `SttStream` spawns **no threads, owns no channels, invokes no callbacks into the caller.** The shell captures audio and, off the real-time thread, calls `push_pcm()` (cheap: buffers samples) then `poll()` (runs the long Metal decode and returns newly-finalized segments). The shell owns cadence — exactly mirroring Plan 05's "the cadence is app-shell policy" (Plan 05 Deferred 3). This is the design that **cannot deadlock**: no worker thread to join, no channel to block on, no Rust→Swift callback re-entrancy. An internal worker would need a shutdown protocol, an mpsc for buffers, and a UniFFI callback interface to emit — three new deadlock/ordering surfaces for zero benefit, since the shell already runs a tick loop for `LiveExtractor`. Interior mutability (`Mutex`) with a strict two-lock order (engine → input, never the reverse) makes the object a `&self` `Arc` that UniFFI exposes directly (Plan 07).

3. **The crate never downloads, never touches the mic, never persists.** Construction takes a **model file path** (the shell's Application Support dir — model management, download, and on-demand-resources plumbing are shell concerns; the crate only opens a file that exists) and a `SttConfig`. Input is pushed PCM; output is pulled segments. No I/O beyond reading the model file at construction.

4. **`crates/stt` and `murmur-core` stay decoupled — the live-extraction wiring is deferred to Plan 07.** `stt` does not depend on `murmur-core` and vice versa. The integration contract (finalized segment → `append_transcript` → `LiveExtractor` cursor) is **documented precisely here and shown as an example**, but the actual tick loop that couples STT.poll to LiveExtractor.maybe_extract is app-shell orchestration living across the FFI boundary. Plan 05's self-review already established this (constraint 4: "STT and live extraction compose without coupling"). Building the loop here would force a crate dependency both plans deliberately avoid. Justified in Task 6 and Deferred.

**Tech stack:** new crate `stt`. Sole non-workspace dep is `whisper-rs =0.16.0` (optional, `metal` feature) + `thiserror` (workspace). Pure-logic tests use only std + a hand-rolled fake. Real-model tests are `#[ignore]`d and env-gated (`MURMUR_WHISPER_MODEL`), mirroring the existing `anthropic_smoke` gate — they need the `whisper` feature **and** a model file, so CI never runs them.

**Spec:** Rev 2 §2 (live, on-device, offline-degradable), §vocabulary point 3 (vocabulary feeds STT contextual biasing — the ≤100-term list from memory), §6 (transcript persists continuously; <8 s budget context). Research: `docs/research/2026-07-04-on-device-stt-frontier.md` (Option B chosen: Rust-side whisper.cpp for biasing control + Android hedge). Spike: `spikes/stt-whisper/RESULTS.md` (GO) and `src/stream.rs` (finalize reference).

---

## File Structure

```
crates/stt/
  Cargo.toml            # NEW: workspace member; optional whisper-rs behind `whisper` feature
  src/
    lib.rs              # NEW: public API — SttStream, SttConfig, FinalizedSegment, SttError, re-exports
    decoder.rs          # NEW: Decoder trait + RawSegment (the whisper seam); ScriptedDecoder (test fake)
    chunk.rs            # NEW: Chunker — PCM accumulation + 5s/1s window cutting (pure)
    finalize.rs         # NEW: LocalAgreement word-level append-only finalizer (the quality lever, pure)
    bias.rs             # NEW: build_bias_prompt (≤100 terms → initial_prompt) — the biasing seam (pure)
    whisper.rs          # NEW (cfg feature="whisper"): WhisperDecoder — the only file that imports whisper-rs
  tests/
    stream_append_only.rs   # NEW: end-to-end append-only contract via ScriptedDecoder (no model, no feature)
Cargo.toml              # MODIFY (root): add "crates/stt" to workspace members
flake.nix               # MODIFY: dev shell gains cmake + clang + LIBCLANG_PATH (for `--features whisper` builds)
README.md               # MODIFY: plan-series line
```

Run cargo via the dev shell or `nix shell nixpkgs#cargo nixpkgs#rustc -c cargo <cmd>` from the repo root. Default builds/tests need **no** native toolchain; `--features whisper` needs the updated dev shell (Task 1).

---

## API sketch (the surface every later plan consumes)

```rust
// Construction (whisper-backed, behind the feature):
let stream = SttStream::with_model(Path::new(&model_path), SttConfig::default(), &vocab_terms)?;
// Construction (any backend — the test/FFI seam):
let stream = SttStream::with_decoder(Box::new(decoder), SttConfig::default(), &vocab_terms);

// Per audio buffer, OFF the real-time thread:
stream.push_pcm(&pcm_f32);              // cheap: buffers 16kHz mono f32 samples
let finalized = stream.poll()?;         // runs decode(s) when a chunk is ready; append-only segments out
let preview   = stream.preview_tail();  // volatile, un-finalized hypothesis for greyed UI (never persisted)

// DONE (supersedes cancel-for-speed canon): flush, don't drop.
let tail = stream.end()?;               // decodes remaining buffered audio, finalizes everything pending
```

`SttStream` is `Send + Sync` (interior `Mutex`), so Plan 07 wraps it in `Arc` and UniFFI exposes the `&self` methods directly — no actor, no async, no callback interface.

---

### Task 1: Workspace member, feature flags, types, and the `Decoder` seam

**Files:** create `crates/stt/Cargo.toml`, `crates/stt/src/lib.rs`, `crates/stt/src/decoder.rs`; modify root `Cargo.toml`, `flake.nix`.

- [ ] **Step 1: Root workspace + crate manifest + dev shell**

Root `Cargo.toml` — add the member (keep `exclude = ["spikes"]`):
```toml
members = ["crates/harness", "crates/murmur-core", "crates/evals", "crates/stt"]
```

`crates/stt/Cargo.toml`:
```toml
[package]
name = "stt"
version = "0.1.0"
edition = "2021"

[features]
default = []
# Enabling `whisper` pulls the native stack (whisper-rs + vendored whisper.cpp, Metal).
# OFF by default so `cargo test --workspace` stays hermetic and cross-platform.
whisper = ["dep:whisper-rs"]

[dependencies]
thiserror = { workspace = true }
whisper-rs = { version = "=0.16.0", features = ["metal"], optional = true }
```

`flake.nix` — the dev shell must gain what the spike's `shell.nix` needed for `whisper-rs-sys` to compile whisper.cpp (`cmake`, `clang`, `LIBCLANG_PATH` for bindgen). These are only exercised on `--features whisper` builds, but the shell must provide them:
```nix
devShells.default = pkgs.mkShell {
  packages = with pkgs; [ cargo rustc clippy rustfmt rust-analyzer cmake clang ];
  # bindgen (whisper-rs-sys) needs libclang on its path for `--features whisper`:
  LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
};
```
> Note (from `RESULTS.md`): this machine is a channel-less flake system; bare `nix-shell` fails on `<nixpkgs>`. The project already uses `flake.nix` (not `shell.nix`), so `direnv`/`nix develop` resolves nixpkgs correctly — no `-I nixpkgs=...` workaround needed here.

- [ ] **Step 2: Write the failing tests** (`crates/stt/src/decoder.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_decoder_returns_scripts_in_order_and_captures_prompts() {
        let mut d = ScriptedDecoder::new(vec![
            vec![RawSegment { start_cs: 0, end_cs: 200, text: "hello world".into() }],
            vec![RawSegment { start_cs: 0, end_cs: 150, text: "again now".into() }],
        ]);
        let a = d.decode(&[0.0; 16], Some("french drain, ledger")).unwrap();
        assert_eq!(a[0].text, "hello world");
        let b = d.decode(&[0.0; 16], None).unwrap();
        assert_eq!(b[0].text, "again now");
        assert_eq!(d.captured_prompts(), &[Some("french drain, ledger".to_string()), None]);
    }

    #[test]
    fn scripted_decoder_errors_when_exhausted() {
        let mut d = ScriptedDecoder::new(vec![]);
        assert!(matches!(d.decode(&[0.0; 8], None), Err(SttError::Decode(_))));
    }
}
```

- [ ] **Step 3: Implement** (`crates/stt/src/decoder.rs`)

```rust
use crate::SttError;

/// One decoded segment as whisper.cpp emits it: timestamps are CHUNK-RELATIVE
/// centiseconds (offset to absolute audio time by the engine, not here).
#[derive(Clone, Debug, PartialEq)]
pub struct RawSegment {
    pub start_cs: i64,
    pub end_cs: i64,
    pub text: String,
}

/// The one seam that touches whisper. Everything above it (chunk cutting,
/// overlap, LocalAgreement finalize, bias prompt) is pure and testable against
/// a fake. `decode` runs ONE window of samples with an optional `initial_prompt`
/// (the biasing surface). Implementations may be slow (Metal); the caller runs
/// them off the real-time thread (see `SttStream::poll`).
pub trait Decoder: Send {
    fn decode(&mut self, samples: &[f32], initial_prompt: Option<&str>)
        -> Result<Vec<RawSegment>, SttError>;
}

/// Test/example fake: replays scripted segment lists and records the prompts it
/// was handed, so the pure engine can be exercised with zero whisper dependency.
pub struct ScriptedDecoder {
    scripts: std::collections::VecDeque<Vec<RawSegment>>,
    captured_prompts: Vec<Option<String>>,
}

impl ScriptedDecoder {
    pub fn new(scripts: Vec<Vec<RawSegment>>) -> Self {
        Self { scripts: scripts.into(), captured_prompts: Vec::new() }
    }
    pub fn captured_prompts(&self) -> &[Option<String>] {
        &self.captured_prompts
    }
}

impl Decoder for ScriptedDecoder {
    fn decode(&mut self, _samples: &[f32], initial_prompt: Option<&str>)
        -> Result<Vec<RawSegment>, SttError> {
        self.captured_prompts.push(initial_prompt.map(str::to_string));
        self.scripts
            .pop_front()
            .ok_or_else(|| SttError::Decode("scripted decoder exhausted".into()))
    }
}
```

`crates/stt/src/lib.rs` — the public shell + error + config (types only for now):
```rust
//! On-device streaming STT over whisper.cpp (spec Rev 2 §2). PCM in → append-only
//! finalized transcript segments out, biased by the user's ≤100-term vocabulary.
//! The whisper backend is behind the `whisper` feature; the pure chunk/finalize/
//! bias logic compiles and tests everywhere with no native toolchain or model file.

mod bias;
mod chunk;
mod decoder;
mod finalize;
#[cfg(feature = "whisper")]
mod whisper;

pub use decoder::{Decoder, RawSegment, ScriptedDecoder};
#[cfg(feature = "whisper")]
pub use whisper::WhisperDecoder;

/// A finalized, never-to-be-revised transcript segment (append-only contract).
/// Timestamps are ABSOLUTE audio milliseconds from stream start. The shell
/// appends `text` to `Store::append_transcript` (Plan 05 cursor feeder).
#[derive(Clone, Debug, PartialEq)]
pub struct FinalizedSegment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct SttConfig {
    /// Decode window length (spike default 5 s).
    pub chunk_secs: f64,
    /// Overlap re-decoded each window for LocalAgreement (spike default 1 s).
    pub overlap_secs: f64,
    /// Sample rate the shell must feed (whisper wants 16 kHz mono f32).
    pub sample_rate: u32,
    /// Whisper language hint ("en" for the *.en models).
    pub language: String,
    /// Hard cap on vocabulary terms injected via initial_prompt (spec: ≤100).
    pub max_bias_terms: usize,
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            chunk_secs: 5.0,
            overlap_secs: 1.0,
            sample_rate: 16_000,
            language: "en".into(),
            max_bias_terms: 100,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SttError {
    #[error("model load failed: {0}")]
    ModelLoad(String),
    #[error("decode failed: {0}")]
    Decode(String),
    #[error("invalid config: {0}")]
    Config(String),
}
```

- [ ] **Step 4: Verify** `nix develop -c cargo test -p stt` (pure tests pass; whisper not compiled) and `cargo build -p stt --features whisper` (native stack compiles in the updated shell).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(stt): scaffold crate — Decoder seam, types, feature-gated whisper, dev shell"
```

---

### Task 2: The Chunker — PCM accumulation and window cutting (pure)

**Files:** create `crates/stt/src/chunk.rs`.

The chunker owns the sample buffer and a `next_window_start` cursor in absolute samples. It cuts `chunk_secs` windows stepping by `chunk_secs - overlap_secs`, and only yields a window once enough samples have arrived to fill it (or on flush). It never decodes — it hands sample slices + the window's absolute start offset to the caller.

- [ ] **Step 1: Write the failing tests** (`crates/stt/src/chunk.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // 16 kHz: 5 s = 80_000 samples, 1 s overlap → step = 4 s = 64_000 samples.
    fn chunker() -> Chunker { Chunker::new(16_000, 5.0, 1.0) }

    #[test]
    fn yields_nothing_until_a_full_window_arrives() {
        let mut c = chunker();
        c.push(&vec![0.0; 79_999]);
        assert!(c.take_ready_window().is_none(), "one sample short of a window");
        c.push(&[0.0]);
        let w = c.take_ready_window().expect("full window now ready");
        assert_eq!(w.start_sample, 0);
        assert_eq!(w.samples.len(), 80_000);
    }

    #[test]
    fn steps_by_chunk_minus_overlap() {
        let mut c = chunker();
        c.push(&vec![0.0; 144_000]); // 9 s → windows [0,5s) and [4s,9s)
        let w0 = c.take_ready_window().unwrap();
        assert_eq!(w0.start_sample, 0);
        let w1 = c.take_ready_window().unwrap();
        assert_eq!(w1.start_sample, 64_000, "advanced by 4 s, re-decoding the 1 s overlap");
        assert!(c.take_ready_window().is_none());
    }

    #[test]
    fn flush_emits_the_short_final_window() {
        let mut c = chunker();
        c.push(&vec![0.0; 32_000]); // 2 s only — never fills a 5 s window
        assert!(c.take_ready_window().is_none());
        let w = c.flush().expect("flush yields the remaining tail");
        assert_eq!(w.start_sample, 0);
        assert_eq!(w.samples.len(), 32_000);
        assert!(w.is_final);
        assert!(c.flush().is_none(), "nothing left after flush");
    }

    #[test]
    fn drops_consumed_prefix_to_bound_memory() {
        let mut c = chunker();
        c.push(&vec![0.0; 144_000]);
        c.take_ready_window().unwrap(); // consumes through step=64_000
        c.take_ready_window().unwrap();
        // Buffer retains only from the last window start onward, not all 9 s.
        assert!(c.buffered_samples() <= 80_000, "old audio behind the cursor is freed");
    }
}
```

- [ ] **Step 2: Implement** (`crates/stt/src/chunk.rs`)

```rust
/// A window ready to decode. `start_sample` is absolute (from stream start) for
/// converting chunk-relative segment timestamps to absolute ms. `is_final` marks
/// the flush tail (finalizer uses ∞ horizon on it — nothing comes after).
pub struct Window {
    pub start_sample: u64,
    pub samples: Vec<f32>,
    pub is_final: bool,
}

/// Accumulates PCM and cuts fixed windows with overlap. Pure; no decode, no I/O.
/// Frees audio behind the window cursor to bound memory over an hour-long session.
pub struct Chunker {
    chunk_len: usize,   // samples per window
    step: usize,        // samples between window starts (chunk_len - overlap)
    buf: Vec<f32>,      // samples from `buf_start` onward
    buf_start: u64,     // absolute sample index of buf[0]
    next_start: u64,    // absolute sample index of the next window to emit
    done: bool,
}

impl Chunker {
    pub fn new(sample_rate: u32, chunk_secs: f64, overlap_secs: f64) -> Self {
        let sr = sample_rate as f64;
        let chunk_len = (chunk_secs * sr) as usize;
        let step = (((chunk_secs - overlap_secs).max(0.1)) * sr) as usize;
        Self { chunk_len, step, buf: Vec::new(), buf_start: 0, next_start: 0, done: false }
    }

    pub fn push(&mut self, pcm: &[f32]) {
        self.buf.extend_from_slice(pcm);
    }

    pub fn buffered_samples(&self) -> usize {
        self.buf.len()
    }

    /// Yields the next full window if enough audio has arrived, advancing the
    /// cursor by `step` and freeing audio behind the new cursor.
    pub fn take_ready_window(&mut self) -> Option<Window> {
        let rel_start = (self.next_start - self.buf_start) as usize;
        let rel_end = rel_start + self.chunk_len;
        if rel_end > self.buf.len() {
            return None;
        }
        let samples = self.buf[rel_start..rel_end].to_vec();
        let window = Window { start_sample: self.next_start, samples, is_final: false };
        self.next_start += self.step as u64;
        self.free_consumed();
        Some(window)
    }

    /// The short final window: everything from the cursor to the buffer end,
    /// marked `is_final`. Call once at end()/flush(); returns None if empty.
    pub fn flush(&mut self) -> Option<Window> {
        if self.done {
            return None;
        }
        self.done = true;
        let rel_start = (self.next_start - self.buf_start) as usize;
        if rel_start >= self.buf.len() {
            return None;
        }
        let samples = self.buf[rel_start..].to_vec();
        Some(Window { start_sample: self.next_start, samples, is_final: true })
    }

    fn free_consumed(&mut self) {
        // Retain from the next window's start (which sits `overlap` before
        // `next_start`)... simplest correct bound: keep from `next_start`.
        let keep_from = self.next_start.min(self.buf_start + self.buf.len() as u64);
        let drop_n = (keep_from - self.buf_start) as usize;
        if drop_n > 0 {
            self.buf.drain(..drop_n);
            self.buf_start = keep_from;
        }
    }
}
```
> `free_consumed` keeps memory O(one window), not O(whole session) — important for the hour-long locked-phone session (research Q5/Q8). It drops to `next_start`; the overlap re-decode reads samples that are still ahead of `next_start` in the next window, so nothing needed is freed.

- [ ] **Step 3: Verify** `cargo test -p stt chunk` — green.

- [ ] **Step 4: Commit**
```bash
git add -A && git commit -m "feat(stt): Chunker — bounded-memory PCM windowing at 5s/1s"
```

---

### Task 3: LocalAgreement word-level finalizer — the quality lever (pure)

**Files:** create `crates/stt/src/finalize.rs`.

This is the plan's single most important algorithm. `RESULTS.md` Table 2: the naive segment-level time-horizon rule is 80% WER; **word-level LocalAgreement dedup is 19%**. We productionize the spike's `reassemble_dedup` idea into an *incremental* finalizer that emits append-only tokens as consecutive chunk hypotheses agree, and exposes the un-agreed tail as the volatile preview.

**Algorithm (LocalAgreement-2, word level):** keep `committed: Vec<String>` (finalized tokens, never revised) and `prev_hyp: Vec<String>` (the previous chunk's full token hypothesis). On each new chunk, tokenize its text into `new_hyp`. The safe-to-commit prefix is the **longest common prefix of `prev_hyp` and `new_hyp`, beyond what's already committed** — two consecutive decodes agreeing on a token is the LocalAgreement confirmation. Append those tokens to `committed` and emit them. On `flush` (final window), commit the entire remaining hypothesis (no successor will confirm it — the spike's ∞-horizon flush). Preview tail = `new_hyp[committed.len()..]` joined.

Carry the spike's three invariants as tests: append-only (committed never revised), no double-emit of overlap, overlap merge.

- [ ] **Step 1: Write the failing tests** (`crates/stt/src/finalize.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn commits_only_the_agreed_prefix_across_two_hypotheses() {
        let mut f = Finalizer::new();
        // chunk 0 hypothesis — nothing to confirm it yet → nothing final.
        assert_eq!(f.ingest(toks("the french drain is")), Vec::<String>::new());
        // chunk 1 agrees on "the french drain is", extends with "backing up".
        // The tail "backing up" is NOT yet confirmed (only one hypothesis has it).
        assert_eq!(f.ingest(toks("the french drain is backing up")), toks("the french drain is"));
        assert_eq!(f.preview(), "backing up");
    }

    #[test]
    fn append_only_never_revises_a_committed_word() {
        let mut f = Finalizer::new();
        f.ingest(toks("the french drain"));
        f.ingest(toks("the french drain along")); // commits "the french drain"
        // A later chunk re-transcribes the overlap DIFFERENTLY ("drane"): the
        // committed words must stand — no revision.
        let emitted = f.ingest(toks("the french drane along the fence"));
        // agreement of prev("the french drain along") vs new("the french drane along the fence")
        // beyond committed(3): prev[3]="along", new[3]="along" agree → commit "along".
        assert_eq!(emitted, toks("along"));
        assert_eq!(f.committed_text(), "the french drain along");
    }

    #[test]
    fn no_double_emit_of_overlap() {
        let mut f = Finalizer::new();
        f.ingest(toks("hello world again"));
        let e = f.ingest(toks("hello world again now")); // commit "hello world again"
        assert_eq!(e, toks("hello world again"));
        // Same tokens re-fed must not re-emit.
        let e2 = f.ingest(toks("hello world again now then"));
        assert_eq!(e2, toks("now"));
    }

    #[test]
    fn flush_commits_the_entire_remaining_tail() {
        let mut f = Finalizer::new();
        f.ingest(toks("order twelve two by tens"));
        f.ingest(toks("order twelve two by tens for the deck")); // commits first 5
        let tail = f.flush();
        assert_eq!(tail, toks("for the deck"), "flush finalizes the unconfirmed tail");
        assert!(f.flush().is_empty());
    }
}
```

- [ ] **Step 2: Implement** (`crates/stt/src/finalize.rs`)

```rust
/// Incremental LocalAgreement-2 word-level finalizer (spike `RESULTS.md` Table 2:
/// 19% streaming WER at ≤3 s latency vs 80% for naive segment finalize). Commits
/// a token once two consecutive chunk hypotheses agree on it in position; the
/// committed stream is append-only — a committed word is never revised, even when
/// a later chunk re-transcribes the overlap differently.
#[derive(Default)]
pub struct Finalizer {
    committed: Vec<String>,
    prev_hyp: Vec<String>,
    flushed: bool,
}

impl Finalizer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the full token hypothesis for the latest chunk. Returns the tokens
    /// newly finalized by this chunk (may be empty). `hyp` is the whole chunk's
    /// text tokenized — the finalizer aligns it against the committed prefix.
    pub fn ingest(&mut self, hyp: Vec<String>) -> Vec<String> {
        // Longest common prefix of prev and new hypotheses = the LocalAgreement
        // confirmation. Commit only what extends beyond what's already committed.
        let agree = common_prefix_len(&self.prev_hyp, &hyp);
        let mut newly = Vec::new();
        if agree > self.committed.len() {
            newly = hyp[self.committed.len()..agree].to_vec();
            self.committed.extend_from_slice(&newly);
        }
        self.prev_hyp = hyp;
        newly
    }

    /// Preview tail: the un-finalized remainder of the latest hypothesis (volatile;
    /// shown greyed, never persisted).
    pub fn preview(&self) -> String {
        if self.prev_hyp.len() > self.committed.len() {
            self.prev_hyp[self.committed.len()..].join(" ")
        } else {
            String::new()
        }
    }

    /// Final window: no successor will confirm the tail, so commit all of it
    /// (spike's ∞-horizon flush). Returns the newly-finalized tail.
    pub fn flush(&mut self) -> Vec<String> {
        if self.flushed {
            return Vec::new();
        }
        self.flushed = true;
        if self.prev_hyp.len() > self.committed.len() {
            let tail = self.prev_hyp[self.committed.len()..].to_vec();
            self.committed.extend_from_slice(&tail);
            tail
        } else {
            Vec::new()
        }
    }

    pub fn committed_text(&self) -> String {
        self.committed.join(" ")
    }
}

fn common_prefix_len(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}
```
> **Design note for reviewers:** this is prefix-agreement LocalAgreement, not the spike's suffix/prefix *merge* (`reassemble_dedup`). Prefix-agreement is the incremental form: it emits finalized tokens as the stream grows, which is exactly the append-only cursor contract Plan 05 needs, whereas `reassemble_dedup` reconstructs a whole string in one shot (fine for a batch WER measurement, wrong for live emission). Both share the "align tokens, never revise committed" core. The word-level operation (not segment level) is what `RESULTS.md` proved matters (boundary re-transcription 75–95% at the segment level). **Field-tuning of the agreement rule (LocalAgreement-2 vs -n, punctuation/casing normalization before compare) is the expected iteration surface — measured against the spike's corpus.**

- [ ] **Step 3: Verify** `cargo test -p stt finalize` — green.

- [ ] **Step 4: Commit**
```bash
git add -A && git commit -m "feat(stt): LocalAgreement word-level append-only finalizer (the quality lever)"
```

---

### Task 4: The bias seam + `SttStream` orchestration (pure end-to-end via the fake)

**Files:** create `crates/stt/src/bias.rs`; extend `crates/stt/src/lib.rs` with `SttStream`.

- [ ] **Step 1: Bias tests** (`crates/stt/src/bias.rs`, bottom)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_terms_yield_no_prompt() {
        assert_eq!(build_bias_prompt(&[], 100), None);
    }

    #[test]
    fn terms_are_joined_and_capped() {
        let terms: Vec<String> = (0..150).map(|i| format!("term{i}")).collect();
        let p = build_bias_prompt(&terms, 100).unwrap();
        assert!(p.contains("term0") && p.contains("term99"));
        assert!(!p.contains("term100"), "capped at max_bias_terms (spec ≤100)");
    }
}
```

- [ ] **Step 2: Implement bias** (`crates/stt/src/bias.rs`)

```rust
/// Assemble the whisper `initial_prompt` from the user's vocabulary terms
/// (memory `vocabulary` section, spec §vocabulary point 3). Spike `RESULTS.md`:
/// initial_prompt injection gave +10–19 pp term recall with zero hallucination.
/// This is the v1 biasing SEAM — a later plan swaps in trie/logit-bias by
/// replacing what `SttStream` does with these terms, not this signature.
pub fn build_bias_prompt(terms: &[String], max_terms: usize) -> Option<String> {
    let kept: Vec<&str> = terms.iter().take(max_terms).map(String::as_str).collect();
    if kept.is_empty() {
        return None;
    }
    // A glossary-style list; whisper reads the prompt as prior context, so a
    // natural comma list biases toward these spellings without a rigid schema.
    Some(format!("Terms used in this session: {}.", kept.join(", ")))
}
```

- [ ] **Step 3: SttStream tests** (`crates/stt/src/lib.rs`, bottom — the end-to-end pure test)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn seg(cs0: i64, cs1: i64, t: &str) -> RawSegment {
        RawSegment { start_cs: cs0, end_cs: cs1, text: t.into() }
    }

    #[test]
    fn bias_prompt_is_passed_to_every_decode() {
        // 5s window at 16kHz = 80_000 samples; push two windows' worth.
        let decoder = ScriptedDecoder::new(vec![
            vec![seg(0, 300, "the french drain")],
            vec![seg(0, 300, "the french drain is backing")],
        ]);
        let stream = SttStream::with_decoder(
            Box::new(decoder),
            SttConfig::default(),
            &["french drain".to_string()],
        );
        stream.push_pcm(&vec![0.0; 144_000]); // 9 s → two windows ready
        stream.poll().unwrap();
        stream.poll().unwrap();
        // The scripted decoder recorded the prompt each decode saw.
        // (accessor exposed on the whisper-free path for exactly this assertion)
        let prompts = stream.debug_captured_prompts();
        assert!(prompts.iter().all(|p| p.as_deref() == Some("Terms used in this session: french drain.")));
    }

    #[test]
    fn poll_yields_append_only_segments_and_end_flushes_tail() {
        let decoder = ScriptedDecoder::new(vec![
            vec![seg(0, 300, "order twelve two by tens")],
            vec![seg(0, 400, "order twelve two by tens for the deck")],
        ]);
        let stream = SttStream::with_decoder(Box::new(decoder), SttConfig::default(), &[]);
        stream.push_pcm(&vec![0.0; 144_000]);
        let a = stream.poll().unwrap(); // chunk 0: nothing confirmed yet
        assert!(a.is_empty());
        let b = stream.poll().unwrap(); // chunk 1 agrees → commits the prefix
        assert_eq!(b.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
                   vec!["order", "twelve", "two", "by", "tens"]);
        let tail = stream.end().unwrap(); // flush finalizes the unconfirmed tail
        assert_eq!(tail.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
                   vec!["for", "the", "deck"]);
        // absolute timestamps are monotonic (append-only in time too)
        let mut prev = 0;
        for s in b.iter().chain(tail.iter()) {
            assert!(s.start_ms >= prev);
            prev = s.start_ms;
        }
    }

    #[test]
    fn poll_is_a_noop_until_a_window_is_ready() {
        let stream = SttStream::with_decoder(
            Box::new(ScriptedDecoder::new(vec![])), SttConfig::default(), &[]);
        stream.push_pcm(&vec![0.0; 1000]); // far short of a window
        assert!(stream.poll().unwrap().is_empty(), "no decode, no scripted panic");
    }
}
```

- [ ] **Step 4: Implement `SttStream`** (`crates/stt/src/lib.rs`)

Threading: two mutexes, strict order **engine → input** (never reverse). `push_pcm` takes `input` only (short); `poll`/`preview_tail`/`end` take `engine`, and `poll` briefly takes `input` inside to drain. No thread, no channel, no callback → no deadlock. Emits one `FinalizedSegment` per newly-committed token, with absolute ms derived from the window's `start_sample` (the whole finalized batch shares the chunk's coarse time span — good enough for the live board; word-precise timestamps are Deferred).

```rust
use std::sync::Mutex;

use chunk::Chunker;
use decoder::Decoder;
use finalize::Finalizer;

struct Engine {
    decoder: Box<dyn Decoder>,
    chunker: Chunker,
    finalizer: Finalizer,
    #[cfg(test)]
    captured_prompts: Vec<Option<String>>,
}

pub struct SttStream {
    cfg: SttConfig,
    bias_prompt: Option<String>,
    input: Mutex<Vec<f32>>,      // pending PCM handed off from the audio thread
    engine: Mutex<Engine>,
}

impl SttStream {
    pub fn with_decoder(decoder: Box<dyn Decoder>, cfg: SttConfig, vocab: &[String]) -> Self {
        let bias_prompt = bias::build_bias_prompt(vocab, cfg.max_bias_terms);
        let chunker = Chunker::new(cfg.sample_rate, cfg.chunk_secs, cfg.overlap_secs);
        SttStream {
            input: Mutex::new(Vec::new()),
            engine: Mutex::new(Engine {
                decoder,
                chunker,
                finalizer: Finalizer::new(),
                #[cfg(test)]
                captured_prompts: Vec::new(),
            }),
            bias_prompt,
            cfg,
        }
    }

    #[cfg(feature = "whisper")]
    pub fn with_model(model: &std::path::Path, cfg: SttConfig, vocab: &[String])
        -> Result<Self, SttError> {
        let decoder = whisper::WhisperDecoder::open(model, &cfg.language)?;
        Ok(Self::with_decoder(Box::new(decoder), cfg, vocab))
    }

    /// Buffer PCM. Cheap: a short lock, no decode. Call OFF the real-time audio
    /// thread (hand buffers over from the AVAudioEngine tap — research Q6).
    pub fn push_pcm(&self, pcm: &[f32]) {
        self.input.lock().unwrap().extend_from_slice(pcm);
    }

    /// Drain buffered PCM into the chunker and decode every window now ready,
    /// returning all segments finalized this call (append-only). Runs the long
    /// Metal decode on the CALLER's thread — the shell calls this from a
    /// background thread on its own cadence (Plan 05 Deferred 3).
    pub fn poll(&self) -> Result<Vec<FinalizedSegment>, SttError> {
        let mut eng = self.engine.lock().unwrap();      // engine first...
        {
            let mut input = self.input.lock().unwrap(); // ...then input, briefly
            eng.chunker.push(&input);
            input.clear();
        }                                                // input released before decode
        let mut out = Vec::new();
        while let Some(w) = eng.chunker.take_ready_window() {
            self.decode_window(&mut eng, w, &mut out)?;
        }
        Ok(out)
    }

    /// Volatile preview tail for greyed UI. Never persisted, never append-only.
    pub fn preview_tail(&self) -> String {
        self.engine.lock().unwrap().finalizer.preview()
    }

    /// DONE (supersedes cancel-for-speed canon): flush the remaining buffered
    /// audio as a final window and finalize everything pending. Idempotent.
    pub fn end(&self) -> Result<Vec<FinalizedSegment>, SttError> {
        let mut eng = self.engine.lock().unwrap();
        {
            let mut input = self.input.lock().unwrap();
            eng.chunker.push(&input);
            input.clear();
        }
        let mut out = Vec::new();
        while let Some(w) = eng.chunker.take_ready_window() {
            self.decode_window(&mut eng, w, &mut out)?;
        }
        if let Some(w) = eng.chunker.flush() {
            let start_ms = self.sample_to_ms(w.start_sample);
            let raw = eng.decode_with_prompt(w.samples.as_slice(), self.bias_prompt.as_deref())?;
            let hyp = tokenize_segments(&raw);
            for t in eng.finalizer.flush_or_ingest_final(hyp) {
                out.push(FinalizedSegment { start_ms, end_ms: start_ms, text: t });
            }
        } else {
            for t in eng.finalizer.flush() {
                out.push(FinalizedSegment { start_ms: 0, end_ms: 0, text: t });
            }
        }
        Ok(out)
    }

    fn decode_window(&self, eng: &mut Engine, w: chunk::Window, out: &mut Vec<FinalizedSegment>)
        -> Result<(), SttError> {
        let start_ms = self.sample_to_ms(w.start_sample);
        let raw = eng.decode_with_prompt(&w.samples, self.bias_prompt.as_deref())?;
        let hyp = tokenize_segments(&raw);
        for t in eng.finalizer.ingest(hyp) {
            out.push(FinalizedSegment { start_ms, end_ms: start_ms, text: t });
        }
        Ok(())
    }

    fn sample_to_ms(&self, sample: u64) -> u64 {
        sample * 1000 / self.cfg.sample_rate as u64
    }

    #[cfg(test)]
    fn debug_captured_prompts(&self) -> Vec<Option<String>> {
        self.engine.lock().unwrap().captured_prompts.clone()
    }
}

impl Engine {
    fn decode_with_prompt(&mut self, samples: &[f32], prompt: Option<&str>)
        -> Result<Vec<RawSegment>, SttError> {
        #[cfg(test)]
        self.captured_prompts.push(prompt.map(str::to_string));
        self.decoder.decode(samples, prompt)
    }
}

fn tokenize_segments(raw: &[RawSegment]) -> Vec<String> {
    raw.iter()
        .flat_map(|s| s.text.split_whitespace())
        .map(str::to_string)
        .collect()
}
```
> Two small `Finalizer` helpers referenced above (`flush_or_ingest_final` is just `ingest` then `flush` fused — implement whichever is cleaner during TDD; the tests pin the *behavior*, not the method name). Keep the final-window path committing the whole tail.

> **`Send + Sync`:** the `Box<dyn Decoder>` is `Send` (trait bound); wrapped in `Mutex`, `SttStream` is `Send + Sync`. Plan 07 wraps it `Arc<SttStream>` and UniFFI exposes `push_pcm`/`poll`/`preview_tail`/`end` as `&self` methods — no async, no callback interface, which is precisely why this can't deadlock across the FFI boundary.

- [ ] **Step 5: Verify** `cargo test -p stt` — all pure tests green (no feature, no model).

- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat(stt): SttStream — caller-driven pump, bias injection, flush-on-end (DONE)"
```

---

### Task 5: The whisper backend (`whisper` feature) + real-model gate

**Files:** create `crates/stt/src/whisper.rs`.

The only file that imports `whisper-rs`. Mirrors the spike's `make_ctx`/`make_params`/`decode` (`spikes/stt-whisper/src/main.rs`), now behind the trait.

- [ ] **Step 1: Implement** (`crates/stt/src/whisper.rs`)

```rust
use std::path::Path;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters,
};

use crate::decoder::{Decoder, RawSegment};
use crate::SttError;

/// whisper.cpp backend (Metal). Owns a loaded model context; each `decode`
/// creates a fresh state (whisper-rs pattern). The crate NEVER downloads the
/// model — `open` reads a file the shell has already provisioned.
pub struct WhisperDecoder {
    ctx: WhisperContext,
    language: String,
}

impl WhisperDecoder {
    pub fn open(model: &Path, language: &str) -> Result<Self, SttError> {
        let mut params = WhisperContextParameters::default();
        params.use_gpu(true); // Metal (spike confirmed `use gpu = 1`, no CPU fallback)
        let ctx = WhisperContext::new_with_params(
            model.to_str().ok_or_else(|| SttError::ModelLoad("non-utf8 model path".into()))?,
            params,
        )
        .map_err(|e| SttError::ModelLoad(e.to_string()))?;
        Ok(Self { ctx, language: language.to_string() })
    }
}

impl Decoder for WhisperDecoder {
    fn decode(&mut self, samples: &[f32], initial_prompt: Option<&str>)
        -> Result<Vec<RawSegment>, SttError> {
        let mut state = self.ctx.create_state().map_err(|e| SttError::Decode(e.to_string()))?;
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(&self.language));
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_special(false);
        params.set_print_timestamps(false);
        params.set_translate(false);
        if let Some(p) = initial_prompt {
            params.set_initial_prompt(p);
        }
        state.full(params, samples).map_err(|e| SttError::Decode(e.to_string()))?;
        let n = state.full_n_segments();
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            if let Some(seg) = state.get_segment(i) {
                let text = seg.to_str_lossy().map(|c| c.into_owned()).unwrap_or_default();
                out.push(RawSegment {
                    start_cs: seg.start_timestamp(),
                    end_cs: seg.end_timestamp(),
                    text: text.trim().to_string(),
                });
            }
        }
        Ok(out)
    }
}
```

- [ ] **Step 2: Real-model smoke test** (`crates/stt/src/whisper.rs`, bottom — env+feature gated, like `anthropic_smoke`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Runs ONLY when the `whisper` feature is on AND MURMUR_WHISPER_MODEL points
    /// at a real ggml model file. #[ignore] keeps it out of `cargo test`; CI never
    /// has the model, so CI never runs it. Manual: reads the model, decodes 1 s of
    /// silence, asserts the pipeline returns without error.
    #[test]
    #[ignore = "needs a real model file via MURMUR_WHISPER_MODEL"]
    fn real_model_decodes_silence() {
        let model = std::env::var("MURMUR_WHISPER_MODEL")
            .expect("set MURMUR_WHISPER_MODEL to a ggml-*.bin path");
        let mut d = WhisperDecoder::open(std::path::Path::new(&model), "en").unwrap();
        let silence = vec![0.0f32; 16_000];
        let segs = d.decode(&silence, Some("Terms used in this session: french drain.")).unwrap();
        // silence may yield zero or a blank segment — the contract is "no error".
        let _ = segs;
    }
}
```

- [ ] **Step 3: Document the model files** — add a `## Models` block to `crates/stt/Cargo.toml`'s neighbouring note or a short `crates/stt/README.md`:
> The crate opens a ggml whisper model the **shell** provisions (download/on-demand-resources is not the crate's job). v1 target files (MIT, from `huggingface.co/ggerganov/whisper.cpp`; `ggml-org` returns 401 today — spike note):
> - `ggml-base.en-q5_1.bin` (~57 MB) — default; RTF 0.009, WER 5.8% clean.
> - `ggml-small.en-q5_1.bin` (~182 MB) — higher accuracy; RTF 0.021, WER 4.7% clean.
> Selection (base vs small, quality vs size/battery) is a shell/config decision, informed by the pending on-device iPhone tier (`RESULTS.md` Table 4).

- [ ] **Step 4: Verify**
- `cargo test -p stt` (no feature) — green, whisper.rs not compiled.
- `cargo test -p stt --features whisper` — compiles the native stack; the smoke test is `#[ignore]`d so it's skipped.
- Manual (dam, on device/mac with a model): `MURMUR_WHISPER_MODEL=… cargo test -p stt --features whisper -- --ignored`.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(stt): whisper.cpp backend behind `whisper` feature; env-gated real-model smoke"
```

---

### Task 6: Integration contract, workspace-green verification, docs

**Files:** create `crates/stt/tests/stream_append_only.rs`; modify `README.md`.

- [ ] **Step 1: End-to-end append-only integration test** (public API only, no feature, no model)

```rust
//! Append-only streaming contract (spec Rev 2 §2) via the public API and a
//! scripted decoder — proves the finalized stream that Plan 05's LiveExtractor
//! will consume never revises a committed word, and end() flushes the tail.

use stt::{RawSegment, ScriptedDecoder, SttConfig, SttStream};

fn seg(t: &str) -> RawSegment {
    RawSegment { start_cs: 0, end_cs: 300, text: t.into() }
}

#[test]
fn finalized_stream_is_append_only_across_a_session() {
    // Overlap re-transcribes "drain" as "drane" in a later chunk — must not revise.
    let decoder = ScriptedDecoder::new(vec![
        vec![seg("the french drain needs")],
        vec![seg("the french drain needs regrading")],
        vec![seg("the french drane needs regrading before the pour")],
    ]);
    let stream = SttStream::with_decoder(Box::new(decoder), SttConfig::default(), &[]);
    stream.push_pcm(&vec![0.0; 208_000]); // ~13 s → three windows (5s/1s → step 4s)

    let mut finalized = Vec::new();
    while { let batch = stream.poll().unwrap(); let n = batch.len();
            finalized.extend(batch); n > 0 } {}
    finalized.extend(stream.end().unwrap());

    let text: Vec<&str> = finalized.iter().map(|s| s.text.as_str()).collect();
    // "the french drain" was committed early; the "drane" re-transcription never
    // overwrites it — the stream only ever appended.
    assert!(text.starts_with(&["the", "french", "drain", "needs"]));
    assert!(text.contains(&"regrading"));
    assert!(!text.contains(&"drane"), "a committed word is never revised");

    // Absolute-ms timestamps are monotonic — append-only in time.
    let mut prev = 0;
    for s in &finalized {
        assert!(s.start_ms >= prev);
        prev = s.start_ms;
    }
}
```

- [ ] **Step 2: Document the murmur-core wiring contract** (in `crates/stt/README.md` — the seam Plan 07 implements)

> **Integration with `murmur-core` (deferred to Plan 07 — the FFI/shell tick loop):**
> `crates/stt` and `murmur-core` do **not** depend on each other. The shell owns both pumps and wires them:
> ```
> // shell background thread, on cadence:
> stt.push_pcm(pcm);                                  // audio thread hands off buffers
> for seg in stt.poll()? {                            // append-only finalized segments
>     store.append_transcript(&session_id, &format!("{} ", seg.text))?;
> }
> live_extractor.maybe_extract().await?;              // Plan 05: cursor advances over new transcript
> // on DONE:
> for seg in stt.end()? { store.append_transcript(&session_id, &format!("{} ", seg.text))?; }
> // then queue end-of-session process() — the AUTHORITATIVE pass (Plan 04).
> ```
> Why deferred, not built here: (1) cadence is shell policy (Plan 05 Deferred 3 already put the LiveExtractor tick in the shell); (2) both `stt.poll` and `LiveExtractor.maybe_extract` are shell-driven pumps with no core-side coupling (Plan 05 self-review constraint 4); (3) building it here forces an `stt ↔ murmur-core` dependency both plans avoid. The contract above is the whole seam — Plan 07 implements it across UniFFI.

- [ ] **Step 3: README plan-series line** (`README.md`)
```markdown
Done: 01 foundation, 02 memory + reflection + context assembler, 03 domain + storage, 04 processing pipeline + reflection coordinator, 05 live extraction, 06 STT crate.
Next: 07 (FFI: UniFFI boundary — wire STT + LiveExtractor + processing into the platform shell).
```

- [ ] **Step 4: Full verification**
- `nix develop -c cargo test --workspace` → all green **on default features** (this is the CI invocation: no model, no cmake/clang needed at compile time for the default `stt` build).
- `nix develop -c cargo build -p stt --features whisper` → native stack compiles.
- `nix develop -c cargo clippy --workspace --all-targets` → zero warnings (fix mechanically; no `#[allow]`; STOP and report if a fix changes behavior). Also run `cargo clippy -p stt --features whisper --all-targets`.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "test(stt): append-only e2e; docs: murmur-core wiring contract; plan 06 done"
```

---

## Deferred (named, for later plans)

1. **The full STT → live-extraction tick loop (Plan 07, FFI + shells).** The contract is documented (Task 6); the loop that couples `stt.poll` → `append_transcript` → `LiveExtractor.maybe_extract` is shell orchestration across UniFFI. Deliberately not built here to keep `stt` and `murmur-core` decoupled.
2. **On-device iPhone tier verification (`RESULTS.md` Table 4, PENDING).** The GO is provisional pending a device check: `base.en`/`small.en` RTF<1.0 and no thermal kill over 10 min locked. Mac margins (RTF 0.009–0.02) make this expected-pass, but it is the one unretired GO condition — run before shipping (needs dam's device; `spikes/stt-whisper/ios/README.md`).
3. **Trie / logit-bias hotword decoder (research §4; the biasing ceiling).** v1 uses `initial_prompt` (+10–19 pp, proven). The deeper decoder-internal biasing (19–22% B-WER lit. gains) is an optimization, not a prerequisite — swaps in behind `build_bias_prompt`'s seam without touching the pipeline. Also untested: a full 100-term list against real noisy jobsite audio (the case most likely to hallucinate).
4. **Word-precise timestamps.** v1 finalized segments carry the chunk's coarse span (`start_ms` = window start). Word-level alignment (whisper cross-attention, or Table 2's segment timestamps) for audio-scrubbing UI is a later concern — the `FinalizedSegment` struct already has the fields.
5. **Model download / on-demand resources / model selection UI.** The crate opens a provisioned file. Fetching, storage, and base-vs-small selection are shell/config concerns (Task 5 doc).
6. **Rolling prior-transcript context in the prompt.** The spike (and v1) use the prompt slot purely for bias terms. Carrying recent transcript for cross-chunk coherence could help but risks diluting the ≤100 bias terms (whisper's 224-token prompt window) — revisit only with evidence.
7. **Battery/thermal instrumentation, chunk-size auto-tuning.** Adaptive chunk/model selection under thermal pressure (research Q8) is a shell-driven policy once on-device numbers exist. Config is already exposed (`chunk_secs`, model choice).
8. **Android backend.** Option B's payoff (research §6): the same pure engine + `Decoder` trait; only a JNI/NDK `WhisperDecoder` and audio handoff differ. Out of v1 scope; the trait keeps the door open cheaply.
9. **Diarization (FluidAudio/pyannote).** Nice-to-have per the brief; no first-party on-device path. Not v1.
10. **VAD-gated decoding.** Skipping silent windows to cut battery/compute is a real optimization but adds a component; the finalizer already tolerates empty hypotheses. Deferred until battery numbers justify it.

## Self-Review Notes

- **Spec coverage:** Rev 2 §2 on-device streaming STT ✓ (Tasks 2–5); append-only finalized stream ✓ (Task 3 finalizer + Task 6 e2e — a committed word is never revised, carrying the spike's `finalized_stream_is_append_only`/`no_double_emit_of_overlap` invariants); volatile preview tail ✓ (`preview_tail`); DONE = flush-not-drop ✓ (`end()`, supersedes cancel-for-speed canon); §vocabulary point 3 biasing ✓ (Task 4 `build_bias_prompt`, initial_prompt injection, ≤100 cap); offline/on-device ✓ (no network, model is a local file). Live-is-provisional / process() authoritative ✓ (stated design constant + integration contract queues process() as truth).
- **The four hard requirements, discharged:** (1) Decode trait — `Decoder` isolates whisper; the entire pipeline is tested against `ScriptedDecoder` with zero whisper dependency (Tasks 2–4, 6). (2) Hermetic CI — `whisper-rs` is `optional`, `default = []`; `cargo test --workspace` compiles on Linux with no model/cmake/clang; real-model test is `#[ignore]` + env-gated + feature-gated (three locks). (3) Threading — caller-driven pump, no thread/channel/callback, strict two-lock order (engine→input) = can't deadlock; `Send + Sync` for a direct UniFFI `&self` object. (4) Build hygiene — `flake.nix` gains cmake/clang/LIBCLANG_PATH (Task 1) for `--features whisper`; CI stays on default features. Feature-gate chosen over macOS-only workspace **because** existing Linux CI for the other three crates must stay green.
- **Design constants traced to measurement:** 5 s/1 s default and word-level LocalAgreement (not naive segment finalize) come straight from `RESULTS.md` Table 2 (19% vs 80% WER); base.en/small.en q5_1 from Table 1/3; initial_prompt biasing from Table 3 (+10–19 pp, 0 hallucination); flush-on-end from the ∞-horizon final chunk in `stream.rs::finalize`.
- **API surface for Plan 07 (FFI):** `SttStream::{with_model, with_decoder, push_pcm, poll, preview_tail, end}` — all `&self`, sync, `Send + Sync`. No async, no callback interface. UniFFI wraps `Arc<SttStream>`. `FinalizedSegment { start_ms, end_ms, text }` and `SttConfig` are plain structs (add `serde`/UniFFI derives in Plan 07 if the boundary needs them — not added now to keep deps minimal).
- **Judgment calls for reviewers:** (a) **caller-driven pump over internal worker** — the shell already ticks LiveExtractor; a worker adds a shutdown protocol + mpsc + callback interface (3 deadlock surfaces) for nothing. (b) **feature-gated optional whisper** over macOS-only workspace — keeps `cargo test --workspace` cross-platform/hermetic; the cost is a `#[cfg(feature)]` on one file and one constructor. (c) **prefix-agreement LocalAgreement-2** over the spike's one-shot `reassemble_dedup` — the incremental form is what emits append-only tokens live; same "never revise committed" core, productionized. (d) **integration loop deferred to Plan 07** — building it here couples `stt`↔`murmur-core`, which both plans avoid; the contract is fully documented instead. (e) **coarse per-chunk timestamps in v1** — the live board doesn't need word-precise times; the struct carries the fields for later. (f) **`end()` idempotent, flushes** — DONE means finalize everything pending; the old cancel-for-speed canon is explicitly superseded (this is a capture whose transcript feeds the authoritative process()).
- **Test-count checkpoint:** T1 +2 (decoder), T2 +4 (chunk), T3 +4 (finalize), T4 +2 (bias) +3 (stream), T5 +1 (#[ignore] smoke), T6 +1 (e2e) ≈ **17 new**, of which 16 run in default CI. Counts are expectations, not gates.
- **Constraints surfaced for Plan 07:** (1) `SttStream` is `&self`/sync — wrap `Arc`, call `poll` from a background thread (long Metal decode); do NOT call from the audio render callback (research Q6). (2) The shell must provision the model file and pass its path — the crate never downloads. (3) The shell owns the tick cadence for BOTH `stt.poll` and `LiveExtractor.maybe_extract`, and appends finalized segments to `Store::append_transcript` between them. (4) Build the app/FFI target with `--features whisper`; CI without it. (5) `SttConfig`/`FinalizedSegment` may need UniFFI/serde derives at the boundary.
```
