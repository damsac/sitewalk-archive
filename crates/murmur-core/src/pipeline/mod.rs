//! End-of-session processing (spec §6): transcript in, structured records +
//! summary out. Reprocessing is idempotent — prior outputs are tombstoned
//! first, so a Failed retry can't duplicate todos.

pub mod tools;

pub(crate) mod prompts;

use std::sync::{Arc, Mutex};

use harness::{
    Agent, AgentConfig, ContextAssembler, ContextSection, LlmProvider, Memory, MemoryStore,
    Message, ToolRegistry, UpdateMemoryTool, Usage,
};

use crate::domain::{Session, SessionStatus};
use crate::error::CoreError;
use crate::store::Store;
use tools::{AddItemTool, UpsertContactTool, WriteReportTool};

#[derive(Debug)]
pub struct ProcessOutcome {
    pub session: Session,
    pub usage: Usage,
}

pub struct SessionProcessor {
    provider: Arc<dyn LlmProvider>,
    pub(crate) store: Arc<Mutex<Store>>,
    memory: Arc<Mutex<Memory>>,
    memory_store: Arc<dyn MemoryStore>,
    /// Extraction-pass agent budget.
    pub max_turns: usize,
    pub max_tokens: u32,
    /// Transcript token budget for both passes (chars/4 approximation).
    pub transcript_budget_tokens: usize,
    /// Summary-call output budget.
    pub summary_max_tokens: u32,
}

