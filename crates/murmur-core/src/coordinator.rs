//! Reflection coordinator (spec §7, Rev 3 §1; the Plan 02/03 deferred
//! contract). Call `maybe_reflect` when there is guaranteed compute and NO
//! active session — the engine swaps the whole memory, so an interleaved
//! in-session update would be silently discarded (see
//! `ReflectionEngine::reflect`).
//!
//! Sequence: policy gate -> activity gate -> PRE-reflection snapshot save ->
//! engine reflect -> swap + persist -> record signals + cost.

use std::sync::{Arc, Mutex};

use harness::{
    Clock, LlmProvider, Memory, MemoryStore, ReflectionEngine, ReflectionPolicy, Usage,
};

use crate::error::CoreError;
use crate::store::Store;

fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct ReflectionCoordinator {
    engine: ReflectionEngine,
    pub policy: ReflectionPolicy,
    store: Arc<Mutex<Store>>,
    memory: Arc<Mutex<Memory>>,
    memory_store: Arc<dyn MemoryStore>,
    clock: Clock,
    /// Most-recent sessions fed to one reflection.
    pub max_activity_sessions: usize,
}

impl ReflectionCoordinator {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        store: Arc<Mutex<Store>>,
        memory: Arc<Mutex<Memory>>,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Self {
        ReflectionCoordinator {
            engine: ReflectionEngine::new(provider),
            policy: ReflectionPolicy::default(),
            store,
            memory,
            memory_store,
            clock: Arc::new(system_clock),
            max_activity_sessions: 8,
        }
    }

