//! Live in-session extraction (spec Rev 2 §2): while a session is *recording*,
//! cheap incremental agent passes lift clearly-stated items onto a live board
//! as they're spoken. End-of-session `process()` (Plan 04) stays the source of
//! truth — it tombstones every live item (`Store::clear_session_outputs`) and
//! re-creates the authoritative set. The live board therefore *swaps* when
//! processing lands; the UI re-queries `list_items_for_session` on the session's
//! status change (Recording → Processed).
//!
//! Serialization by construction: `maybe_extract` runs ONLY while the session is
//! `Recording`; `SessionProcessor::process` accepts only
//! `AwaitingProcessing | Failed`. The two are temporally disjoint — the status
//! gate is the boundary. A pass that finds the session no longer recording skips
//! silently (handles the stop-button race).
//!
//! Failure posture: a failed pass never disrupts recording. Non-zero usage is
//! logged (R9), the cursor is NOT advanced, and the next tick retries the same
//! window. Items a failed pass already wrote stay on the board and are
//! de-duplicated by the "already captured" list on the retry.

use std::sync::{Arc, Mutex};

use harness::{
    Agent, AgentConfig, ContextAssembler, ContextSection, LlmProvider, Memory, Message,
    ToolRegistry, Usage,
};

use crate::domain::SessionStatus;
use crate::error::CoreError;
use crate::store::Store;

use super::prompts;
use super::tools::AddItemTool;

/// Result of one live pass. The app shell re-queries the board regardless; this
/// tells it whether a pass ran and what it cost.
#[derive(Clone, Debug, PartialEq)]
pub enum LiveExtractOutcome {
    /// A pass ran. `items_added` is the net change in this session's live item
    /// count — a refresh hint, approximate under concurrent manual edits, not an
    /// authority. Cursor advanced.
    Extracted { items_added: usize, usage: Usage },
    /// No LLM call: too little new transcript since the last pass, or the session
    /// is no longer recording (stop-button race).
    Skipped,
    /// The pass failed and was swallowed to protect recording. Non-zero usage is
    /// logged; the cursor is unchanged so the next tick retries.
    Failed { usage: Usage },
}

/// Drives incremental extraction for ONE recording session. One instance per
/// session, ticked by the app shell (the cadence — every N seconds / on pause —
/// is app-shell policy). `&mut self` makes sequential-only calls a compile-time
/// guarantee: the in-memory cursor is never raced.
pub struct LiveExtractor {
    /// A *cheaper* provider than the end-of-session processor (Rev 2 §2: live
    /// passes optimize for cost; routing is the separate-provider seam).
    provider: Arc<dyn LlmProvider>,
    store: Arc<Mutex<Store>>,
    memory: Arc<Mutex<Memory>>,
    session_id: String,
    /// Chars of transcript covered by a *successful* pass (in-memory: a crash
    /// just re-extracts from 0 or waits for end-of-session truth — no migration).
    cursor: usize,
    /// Floor on new transcript before a pass is worth making. The *cadence* is
    /// app-shell policy; this only guards against passes on a few new chars.
    pub min_new_chars: usize,
    /// Budget for the new-transcript window (chars/4 ≈ tokens).
    pub transcript_window_tokens: usize,
    /// Budget for the already-captured dedup list (newest-first; oldest cut).
    pub already_captured_budget_tokens: usize,
    pub max_turns: usize,
    pub max_tokens: u32,
}