impl SessionProcessor {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        store: Arc<Mutex<Store>>,
        memory: Arc<Mutex<Memory>>,
        memory_store: Arc<dyn MemoryStore>,
    ) -> Self {
        SessionProcessor {
            provider,
            store,
            memory,
            memory_store,
            max_turns: 16,
            max_tokens: 4096,
            transcript_budget_tokens: 12_000,
            summary_max_tokens: 512,
        }
    }

    fn locked(&self) -> Result<std::sync::MutexGuard<'_, Store>, CoreError> {
        self.store
            .lock()
            .map_err(|_| CoreError::InvalidState("store lock poisoned".into()))
    }

    /// Processes one ended session. Valid from AwaitingProcessing or Failed
    /// (retry). On success: outputs written, summary set, status Processed.
    /// On LLM failure: status Failed, cost still logged (R9), error returned.
    ///
    /// The app shell must not delete or mutate a session while it is being
    /// processed — status is re-validated only at the exit write, so a
    /// concurrent tombstone would produce a silent no-op or a store error.
    pub async fn process(&self, session_id: &str) -> Result<ProcessOutcome, CoreError> {
        // Phase 0: validate, clear prior outputs, snapshot the transcript.
        let transcript = {
            let store = self.locked()?;
            let session = store.get_session(session_id)?;
            if !matches!(
                session.status,
                SessionStatus::AwaitingProcessing | SessionStatus::Failed
            ) {
                return Err(CoreError::InvalidState(format!(
                    "cannot process a {} session",
                    session.status.as_str()
                )));
            }
            store.clear_session_outputs(session_id)?;
            session.transcript
        };
        // Memory lock in its own scope — never held alongside the store guard
        // (no store→memory lock ordering for a second caller to deadlock on).
        let memory_prompt = self
            .memory
            .lock()
            .map_err(|_| CoreError::InvalidState("memory lock poisoned".into()))?
            .to_prompt();

        let assembled = ContextAssembler::assemble(&[ContextSection {
            title: "transcript".into(),
            content: transcript,
            budget_tokens: self.transcript_budget_tokens,
        }]);

        // Phase 1+2: extraction agent pass, then forced summary.
        let mut usage = Usage::default();
        let result =
            self.run_llm_phases(session_id, &assembled.text, &memory_prompt, &mut usage).await;

        // Exit: persist outcome + cost atomically, success or not.
        let store = self.locked()?;
        match result {
            Ok(summary) => {
                let session = store.finish_session_processed(session_id, &summary, &usage)?;
                Ok(ProcessOutcome { session, usage })
            }
            Err(e) => {
                // Bookkeeping errors are secondary: the original LLM error is
                // what the caller must see — never mask it with a DB failure.
                let _ = store.finish_session_failed(session_id, &usage);
                Err(e.into())
            }
        }
    }

    async fn run_llm_phases(
        &self,
        session_id: &str,
        assembled_transcript: &str,
        memory_prompt: &str,
        usage: &mut Usage,
    ) -> Result<String, harness::HarnessError> {
        let mut registry = ToolRegistry::new();
        registry.register(AddItemTool::new(self.store.clone(), session_id));
        registry.register(UpsertContactTool::new(self.store.clone()));
        registry.register(WriteReportTool::new(self.store.clone(), session_id));
        registry.register(
            UpdateMemoryTool::new(self.memory.clone(), self.memory_store.clone())
                .for_session(session_id),
        );

        let agent = Agent::new(
            self.provider.clone(),
            registry,
            AgentConfig {
                system_prompt: prompts::extraction_system_prompt(memory_prompt),
                max_turns: self.max_turns,
                max_tokens: self.max_tokens,
            },
        );
        let outcome = match agent
            .run(vec![Message::user_text(format!(
                "Process this session.\n\n{assembled_transcript}"
            ))])
            .await
        {
            Ok(o) => o,
            Err(run_err) => {
                // Accumulate partial usage before propagating (R9: cost is measured
                // from day one, even when the agent aborts mid-run).
                usage.add(&run_err.usage);
                return Err(run_err.source);
            }
        };
        usage.add(&outcome.usage);

        let (summary, summary_usage) = prompts::summarize(
            self.provider.clone(),
            assembled_transcript,
            self.summary_max_tokens,
        )
        .await?;
        // Count the summary call's tokens BEFORE judging its content (R9:
        // a model that skipped the tool still cost us the call).
        usage.add(&summary_usage);
        summary.ok_or_else(|| {
            harness::HarnessError::Provider("summary response missing write_summary call".into())
        })
    }

    /// Drains the awaiting_processing queue (spec §6: offline sessions queue
    /// and process on reconnect). One session at a time — failures mark that
    /// session Failed and the drain continues. Failed sessions are NOT
    /// auto-retried here; retry is an explicit `process()` call (user-visible
    /// retry affordance, R7).
    ///
    /// Drain order: newest-first — the most recent session is what the user
    /// is waiting on; a reconnect backlog processes LIFO.
    pub async fn process_pending(
        &self,
    ) -> Result<Vec<(String, Result<ProcessOutcome, CoreError>)>, CoreError> {
        let queued = self
            .locked()?
            .list_session_summaries_by_status(SessionStatus::AwaitingProcessing)?;
        let mut results = Vec::with_capacity(queued.len());
        for summary in queued {
            let outcome = self.process(&summary.id).await;
            results.push((summary.id, outcome));
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use harness::{
        CompletionResponse, ContentBlock, FactSource, HarnessError, Memory, MemoryStore,
        MockProvider, StopReason, Usage,
    };

    use crate::domain::SessionStatus;
    use crate::error::CoreError;
    use crate::store::Store;

    use super::*;

    /// Memory store stub — pipeline tests don't touch disk.
    struct NullMemoryStore;
    impl MemoryStore for NullMemoryStore {
        fn load(&self) -> Result<Memory, HarnessError> {
            Ok(Memory::default())
        }
        fn save(&self, _m: &Memory) -> Result<(), HarnessError> {
            Ok(())
        }
    }

    fn processor_with(
        responses: Vec<CompletionResponse>,
    ) -> (SessionProcessor, Arc<Mutex<Store>>, String) {
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store
            .append_transcript(&session.id, "we need lumber. call Dev the framer.")
            .unwrap();
        store.end_and_record_session(&session.id).unwrap();
        let store = Arc::new(Mutex::new(store));
        let processor = SessionProcessor::new(
            Arc::new(MockProvider::new(responses)),
            store.clone(),
            Arc::new(Mutex::new(Memory::default())),
            Arc::new(NullMemoryStore),
        );
        (processor, store, session.id)
    }

    fn tool_use(name: &str, input: serde_json::Value) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::ToolUse { id: "tu_1".into(), name: name.into(), input }],
            stop_reason: StopReason::ToolUse,
            usage: Usage { input_tokens: 100, output_tokens: 20 },
        }
    }

    fn end_turn(text: &str) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage { input_tokens: 50, output_tokens: 10 },
        }
    }

    fn summary_response(text: &str) -> CompletionResponse {
        tool_use("write_summary", serde_json::json!({"summary": text}))
    }

    #[tokio::test]
    async fn processes_a_session_end_to_end() {
        let (processor, store, sid) = processor_with(vec![
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
            tool_use("upsert_contact", serde_json::json!({"name": "Dev", "trade": "framer"})),
            end_turn("done"),
            summary_response("Ordered lumber; Dev handles framing."),
        ]);
        let outcome = processor.process(&sid).await.unwrap();
        assert_eq!(outcome.session.status, SessionStatus::Processed);
        assert_eq!(outcome.session.summary.as_deref(), Some("Ordered lumber; Dev handles framing."));
        // usage: 100+20, 100+20, 50+10 agent + 100+20 summary
        assert_eq!(outcome.usage, Usage { input_tokens: 350, output_tokens: 70 });

        let store = store.lock().unwrap();
        assert_eq!(store.list_items_for_session(&sid).unwrap().len(), 1);
        assert_eq!(store.list_contacts().unwrap().len(), 1);
        let usage_rows = store.list_llm_usage_for_session(&sid).unwrap();
        assert_eq!(usage_rows.len(), 1);
        assert_eq!(usage_rows[0].purpose, "processing");
        assert_eq!(usage_rows[0].input_tokens, 350);
    }

    #[tokio::test]
    async fn failure_marks_failed_and_still_logs_usage() {
        // agent pass succeeds, summary response has no tool call -> Provider error
        let (processor, store, sid) = processor_with(vec![
            end_turn("nothing to extract"),
            end_turn("I refuse to call tools"),
        ]);
        let err = processor.process(&sid).await.unwrap_err();
        assert!(matches!(err, CoreError::Agent(_)));
        let store = store.lock().unwrap();
        assert_eq!(store.get_session(&sid).unwrap().status, SessionStatus::Failed);
        let usage_rows = store.list_llm_usage_for_session(&sid).unwrap();
        assert_eq!(usage_rows.len(), 1, "cost is logged even on failure (R9)");
        // agent pass (50) + summary call that skipped the tool (50) — the
        // failed summary call still cost tokens and they must be counted
        assert_eq!(usage_rows[0].input_tokens, 100);
        assert_eq!(usage_rows[0].output_tokens, 20);
    }

    #[tokio::test]
    async fn retry_after_failure_does_not_duplicate_outputs() {
        let (processor, store, sid) = processor_with(vec![
            // attempt 1: extracts one item, then summary fails
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
            end_turn("done"),
            end_turn("no summary tool"),
            // attempt 2: extracts the same item again, summary succeeds
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
            end_turn("done"),
            summary_response("Lumber ordered."),
        ]);
        assert!(processor.process(&sid).await.is_err());
        processor.process(&sid).await.unwrap();
        let store = store.lock().unwrap();
        assert_eq!(
            store.list_items_for_session(&sid).unwrap().len(),
            1,
            "attempt 1's item was cleared before retry"
        );
    }

    #[tokio::test]
    async fn recording_session_is_rejected() {
        let (processor, store, _sid) = processor_with(vec![]);
        let recording = store.lock().unwrap().start_session(None).unwrap();
        let err = processor.process(&recording.id).await.unwrap_err();
        assert!(matches!(err, CoreError::InvalidState(_)));
    }

    #[tokio::test]
    async fn memory_reaches_the_system_prompt() {
        let provider = Arc::new(MockProvider::new(vec![end_turn("done"), summary_response("s")]));
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.append_transcript(&session.id, "talk about the french drain").unwrap();
        store.end_and_record_session(&session.id).unwrap();
        let mut memory = Memory::default();
        memory.remember_from("vocabulary", "french drain", 1, FactSource::Stated, None);
        let processor = SessionProcessor::new(
            provider.clone(),
            Arc::new(Mutex::new(store)),
            Arc::new(Mutex::new(memory)),
            Arc::new(NullMemoryStore),
        );
        processor.process(&session.id).await.unwrap();
        let reqs = provider.requests();
        assert!(reqs[0].system.contains("french drain"));
    }

    #[tokio::test]
    async fn unknown_session_is_not_found() {
        let (processor, _store, _sid) = processor_with(vec![]);
        let err = processor.process("no-such-session").await.unwrap_err();
        assert!(matches!(err, CoreError::NotFound { entity: "session", .. }));
    }

    /// MaxTurns burns real tokens and those tokens must appear in the usage row
    /// even though the agent never returned a successful TurnOutcome (R9).
    #[tokio::test]
    async fn max_turns_logs_partial_usage() {
        // agent pass: one tool_use response with real usage, then MaxTurns fires
        // max_turns = 1 so the loop fires after the first tool_use response
        let (mut processor, store, sid) = processor_with(vec![
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
        ]);
        processor.max_turns = 1;

        let err = processor.process(&sid).await.unwrap_err();
        assert!(matches!(err, CoreError::Agent(harness::HarnessError::MaxTurns(1))));

        let store = store.lock().unwrap();
        assert_eq!(store.get_session(&sid).unwrap().status, SessionStatus::Failed);
        let usage_rows = store.list_llm_usage_for_session(&sid).unwrap();
        assert_eq!(usage_rows.len(), 1, "usage logged even when agent hits MaxTurns");
        // tool_use response has Usage { input_tokens: 100, output_tokens: 20 }
        assert!(
            usage_rows[0].input_tokens > 0,
            "burned tokens from MaxTurns run must be non-zero"
        );
    }

    /// A failed session stays Failed and process_pending does NOT re-pull it on
    /// a second call — only AwaitingProcessing sessions are drained.
    #[tokio::test]
    async fn process_pending_does_not_retry_failed_sessions() {
        let (processor, store, sid) = processor_with(vec![
            end_turn("nothing to extract"),
            end_turn("no summary tool"), // summary call returns no tool → Provider error
        ]);
        // First drain: session goes Failed
        let results1 = processor.process_pending().await.unwrap();
        assert_eq!(results1.len(), 1);
        assert!(results1[0].1.is_err());
        assert_eq!(
            store.lock().unwrap().get_session(&sid).unwrap().status,
            SessionStatus::Failed
        );

        // Second drain: nothing in AwaitingProcessing → empty
        let results2 = processor.process_pending().await.unwrap();
        assert!(
            results2.is_empty(),
            "failed session must not be re-pulled by a second process_pending call"
        );
    }

    #[tokio::test]
    async fn empty_transcript_still_processes() {
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.end_and_record_session(&session.id).unwrap();
        let processor = SessionProcessor::new(
            Arc::new(MockProvider::new(vec![
                end_turn("nothing here"),
                summary_response("Empty session."),
            ])),
            Arc::new(Mutex::new(store)),
            Arc::new(Mutex::new(Memory::default())),
            Arc::new(NullMemoryStore),
        );
        let outcome = processor.process(&session.id).await.unwrap();
        assert_eq!(outcome.session.status, SessionStatus::Processed);
    }

    #[tokio::test]
    async fn process_pending_on_empty_queue_is_ok_and_empty() {
        let processor = SessionProcessor::new(
            Arc::new(MockProvider::new(vec![])),
            Arc::new(Mutex::new(Store::open_in_memory("device-a").unwrap())),
            Arc::new(Mutex::new(Memory::default())),
            Arc::new(NullMemoryStore),
        );
        assert!(processor.process_pending().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn process_pending_drains_the_queue_and_survives_failures() {
        let store = Store::open_in_memory("device-a").unwrap();
        let a = store.start_session(None).unwrap();
        store.append_transcript(&a.id, "session a words").unwrap();
        store.end_and_record_session(&a.id).unwrap();
        let b = store.start_session(None).unwrap();
        store.append_transcript(&b.id, "session b words").unwrap();
        store.end_and_record_session(&b.id).unwrap();
        // still recording — must be untouched
        let c = store.start_session(None).unwrap();

        // queue order is reverse-chron (b first): b succeeds, a fails on summary
        let processor = SessionProcessor::new(
            Arc::new(MockProvider::new(vec![
                end_turn("done b"),
                summary_response("B done."),
                end_turn("done a"),
                end_turn("no summary tool"),
            ])),
            Arc::new(Mutex::new(store)),
            Arc::new(Mutex::new(Memory::default())),
            Arc::new(NullMemoryStore),
        );

        let results = processor.process_pending().await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|(id, r)| id == &b.id && r.is_ok()));
        assert!(results.iter().any(|(id, r)| id == &a.id && r.is_err()));

        let store = processor.store.lock().unwrap();
        assert_eq!(store.get_session(&b.id).unwrap().status, SessionStatus::Processed);
        assert_eq!(store.get_session(&a.id).unwrap().status, SessionStatus::Failed);
        assert_eq!(store.get_session(&c.id).unwrap().status, SessionStatus::Recording);
    }
}