    /// Replaces the clock (tests inject deterministic time).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn locked_store(&self) -> Result<std::sync::MutexGuard<'_, Store>, CoreError> {
        self.store
            .lock()
            .map_err(|_| CoreError::InvalidState("store lock poisoned".into()))
    }

    /// Runs a reflection if the cadence policy and activity feed warrant one.
    /// Returns `Some(churn)` when a reflection ran, `None` when skipped.
    /// On engine failure: memory and signals are untouched (the pre-reflection
    /// snapshot save has already rotated, which is harmless), error returned.
    ///
    /// Post-swap persist-failure divergence: if the post-swap
    /// `memory_store.save(&outcome.memory)` fails, the in-memory `Memory`
    /// holds the NEW memory but disk holds the OLD one (the pre-reflection
    /// snapshot). Signals are not reset, so the next `maybe_reflect` will
    /// fire again; a restart silently loads the OLD memory until the next
    /// successful reflection persists.
    pub async fn maybe_reflect(&self) -> Result<Option<f32>, CoreError> {
        // Store guard: policy + activity gates — drop before taking memory guard
        // (no overlapping locks; Batch C review: never hold store guard across
        // an await and never hold store + memory together).
        let activity = {
            let store = self.locked_store()?;
            let signals = store.reflection_signals()?;
            if !self.policy.should_reflect(&signals) {
                return Ok(None);
            }
            let activity = store.activity_for_reflection(self.max_activity_sessions)?;
            if activity.is_empty() {
                return Ok(None);
            }
            activity
        }; // store guard dropped here

        // Memory guard in its own scope — no overlap with the store guard above.
        let current_memory = {
            self.memory
                .lock()
                .map_err(|_| CoreError::InvalidState("memory lock poisoned".into()))?
                .clone()
        };

        // Pre-reflection snapshot: saving the CURRENT memory rotates it into
        // the store's snapshot slots, guaranteeing a rollback point that this
        // reflection cannot erode (Plan 02 final-review note).
        self.memory_store.save(&current_memory).map_err(CoreError::Agent)?;

        let outcome = match self
            .engine
            .reflect(&current_memory, &activity, (self.clock)())
            .await
        {
            Ok(o) => o,
            Err(run_err) => {
                // Zero usage means the provider call itself failed (network, auth, etc.)
                // — no tokens were burned, so writing a noise row would be misleading.
                if run_err.usage != Usage::default() {
                    // best-effort: a store failure here must not mask the original
                    // engine error (same precedence pattern as finish_session_failed).
                    if let Ok(store) = self.locked_store() {
                        let _ = store.record_llm_usage(None, "reflection", &run_err.usage);
                    }
                }
                return Err(CoreError::Agent(run_err.source));
            }
        };

        {
            let mut memory = self
                .memory
                .lock()
                .map_err(|_| CoreError::InvalidState("memory lock poisoned".into()))?;
            *memory = outcome.memory.clone();
        }
        self.memory_store.save(&outcome.memory).map_err(CoreError::Agent)?;

        self.locked_store()?.finish_reflection(outcome.churn, &outcome.usage)?;
        Ok(Some(outcome.churn))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use harness::{
        CompletionResponse, ContentBlock, HarnessError, Memory, MemoryStore, MockProvider,
        StopReason, Usage,
    };

    use crate::store::Store;

    use super::*;

    type CoordinatorFixture =
        (ReflectionCoordinator, Arc<Mutex<Memory>>, Arc<SpyMemoryStore>, Arc<Mutex<Store>>);

    /// Records every save so tests can assert the snapshot-then-swap order.
    struct SpyMemoryStore {
        saved: Mutex<Vec<Memory>>,
    }
    impl SpyMemoryStore {
        fn new() -> Arc<Self> {
            Arc::new(SpyMemoryStore { saved: Mutex::new(Vec::new()) })
        }
    }
    impl MemoryStore for SpyMemoryStore {
        fn load(&self) -> Result<Memory, HarnessError> {
            Ok(Memory::default())
        }
        fn save(&self, m: &Memory) -> Result<(), HarnessError> {
            self.saved.lock().unwrap().push(m.clone());
            Ok(())
        }
    }

    fn write_memory_response(sections: serde_json::Value) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "write_memory".into(),
                input: serde_json::json!({"sections": sections}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage { input_tokens: 200, output_tokens: 40 },
        }
    }

    fn store_with_ended_session() -> Store {
        let s = Store::open_in_memory("device-a").unwrap().with_clock(Arc::new(|| 1000));
        let session = s.start_session(None).unwrap();
        s.append_transcript(&session.id, "walked the deck with Dev").unwrap();
        s.end_and_record_session(&session.id).unwrap();
        s
    }

    fn coordinator_with(
        responses: Vec<CompletionResponse>,
        store: Store,
    ) -> CoordinatorFixture {
        let memory = Arc::new(Mutex::new(Memory::default()));
        let memory_store = SpyMemoryStore::new();
        let store = Arc::new(Mutex::new(store));
        let coordinator = ReflectionCoordinator::new(
            Arc::new(MockProvider::new(responses)),
            store.clone(),
            memory.clone(),
            memory_store.clone(),
        );
        (coordinator, memory, memory_store, store)
    }

    #[tokio::test]
    async fn reflects_when_policy_says_so() {
        let (coordinator, memory, memory_store, store) = coordinator_with(
            vec![write_memory_response(serde_json::json!({"people": ["Dev — framer"]}))],
            store_with_ended_session(),
        );
        let churn = coordinator.maybe_reflect().await.unwrap();
        assert!(churn.is_some());

        // memory swapped
        assert_eq!(memory.lock().unwrap().section_texts("people"), vec!["Dev — framer"]);
        // snapshot-then-swap: first save is the PRE-reflection memory (empty),
        // second is the new one
        let saves = memory_store.saved.lock().unwrap();
        assert_eq!(saves.len(), 2);
        assert!(saves[0].sections.is_empty());
        assert_eq!(saves[1].section_texts("people"), vec!["Dev — framer"]);

        // signals recorded + cost logged
        let store = store.lock().unwrap();
        let signals = store.reflection_signals().unwrap();
        assert_eq!(signals.completed_reflections, 1);
        assert_eq!(signals.sessions_since_reflection, 0);
        assert_eq!(store.usage_totals().unwrap(), (200, 40));
    }

    #[tokio::test]
    async fn second_call_after_success_is_skipped() {
        let (coordinator, _memory, memory_store, _store) = coordinator_with(
            vec![write_memory_response(serde_json::json!({"people": ["Dev — framer"]}))],
            store_with_ended_session(),
        );
        assert!(coordinator.maybe_reflect().await.unwrap().is_some());
        // reset signals gate the second call: no reflection, no extra saves
        let churn = coordinator.maybe_reflect().await.unwrap();
        assert!(churn.is_none());
        assert_eq!(
            memory_store.saved.lock().unwrap().len(),
            2,
            "only the first run's snapshot + swap saves"
        );
    }

    #[tokio::test]
    async fn skips_when_policy_says_no() {
        // fresh store: zero sessions since reflection -> policy false
        let store = Store::open_in_memory("device-a").unwrap();
        let (coordinator, _memory, memory_store, _store) = coordinator_with(vec![], store);
        let churn = coordinator.maybe_reflect().await.unwrap();
        assert!(churn.is_none());
        assert!(memory_store.saved.lock().unwrap().is_empty(), "no saves when skipped");
    }

    #[tokio::test]
    async fn skips_when_no_activity() {
        // a completed-session counter without any ended session content
        // (e.g. all sessions tombstoned since): policy says yes, activity is empty
        let s = Store::open_in_memory("device-a").unwrap().with_clock(Arc::new(|| 1000));
        let session = s.start_session(None).unwrap();
        s.end_and_record_session(&session.id).unwrap(); // empty transcript -> blank activity entry skipped
        let (coordinator, _memory, memory_store, _store) = coordinator_with(vec![], s);
        let churn = coordinator.maybe_reflect().await.unwrap();
        assert!(churn.is_none());
        assert!(memory_store.saved.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn engine_failure_leaves_memory_and_signals_untouched() {
        // engine errors (no write_memory in response)
        let bad = CompletionResponse {
            content: vec![ContentBlock::Text { text: "refused".into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        };
        let (coordinator, memory, memory_store, store) =
            coordinator_with(vec![bad], store_with_ended_session());
        let err = coordinator.maybe_reflect().await.unwrap_err();
        assert!(matches!(err, CoreError::Agent(_)));
        assert!(memory.lock().unwrap().sections.is_empty(), "memory not swapped");
        // only the pre-reflection snapshot save happened
        assert_eq!(memory_store.saved.lock().unwrap().len(), 1);
        let signals = store.lock().unwrap().reflection_signals().unwrap();
        assert_eq!(signals.completed_reflections, 0, "failed reflection is not recorded");
    }

    /// A content failure (post-completion — write_memory has malformed sections)
    /// returns an error, leaves memory and signals untouched, AND records a
    /// "reflection" usage row for the tokens that were burned (R9).
    #[tokio::test]
    async fn content_failure_with_real_usage_records_cost() {
        // Provider call succeeds (real usage), but write_memory sections is not an object.
        let malformed = CompletionResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "write_memory".into(),
                input: serde_json::json!({ "sections": "not an object" }),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage { input_tokens: 200, output_tokens: 40 },
        };
        let (coordinator, memory, _memory_store, store) =
            coordinator_with(vec![malformed], store_with_ended_session());
        let err = coordinator.maybe_reflect().await.unwrap_err();
        assert!(matches!(err, CoreError::Agent(_)));

        // memory and signals untouched
        assert!(memory.lock().unwrap().sections.is_empty(), "memory not swapped");
        let store = store.lock().unwrap();
        assert_eq!(
            store.reflection_signals().unwrap().completed_reflections,
            0,
            "failed reflection is not counted"
        );
        // but the burned tokens must appear as a "reflection" usage row
        assert_eq!(
            store.usage_totals().unwrap(),
            (200, 40),
            "post-completion failure usage must be logged (R9)"
        );
    }
}
