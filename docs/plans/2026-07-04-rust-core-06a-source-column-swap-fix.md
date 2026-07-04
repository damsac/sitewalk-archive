# Murmur Rust Core — Plan 06a: Item `source` Column + Swap-Contract Fix

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the swap gap pinned by Plan 05's carried characterization test: today `process()` tombstones the live board at *entry* (`clear_session_outputs`, Phase 0), so a transient LLM failure leaves the user with **zero items** — the live board is gone and the authoritative set never lands. The fix gives `items` a `source` column (`live | authoritative | manual`) and moves the board replacement from a destructive **clear-at-entry** to an atomic **swap-at-finish**: extraction runs against the still-visible live board, and only in the *same transaction* that marks the session Processed do we tombstone the old board. A failed process leaves the live board fully intact.

**Architecture:** One migration (v2: `items.source`, backfill `authoritative`), one new domain enum (`ItemSource`), storage plumbing on the existing single-writer `Store`, a rewritten `finish_session_processed` that performs the swap, and a small id-collection seam in `SessionProcessor` so the finish transaction knows which items *this run* created. No new crate, no new public type beyond `ItemSource`.

**The swap rule (dam, locked 2026-07-04).** In the finish transaction, tombstone every item for the session that is:
- (a) `source = live`, **or**
- (b) `source = authoritative` created by a *prior* run (retry case),

i.e. everything **not** created by this processing run and **not** `source = manual`. Manual items are **never** tombstoned by the swap. On failure, nothing is tombstoned — the live board survives.

**Bounding duplication across repeated failed retries (entry clear, defense in depth).** The finish swap alone leaves a gap: nothing sweeps a *failed* attempt's partial `authoritative` items until the **next success**. The extraction prompt has no dedup context, so N consecutive failures can pile up N attempts' worth of duplicate authoritative todos — visible in `list_open_todos` the whole time. Fix: at `process()` entry, do a **scoped, source-filtered clear** — tombstone the session's `source='authoritative'` items **and all artifacts** (artifacts are only ever written by processing, so every one is authoritative-equivalent), and *only* those. Never `live`, never `manual`. This bounds duplication to a single in-flight attempt, preserves the live board as the safety net (the whole point of 06a), and is crash-safe (an idempotent pure delete). Keep **both** mechanisms: the entry clear makes the "not-this-run authoritative" set empty in practice, and the finish swap still handles the actual live→authoritative replacement atomically at success and defends against any authoritative straggler the entry clear missed (e.g. a crash between entry and finish). On the first attempt the entry clear is a no-op for items (only `live` items exist pre-extraction).

**Design problem 1 — "created by this run" (the crux).** Both this-run and prior-run authoritative items carry `source = authoritative`; the discriminator can't be the source value, and `created_at` (epoch **seconds**) is too coarse to trust at a run boundary. **Chosen: an explicit created-id set.** The processor holds an `Arc<Mutex<Vec<String>>>` sink; its `AddItemTool` pushes each inserted item's id as the agent's tool calls land (the tool is the executor seam that already sees the write — see tools.rs). At finish, the swap is `... AND source IN ('live','authoritative') AND id NOT IN (<run ids>)`. Rejected alternatives: (i) a `created_at`/timestamp cutoff — second-granularity clock makes the boundary ambiguous and the injected test clock makes it worse; (ii) a per-run generation-stamp column — a whole extra column and migration to encode what an in-memory id list already captures exactly. **Crash-safety:** the sink is in-memory and the authoritative inserts autocommit immediately (each is its own statement), but the finish tx is the only place status flips to Processed. A crash mid-run leaves those authoritative rows on the board with `source = authoritative` and the session still `AwaitingProcessing`/`Failed`; the **next** successful run's swap sweeps them by rule (b) because they are not in *that* run's id set. No stamp to corrupt, no partial-swap state — the swap is a pure function of (source, this-run ids).

**Design problem 2 — retry idempotency.** Directly implied by rule (b) and proven by two tests: the existing `retry_after_failure_does_not_duplicate_outputs` (mechanism shifts from clear-at-entry to swap-at-finish; result — one item — is unchanged) and a new store-level `swap_tombstones_prior_run_authoritative_but_keeps_this_run`.

**Design problem 3 — `clear_session_outputs`'s fate: deleted and replaced.** Its semantics — tombstone *all* outputs unconditionally, **including manual** — are exactly what neither the entry clear nor the swap may do; leaving a method with those semantics invites misuse. Delete it and its two tests. In its place add `clear_authoritative_outputs(session_id)`: tombstone `source='authoritative'` items + all artifacts, never `live`, never `manual`. This is the Phase-0 entry clear (see the swap-rule section) — narrower and safe. `delete_session`'s cascade is unchanged and *correct*: a full session delete tombstones everything including manual (no source filter), as required.