impl LiveExtractor {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        store: Arc<Mutex<Store>>,
        memory: Arc<Mutex<Memory>>,
        session_id: &str,
    ) -> Self {
        LiveExtractor {
            provider,
            store,
            memory,
            session_id: session_id.to_string(),
            cursor: 0,
            min_new_chars: 120,
            transcript_window_tokens: 2_000,
            already_captured_budget_tokens: 400,
            max_turns: 8,
            max_tokens: 1_024,
        }
    }

    /// Chars of transcript covered by the last successful pass.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Store>, CoreError> {
        self.store
            .lock()
            .map_err(|_| CoreError::InvalidState("store lock poisoned".into()))
    }

    /// Runs one incremental extraction pass if warranted. Never surfaces an LLM
    /// error — recording must not be disrupted (see module docs). `Err` is
    /// reserved for genuine store faults (lock poisoned, session vanished).
    pub async fn maybe_extract(&mut self) -> Result<LiveExtractOutcome, CoreError> {
        // Gate + snapshot under a scoped store guard (never held across an await,
        // never overlapping the memory guard).
        let (window, already_captured, items_before, seen_chars) = {
            let store = self.locked()?;
            let session = store.get_session(&self.session_id)?;
            if session.status != SessionStatus::Recording {
                return Ok(LiveExtractOutcome::Skipped);
            }
            let total_chars = session.transcript.chars().count();
            if total_chars.saturating_sub(self.cursor) < self.min_new_chars {
                return Ok(LiveExtractOutcome::Skipped);
            }
            let window: String = session.transcript.chars().skip(self.cursor).collect();
            let items = store.list_items_for_session(&self.session_id)?;
            (window, prompts::format_already_captured(&items), items.len(), total_chars)
        };

        // Memory guard in its own scope — no overlap with the store guard above.
        let memory_prompt = self
            .memory
            .lock()
            .map_err(|_| CoreError::InvalidState("memory lock poisoned".into()))?
            .to_prompt();

        let assembled = ContextAssembler::assemble(&[
            ContextSection {
                title: "already captured".into(),
                content: already_captured,
                budget_tokens: self.already_captured_budget_tokens,
            },
            ContextSection {
                title: "new transcript".into(),
                content: window,
                budget_tokens: self.transcript_window_tokens,
            },
        ]);

        let mut registry = ToolRegistry::new();
        registry.register(AddItemTool::new(self.store.clone(), &self.session_id));
        let agent = Agent::new(
            self.provider.clone(),
            registry,
            AgentConfig {
                system_prompt: prompts::live_extraction_system_prompt(&memory_prompt),
                max_turns: self.max_turns,
                max_tokens: self.max_tokens,
            },
        );

        match agent.run(vec![Message::user_text(assembled.text)]).await {
            Ok(outcome) => {
                let items_after = {
                    let store = self.locked()?;
                    // Cost first (R9), then read the new count.
                    store.record_llm_usage(
                        Some(&self.session_id),
                        "live_extraction",
                        &outcome.usage,
                    )?;
                    store.list_items_for_session(&self.session_id)?.len()
                };
                // Advance only on success — a failed pass re-reads this window.
                self.cursor = seen_chars;
                Ok(LiveExtractOutcome::Extracted {
                    items_added: items_after.saturating_sub(items_before),
                    usage: outcome.usage,
                })
            }
            Err(run_err) => {
                // Swallow. Log only real spend: a turn-1 provider error burned no
                // tokens, so a zero row would be noise (coordinator precedent). A
                // store failure here must not mask the swallow — best-effort.
                if run_err.usage != Usage::default() {
                    if let Ok(store) = self.locked() {
                        let _ = store.record_llm_usage(
                            Some(&self.session_id),
                            "live_extraction",
                            &run_err.usage,
                        );
                    }
                }
                Ok(LiveExtractOutcome::Failed { usage: run_err.usage })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use harness::{
        CompletionResponse, ContentBlock, FactSource, Memory, MockProvider, StopReason, Usage,
    };

    use crate::store::Store;

    use super::*;

    // No NullMemoryStore stub here: unlike SessionProcessor, LiveExtractor takes
    // Arc<Mutex<Memory>> directly (no MemoryStore param) — a stub would be
    // dead code and fail Task 4's zero-clippy-warning gate.

    fn tool_use(name: &str, input: serde_json::Value) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::ToolUse { id: "tu_1".into(), name: name.into(), input }],
            stop_reason: StopReason::ToolUse,
            usage: Usage { input_tokens: 30, output_tokens: 8 },
        }
    }

    fn end_turn(text: &str) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage { input_tokens: 10, output_tokens: 2 },
        }
    }

    /// Recording session with `transcript`, plus a shared store and memory.
    /// `min_new_chars` is dropped to 1 so short test transcripts trigger.
    fn extractor_with(
        responses: Vec<CompletionResponse>,
        transcript: &str,
    ) -> (LiveExtractor, Arc<Mutex<Store>>, Arc<Mutex<Memory>>, String) {
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        if !transcript.is_empty() {
            store.append_transcript(&session.id, transcript).unwrap();
        }
        let sid = session.id;
        let store = Arc::new(Mutex::new(store));
        let memory = Arc::new(Mutex::new(Memory::default()));
        let mut extractor = LiveExtractor::new(
            Arc::new(MockProvider::new(responses)),
            store.clone(),
            memory.clone(),
            &sid,
        );
        extractor.min_new_chars = 1;
        (extractor, store, memory, sid)
    }

    #[tokio::test]
    async fn extracts_items_and_advances_cursor() {
        let (mut extractor, store, _mem, sid) = extractor_with(
            vec![
                tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
                end_turn("captured"),
            ],
            "we need to order lumber for the deck",
        );
        assert_eq!(extractor.cursor(), 0);
        let outcome = extractor.maybe_extract().await.unwrap();
        assert_eq!(
            outcome,
            LiveExtractOutcome::Extracted {
                items_added: 1,
                usage: Usage { input_tokens: 40, output_tokens: 10 },
            }
        );
        // cursor advanced to the transcript length in chars
        assert_eq!(extractor.cursor(), "we need to order lumber for the deck".chars().count());

        let store = store.lock().unwrap();
        let items = store.list_items_for_session(&sid).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "todo");
        // R9: the live pass is billed to the session under its own purpose
        let usage = store.list_llm_usage_for_session(&sid).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].purpose, "live_extraction");
        assert_eq!(usage[0].input_tokens, 40);
    }

    #[tokio::test]
    async fn skips_when_too_little_new_transcript() {
        // default min_new_chars (120) with a 5-char transcript → no call
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.append_transcript(&session.id, "hi ok").unwrap();
        let sid = session.id;
        let store = Arc::new(Mutex::new(store));
        let provider = Arc::new(MockProvider::new(vec![]));
        let mut extractor = LiveExtractor::new(
            provider.clone(),
            store.clone(),
            Arc::new(Mutex::new(Memory::default())),
            &sid,
        );
        let outcome = extractor.maybe_extract().await.unwrap();
        assert_eq!(outcome, LiveExtractOutcome::Skipped);
        assert!(provider.requests().is_empty(), "no LLM call when below the floor");
        assert_eq!(extractor.cursor(), 0);
    }

    #[tokio::test]
    async fn skips_when_no_new_transcript_since_last_pass() {
        let (mut extractor, _store, _mem, _sid) = extractor_with(
            vec![
                tool_use("add_item", serde_json::json!({"kind": "todo", "text": "x"})),
                end_turn("ok"),
            ],
            "order lumber for the deck today",
        );
        assert!(matches!(extractor.maybe_extract().await.unwrap(), LiveExtractOutcome::Extracted { .. }));
        // no new transcript appended → second pass skips (and would panic on an
        // exhausted mock if it tried to call the provider)
        assert_eq!(extractor.maybe_extract().await.unwrap(), LiveExtractOutcome::Skipped);
    }

    #[tokio::test]
    async fn skips_when_not_recording() {
        // end the session first: process()'s domain, not the extractor's
        let (mut extractor, store, _mem, sid) =
            extractor_with(vec![], "a long enough transcript to clear the floor");
        store.lock().unwrap().end_and_record_session(&sid).unwrap();
        let outcome = extractor.maybe_extract().await.unwrap();
        assert_eq!(outcome, LiveExtractOutcome::Skipped, "not recording → no live pass");
        assert_eq!(extractor.cursor(), 0);
    }

    #[tokio::test]
    async fn provider_error_is_swallowed_and_cursor_held() {
        // empty script → first complete() errors: RunError carries zero usage
        let (mut extractor, store, _mem, sid) =
            extractor_with(vec![], "order lumber for the deck today");
        let outcome = extractor.maybe_extract().await.unwrap();
        assert_eq!(outcome, LiveExtractOutcome::Failed { usage: Usage::default() });
        // cursor NOT advanced → next tick retries the same window
        assert_eq!(extractor.cursor(), 0);
        let store = store.lock().unwrap();
        // no tokens burned → no usage row (a zero row would be noise)
        assert!(store.list_llm_usage_for_session(&sid).unwrap().is_empty());
        assert!(store.list_items_for_session(&sid).unwrap().is_empty());
    }

    #[tokio::test]
    async fn mid_run_failure_logs_partial_usage_and_holds_cursor() {
        // one tool_use response, then the script is exhausted → the agent loops
        // for a second turn and the provider errors. RunError carries the first
        // turn's usage AND the add_item already wrote one live item.
        let (mut extractor, store, _mem, sid) = extractor_with(
            vec![tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"}))],
            "order lumber for the deck today",
        );
        let outcome = extractor.maybe_extract().await.unwrap();
        assert_eq!(
            outcome,
            LiveExtractOutcome::Failed { usage: Usage { input_tokens: 30, output_tokens: 8 } }
        );
        assert_eq!(extractor.cursor(), 0, "cursor held so the window retries");

        let store = store.lock().unwrap();
        // the item the failing pass already wrote stays on the board (R7)
        assert_eq!(store.list_items_for_session(&sid).unwrap().len(), 1);
        // partial spend is logged (R9)
        let usage = store.list_llm_usage_for_session(&sid).unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].purpose, "live_extraction");
        assert_eq!(usage[0].input_tokens, 30);
    }

    #[tokio::test]
    async fn already_captured_list_is_in_the_user_message() {
        let provider = Arc::new(MockProvider::new(vec![end_turn("noted")]));
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.append_transcript(&session.id, "still need to order the lumber today").unwrap();
        store.add_item(&session.id, "todo", "order lumber").unwrap();
        let sid = session.id;
        let mut extractor = LiveExtractor::new(
            provider.clone(),
            Arc::new(Mutex::new(store)),
            Arc::new(Mutex::new(Memory::default())),
            &sid,
        );
        extractor.min_new_chars = 1;
        extractor.maybe_extract().await.unwrap();
        let reqs = provider.requests();
        assert!(matches!(
            &reqs[0].messages[0].content[0],
            ContentBlock::Text { text }
                if text.contains("already captured") && text.contains("order lumber")
        ));
    }

    #[tokio::test]
    async fn memory_reaches_the_live_system_prompt() {
        let provider = Arc::new(MockProvider::new(vec![end_turn("nothing new")]));
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.append_transcript(&session.id, "talk about the french drain regrade").unwrap();
        let sid = session.id;
        let mut memory = Memory::default();
        memory.remember_from("vocabulary", "french drain", 1, FactSource::Stated, None);
        let mut extractor = LiveExtractor::new(
            provider.clone(),
            Arc::new(Mutex::new(store)),
            Arc::new(Mutex::new(memory)),
            &sid,
        );
        extractor.min_new_chars = 1;
        extractor.maybe_extract().await.unwrap();
        let reqs = provider.requests();
        assert!(reqs[0].system.contains("french drain"));
        // and the new transcript reached the user message
        assert!(matches!(
            &reqs[0].messages[0].content[0],
            ContentBlock::Text { text } if text.contains("french drain regrade")
        ));
    }
}
