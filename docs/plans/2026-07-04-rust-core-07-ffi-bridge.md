# Murmur Rust Core — Plan 07: The FFI Bridge (real core behind sac's app)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Rust tasks are hermetic (MockProvider/wiremock, no network). Swift tasks carry sim-testable checks.

**Goal:** Replace `DemoWalkEngine` with a real bridge that drives sac's finished iOS app off `murmur-core`. This is the **last plan before "first real walk"** — every decision biases toward the demo-able end-to-end path: real on-device STT text → live board ticks → DONE → a real LLM-built document in review → PDF → sent. The seam sac built (`WalkEngine` protocol, swap at `AppModel.init(engine:)`) does not move; we implement behind it.

**HARD DEPENDENCY — execution order `06a → 06 → 07`:** Plan 06a (items `source` column + atomic-at-finish swap: `clear_session_outputs` runs only *after* a successful re-extract) is a **prerequisite of Tasks 7 and 8**, not merely a nicety. Rationale: without it, `process()` opens a window where the live board is tombstoned but the authoritative set hasn't been written yet, and a live-extractor tick that re-queries the board during that window would broadcast an **empty board** (see D3). This plan additionally defends against the race with in-process mutual exclusion (D3), but the storage-level fix is the real closure and must land first.

**The contract we implement** (sac's `docs/HANDOFF-ios-ffi.md`, verified against `apps/ios/Sources/Engine/WalkEngine.swift`):

```swift
@MainActor protocol WalkEngine: AnyObject {
    func begin(trade: TradeFixture) -> AsyncStream<WalkEvent>   // per-session stream
    func append(transcript: String)
    func finish() async -> DocumentModel
}
```

- `begin` → `Store::start_session(job_id)` + persist the template key, hand back a **fresh per-session** event stream.
- `append(transcript:)` → `Store::append_transcript` + tick the session's `LiveExtractor` (`maybe_extract`).
- events → one **whole-board snapshot per live pass** (batched), delivered on the main thread.
- `finish()` → `end_and_record_session` + `SessionProcessor::process` (two-phase, budgeted < 8 s) → a structured **document artifact** → mapped to `DocumentModel` by a **bridge-side Swift formatting layer**.

---

## Architecture — decisions, justified

### D1. A new `crates/ffi` crate; `murmur-core` stays UniFFI-free
CANON's architecture list already names `ffi (UniFFI, planned)` as its own crate. The FFI-facing dictionaries/enums (`BoardItem`, `DocumentPayload`, `WalkEvent`, `EngineConfig`) are **thin projections of `murmur-core` domain types**, defined and `uniffi`-derived in `crates/ffi` — so `murmur-core` keeps zero binding-generator deps and stays a clean, testable library (Plan 01–05 posture preserved). The boundary is **domain types only** (CANON): never harness wire types (`Message`, `ContentBlock`, `Usage`), never `serde_json::Value`, never an async trait object. `crates/ffi` depends on `murmur-core` + `harness`; it is added to the workspace `members`. `apps/ios` stays outside the cargo workspace and consumes the generated Swift package.

### D2. Formatting layer lives in **Swift (bridge-side)**, not core
`murmur-core` must never carry pre-formatted UI copy (`"TUE — JUL 01"`, `"$285"`, `"EST-0047"`, `"SEND ESTIMATE"`). The core `DocumentPayload` is **display-copy-free structured data**:

```
DocumentPayload {
  doc_kind: String,            // "estimate" | "report" | "inspection" (data, not "ESTIMATE")
  doc_number: u64,             // core-minted integer (see D5); Swift renders "EST-%04d"
  job_date_unix: u64,          // Swift renders "JUL 01 2026" in the device locale
  total_kind: String,          // "sum" | "static" — how the total is computed
  total_label_key: String,     // "total" | "deposit_deduction" | "findings" — a KEY, not copy
  static_total_cents: i64?,    // for non-summing templates (inspection findings)
  lines: [DocLine],
  queued: bool,                // true when finish() degraded offline (D9)
}
DocLine {
  id: String,                  // stable core item id (for edit round-trips)
  title: String,               // speaker's own terms, from the transcript
  detail: String,              // e.g. "DELIVERED + INSTALLED"
  qty: String,                 // "3 CU YD", "× 4", "60 LF" — trade unit, spoken
  amount_cents: i64?,          // None ⇒ no dollar amount (see gap semantics below)
  section: String?,            // template section ref, e.g. TREC "§ 5.3"
  is_gap: bool,                // TEMPLATE-AWARE (see D2a) — NOT simply amount_cents.is_none()
}
```

**D2a. Gap semantics are per `doc_kind` — `is_gap` is set by the builder, not derived from `amount_cents`.** A missing dollar amount is a gap only for **dollar templates**; other templates have intentional non-dollar lines that are *not* R6 gaps:

| `doc_kind` | Normal non-dollar line | What counts as a gap (`is_gap = true`) |
|---|---|---|
| `estimate` (landscape) | — (every line is priced) | `amount_cents == None` — an unheard price/qty (R6: `——`, never guessed) |
| `report` (property) | `"—"` / `"OK"` / `"NOTE"` rows (walls normal, water heater logged) are *deliberate* non-dollar lines — **not** gaps | a deduction line whose amount wasn't heard — the builder sets `is_gap` explicitly |
| `inspection` | `§`-section findings carry no dollar amount at all — `amount_cents` is *always* `None` and that is **normal** | a finding the inspector flagged as not-yet-assessed — a distinct `is_gap` signal from the extraction pass, never "no amount" |

Therefore `is_gap` is an **explicit field the `build_document` tool emits** (from what the model heard was left open), and the Swift layer renders `is_gap` (the `subWarn`/`——`/"NOT HEARD" treatment) — it must **never** infer a gap from `amount_cents == None`, which would wrongly flag every inspection finding and every property "OK" row. Task 4 enforces this in the tool schema (`is_gap` optional, defaulting per-template) and prompt.

The **Swift adapter** (`MurmurEngine`) maps `DocumentPayload → DocumentModel`: cents → `"$285"`, `doc_number` → `"EST-0047"`, `job_date_unix` → `"JUL 01 2026"`, `total_label_key` → the template's display string. **Letterhead / board-chrome display copy** (`bizCaps`, `bizSub`, `boardMeta`, `dateLabel`, `openLabel`, `countTitle`) is **business-profile + template chrome that never belonged in core** — it stays in the Swift `TradeFixture`/business-profile layer (spec §5 letterhead onboarding). Core owns *what was said and computed*; Swift owns *how it reads*. This is Commitment 2 ("one schema, many renderings") expressed at the boundary: the same `DocumentPayload` will later feed the PDF and CSV without re-crossing FFI.

Justification for Swift over core-side formatting: (a) locale/currency formatting is a platform concern (`NumberFormatter`, `Date`); (b) keeps R7 inspectability — core rows are raw data an auditor can diff; (c) avoids a core dependency on presentation that would have to change per-renderer.

### D3. Event story for the swap: **whole-board snapshot per live pass**, not per-item diffs
One event case carries the current board: `WalkEvent.boardUpdated(items: [BoardItem])`. The bridge emits it exactly **once per `maybe_extract` pass** (after the pass lands, re-query `list_items_for_session`, project, emit) — this *is* "batched per live pass." The **swap at finish is the same event**: after `SessionProcessor::process` tombstones the live board and re-creates the authoritative set, the bridge emits one terminal `boardUpdated` with the authoritative items.

Why snapshot over per-item `itemCaptured` diffs: Plan 05's swap contract **tombstones live items and creates brand-new authoritative ids** (`live_extraction_e2e` asserts `live_ids` do not survive). Per-item diff events would therefore need id remapping the core does not provide, or would animate a full teardown/rebuild anyway. A whole-board snapshot sidesteps it entirely — the adapter yields `[BoardItem]`, SwiftUI's `ForEach(id: \.id)` computes the visual diff, and the live→authoritative swap is just the last snapshot the stream carries.

**The tick/finish race — and why the terminal-snapshot claim alone is insufficient.** It is *not* true that the swap window is invisible merely because `finish()` emits its snapshot after `process()` returns. A live-extractor **tick** (an in-flight `maybe_extract` fired by an `append` that arrived just before or during DONE) re-queries `list_items_for_session` to build its own `boardUpdated`. If that re-query lands inside `process()`'s pre-06a "cleared but not yet re-extracted" window, the tick broadcasts an **empty board** — a visible flicker the terminal snapshot can't prevent because it's a *different* event. Two mitigations, both adopted:

- **(a) Plan 06a is a hard dependency** (see plan header). It makes `clear_session_outputs` run only *after* a successful re-extract, so the empty window closes at the storage layer — a mid-swap re-query never sees an empty board.
- **(b) In-process mutual exclusion.** The session's `LiveExtractor` lives behind a per-session `tokio::sync::Mutex` (D7). `finish()` **acquires that same mutex and holds it across the entire `process().await`**, so no tick can interleave with end-of-session processing. Ticks during `finish` are semantically pointless anyway (the board is about to be replaced), so serializing them behind `finish` costs nothing and removes the race independent of 06a's timing. `append` calls that arrive during `finish` still persist their transcript (a short scoped `Store` lock, never the extractor mutex) and are simply not tick-processed until — well, there is no "until": the session ends. Their text is still caught by `process()`'s authoritative extraction.

With both in place the only board snapshots the UI can observe are: live-pass snapshots *before* DONE, and the single authoritative snapshot *after* `process()`. No empty intermediate is reachable.

**Coordinated change with sac (small, owned by sac):** `WalkEvent` gains `case boardUpdated([CapturedFixture])` and `AppModel.startWalk()`'s event loop changes `self.items.append(item)` → `self.items = items` (assign, not append). See Task 10 for the exact diff (including the `addPhoto()` invariant fix this forces). This is inside sac's `AppModel`; merged via sac's PR lane. `.itemCaptured` may be kept as a deprecated alias during the transition or removed — decide with sac; the adapter only emits `boardUpdated`.

### D4. Template keys `landscape | property | inspection` — persisted on the session
Assume ack (ROADMAP: "dam: yes — needs sac's ack"; treated as canon here). Core gains a nullable `template` column on `sessions` (additive migration). `begin(template:)` sets it; `SessionProcessor` reads it to select the extraction vocabulary and the document layout. Persisting (not pass-through) keeps reprocessing template-consistent and is sync-ready. The three keys are **data that selects a prompt + a document shape**, never trade logic in code (spec §3: "templates as the spine… trade #4 must be a file we add").

### D5. Core mints document numbers (per-job-kind sequences)
Constraint: "core mints document numbers (per-job sequences) — needs a small store addition." Add a `document_sequences` table and `Store::mint_document_number(doc_kind) -> u64` (monotonic per `doc_kind` per device, transactional). Core returns the **integer**; Swift renders the prefix (`EST-`, `MO-`, `IR-`). Minting happens once, when the document artifact is built (finish path), so a re-`process()` of the same session reuses the already-minted number (idempotent: store the minted number in the document artifact JSON; reprocessing reads it back rather than minting again).

### D6. `finish()` is two-phase and budgeted < 8 s
The build-beat animation is timed to < 8 s with no spinner (CORE.md quality bar). `finish()` =
1. `end_and_record_session` (instant, local).
2. `SessionProcessor::process` — **phase A** extraction agent (already exists), **phase B** a forced `build_document` call that emits the structured document artifact (new; analogous to the existing forced `summarize`). Budget: phase A ≈ 4 s, phase B ≈ 3 s, with a documented total ceiling; both share `transcript_budget_tokens`. The bridge documents the budget in code and the FFI surface exposes no spinner state — the beat is the show.

`build_document` is a template-aware tool: it emits `lines` with `amount_cents` **only for amounts actually spoken** ("call it twelve hundred" → a target/price line); everything unheard is a **gap** (`amount_cents: null`, `is_gap: true`) — R6, never a guessed value. This is the single most important core addition for a demo-able document.

### D7. `LiveExtractor` across FFI: an internal actor; the Store lock is never held across `maybe_extract`
`LiveExtractor::maybe_extract` is `&mut self` (Plan 05 D7 / Deferred 3). The bridge owns one `LiveExtractor` **per active `WalkSession`** behind an async actor (a `tokio::sync::Mutex<LiveExtractor>` guarding only the extractor, driven from a single-consumer task). `append_transcript` is fire-and-forget: it writes the chunk through the `Store` (a short scoped lock) and signals the tick task; the tick task calls `maybe_extract` **holding no `Store` lock** — the extractor takes scoped `Store` guards internally (verified in `live.rs`: every guard is dropped before the `await`). This directly honors "NEVER hold the Store lock across `maybe_extract` (self-deadlock)": the bridge never wraps the extractor call in a `store.lock()`.

The `tokio::sync::Mutex<LiveExtractor>` doubles as the **tick/finish serialization point** (D3b): `finish()` acquires it and holds it across `process().await`, so no live tick can interleave with end-of-session processing. This is an *extractor* mutex, never the *Store* mutex — the "no Store lock across `maybe_extract`/`process().await`" rule is unaffected (the Store is still locked only in scoped guards inside both paths).

### D8. STT is staged; the FFI surface makes stage 2 additive
- **Stage 1 (this plan):** TEXT append. sac's `SFSpeechRecognizer`/`ScriptedSource` keep working unchanged; `append(transcript:)` → `store.append_transcript` + `LiveExtractor` tick. The engine only ever receives text (CANON's HANDOFF posture for v1 STT).
- **Stage 2 (after `crates/stt` lands, follow-up):** an **additive** FFI method `append_audio(buffer)` whose finalized STT segments call the *same* `append_transcript` path. The surface is designed so stage 2 adds a method, never changes `append(transcript:)` — the `WalkSession` object grows, the text path is untouched. **Threading note (from Plan 06's contract):** `SttStream::poll()` runs a long Metal/whisper decode and **must be driven off the audio-render thread** (never block the real-time audio callback); the bridge will pump it on a dedicated task/thread and feed finalized segments into `append_transcript` — so `append_audio` is a cheap enqueue, decode happens elsewhere. Flagged here so the stage-2 method signature (enqueue, not decode-inline) is designed correctly now.

### D9. Offline degradation (contract 3): capture never lost
`append` is always safe (pure local write; a failed live pass is already swallowed, Plan 05). `finish()` on no network: `process()`'s LLM phase fails → session marked `Failed` (retryable, cost logged) — but **the transcript and the live items persist**. The bridge catches the process error and returns a **partial `DocumentPayload`** built from the current live `items` (all `amount_cents: null` → all gaps) with `queued: true`, so the UI shows a partial document and the operator loses nothing. Retry rides `SessionProcessor::process_pending` on reconnect (app-shell trigger, Deferred). No capture is ever lost.

### D10. `DemoWalkEngine` kept behind a launch arg
Delete nothing (the seam promise). `GalleryApp`/`AppModel` selects the engine at init: real `MurmurEngine` when an API key + config resolve; `DemoWalkEngine` when launched with `demo=1` **or** when no key is configured (so the design gallery and scripted `autoflow` demos still run with zero backend). This preserves every existing launch arg and keeps the demo path alive for screenshots/CI.

### D11. Provider routing + BYOK key hygiene
`EngineConfig` carries **three routing purposes** — `live_extraction` (cheap, e.g. `claude-haiku-4-5`), `processing` (strong, e.g. `claude-sonnet-4-5`), `reflection` (cheap) — plus one **opaque BYOK key string** (from the iOS Keychain, crosses FFI as an opaque `String`) and an optional `base_url` (PPQ). The key is **never logged** (no `Debug` derive that prints it; a manual `Debug` redacts it). The bridge builds one `AnthropicProvider` per distinct (model, key, base_url) and shares `Arc`s across `LiveExtractor`/`SessionProcessor`/`ReflectionCoordinator` by purpose.

### D12. Harness patches for PPQ (from HANDOFF "also worth upstreaming")
Verified against `crates/harness/src/providers/anthropic.rs`: `with_base_url` **already exists**; **`Authorization: Bearer` is missing**, and the `walk` example does **not** honor `ANTHROPIC_BASE_URL`. PPQ exposes an Anthropic-compatible `/v1/messages` with **Bearer-only** auth. If sac's uncommitted patch hasn't landed when this plan runs, Task 1 adds both properly (Bearer header alongside `x-api-key`, env override in the example) with wiremock tests.

### DE-SCOPED (explicit, for dam)
The vision's **generative layout-ops protocol** (spec §5) is **not** in this plan. sac built concrete SwiftUI views; the milestone path is the WalkEngine bridge to those views. The layout-op vocabulary (per-item arrival diffs, agent-driven recomposition) returns **post-milestone** as its own plan. `boardUpdated` snapshots are deliberately coarse for exactly this reason.

**Tech Stack:** `uniffi` (0.28+, proc-macro + `uniffi-bindgen` for Swift, `async_runtime = "tokio"`); existing `murmur-core`/`harness` deps; `reqwest`/`tokio` already in the workspace. iOS ≥ 17 (project deployment target), Swift 5.9. All Rust tests hermetic (MockProvider/wiremock). Swift adapter tests use a fixture-backed engine + a sim smoke run.

**Spec:** CORE.md §1 (bulletproof capture / offline), §2 (live board), §3 (templates as spine), §4 (honest gaps — R6), §5 (the paper / one-schema-many-renderings — Commitment 2), §6 (< 8 s), R7 (inspectable), R9 (cost logged). CANON: FFI boundary at domain types; swap-contract; template keys; secrets never in context. Plan 05 Deferred 3 (FFI wraps `maybe_extract`) and 4 (model routing config).

---

## File Structure

```
crates/
  ffi/                              # NEW crate (workspace member)
    Cargo.toml                      # uniffi (proc-macro, NO build feature), murmur-core, harness, tokio
    src/
      lib.rs                        # uniffi::setup_scaffolding!(); re-exports
      engine.rs                     # MurmurEngine, EngineConfig, provider routing
      session.rs                    # WalkSession: append/finish, LiveExtractor actor, event emit
      events.rs                     # WalkEvent, BoardItem, WalkEventListener (callback iface)
      document.rs                   # DocumentPayload, DocLine; Artifact(JSON) -> payload mapping
      convert.rs                    # domain -> FFI dictionary projections (CapturedItem -> BoardItem)
    tests/
      bridge_e2e.rs                 # begin→append→(live snapshots)→finish→payload, MockProvider
  harness/
    src/providers/anthropic.rs      # MODIFY (Task 1): Authorization: Bearer + tests
  murmur-core/
    src/store/
      migrations.rs                 # MODIFY: v_next template column; v_next document_sequences
      sessions.rs                   # MODIFY: set_session_template; template on Session
      documents.rs                  # NEW: mint_document_number + document_sequences access
    src/pipeline/
      tools.rs                      # MODIFY: BuildDocumentTool (structured document artifact)
      prompts.rs                    # MODIFY: build_document prompt, template-aware
      mod.rs                        # MODIFY: SessionProcessor phase B = build_document; reads template
    src/domain.rs                   # MODIFY: Session.template; (optional) StructuredDoc types if not JSON
    examples/walk.rs                # MODIFY (Task 1): honor ANTHROPIC_BASE_URL
apps/ios/
  Sources/Engine/
    WalkEngine.swift                # MODIFY (with sac): WalkEvent.boardUpdated
    MurmurEngine.swift              # NEW: MurmurEngine: WalkEngine adapter + formatting layer
  Sources/App/
    AppModel.swift                  # MODIFY (with sac): event loop assign; engine selection
    GalleryApp.swift                # MODIFY: construct MurmurEngine or DemoWalkEngine (demo=1 / no key)
  project.yml                       # MODIFY: add generated MurmurCoreFFI Swift package + xcframework
  Packages/MurmurCoreFFI/           # NEW: generated Swift bindings + xcframework (checked in or built)
README.md                          # MODIFY: plan-series line
```

Run cargo via the dev shell or `nix shell nixpkgs#cargo nixpkgs#rustc -c cargo <cmd>` from the repo root. iOS builds via `cd apps/ios && xcodegen generate && xcodebuild … -destination 'platform=iOS Simulator,name=iPhone 17 Pro'`.

---

## Part A — Core & harness prerequisites (hermetic, land first)

### Task 1: Harness PPQ patches — `Authorization: Bearer` + `ANTHROPIC_BASE_URL`

**Files:** Modify `crates/harness/src/providers/anthropic.rs`, `crates/murmur-core/examples/walk.rs`.

> Skip only if sac's patch PR has already merged both. Verify with `grep -n "Bearer" crates/harness/src/providers/anthropic.rs` and `grep -n ANTHROPIC_BASE_URL crates/murmur-core/examples/walk.rs`.

- [ ] **Step 1 — failing test** (add to the tests module in `anthropic.rs`):

```rust
    #[tokio::test]
    async fn sends_bearer_and_x_api_key_for_ppq_compat() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("authorization", "Bearer sk-test"))
            .and(header("x-api-key", "sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let provider = AnthropicProvider::new("sk-test", "claude-haiku-4-5")
            .with_base_url(server.uri());
        provider.complete(request()).await.unwrap();
    }
```

- [ ] **Step 2 — run to see failure:** `nix shell nixpkgs#cargo nixpkgs#rustc -c cargo test -p harness anthropic` (missing Bearer header → mock not matched).
- [ ] **Step 3 — implement:** add one header line in `complete` after `.header("x-api-key", &self.api_key)`:

```rust
    .header("authorization", format!("Bearer {}", self.api_key))
```

  Anthropic ignores the extra `Authorization` header; PPQ requires it. Both are sent — one provider, both hosts. In `walk.rs`, after building the provider, honor the env override:

```rust
    let mut provider = AnthropicProvider::new(api_key, MODEL);
    if let Ok(base) = std::env::var("ANTHROPIC_BASE_URL") {
        if !base.trim().is_empty() { provider = provider.with_base_url(base); }
    }
    let provider = Arc::new(provider);
```

- [ ] **Step 4 — verify:** `cargo test -p harness` green; existing `sends_correct_request_and_parses_response` still passes (it doesn't assert header absence).
- [ ] **Step 5 — re-grep for sac's landed patch (immediately before committing, not just at task start):** sac's PR may have merged *while this task was in flight*. Re-run `grep -n "Bearer" crates/harness/src/providers/anthropic.rs` and `grep -n ANTHROPIC_BASE_URL crates/murmur-core/examples/walk.rs`. If sac's version is already present, **discard this task's duplicate** and keep sac's (avoid a redundant/conflicting commit); otherwise commit ours.
- [ ] **Step 6 — commit:** `feat(harness): PPQ compat — Authorization: Bearer + ANTHROPIC_BASE_URL in walk example`

---

### Task 2: Session template — column, migration, setter

**Files:** Modify `crates/murmur-core/src/store/migrations.rs`, `sessions.rs`, `domain.rs`.

- [ ] **Step 1 — failing tests** (add to `sessions.rs` tests):

```rust
    #[test]
    fn session_template_defaults_none_and_round_trips() {
        let s = store();
        let session = s.start_session(None).unwrap();
        assert_eq!(session.template, None);
        s.set_session_template(&session.id, "landscape").unwrap();
        assert_eq!(s.get_session(&session.id).unwrap().template.as_deref(), Some("landscape"));
    }

    #[test]
    fn set_template_rejects_non_recording() {
        let s = store();
        let session = s.start_session(None).unwrap();
        s.end_and_record_session(&session.id).unwrap();
        assert!(matches!(
            s.set_session_template(&session.id, "landscape"),
            Err(CoreError::InvalidState(_))
        ));
    }
```

- [ ] **Step 2 — run to see failure** (`template` field + method don't exist).
- [ ] **Step 3 — implement:**
  - `migrations.rs`: **append** a new entry (never edit v1): `ALTER TABLE sessions ADD COLUMN template TEXT;`
  - `domain.rs`: add `pub template: Option<String>` to `Session` (after `job_id`). Update `Session` construction in `start_session` (`template: None`).
  - `sessions.rs`: add `template` to `SESSION_COLS`, read it in `session_from_row`, and add:

```rust
    pub fn set_session_template(&self, id: &str, template: &str) -> Result<(), CoreError> {
        let session = self.get_session(id)?;
        if session.status != SessionStatus::Recording {
            return Err(CoreError::InvalidState(format!(
                "cannot set template on a {} session", session.status.as_str())));
        }
        self.conn.execute(
            "UPDATE sessions SET template = ?1, updated_at = ?2 WHERE id = ?3",
            rusqlite::params![template, self.now() as i64, id])?;
        Ok(())
    }
```

- [ ] **Step 4 — verify:** `cargo test -p murmur-core sessions`; also confirm `open_in_memory_migrates_to_latest` still equals `MIGRATIONS.len()`.
- [ ] **Step 5 — commit:** `feat(core): persist per-session template key (landscape|property|inspection)`

---

### Task 3: Document-number minting

**Files:** Modify `crates/murmur-core/src/store/migrations.rs`, `src/store/mod.rs` (add `mod documents;`); create `src/store/documents.rs`.

- [ ] **Step 1 — failing tests** (`documents.rs`):

```rust
    #[test]
    fn mint_is_monotonic_per_kind() {
        let s = store();
        assert_eq!(s.mint_document_number("estimate").unwrap(), 1);
        assert_eq!(s.mint_document_number("estimate").unwrap(), 2);
        assert_eq!(s.mint_document_number("report").unwrap(), 1); // independent sequence
        assert_eq!(s.mint_document_number("estimate").unwrap(), 3);
    }
```

- [ ] **Step 2 — run to see failure.**
- [ ] **Step 3 — implement:** append migration `CREATE TABLE document_sequences (doc_kind TEXT PRIMARY KEY, next INTEGER NOT NULL, device_id TEXT NOT NULL);`. In `documents.rs`, `mint_document_number` reads-or-inserts and increments in one `unchecked_transaction` (upsert `next`, return the pre-increment value). Local bookkeeping — matches `reflection_state` posture (no tombstone/sync fields needed for a counter; document numbers are device-local in v1).
- [ ] **Step 4 — verify:** `cargo test -p murmur-core documents`.
- [ ] **Step 5 — commit:** `feat(core): per-kind document-number minting (document_sequences)`

---

### Task 4: `BuildDocumentTool` + template-aware document build phase

**Files:** Modify `crates/murmur-core/src/pipeline/tools.rs`, `prompts.rs`, `mod.rs`.

The structured document is stored as an `Artifact` with `kind = "document"` and a **JSON `body`** — `domain.rs` already blesses "body is markdown (or JSON for structured kinds)", so no new domain type and no migration. The JSON is the display-copy-free `DocumentPayload` shape (D2), minus Swift formatting.

- [ ] **Step 1 — failing tests** (`tools.rs` tests): a `BuildDocumentTool` that, given a session and a minted number, writes one `document` artifact whose JSON body parses to the expected line set, with unheard fields as `amount_cents: null`.

```rust
    #[tokio::test]
    async fn build_document_writes_structured_json_artifact_with_gaps() {
        let (store, sid) = shared_store_with_session();
        let tool = super::BuildDocumentTool::new(store.clone(), &sid, "estimate", 47);
        let out = tool.execute(serde_json::json!({
            "total_kind": "sum",
            "total_label_key": "total",
            "lines": [
                {"title":"Bark mulch — front beds","detail":"DELIVERED + INSTALLED","qty":"3 CU YD","amount_cents":28500},
                {"title":"Haul & disposal","detail":"NOT HEARD","qty":"× 1"}   // no amount ⇒ gap
            ]
        })).await.unwrap();
        assert!(out.contains("document"));
        let arts = store.lock().unwrap().list_artifacts_for_session(&sid).unwrap();
        let doc = arts.iter().find(|a| a.kind == "document").unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc.body).unwrap();
        assert_eq!(v["doc_number"], 47);
        assert_eq!(v["lines"][1]["amount_cents"], serde_json::Value::Null);
        assert_eq!(v["lines"][1]["is_gap"], true, "unheard amount on a dollar template ⇒ gap");
    }

    #[tokio::test]
    async fn inspection_findings_have_no_amount_but_are_not_gaps() {
        let (store, sid) = shared_store_with_session();
        let tool = super::BuildDocumentTool::new(store.clone(), &sid, "inspection", 389);
        tool.execute(serde_json::json!({
            "total_kind":"static","total_label_key":"findings",
            "lines":[
                {"title":"Attic ventilation","detail":"ADEQUATE","qty":"OK","section":"§ 3.2"},          // normal finding, no $
                {"title":"Water heater TPR valve","detail":"NOT ACCESSED","section":"§ 5.3","is_gap":true} // flagged open
            ]
        })).await.unwrap();
        let arts = store.lock().unwrap().list_artifacts_for_session(&sid).unwrap();
        let v: serde_json::Value = serde_json::from_str(&arts.iter().find(|a| a.kind=="document").unwrap().body).unwrap();
        assert_eq!(v["lines"][0]["amount_cents"], serde_json::Value::Null);
        assert_eq!(v["lines"][0]["is_gap"], false, "a normal §-finding is NOT a gap despite no amount");
        assert_eq!(v["lines"][1]["is_gap"], true, "only the explicitly-flagged finding is a gap");
    }
```

- [ ] **Step 2 — run to see failure.**
- [ ] **Step 3 — implement `BuildDocumentTool`:** captures `Arc<Mutex<Store>>`, `session_id`, `doc_kind`, pre-minted `doc_number`. `input_schema` = `{ total_kind, total_label_key, static_total_cents?, lines: [{title, detail?, qty?, amount_cents?, section?, is_gap?}] }` with both `amount_cents` and `is_gap` **optional**. **`is_gap` is template-aware, NOT derived from `amount_cents` (D2a):**
  - `estimate` (dollar template): the builder defaults `is_gap = amount_cents.is_none()` when the model omits `is_gap` (an unheard price *is* the gap) — R6.
  - `report` (property, mixed): the builder honors the model's explicit `is_gap`; a normal `"OK"`/`"NOTE"`/`"—"` line has `is_gap = false` even with no amount. Never auto-flag from missing `amount_cents`.
  - `inspection` (no dollars): `amount_cents` is always absent and that is normal; `is_gap` defaults `false` and is `true` only when the model explicitly flags a finding as not-yet-assessed.

  `execute` serializes the normalized payload (stamping `doc_number`, `job_date_unix` from the session, resolving `is_gap` per the above per-`doc_kind` rule) to JSON and writes it via `add_artifact(session_id, "document", title, json)`. Add `build_document_prompt(template, memory_prompt)` in `prompts.rs` — template-parameterized (landscape=estimate line items + target price; property=deductions with an explicit "OK/normal-wear lines are not gaps" instruction; inspection=findings by section with "flag a finding as a gap only when you couldn't assess it"), with the **hard R6 rule**: *"Put an amount only on a line whose number was actually spoken. If a quantity or price was not said, omit `amount_cents` — never guess. On a priced template an unheard amount is a gap; on a report/inspection, only mark a line a gap when it was genuinely left open — a normal 'OK' or a §-section finding with no dollar figure is not a gap."*
- [ ] **Step 4 — wire into `SessionProcessor` (phase B):** after the extraction pass, mint the number once (only if no `document` artifact already exists for this session — idempotent re-process), register `BuildDocumentTool` gated to the session's `template`, and make a **forced** `build_document` call (`tool_choice = Some("build_document")`, mirroring `summarize`). Read `session.template` (default `"report"` if `None`). Keep the existing summary call. Budget: this is the documented < 8 s phase B. Add a pipeline test `processes_and_builds_a_document_artifact` asserting a `document` artifact exists post-`process` and the usage row purpose stays `"processing"` (one folded row, per `LlmUsageRow` doc).
- [ ] **Step 5 — commit:** `feat(core): build_document tool + processing phase B → structured document artifact (gaps, R6)`

---

## Part B — The `crates/ffi` bridge (hermetic)

### Task 5: Crate scaffolding + FFI projections + conversions

**Files:** create `crates/ffi/{Cargo.toml,src/lib.rs,src/events.rs,src/document.rs,src/convert.rs}`; modify root `Cargo.toml` (`members += "crates/ffi"`).

> **Proc-macro mode only — no `build.rs`, no UDL, no `uniffi` build-dependency.** UniFFI scaffolding is generated entirely by the `uniffi::setup_scaffolding!()` macro + the `#[derive(uniffi::…)]` / `#[uniffi::export]` attributes at compile time. The UDL-mode `build.rs` calling `uniffi::generate_scaffolding(...)` and the `[build-dependencies] uniffi = { features = ["build"] }` were an incorrect graft of two mutually exclusive modes and would fail to build. Bindings for Swift are produced separately by the `uniffi-bindgen` CLI in Task 9 (from the built library) — that is *not* a build-dependency of this crate.

- [ ] **Step 1 — failing test** (`crates/ffi/src/convert.rs` unit test): a `CapturedItem` projects to a `BoardItem` with kind/text/id preserved; a `document`-kind `Artifact` JSON parses to a `DocumentPayload` with a gap line surfacing `amount_cents: None, is_gap: true`.
- [ ] **Step 2 — run to see failure.**
- [ ] **Step 3 — implement:**
  - `Cargo.toml`: `crate-type = ["cdylib", "staticlib", "lib"]`; deps `uniffi` (proc-macro mode — **no `features = ["build"]`, no `[build-dependencies]`**), `murmur-core` (path), `harness` (path), `tokio`, `serde_json`.
  - `lib.rs`: `uniffi::setup_scaffolding!();` (proc-macro mode — no UDL file, no `build.rs`), `mod`s.
  - `events.rs`: `#[derive(uniffi::Enum)] pub enum WalkEvent { BoardUpdated { items: Vec<BoardItem> } }`; `#[derive(uniffi::Record)] pub struct BoardItem { id, kind, text, right, photo_count }`; the foreign-implemented listener trait — see Task 7 for the exact `#[uniffi::export(with_foreign)]` + `Arc<dyn …>` shape (define the trait in `events.rs`, export it there).
  - `document.rs`: `#[derive(uniffi::Record)]` `DocumentPayload` + `DocLine` (D2 shape; `amount_cents: Option<i64>`, `doc_number: u64`).
  - `convert.rs`: `fn board_item(&CapturedItem) -> BoardItem`; `fn document_payload(&Artifact) -> Result<DocumentPayload, ...>` (parse JSON body). Keep every mapping in one place — the FFI boundary is exactly this file plus the records.
- [ ] **Step 4 — verify:** `cargo test -p ffi convert`; `cargo build -p ffi`.
- [ ] **Step 5 — commit:** `feat(ffi): crate scaffolding, board/document FFI records, domain projections`

---

### Task 6: `MurmurEngine` + `EngineConfig` + provider routing (key hygiene)

**Files:** create `crates/ffi/src/engine.rs`.

- [ ] **Step 0 — compile-spike (timebox ~20 min, do this before writing the full surface):** two upstream doc searches disagreed on confidence for the exact 0.28 proc-macro shapes, so *prove them* with a throwaway minimal example before committing to the API surface. Verify all of the following compile under uniffi 0.28 proc-macro mode: (i) an exported method with a `self: Arc<Self>` receiver; (ii) an `#[uniffi::export(async_runtime = "tokio")] async fn` that returns `Result<T, MyError>` where `MyError` is a `#[derive(uniffi::Error)]` enum; (iii) `#[uniffi::export(with_foreign)]` trait taken as `Arc<dyn Trait>`. **If any fails, fall back** to the documented alternative and record it in the task: `&self` receivers with the constructor returning `Arc<Self>`; async fns returning the payload directly (errors surfaced as a `queued`/status field rather than a thrown error — which the offline path in D9 already does); and adjust Task 7's listener shape accordingly. Delete the spike; write the surface to whichever shapes compiled.
- [ ] **Step 1 — failing test:** an `EngineConfig` builds a `MurmurEngine` over an in-memory store; `Debug` on `EngineConfig` (or the engine) **does not** contain the api key substring (redaction). Provider construction is exercised indirectly in Task 8's e2e (real providers are network; here assert wiring/redaction only, using a test-only constructor that injects a `MockProvider` per purpose). This task's `cargo test -p ffi engine` **must compile standalone** — it does not reference `WalkSession` (that type and `begin_walk` land in Task 7); assert only construction + redaction + a `#[cfg(test)] with_providers` injection point.

```rust
    #[test]
    fn config_debug_redacts_the_api_key() {
        let cfg = EngineConfig {
            db_path: ":memory:".into(), device_id: "dev".into(),
            api_key: "sk-super-secret".into(), base_url: None,
            model_live: "claude-haiku-4-5".into(),
            model_processing: "claude-sonnet-4-5".into(),
            model_reflection: "claude-haiku-4-5".into(),
        };
        assert!(!format!("{cfg:?}").contains("sk-super-secret"), "api key must never be printable");
    }
```

- [ ] **Step 2 — run to see failure.**
- [ ] **Step 3 — implement:**
  - `#[derive(uniffi::Record)] EngineConfig { db_path, device_id, api_key, base_url: Option<String>, model_live, model_processing, model_reflection }` with a **hand-written `Debug`** that redacts `api_key` (never derive `Debug`). Key crosses FFI as an opaque `String` from Keychain; nothing logs it.
  - `#[derive(uniffi::Object)] MurmurEngine { store: Arc<Mutex<Store>>, memory: Arc<Mutex<Memory>>, memory_store: Arc<dyn MemoryStore>, providers: Providers, ... }` where `Providers { live: Arc<dyn LlmProvider>, processing: Arc<dyn LlmProvider>, reflection: Arc<dyn LlmProvider> }`, each an `AnthropicProvider` (Task 1's Bearer + base_url) built per purpose model, `Arc`-deduped when models match.
  - `#[uniffi::export] impl MurmurEngine { #[uniffi::constructor] fn new(config: EngineConfig) -> Arc<Self>; }` plus a `#[cfg(test)]` constructor `with_providers(...)` that injects mocks. **`begin_walk` is added in Task 7** (it constructs a `WalkSession`, which Task 7 defines) — keeping it out of Task 6 lets Task 6 compile and test standalone (fix for the cross-task verify dependency).
- [ ] **Step 4 — verify:** `cargo test -p ffi engine` (compiles and passes without any `WalkSession` reference).
- [ ] **Step 5 — commit:** `feat(ffi): MurmurEngine, EngineConfig with redacted key, per-purpose provider routing`

---

### Task 7: `WalkSession` — append, the LiveExtractor actor, batched board events

**Files:** create `crates/ffi/src/session.rs`.

- [ ] **Step 1 — failing test** (uses the mock-injected engine): `begin_walk` → set a recording `WalkEventListener` that pushes events to a shared `Vec` → `append_transcript` with enough text → after the tick resolves, the listener received exactly one `BoardUpdated` whose `items` contains the extracted item. Assert the `Store` lock is **not** held across the extractor call (structural: `append_transcript` returns before the async tick completes; the tick uses scoped guards — covered by the fact that a second `append_transcript` doesn't deadlock).

```rust
    #[tokio::test]
    async fn append_ticks_live_extractor_and_emits_one_board_snapshot_per_pass() { /* ... */ }
```

- [ ] **Step 2 — run to see failure.**
- [ ] **Step 3 — implement:**
  - **Listener trait (in `events.rs`), foreign-implemented — NOT `callback_interface`:**

    ```rust
    #[uniffi::export(with_foreign)]
    pub trait WalkEventListener: Send + Sync {
        fn on_event(&self, event: WalkEvent);
    }
    ```

    Use `#[uniffi::export(with_foreign)]` and pass/store the listener as **`Arc<dyn WalkEventListener>`**, never `Box<dyn …>`. The `#[uniffi::export(callback_interface)]` + `Box<dyn WalkEventListener>`-as-parameter shape is soft-deprecated upstream and the boxed-trait-object parameter fails to compile under 0.28 (mozilla/uniffi-rs#2797). `with_foreign` accepts both a foreign (Swift) impl and a Rust impl and hands you an `Arc`.
  - **Add `begin_walk` to `MurmurEngine`** (deferred from Task 6 so this is where `WalkSession` first appears): `#[uniffi::export] impl MurmurEngine { fn begin_walk(self: Arc<Self>, job_id: Option<String>, template: String) -> Arc<WalkSession> }` → `store.start_session(job_id)` + `set_session_template` + construct the `WalkSession` with a fresh `LiveExtractor`.
  - `#[derive(uniffi::Object)] WalkSession` holds `session_id`, `Arc<Mutex<Store>>`, `Arc<tokio::sync::Mutex<LiveExtractor>>`, `Arc<ArcSwapOption<dyn WalkEventListener>>` (listener slot; per-session), and the engine's `Arc`s.
  - `#[uniffi::export] fn set_event_listener(self: Arc<Self>, listener: Arc<dyn WalkEventListener>)` — stores it (fresh per session; D3/HANDOFF per-session streams).
  - `#[uniffi::export] fn append_transcript(self: Arc<Self>, text: String)` — write `store.append_transcript` under a scoped lock, then spawn/notify the tick: acquire the **extractor** mutex (not the store), call `maybe_extract().await`; on `Extracted { .. }`, re-query `list_items_for_session` (scoped store lock, released), project to `[BoardItem]`, and invoke `listener.on_event(WalkEvent::BoardUpdated { items })` **once**. The `tokio::sync::Mutex<LiveExtractor>` serializes ticks *and* excludes them from `finish`'s `process()` (D3b) — a tick that can't get the mutex because `finish` holds it simply doesn't run (correct: the board is about to be replaced). Never hold the store lock across `maybe_extract` (D7).
  - `#[uniffi::export(async_runtime = "tokio")] async fn finish(self: Arc<Self>) -> DocumentPayload` — Task 8.
  - **Add a test** `tick_cannot_interleave_with_finish`: start a `finish()` whose processing provider blocks on a barrier, fire an `append_transcript` mid-`finish`, release the barrier — assert no `BoardUpdated` empty-board event was delivered and the final snapshot is authoritative.
- [ ] **Step 4 — verify:** `cargo test -p ffi session`.
- [ ] **Step 5 — commit:** `feat(ffi): WalkSession append + LiveExtractor actor + batched board snapshots`

---

### Task 8: `finish()` — two-phase process, swap snapshot, offline degradation

**Files:** modify `crates/ffi/src/session.rs`; create `crates/ffi/tests/bridge_e2e.rs`.

- [ ] **Step 1 — failing e2e test** (`bridge_e2e.rs`, MockProvider via the test constructor): mirror `live_extraction_e2e` at the bridge level —
  1. `begin_walk("landscape")`, register a listener collecting events.
  2. `append_transcript(...)` (cheap mock adds a live item) → one `BoardUpdated` with the live item.
  3. `finish()` (strong mock: extraction re-creates authoritative items + a forced `build_document`) resolves to a `DocumentPayload` with the expected lines and a gap line (`amount_cents == None`); a **terminal** `BoardUpdated` carrying the **authoritative** board was delivered (swap); and `doc_number == 1`.
  4. A second test: `finish()` with a provider that errors → returns a `queued: true` partial payload built from the live items (all gaps), session left `Failed`/`AwaitingProcessing`, transcript intact (offline contract, D9).

- [ ] **Step 2 — run to see failure.**
- [ ] **Step 3 — implement `finish`:**

```
0. let _tick_guard = extractor_mutex.lock().await;     // D3b: exclude live ticks for the whole finish
1. store.end_and_record_session(session_id)            // instant, local
2. match SessionProcessor::new(processing_provider, ...).process(session_id).await {
     Ok(_) => {
        // swap: re-query authoritative board, emit terminal BoardUpdated
        emit_board_snapshot();
        let art = store.latest_document_artifact(session_id)?;   // helper on Store or via list_artifacts
        convert::document_payload(&art)                          // display-copy-free
     }
     Err(_) => {
        // offline / LLM down: capture is safe; return a partial doc from live items
        let items = store.list_items_for_session(session_id)?;
        DocumentPayload::partial_from_items(&items, queued=true) // all gaps, R6
     }
   }
```

  Document the phase-A/phase-B budget split in a comment (< 8 s ceiling, no spinner). `finish` holds the **extractor** mutex across the whole call (D3b — no tick can interleave) but holds **no Store lock** across the `process().await`.
- [ ] **Step 4 — verify:** `cargo test -p ffi --test bridge_e2e`; then `cargo test --workspace` and `cargo clippy --workspace --all-targets` → zero warnings (fix mechanically; STOP and report if a fix changes behavior).
- [ ] **Step 5 — commit:** `feat(ffi): finish() two-phase process, swap snapshot, offline partial document`

---

## Part C — Swift adapter, packaging, and the swap (sim-testable)

### Task 9: Generate bindings + package + wire into xcodegen

**Files:** create `apps/ios/Packages/MurmurCoreFFI/` (generated); modify `apps/ios/project.yml`.

- [ ] **Step 1 — build the static lib + generate Swift:** from repo root, build `crates/ffi` for the simulator target(s) and run `uniffi-bindgen generate --library <lib> --language swift`. Produce a `MurmurCoreFFI` Swift package wrapping the generated `.swift` + a `.xcframework` (device + sim slices; for the milestone, sim-only is acceptable and faster — note the omission with `log`/README). Document the exact commands in `apps/ios/Packages/MurmurCoreFFI/README.md` so the build is reproducible (this replaces a Makefile target the milestone doesn't yet need).
- [ ] **Step 2 — reference it:** add the local Swift package + framework to `project.yml` (`packages:` + target `dependencies:`), keeping `CODE_SIGNING_ALLOWED: NO` for the sim.
- [ ] **Step 3 — verify:** `cd apps/ios && xcodegen generate && xcodebuild -project SitewalkGallery.xcodeproj -scheme SitewalkGallery -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build` compiles with the package linked (no adapter yet — just linkage).
- [ ] **Step 4 — commit:** `build(ios): MurmurCoreFFI generated Swift package + xcframework wired into xcodegen`

---

### Task 10: `WalkEvent.boardUpdated` + `AppModel` event-loop change (with sac)

**Files:** modify `apps/ios/Sources/Engine/WalkEngine.swift`, `apps/ios/Sources/App/AppModel.swift`. **Coordinate via sac's PR lane** (sac owns `AppModel`).

- [ ] **Step 1 — change the enum:** `enum WalkEvent { case boardUpdated([CapturedFixture]) }` (D3). Keep `DocumentModel` unchanged.
- [ ] **Step 2 — change the loop:** in `AppModel.startWalk()`:

```swift
    case .boardUpdated(let items):
        withAnimation(.easeOut(duration: 0.25)) { self.items = items }
        // track the newest by id, NOT array position (see Step 2a)
        self.lastCapturedID = items.last?.id
```

- [ ] **Step 2a — fix the `addPhoto()` invariant (REQUIRED — the array-replace breaks it):** today `addPhoto()` does `items.indices.last` then mutates `items[lastIndex]`, relying on "array tail == most-recently-captured." Under whole-board `boardUpdated` replace that invariant is **not load-bearing**: re-extraction mints new ids mid-swap and store ordering (UUIDv7 `id ASC`) is *insertion* order, not *mention* order, so the tail may not be the line the operator is currently speaking about. Replace position-based access with an explicit id:

```swift
    // AppModel: var lastCapturedID: UUID?  (set in the event loop, Step 2)
    func addPhoto() {
        guard let id = lastCapturedID,
              let idx = items.firstIndex(where: { $0.id == id }) else { return }
        items[idx].photos += 1
    }
```

  Note in the plan: which item a photo pins to is ultimately a core concern (HANDOFF open Q3, photo sync schema — Deferred 6); until that lands, "most-recently-captured id" is the honest interim rule, and it must be tracked explicitly, never inferred from array order.
- [ ] **Step 3 — update `DemoWalkEngine`:** emit `boardUpdated` with the cumulative matched items instead of per-item `itemCaptured` (so the demo path exercises the same event shape). Trivial: accumulate into an array and yield the snapshot. Because `BoardItem.id` must be **stable across snapshots** for `ForEach`/`lastCapturedID` to work, the demo engine keeps one `CapturedFixture` array and yields it (never rebuilds fixtures per event).
- [ ] **Step 4 — sim check:** `demo=1 autoflow=1` still plays the scripted walk and the board fills (screenshot `make sim-screenshot`-style, or XcodeBuildMCP `screenshot`). Verify no regression in the demo flow.
- [ ] **Step 5 — commit:** `refactor(ios): WalkEvent.boardUpdated snapshots (batched per pass); AppModel assigns board`

---

### Task 11: `MurmurEngine: WalkEngine` adapter + formatting layer

**Files:** create `apps/ios/Sources/Engine/MurmurEngine.swift`; modify `apps/ios/Sources/App/GalleryApp.swift`.

- [ ] **Step 1 — adapter:**

```swift
@MainActor
final class MurmurEngine: WalkEngine {
    private let engine: FFIMurmurEngine   // from MurmurCoreFFI
    private var session: FFIWalkSession?
    private var continuation: AsyncStream<WalkEvent>.Continuation?

    init(config: FFIEngineConfig) { self.engine = FFIMurmurEngine(config: config) }

    func begin(trade: TradeFixture) -> AsyncStream<WalkEvent> {
        continuation?.finish()
        let (stream, cont) = AsyncStream<WalkEvent>.makeStream()
        continuation = cont
        let s = engine.beginWalk(jobId: nil, template: trade.key)   // template key = trade.key (D4)
        s.setEventListener(listener: BoardListener { [weak self] items in
            // Rust callback → hop to main → yield (events on main, D3)
            Task { @MainActor in self?.continuation?.yield(.boardUpdated(items.map(Self.board))) }
        })
        session = s
        return stream
    }

    func append(transcript: String) { session?.appendTranscript(text: transcript) }

    func finish() async -> DocumentModel {
        continuation?.finish(); continuation = nil
        guard let s = session else { return DocumentModel.empty }
        let payload = await s.finish()
        return Self.document(payload)   // formatting layer
    }
}
```

- [ ] **Step 2 — formatting layer** (`static func document(_:) -> DocumentModel`, `static func board(_:) -> CapturedFixture`). **Every `DocumentModel`/`DocRowFixture` field gets a named source — no silent gaps.** The payload carries `doc_kind, doc_number, job_date_unix, total_kind, total_label_key, static_total_cents?, lines[], queued`; the rest come from a Swift-side per-template table or a documented default:

  | Target field | Source |
  |---|---|
  | `DocRowFixture.title` | `DocLine.title` |
  | `DocRowFixture.sub` | `DocLine.detail` when present; when `is_gap`, the template's "not heard" copy (`"NOT HEARD — TAP OR SAY IT"`) |
  | `DocRowFixture.qty` | `DocLine.qty` (falls back to `""`) |
  | `DocRowFixture.amount` | `amount_cents` → `"$285"`; `nil` → `"——"` |
  | `DocRowFixture.isGap` | `DocLine.is_gap` (template-aware from core, D2a — **not** derived from `amount_cents`) |
  | `DocRowFixture.subWarn` | `= DocLine.is_gap` (gap rows get the warn styling) |
  | `DocRowFixture.hint` | **`nil`** for the milestone — the `↺ LAST 3 / SCHEDULE` price-book hint is **Deferred 4** (price-book autofill). Named default, revisit at that plan. |
  | `DocRowFixture.isEdit` | **`false`** for the milestone — `isEdit` is the "pre-filled from your history, confirm me" affordance, which only has meaning once price-book hints exist (Deferred 4). Named default. |
  | `DocumentModel.totalKey` | `total_label_key` → display copy via the Swift template table (`"total"→"TOTAL"`, `"deposit_deduction"→"DEPOSIT DEDUCTION"`, `"findings"→"FINDINGS"`) |
  | `DocumentModel.staticTotal` | `static_total_cents` → formatted (`total_kind == "static"`); for `"sum"` templates `DocumentModel.totalValue` already sums the rows, so `staticTotal` is the fallback string |
  | `DocumentModel.note` | Swift-side per-`doc_kind` constant (footer guidance copy, e.g. the landscape gap note); **when `queued == true`, overridden** with the "saved offline — will finish when you reconnect" partial-document note |
  | `DocumentModel.send` | Swift-side per-`doc_kind` constant (`"SEND ESTIMATE"`/`"SEND REPORT"`) — **live-used at `ReviewView.swift:65`**, so it must never be empty; the table is the source of truth |
  | `CapturedFixture.tag/right` | `board(_:)` maps `BoardItem.kind`→`TagFixture` + `BoardItem.right` |

  Cents → `"$\(formatted)"` via `NumberFormatter(.decimal)`; `doc_number` → template prefix (`EST-%04d` etc., keyed off `doc_kind`); `job_date_unix` → `"JUL 01 2026"` via `DateFormatter`. Letterhead/board chrome stays in `TradeFixture` (D2) — the adapter only fills `rows/totalKey/staticTotal/note/send` on `DocumentModel`.
- [ ] **Step 3 — engine selection** (`GalleryApp`): build `FFIEngineConfig` from the API key (Info.plist `PPQ_API_KEY`-style / Keychain later) + `ANTHROPIC_BASE_URL`. If `demo=1` **or** no key → `DemoWalkEngine()`; else `MurmurEngine(config:)`. Pass into `AppModel(engine:)`. **Delete nothing** (D10).
- [ ] **Step 4 — sim check (the milestone gate):** launch with a real key + `ANTHROPIC_BASE_URL` (PPQ), run one scripted walk end-to-end (`autoflow=1` with the real engine): board ticks from live passes, DONE builds a real document in review with at least one honest gap, PDF renders, send marks the job. Record a short `record_sim_video`/`gif_creator` of the flow as the milestone artifact. If a real key isn't available in the sandbox, gate this step behind `demo=1` and note the real-key run as an owed manual verification.
- [ ] **Step 5 — commit:** `feat(ios): MurmurEngine bridge adapter + Swift formatting layer; select real engine at init`

---

### Task 12: Docs + final whole-artifact review

- [ ] **Step 1 — README:** update the plan-series line: `… 05 live extraction, 05b eval suite, 06-spike STT, 07 FFI bridge.` Note `crates/ffi` in the architecture line.
- [ ] **Step 2 — full verification:** `cargo test --workspace` + `cargo clippy --workspace --all-targets` (zero warnings) + the iOS build + the sim flow from Task 11.4.
- [ ] **Step 3 — whole-artifact final review** (CANON: "caught a real cross-module issue in six of six plans — never skip it"): read the diff across `harness` → `murmur-core` → `crates/ffi` → `apps/ios` as one artifact. Specifically re-check: the Store lock is never held across `maybe_extract` or `process().await`; the key never reaches a log line; gaps never carry a guessed amount end-to-end; per-session stream lifetime (a second `begin` cancels the first cleanly).
- [ ] **Step 4 — commit:** `docs: plan 07 done — FFI bridge; core behind the app`

---

## Deferred (named, for later plans)

1. **STT stage 2 (`append_audio`)** — additive FFI method feeding finalized segments into `append_transcript`; lands after `crates/stt` (Plan 06 verdict). The surface (D8) is designed so this is a new method, not a breaking change.
2. **Generative layout-ops protocol (spec §5)** — DE-SCOPED for the milestone (concrete SwiftUI views + `boardUpdated` snapshots instead). Returns as its own plan; per-item arrival diffs need stable ids across the swap, which the layout-op vocabulary would define.
3. **Voice gap-fill / multi-turn correction during review** — HANDOFF's "future gap fill via voice." The bridge exposes `finish()`'s document; a correction pass (`append`-then-rebuild against the artifact) is a follow-up.
4. **Price-book autofill (CORE.md §6)** — "you charged $95/yd last time" as a *hint* (never an auto-filled amount — R6). Rides reflection/memory output into `build_document`'s context; deferred until the price history exists.
5. **`process_pending` reconnect trigger** — the app-shell timer/reachability hook that drains queued (offline-`finish`ed) sessions. Core already exposes `process_pending`; the bridge needs a `retry_pending()` FFI method + an app-side trigger.
6. **Photo attachment sync schema** — HANDOFF open Q3; ROADMAP has it riding a migration after `source`. UI pins photos locally today; core learns about them later. Not on the milestone path.
7. **Device xcframework slices / signed distribution** — Task 9 may ship sim-only for speed; a Makefile target + device slices for TestFlight is a packaging follow-up.
8. **Plan 06a `source` column + atomic-at-finish swap** — a **HARD dependency** (plan header; execution order 06a → 06 → 07), not owned here. It closes the "cleared but not re-extracted" empty window at the storage layer. Without it, a live tick re-querying mid-swap would broadcast an empty board (D3); the in-process extractor-mutex exclusion (D3b) is the second, independent guard, but 06a is the real fix and must land first.

## Self-Review Notes

- **Riskiest FFI assumption (named):** that UniFFI's async export (`async_runtime = "tokio"`) + a callback interface cleanly bridge Rust's tokio async into Swift's `async/await` and an `AsyncStream`, **while the `Store` behind a `std::sync::Mutex` is locked only in short scoped guards inside async methods** — i.e. that no scoped `std` lock stalls the foreign async executor, and that the Rust→Swift callback (invoked off-main) hopping to `@MainActor` before yielding preserves ordering and per-session stream lifetime. Mitigations designed in: the extractor is serialized by a `tokio::sync::Mutex` (not std), every `Store` guard is dropped before an `await` (verified in `live.rs` and required in `session.rs`/`finish`), and the adapter yields on `MainActor`. If UniFFI async proves fragile, the fallback is a synchronous `finish()` exported as a blocking call the Swift side wraps in `Task.detached` — noted, not chosen.
- **Formatting-layer decision:** **bridge-side Swift.** Core emits display-copy-free `DocumentPayload` (cents, unix seconds, integer doc number, label *keys*); Swift formats currency/date/prefix and owns letterhead/board chrome. Justified by locale/currency being platform concerns, R7 inspectability of raw core rows, and Commitment 2 (one schema feeds preview + PDF + future CSV without re-crossing FFI). Core domain types (`CapturedItem`, `Artifact`) are untouched by presentation.
- **Event-batching design:** one `WalkEvent.boardUpdated([BoardItem])` per live pass (batched by construction), delivered on main; the finish swap is the terminal snapshot. Chosen over per-item diffs because Plan 05 recreates ids at the swap, so diffs would remap ids the core doesn't expose or animate a teardown anyway. The tick/finish race (a mid-swap tick broadcasting an empty board) is closed by **two** independent guards: Plan 06a's atomic-at-finish swap (hard dependency) and the extractor-mutex held across `finish`'s `process().await` (D3b). Costs an `AppModel` change (append→assign **plus** an explicit `lastCapturedID` so `addPhoto()` no longer relies on array position — Task 10), coordinated with sac.
- **Judgment calls for reviewers:** (a) new `crates/ffi` crate over `uniffi`-on-`murmur-core` — keeps core binding-free per CANON, costs one workspace member; (b) structured document as a `document`-kind JSON `Artifact` over a new domain table — `domain.rs` already blesses JSON bodies, no migration; (c) document generation folded into `SessionProcessor` phase B (forced `build_document`) over a separate builder — reuses the extraction transcript + budget, keeps `finish()` two calls; (d) template persisted on the session over pass-through — reprocessing stays consistent, sync-ready; (e) `DemoWalkEngine` kept behind `demo=1`/no-key over deleted — preserves the gallery/CI demo path and the "swap at init" seam; (f) offline `finish()` returns a partial all-gaps document with `queued:true` over throwing — contract 3, capture never lost.
- **Constraints surfaced for follow-ups:** STT stage 2 is a new method not a rewrite (D8); `process_pending` needs an app-side reconnect trigger (Deferred 5); price-book hints must never become auto-filled amounts (R6, Deferred 4); Plan 06a is a HARD dependency (execution order 06a → 06 → 07) — its absence would let a mid-swap tick flash an empty board; the extractor-mutex exclusion (D3b) is the second guard (Deferred 8).
- **Spec coverage:** §1 offline/capture-never-lost ✓ (D9, Task 8.4); §2 live board ✓ (D3, Task 7); §3 templates as data ✓ (D4, Task 2/4); §4 honest gaps ✓ (R6 enforced in `build_document` prompt+schema and the Swift `——`/`isGap` mapping); §5 the paper / one-schema-many-renderings ✓ (D2); §6 < 8 s two-phase ✓ (D6, Task 8); R7 ✓ (raw core rows, redacted key); R9 ✓ (`live_extraction`/`processing` usage rows unchanged). The DE-SCOPED layout protocol is explicitly named, not silently dropped.