**Design problem 4 — reads during processing.** The Phase-0 clear now touches only `source='authoritative'` (+ artifacts), never the live board — so the live board stays visible for the entire processing window and swaps once, atomically, at finish. Authoritative items autocommit incrementally during the run, so a reader mid-window sees live + partial-authoritative overlapping briefly (acceptable — strictly better than today's blank board; documented, not hidden). Proven by the flipped characterization test (failure → live board intact) plus an e2e asserting the board is non-empty across a failed process and holds only authoritative items after success.

**Design problem 5 — the evals pin.** `crates/evals/tests/carried_scenarios.rs::failed_processing_after_live_capture_leaves_empty_board` pins TODAY's broken behavior. Task 4 rewrites it to the NEW contract (`..._preserves_live_board`, asserting the live item survives a failed process) — turning the characterization into the regression test.

**Design problem 6 — reader source-awareness (audited, decision recorded).** The only source-aware logic in the codebase is the swap. Readers stay source-agnostic:
- `list_open_todos` (morning glance): **shows all sources, including live.** Rationale: live items are real, inspectable rows (R7), and post-fix they are the *safety net* for a failed/awaiting session — hiding `source=live` would make a failed-processing session's captured todos vanish from the glance, the opposite of this plan's intent. A currently-recording session's live todos also appearing is acceptable (you see what you just captured). A new test pins this.
- `search_sessions`: matches transcript/summary text, not items — source-agnostic, no change.
- `activity_for_reflection`: reads session summary/transcript, not items — source-agnostic, no change.

**Spec:** vision spec Rev 2 §2 (end-of-session pass is truth; live board degrades gracefully), §6 (<8s window; a dead battery / transient failure loses nothing), R7 (inspectable outcomes — a failed process still shows the captured board), R9 (cost logged on success and failure — unchanged). Plan 05 Deferred + the carried swap-gap characterization.

**Tech Stack:** existing deps only. `rusqlite::params_from_iter` (already a dep) builds the dynamic `NOT IN` list. All tests hermetic — `MockProvider`, no network.

---

## File Structure

```
crates/murmur-core/
  src/
    domain.rs                 # MODIFY: ItemSource enum; CapturedItem.source field
    store/
      migrations.rs           # MODIFY: append v2 — items.source column
      items.rs                # MODIFY: ITEM_COLS, item_from_row, insert_item(source),
                              #         add_item (=Manual), add_item_with_source,
                              #         add_item_if_status(+source)
      sessions.rs             # MODIFY: finish_session_processed does the swap;
                              #         DELETE clear_session_outputs (+ its 2 tests);
                              #         ADD clear_authoritative_outputs (entry clear)
    pipeline/
      tools.rs                # MODIFY: AddItemTool carries source + optional id sink;
                              #         constructors manual/authoritative/live
      mod.rs                  # MODIFY: Phase-0 clear -> scoped authoritative clear;
                              #         id sink; swap at finish
      live.rs                 # MODIFY: module docs; RacingProvider drops clear call;
                              #         AddItemTool::gated -> ::live
  tests/
    source_swap_e2e.rs        # NEW: live board survives failure; swaps clean on success
crates/evals/
  tests/carried_scenarios.rs  # MODIFY: flip 4a pin to the new contract (regression)
README.md                     # MODIFY: plan-series line
```

Run cargo via the dev shell or `nix shell nixpkgs#cargo nixpkgs#rustc -c cargo <cmd>` from the repo root.

---

### Task 1: Item `source` — domain enum, migration v2, storage plumbing

Adding `CapturedItem.source` and the column together is one atomic compile unit — the field can't exist without the storage read/write that populates it.

**Files:** Modify `src/domain.rs`, `src/store/migrations.rs`, `src/store/items.rs`

- [ ] **Step 1: Write the failing tests**

In `src/domain.rs` tests (add a module if none):
```rust
    #[test]
    fn item_source_round_trips_through_str() {
        for s in [ItemSource::Live, ItemSource::Authoritative, ItemSource::Manual] {
            assert_eq!(ItemSource::parse(s.as_str()).unwrap(), s);
        }
        assert!(ItemSource::parse("bogus").is_err());
    }
```

In `src/store/items.rs` tests:
```rust
    #[test]
    fn add_item_defaults_to_manual_source() {
        use crate::domain::ItemSource;
        let (s, sid) = store_with_session();
        let item = s.add_item(&sid, "todo", "order lumber").unwrap();
        assert_eq!(item.source, ItemSource::Manual);
        // round-trips through the DB read
        assert_eq!(s.list_items_for_session(&sid).unwrap()[0].source, ItemSource::Manual);
    }

    #[test]
    fn add_item_with_source_persists_the_source() {
        use crate::domain::ItemSource;
        let (s, sid) = store_with_session();
        let live = s.add_item_with_source(&sid, "todo", "live one", ItemSource::Live).unwrap();
        let auth = s.add_item_with_source(&sid, "todo", "auth one", ItemSource::Authoritative).unwrap();
        assert_eq!(live.source, ItemSource::Live);
        assert_eq!(auth.source, ItemSource::Authoritative);
    }

    #[test]
    fn existing_rows_backfill_as_authoritative() {
        // simulate a pre-migration row: raw insert without a source column value
        let (s, sid) = store_with_session();
        s.conn.execute(
            "INSERT INTO items (id, session_id, kind, text, done, created_at, updated_at, device_id)
             VALUES ('legacy', ?1, 'todo', 'old', 0, 1, 1, 'device-a')",
            [&sid],
        ).unwrap();
        let legacy = s.list_items_for_session(&sid).unwrap()
            .into_iter().find(|i| i.id == "legacy").unwrap();
        assert_eq!(legacy.source, crate::domain::ItemSource::Authoritative,
            "the column DEFAULT backfills rows that predate the source column");
    }
```
Update `add_item_if_status`'s two existing tests to pass a source (they exercise the gate, source is incidental). Each fn currently has only `use crate::domain::SessionStatus;` — add `use crate::domain::ItemSource;` to BOTH so `ItemSource::Live` resolves:
```rust
    // In both add_item_if_status_writes_when_status_matches AND
    // add_item_if_status_no_ops_when_status_mismatches, add the import:
    use crate::domain::ItemSource;
    // …and pass the source through the call:
    s.add_item_if_status(&sid, "todo", "order lumber", SessionStatus::Recording, ItemSource::Live)
```

- [ ] **Step 2: Run to see failure** — `cargo test -p murmur-core items` and `... domain`; expect compile FAIL (no `ItemSource`, no `source` field/method).

- [ ] **Step 3: Implement**

`src/domain.rs` — add the enum and the field:
```rust
/// Where a captured item came from. Drives the end-of-session swap
/// (`Store::finish_session_processed`): `live` items and *prior-run*
/// `authoritative` items are tombstoned when a new authoritative pass lands;
/// `manual` items are never swept by processing. Free of a migration for new
/// values would be nice, but the swap logic depends on the closed set — keep it
/// closed and parse defensively.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemSource {
    /// Written by a live in-session pass (Plan 05). Provisional; swept on the
    /// next successful process().
    Live,
    /// Written by an end-of-session processing run (Plan 04). The source of
    /// truth once its run finishes.
    Authoritative,
    /// User-entered (story 10 parity) or a direct `add_item`. Never swept by
    /// processing; only a full session delete removes it.
    Manual,
}

impl ItemSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemSource::Live => "live",
            ItemSource::Authoritative => "authoritative",
            ItemSource::Manual => "manual",
        }
    }
    pub fn parse(raw: &str) -> Result<Self, crate::error::CoreError> {
        match raw {
            "live" => Ok(ItemSource::Live),
            "authoritative" => Ok(ItemSource::Authoritative),
            "manual" => Ok(ItemSource::Manual),
            other => Err(crate::error::CoreError::Corrupt(format!(
                "unknown item source: {other}"
            ))),
        }
    }
}
```
Add to `CapturedItem` (after `text`, before `done`):
```rust
    pub source: ItemSource,
```
Re-export `ItemSource` from `src/lib.rs` alongside the other domain re-exports (the Task 4 evals-flip test and `source_swap_e2e.rs` reach it as `murmur_core::ItemSource`):
```rust
pub use domain::ItemSource;   // add to the existing `pub use domain::{...}` line
```

`src/store/migrations.rs` — append a v2 entry to `MIGRATIONS` (NEVER edit v1). The existing `migrate_with` already wraps each entry in `BEGIN; …; PRAGMA user_version = N; COMMIT;` with rollback recovery — v2 inherits that transactional guarantee:
```rust
    // v2: items.source (Plan 06a). Backfill existing rows as 'authoritative' —
    // pre-06a items were all written by the processing pipeline (Plan 04) or by
    // manual add_item; treating them as authoritative is the safe default (they
    // are never swept unless a *new* run supersedes them, exactly today's
    // behavior). SQLite ADD COLUMN with NOT NULL requires the DEFAULT.
    r#"
    ALTER TABLE items ADD COLUMN source TEXT NOT NULL DEFAULT 'authoritative';
    "#,
```

`src/store/items.rs`:
```rust
const ITEM_COLS: &str =
    "id, session_id, kind, text, source, done, created_at, updated_at, device_id";
```
In `item_from_row`, read+parse source (place after `text`):
```rust
        source: {
            let raw: String = row.get("source").map_err(CoreError::Sqlite)?;
            crate::domain::ItemSource::parse(&raw)?
        },
```
Rework the writers. `add_item` keeps its signature and becomes a Manual add (truthful: a bare store add is manual/parity):
```rust
    pub fn add_item(&self, session_id: &str, kind: &str, text: &str) -> Result<CapturedItem, CoreError> {
        self.add_item_with_source(session_id, kind, text, ItemSource::Manual)
    }

    /// Adds an item with an explicit source, ungated. The processing pipeline
    /// uses this for `authoritative` writes (it owns the session; no status
    /// gate applies during processing).
    pub fn add_item_with_source(
        &self,
        session_id: &str,
        kind: &str,
        text: &str,
        source: ItemSource,
    ) -> Result<CapturedItem, CoreError> {
        self.get_session(session_id)?; // NotFound if missing/tombstoned
        self.insert_item(session_id, kind, text, source)
    }
```
`add_item_if_status` grows a `source` param (only callers: the live tool + two tests):
```rust
    pub fn add_item_if_status(
        &self,
        session_id: &str,
        kind: &str,
        text: &str,
        required: SessionStatus,
        source: ItemSource,
    ) -> Result<Option<CapturedItem>, CoreError> {
        let session = self.get_session(session_id)?;
        if session.status != required {
            return Ok(None);
        }
        self.insert_item(session_id, kind, text, source).map(Some)
    }
```
`insert_item` grows `source` and writes the column:
```rust
    fn insert_item(&self, session_id: &str, kind: &str, text: &str, source: ItemSource)
        -> Result<CapturedItem, CoreError>
    {
        let now = self.now();
        let item = CapturedItem {
            id: new_id(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            text: text.to_string(),
            source,
            done: false,
            created_at: now,
            updated_at: now,
            device_id: self.device_id.clone(),
        };
        self.conn.execute(
            "INSERT INTO items (id, session_id, kind, text, source, done, created_at, updated_at, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7, ?8)",
            rusqlite::params![
                item.id, item.session_id, item.kind, item.text, item.source.as_str(),
                item.created_at as i64, item.updated_at as i64, item.device_id,
            ],
        )?;
        Ok(item)
    }
```
Add `use crate::domain::ItemSource;` to the imports.

- [ ] **Step 4: Run** — `cargo test -p murmur-core items domain` → green.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(core): items.source column (live|authoritative|manual), migration v2"
```

---

### Task 2: Swap at finish; delete `clear_session_outputs`

**Files:** Modify `src/store/sessions.rs`

- [ ] **Step 1: Write the failing tests** (in `sessions.rs` tests)
```rust
    #[test]
    fn finish_processed_swaps_out_live_and_prior_authoritative_keeps_manual_and_this_run() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        // Board before this run: a live item, a prior-run authoritative item,
        // and a manual item.
        let live = s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        let prior = s.add_item_with_source(&sid, "todo", "prior auth", ItemSource::Authoritative).unwrap();
        let manual = s.add_item_with_source(&sid, "note", "manual", ItemSource::Manual).unwrap();
        // This run creates two authoritative items.
        let a1 = s.add_item_with_source(&sid, "todo", "new 1", ItemSource::Authoritative).unwrap();
        let a2 = s.add_item_with_source(&sid, "safety", "new 2", ItemSource::Authoritative).unwrap();

        s.end_session(&sid).unwrap();
        s.finish_session_processed(&sid, "done", &harness::Usage::default(), &[a1.id.clone(), a2.id.clone()]).unwrap();

        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert!(ids.contains(&manual.id), "manual survives the swap");
        assert!(ids.contains(&a1.id) && ids.contains(&a2.id), "this-run authoritative survives");
        assert!(!ids.contains(&live.id), "live is swept");
        assert!(!ids.contains(&prior.id), "prior-run authoritative is swept");
        assert_eq!(ids.len(), 3);
        // status + cost still land in the same tx
        assert_eq!(s.get_session(&sid).unwrap().status, SessionStatus::Processed);
        assert_eq!(s.list_llm_usage_for_session(&sid).unwrap().len(), 1);
    }

    #[test]
    fn finish_processed_with_no_run_ids_sweeps_all_live_and_authoritative() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        let manual = s.add_item_with_source(&sid, "note", "manual", ItemSource::Manual).unwrap();
        s.end_session(&sid).unwrap();
        s.finish_session_processed(&sid, "(empty session)", &harness::Usage::default(), &[]).unwrap();
        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![manual.id], "only manual survives an empty-run swap");
    }

    #[test]
    fn finish_failed_leaves_the_live_board_intact() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        let live = s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        s.end_session(&sid).unwrap();
        s.finish_session_failed(&sid, &harness::Usage::default()).unwrap();
        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![live.id], "a failed process must not sweep the live board (the whole fix)");
        assert_eq!(s.get_session(&sid).unwrap().status, SessionStatus::Failed);
    }
```
```rust
    #[test]
    fn clear_authoritative_outputs_spares_live_and_manual_and_sweeps_artifacts() {
        use crate::domain::ItemSource;
        let s = store();
        let session = s.start_session(None).unwrap();
        let sid = session.id.clone();
        let live = s.add_item_with_source(&sid, "todo", "live", ItemSource::Live).unwrap();
        let manual = s.add_item_with_source(&sid, "note", "manual", ItemSource::Manual).unwrap();
        s.add_item_with_source(&sid, "todo", "stale auth", ItemSource::Authoritative).unwrap();
        s.add_artifact(&sid, "report", "old", "body").unwrap();

        let cleared = s.clear_authoritative_outputs(&sid).unwrap();
        assert_eq!(cleared, 2, "one authoritative item + one artifact");
        let ids: Vec<String> = s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![live.id, manual.id], "live and manual are spared");
        assert!(s.list_artifacts_for_session(&sid).unwrap().is_empty(), "all artifacts swept");
        // idempotent: nothing left to clear
        assert_eq!(s.clear_authoritative_outputs(&sid).unwrap(), 0);
        // missing session errors
        assert!(matches!(
            s.clear_authoritative_outputs("nope"),
            Err(CoreError::NotFound { entity: "session", .. })
        ));
    }
```
DELETE the two obsolete tests `clear_session_outputs_tombstones_live_children` and `clear_session_outputs_on_failed_session`.

- [ ] **Step 2: Run to see failure** — compile FAIL (`finish_session_processed` arity; `clear_session_outputs` deleted referenced elsewhere — that's fixed in Tasks 3/4).

- [ ] **Step 3: Implement**

Rewrite `finish_session_processed` to take the run-id set and perform the swap in the same transaction:
```rust
    /// Pipeline success exit. In ONE transaction: swap out the old board, mark
    /// the session Processed, and log LLM cost. The swap tombstones every item
    /// for the session that is `source = live` or `source = authoritative` but
    /// was NOT created by this run (`run_item_ids`) — i.e. the prior live board
    /// and any authoritative leftovers from a failed prior run. `manual` items
    /// and this run's own items are never swept. A crash before this commit
    /// leaves the live board intact (nothing was swept); the next successful
    /// run sweeps the stragglers via the same rule.
    pub fn finish_session_processed(
        &self,
        session_id: &str,
        summary: &str,
        usage: &harness::Usage,
        run_item_ids: &[String],
    ) -> Result<Session, CoreError> {
        let tx = self.conn.unchecked_transaction()?;
        let now = self.now() as i64;

        let mut sql = String::from(
            "UPDATE items SET deleted_at = ?1, updated_at = ?1
             WHERE session_id = ?2 AND deleted_at IS NULL
               AND source IN ('live', 'authoritative')",
        );
        // params: [now, session_id, run_item_ids...]
        let mut params: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(now), Box::new(session_id.to_string())];
        if !run_item_ids.is_empty() {
            let placeholders: Vec<String> =
                (0..run_item_ids.len()).map(|i| format!("?{}", i + 3)).collect();
            sql.push_str(&format!(" AND id NOT IN ({})", placeholders.join(", ")));
            for id in run_item_ids {
                params.push(Box::new(id.clone()));
            }
        }
        self.conn.execute(
            &sql,
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
        )?;

        let session = self.mark_session_processed(session_id, summary)?;
        self.record_llm_usage(Some(session_id), "processing", usage)?;
        tx.commit()?;
        Ok(session)
    }
```
DELETE the `clear_session_outputs` method entirely. Leave `finish_session_failed` untouched (it never cleared — that is now the point). ADD the scoped entry clear:
```rust
    /// Clears a session's AUTHORITATIVE outputs before a (re)processing attempt
    /// (Phase 0): tombstones items with `source='authoritative'` and ALL
    /// artifacts (artifacts are only ever written by processing, so every one is
    /// authoritative-equivalent). NEVER touches `source='live'` (the safety-net
    /// board that must survive a failed retry, Plan 06a) or `source='manual'`
    /// (the user's own). Bounds duplicate accumulation across repeated FAILED
    /// attempts to a single in-flight attempt's worth. Idempotent pure delete →
    /// crash-safe on retry. Returns rows tombstoned.
    pub fn clear_authoritative_outputs(&self, session_id: &str) -> Result<usize, CoreError> {
        let now = self.now() as i64;
        let tx = self.conn.unchecked_transaction()?;
        self.get_session(session_id)?; // NotFound if missing/tombstoned
        let items = self.conn.execute(
            "UPDATE items SET deleted_at = ?1, updated_at = ?1
             WHERE session_id = ?2 AND deleted_at IS NULL AND source = 'authoritative'",
            rusqlite::params![now, session_id],
        )?;
        let artifacts = self.conn.execute(
            "UPDATE artifacts SET deleted_at = ?1, updated_at = ?1
             WHERE session_id = ?2 AND deleted_at IS NULL",
            rusqlite::params![now, session_id],
        )?;
        tx.commit()?;
        Ok(items + artifacts)
    }
```

- [ ] **Step 4: Run** — `cargo test -p murmur-core sessions` → green (Tasks 3/4 fix the remaining callers).

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(core): swap board at finish (source-aware), delete clear_session_outputs"
```

---

### Task 3: `AddItemTool` sources + processor id-collection + scoped Phase-0 clear

**Files:** Modify `src/pipeline/tools.rs`, `src/pipeline/mod.rs`, `src/pipeline/live.rs`

- [ ] **Step 1: Write the failing tests**

In `tools.rs` tests, add source coverage and switch `gated` → `live`:
```rust
    #[tokio::test]
    async fn live_tool_writes_live_source_when_recording() {
        use crate::domain::{ItemSource, SessionStatus};
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::live(store.clone(), &sid);
        tool.execute(serde_json::json!({"kind":"todo","text":"order lumber"})).await.unwrap();
        let items = store.lock().unwrap().list_items_for_session(&sid).unwrap();
        assert_eq!(items[0].source, ItemSource::Live);
        let _ = SessionStatus::Recording;
    }

    #[tokio::test]
    async fn authoritative_tool_writes_source_and_records_the_id() {
        use crate::domain::ItemSource;
        let (store, sid) = shared_store_with_session();
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let tool = super::AddItemTool::authoritative(store.clone(), &sid, sink.clone());
        tool.execute(serde_json::json!({"kind":"todo","text":"order lumber"})).await.unwrap();
        let items = store.lock().unwrap().list_items_for_session(&sid).unwrap();
        assert_eq!(items[0].source, ItemSource::Authoritative);
        assert_eq!(sink.lock().unwrap().as_slice(), &[items[0].id.clone()],
            "the finish tx learns which items this run created");
    }
```
Rename the two `gated_add_item_*` tests' constructor calls `AddItemTool::gated(store, &sid, SessionStatus::Recording)` → `AddItemTool::live(store, &sid)` (drop the `required` arg — `live` is gated to Recording by construction). This makes each fn's `use crate::domain::SessionStatus;` dead — **delete that `use` line in both** or the Task 4 zero-warnings clippy gate fails on `unused_imports`.

In `mod.rs` tests, `processes_a_session_end_to_end` already asserts the board holds the extracted item after success — it now also proves the swap (the run's authoritative item is in the id set, so it survives). Add one focused test that the retry path is idempotent through the swap (or lean on the existing `retry_after_failure_does_not_duplicate_outputs`, whose result is unchanged). Add:
```rust
    #[tokio::test]
    async fn live_item_survives_a_failed_process_then_is_swapped_on_retry() {
        use crate::domain::ItemSource;
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.add_item_with_source(&session.id, "todo", "live capture", ItemSource::Live).unwrap();
        store.append_transcript(&session.id, "order the framing lumber today").unwrap();
        store.end_and_record_session(&session.id).unwrap();
        let sid = session.id.clone();
        let store = Arc::new(Mutex::new(store));
        // attempt 1 fails (summary returns no tool); attempt 2 succeeds.
        let processor = SessionProcessor::new(
            Arc::new(MockProvider::new(vec![
                end_turn("no extraction"), end_turn("no summary tool"),
                tool_use("add_item", serde_json::json!({"kind":"todo","text":"order 12 2x10s"})),
                end_turn("done"),
                summary_response("Lumber ordered."),
            ])),
            store.clone(), Arc::new(Mutex::new(Memory::default())), Arc::new(NullMemoryStore),
        );
        assert!(processor.process(&sid).await.is_err());
        // R7: the live board survived the failure.
        assert_eq!(store.lock().unwrap().list_items_for_session(&sid).unwrap().len(), 1);
        processor.process(&sid).await.unwrap();
        // swap: live capture gone, exactly the authoritative item remains.
        let items = store.lock().unwrap().list_items_for_session(&sid).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "order 12 2x10s");
        assert_eq!(items[0].source, ItemSource::Authoritative);
    }

    /// REQUIRED (review): repeated FAILED retries must not accumulate duplicate
    /// authoritative todos. The Phase-0 scoped clear bounds them to one in-flight
    /// attempt while the live board (safety net) and manual items persist.
    #[tokio::test]
    async fn repeated_failed_retries_do_not_accumulate_authoritative_dupes() {
        use crate::domain::ItemSource;
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        let sid = session.id.clone();
        store.add_item_with_source(&sid, "todo", "live capture", ItemSource::Live).unwrap();
        store.add_item_with_source(&sid, "note", "manual note", ItemSource::Manual).unwrap();
        store.append_transcript(&sid, "order the framing lumber today").unwrap();
        store.end_and_record_session(&sid).unwrap();
        let store = Arc::new(Mutex::new(store));

        // Two attempts that extract one authoritative item then fail on summary,
        // then a success.
        let processor = SessionProcessor::new(
            Arc::new(MockProvider::new(vec![
                tool_use("add_item", serde_json::json!({"kind":"todo","text":"order lumber"})),
                end_turn("done"), end_turn("no summary tool"),          // attempt 1 fails
                tool_use("add_item", serde_json::json!({"kind":"todo","text":"order lumber"})),
                end_turn("done"), end_turn("no summary tool"),          // attempt 2 fails
                tool_use("add_item", serde_json::json!({"kind":"todo","text":"order 12 2x10s"})),
                end_turn("done"), summary_response("Lumber ordered."),  // attempt 3 succeeds
            ])),
            store.clone(), Arc::new(Mutex::new(Memory::default())), Arc::new(NullMemoryStore),
        );

        let auth = |s: &Store| s.list_items_for_session(&sid).unwrap()
            .into_iter().filter(|i| i.source == ItemSource::Authoritative).count();
        let live = |s: &Store| s.list_items_for_session(&sid).unwrap()
            .into_iter().any(|i| i.source == ItemSource::Live);

        assert!(processor.process(&sid).await.is_err());
        { let s = store.lock().unwrap();
          assert!(live(&s), "live board survives failure #1");
          assert_eq!(auth(&s), 1, "one attempt's authoritative items after failure #1"); }

        assert!(processor.process(&sid).await.is_err());
        { let s = store.lock().unwrap();
          assert!(live(&s), "live board survives failure #2");
          assert_eq!(auth(&s), 1, "still one attempt's worth — the entry clear bounds dupes"); }

        processor.process(&sid).await.unwrap();
        let s = store.lock().unwrap();
        let items = s.list_items_for_session(&sid).unwrap();
        assert_eq!(items.len(), 2, "exactly this-run authoritative + the manual entry");
        assert!(items.iter().any(|i| i.text == "order 12 2x10s" && i.source == ItemSource::Authoritative));
        assert!(items.iter().any(|i| i.text == "manual note" && i.source == ItemSource::Manual));
        assert!(!items.iter().any(|i| i.source == ItemSource::Live), "live board swapped out on success");
    }
```

- [ ] **Step 2: Run to see failure** — compile FAIL (`AddItemTool::live`/`authoritative` don't exist; `mod.rs` still calls `clear_session_outputs`/`::new` and 3-arg `finish_session_processed`).

- [ ] **Step 3: Implement**

`tools.rs` — give `AddItemTool` a `source` and an optional id sink; provide three constructors. Keep `new` as a Manual, ungated, no-sink tool so the existing generic-behavior tests stay valid:
```rust
pub struct AddItemTool {
    store: Arc<Mutex<Store>>,
    session_id: String,
    source: crate::domain::ItemSource,
    /// When set, only write while the session's status matches (checked
    /// atomically with the insert — closes the live-vs-processing TOCTOU).
    required_status: Option<SessionStatus>,
    /// When set, each inserted item's id is pushed here so the end-of-session
    /// swap knows what THIS run created (Plan 06a design problem 1).
    created_ids: Option<Arc<Mutex<Vec<String>>>>,
}

impl AddItemTool {
    /// Manual, ungated write (source=Manual). Kept for direct/test use.
    pub fn new(store: Arc<Mutex<Store>>, session_id: &str) -> Self {
        AddItemTool { store, session_id: session_id.to_string(),
            source: crate::domain::ItemSource::Manual, required_status: None, created_ids: None }
    }
    /// Live in-session write (source=Live), gated to `Recording`.
    pub fn live(store: Arc<Mutex<Store>>, session_id: &str) -> Self {
        AddItemTool { store, session_id: session_id.to_string(),
            source: crate::domain::ItemSource::Live,
            required_status: Some(SessionStatus::Recording), created_ids: None }
    }
    /// Authoritative processing write (source=Authoritative), ungated, records
    /// each new id into `created_ids` for the finish swap.
    pub fn authoritative(
        store: Arc<Mutex<Store>>, session_id: &str, created_ids: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        AddItemTool { store, session_id: session_id.to_string(),
            source: crate::domain::ItemSource::Authoritative,
            required_status: None, created_ids: Some(created_ids) }
    }
}
```
Rewrite `execute`'s write branch to thread source and capture the id:
```rust
        let guard = lock(&self.store, "add_item")?;
        let item = match self.required_status {
            None => guard
                .add_item_with_source(&self.session_id, kind, text, self.source)
                .map_err(|e| tool_err("add_item", e.to_string()))?,
            Some(required) => guard
                .add_item_if_status(&self.session_id, kind, text, required, self.source)
                .map_err(|e| tool_err("add_item", e.to_string()))?
                .ok_or_else(|| tool_err("add_item", "session no longer recording"))?,
        };
        if let Some(sink) = &self.created_ids {
            sink.lock().map_err(|_| tool_err("add_item", "created-ids lock poisoned"))?
                .push(item.id.clone());
        }
        Ok(format!("added {kind}: {text}"))
```

`live.rs` — swap the registration and clean the docs:
```rust
        registry.register(AddItemTool::live(self.store.clone(), &self.session_id));
```
In the `RacingProvider` test helper (line ~257), DELETE the `store.clear_session_outputs(...)` line — `end_and_record_session` alone flips the status, which is exactly what the write-gate must catch (the test's stated purpose). Rewrite the module-doc paragraph (lines ~3-19) to describe swap-at-finish instead of `clear_session_outputs`, e.g.:
> End-of-session `process()` (Plan 04) stays the source of truth. It runs against the still-visible live board and, in the finish transaction, **swaps** it out: `source=live` items (and prior-run `authoritative` leftovers) are tombstoned as the new authoritative set commits (`Store::finish_session_processed`). A failed process leaves the live board intact. The UI re-queries `list_items_for_session` on the status change.

`mod.rs`:
- Replace the Phase-0 clear (which called the now-deleted `clear_session_outputs`, tombstoning live+manual too) with the scoped `clear_authoritative_outputs` — validate, sweep only prior authoritative outputs, snapshot the transcript:
```rust
        let transcript = {
            let store = self.locked()?;
            let session = store.get_session(session_id)?;
            if !matches!(session.status, SessionStatus::AwaitingProcessing | SessionStatus::Failed) {
                return Err(CoreError::InvalidState(format!(
                    "cannot process a {} session", session.status.as_str()
                )));
            }
            // Sweep a prior FAILED attempt's authoritative leftovers (+ artifacts)
            // so repeated retries can't accumulate duplicate todos. Never touches
            // the live board (the safety net) or manual items.
            store.clear_authoritative_outputs(session_id)?;
            session.transcript
        };
```
- Empty-session path passes an empty run set (`&[]`) — the swap still clears any stray live board for a session that processed to "(empty session)":
```rust
            let session = self.locked()?.finish_session_processed(
                session_id, "(empty session)", &usage, &[],
            )?;
```
- Create the id sink, thread it into the phases, read it at finish:
```rust
        let created_ids = Arc::new(Mutex::new(Vec::<String>::new()));
        let result = self
            .run_llm_phases(session_id, &assembled.text, &memory_prompt, &mut usage, created_ids.clone())
            .await;

        let store = self.locked()?;
        match result {
            Ok(summary) => {
                let ids = created_ids
                    .lock()
                    .map_err(|_| CoreError::InvalidState("created-ids lock poisoned".into()))?
                    .clone();
                let session = store.finish_session_processed(session_id, &summary, &usage, &ids)?;
                Ok(ProcessOutcome { session, usage })
            }
            Err(e) => {
                let _ = store.finish_session_failed(session_id, &usage);
                Err(e.into())
            }
        }
```
- `run_llm_phases` grows `created_ids: Arc<Mutex<Vec<String>>>` and registers the authoritative tool:
```rust
        registry.register(AddItemTool::authoritative(self.store.clone(), session_id, created_ids));
```
- Update the file-top module doc: "Reprocessing is idempotent — the old board is **swapped out in the finish transaction** (source-aware), so a Failed retry can't duplicate todos and a *failure* leaves the live board intact."

- [ ] **Step 4: Run** — `cargo test -p murmur-core` → green.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(core): source-aware AddItemTool + run-id swap wiring; scope Phase-0 clear to authoritative"
```

---

### Task 4: Flip the evals pin; e2e; reader audit; docs; verify

**Files:** Modify `crates/evals/tests/carried_scenarios.rs`, `src/store/items.rs` (one reader test), create `crates/murmur-core/tests/source_swap_e2e.rs`, `README.md`

- [ ] **Step 1: Flip the carried characterization test to the new contract**

In `carried_scenarios.rs`, replace `failed_processing_after_live_capture_leaves_empty_board` with the regression:
```rust
/// 4a — REGRESSION (was a characterization pin of the swap gap). Plan 06a moved
/// the board replacement from clear-at-entry to swap-at-finish, so a failed
/// process now PRESERVES the live board (the whole point: a transient LLM error
/// no longer leaves the user with zero items). This test, formerly asserting an
/// empty board, now asserts the live item survives.
#[tokio::test]
async fn failed_processing_after_live_capture_preserves_live_board() {
    let store = Store::open_in_memory("dev").unwrap();
    let session = store.start_session(None).unwrap();
    store.append_transcript(&session.id, "order lumber for the deck framing today").unwrap();
    let sid = session.id.clone();
    let store = Arc::new(Mutex::new(store));
    let memory = Arc::new(Mutex::new(Memory::default()));

    let mut live = LiveExtractor::new(
        Arc::new(MockProvider::new(vec![
            tool_use("add_item", serde_json::json!({"kind":"todo","text":"order lumber"})),
            end_turn("captured"),
        ])), store.clone(), memory.clone(), &sid);
    live.min_new_chars = 1;
    live.maybe_extract().await.unwrap();
    assert_eq!(store.lock().unwrap().list_items_for_session(&sid).unwrap().len(), 1);

    store.lock().unwrap().end_and_record_session(&sid).unwrap();
    let processor = SessionProcessor::new(
        Arc::new(MockProvider::new(vec![end_turn("no extraction"), end_turn("no summary tool")])),
        store.clone(), memory, Arc::new(evals::run::NullMemoryStore));
    assert!(processor.process(&sid).await.is_err());

    // FIXED: the live board survives a failed process (R7 — inspectable, and the
    // user keeps what was captured until a retry lands the authoritative set).
    let after = store.lock().unwrap().list_items_for_session(&sid).unwrap();
    assert_eq!(after.len(), 1, "live board preserved on processing failure (swap-at-finish)");
    assert_eq!(after[0].source, murmur_core::ItemSource::Live);
    assert_eq!(store.lock().unwrap().get_session(&sid).unwrap().status, SessionStatus::Failed);
}
```
Add `ItemSource` to the `murmur_core::{...}` import. Leave 4b and 4c unchanged (they exercise live dedup / windowing, which is source-independent — the `add_item` calls in 4b are now `source=Manual`, irrelevant to their assertions).

- [ ] **Step 2: Reader-audit test** (problem 6) — in `items.rs` tests:
```rust
    #[test]
    fn open_todos_include_live_items() {
        use crate::domain::ItemSource;
        let (s, sid) = store_with_session();
        s.add_item_with_source(&sid, "todo", "live todo", ItemSource::Live).unwrap();
        // Morning glance surfaces live items too — post-06a they are the safety
        // net for a still-processing / failed session (decision recorded in plan).
        let open: Vec<_> = s.list_open_todos().unwrap().into_iter().map(|i| i.text).collect();
        assert_eq!(open, vec!["live todo".to_string()]);
    }
```

- [ ] **Step 3: E2e — board visibility across failure, clean swap on success** (`tests/source_swap_e2e.rs`, public API only)
```rust
//! Plan 06a swap contract via public API: the live board stays visible across a
//! failed process (no vanish-then-reappear), and a successful process swaps it
//! out atomically for the authoritative set — never leaving the board blank.

use std::sync::{Arc, Mutex};
use harness::{CompletionResponse, ContentBlock, HarnessError, Memory, MemoryStore, MockProvider, StopReason, Usage};
use murmur_core::{ItemSource, LiveExtractor, SessionProcessor, SessionStatus, Store};

struct NullMemoryStore;
impl MemoryStore for NullMemoryStore {
    fn load(&self) -> Result<Memory, HarnessError> { Ok(Memory::default()) }
    fn save(&self, _m: &Memory) -> Result<(), HarnessError> { Ok(()) }
}
fn tool_use(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse { content: vec![ContentBlock::ToolUse { id: "tu".into(), name: name.into(), input }],
        stop_reason: StopReason::ToolUse, usage: Usage { input_tokens: 30, output_tokens: 8 } }
}
fn end_turn(t: &str) -> CompletionResponse {
    CompletionResponse { content: vec![ContentBlock::Text { text: t.into() }],
        stop_reason: StopReason::EndTurn, usage: Usage { input_tokens: 10, output_tokens: 2 } }
}
fn summary(t: &str) -> CompletionResponse { tool_use("write_summary", serde_json::json!({"summary": t})) }

#[tokio::test]
async fn live_board_survives_failure_and_swaps_clean_on_success() {
    let store = Store::open_in_memory("marcos-phone").unwrap();
    let session = store.start_session(None).unwrap();
    store.append_transcript(&session.id, "order twelve two-by-tens for the deck framing today").unwrap();
    let sid = session.id.clone();
    let store = Arc::new(Mutex::new(store));
    let memory = Arc::new(Mutex::new(Memory::default()));

    // Live pass captures one item (cheap provider).
    let mut live = LiveExtractor::new(
        Arc::new(MockProvider::new(vec![
            tool_use("add_item", serde_json::json!({"kind":"todo","text":"order twelve 2x10s"})),
            end_turn("captured"),
        ])), store.clone(), memory.clone(), &sid);
    live.min_new_chars = 1;
    live.maybe_extract().await.unwrap();
    let live_id = store.lock().unwrap().list_items_for_session(&sid).unwrap()[0].id.clone();

    store.lock().unwrap().end_and_record_session(&sid).unwrap();

    // Attempt 1 FAILS — board must be UNTOUCHED (still the live item).
    let failing = SessionProcessor::new(
        Arc::new(MockProvider::new(vec![end_turn("x"), end_turn("no summary tool")])),
        store.clone(), memory.clone(), Arc::new(NullMemoryStore));
    assert!(failing.process(&sid).await.is_err());
    let mid = store.lock().unwrap().list_items_for_session(&sid).unwrap();
    assert_eq!(mid.len(), 1, "no clear-at-entry: the board never went blank");
    assert_eq!(mid[0].id, live_id);
    assert_eq!(mid[0].source, ItemSource::Live);

    // Attempt 2 succeeds — swap: live gone, authoritative set present.
    let ok = SessionProcessor::new(
        Arc::new(MockProvider::new(vec![
            tool_use("add_item", serde_json::json!({"kind":"todo","text":"order 12 2x10 joists"})),
            tool_use("add_item", serde_json::json!({"kind":"safety","text":"verify ledger attachment"})),
            end_turn("done"),
            summary("Deck framing: lumber ordered."),
        ])), store.clone(), memory, Arc::new(NullMemoryStore));
    assert_eq!(ok.process(&sid).await.unwrap().session.status, SessionStatus::Processed);

    let after = store.lock().unwrap().list_items_for_session(&sid).unwrap();
    assert_eq!(after.len(), 2, "authoritative set replaced the live board");
    assert!(after.iter().all(|i| i.id != live_id), "live id did not survive the swap");
    assert!(after.iter().all(|i| i.source == ItemSource::Authoritative));
}
```
(`ItemSource` is already re-exported from `src/lib.rs` — added in Task 1.)

- [ ] **Step 4: README** — bump the plan-series line:
```markdown
Done: 01 foundation, 02 memory + reflection + context assembler, 03 domain + storage, 04 processing pipeline + reflection coordinator, 05 live extraction, 05b eval suite, 06a source column + swap fix.
Next: 06 STT.
```

- [ ] **Step 5: Full verification**
```
cargo test                                   # workspace green (murmur-core + evals + harness)
cargo clippy --all-targets                   # zero warnings; mechanical fixes only, no #[allow], no behavior change
```
The existing `live_extraction_e2e.rs` (Plan 05) must still pass: its live item is now swept by the swap (not the deleted clear), its authoritative items are in the run set and survive. If it fails, the swap or the id sink is wrong — fix the code, not the test.

- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "test(core): flip swap-gap pin to regression; e2e + reader audit; docs: plan 06a done"
```

---

## Deferred (named, for later plans)

1. **`ItemSource` as a synced/CRDT field.** The column is local today. When the change-log/CRDT layer (spec §9) lands, `source` participates like any other field; the swap becomes a merge concern (two devices, one processing run). Out of scope until sync exists.
2. **Compacting swept rows.** Swept live/authoritative items are tombstones (`deleted_at`), never erased — consistent with the rest of the store. A vacuum/compaction pass is a storage-maintenance plan, not this one.
3. **Transient overlap during processing.** Authoritative items autocommit as tool calls land, so a reader mid-window briefly sees live + partial-authoritative together before the finish swap. Acceptable (strictly better than a blank board). If field testing shows the overlap reads as duplicates, a "processing" board filter is a UI concern for the render plan — not core.
4. **Manual-vs-live provenance in the UI.** `source` is now available to the renderer (e.g. a "captured live" chip vs a processed badge). Surfacing it is a UI plan; core just stores and swaps.
5. **New item sources.** The set is closed by design (the swap depends on it). A future `imported`/`suggested` source would extend the enum and revisit the swap's `IN (...)` filter deliberately — not a free-form string.

## Self-Review Notes

- **Spec coverage:** Rev 2 §2 end-of-session-is-truth ✓ (swap at finish, Task 2/3); graceful degradation / "loses nothing" ✓ (failure preserves the live board — the headline fix, Task 2 `finish_failed_leaves_the_live_board_intact` + flipped evals pin); R7 inspectable ✓ (failed session still shows its captured board; `open_todos_include_live_items`); R9 unchanged ✓ (cost logged on success and failure exactly as before — `finish_session_failed` untouched, `finish_session_processed` still logs `"processing"`).
- **Design problem 1 (created-by-this-run):** solved by an explicit id sink pushed by the authoritative `AddItemTool` and read into the finish tx. Justified over timestamp cutoffs (coarse clock, injected test clock) and a generation-stamp column (extra migration for what an id list captures exactly). Crash-safe: the swap is a pure function of `(source, run_ids)`; a mid-run crash commits no swap and the next run's swap sweeps the stragglers via rule (b). Proven by `finish_processed_swaps_out_live_and_prior_authoritative...` and `live_item_survives_a_failed_process_then_is_swapped_on_retry`.
- **Design problem 2 (retry idempotency + repeated-failure dedup):** two layers — the Phase-0 `clear_authoritative_outputs` (scoped: authoritative items + artifacts, spares live/manual) bounds duplicate accumulation across *failed* retries to one in-flight attempt, and rule (b)'s finish swap replaces the live board atomically on success. Proven by `repeated_failed_retries_do_not_accumulate_authoritative_dupes` (REQUIRED), the pre-existing `retry_after_failure_does_not_duplicate_outputs` (result unchanged), and the retry e2e. Both layers kept for defense in depth (a crash between entry-clear and finish is swept by the other).
- **Design problem 3 (`clear_session_outputs`):** deleted (dangerous manual-killing semantics) and REPLACED by `clear_authoritative_outputs` (authoritative items + all artifacts; never live/manual). `delete_session` cascade unchanged and correct (full delete tombstones manual too).
- **Design problem 4 (reads during processing):** the Phase-0 clear is scoped to authoritative — the live board is never cleared at entry, so it stays visible throughout; `source_swap_e2e` asserts non-blank across failure and clean swap on success.
- **Design problem 5 (evals pin):** `..._leaves_empty_board` → `..._preserves_live_board` regression.
- **Design problem 6 (readers):** audited — only the swap is source-aware. `list_open_todos` shows live (safety net; pinned). `search_sessions` / `activity_for_reflection` read session text, not items — no change.
- **Type consistency:** every referenced item exists in the read source — `Store::{add_item, add_item_with_source(new), add_item_if_status(+source), clear_authoritative_outputs(new), finish_session_processed(+run_item_ids), finish_session_failed, mark_session_processed, record_llm_usage, list_items_for_session, list_artifacts_for_session, add_artifact, list_open_todos, get_session, end_session, end_and_record_session}`; `AddItemTool::{new, live, authoritative}`; `ItemSource::{as_str, parse}` (+`Serialize/Deserialize` for `CapturedItem`); `rusqlite::{params_from_iter, ToSql}`; `harness::Usage`. Migration v2 rides the existing transactional `migrate_with` (BEGIN/version-bump/COMMIT + ROLLBACK recovery) — verified against `failed_migration_rolls_back_cleanly`.
- **Churn surfaced:** `add_item_if_status` grows a `source` param (2 test call sites); `AddItemTool::gated` → `::live` (2 test call sites + live.rs registration); `finish_session_processed` grows `run_item_ids` (3 call sites: 2 in mod.rs, tests); `clear_session_outputs` deleted and replaced by `clear_authoritative_outputs` (Phase-0 caller updated); `RacingProvider` drops one `clear_session_outputs` line; `CapturedItem` gains a field (all constructors are `insert_item` + `item_from_row`, both updated — no scattered struct literals per grep).
- **Test-count checkpoints (expectations, not gates):** T1 +4 (updates 2 in place), T2 +4 (3 swap + 1 scoped-clear, −2 deleted clear tests), T3 +4 (retry e2e, repeated-failure REQUIRED, +2 tool source tests), T4 +1 e2e +1 reader (flip is in-place). Net ≈ +12. Run `cargo test` for the real number.
- **Constraints for later plans:** (1) the swap depends on the closed `ItemSource` set — new sources revisit the `IN (...)` filter deliberately (Deferred 5). (2) The id sink is per-`process()` call and in-memory; the FFI plan (07) wiring a long-lived processor must create a fresh sink per run (the code already does — sink is a local in `process()`). (3) `source` is now available to the renderer for provenance UI (Deferred 4).
```