//! End-of-session processing (spec §6): transcript in, structured records +
//! summary out. Reprocessing is idempotent — the old board is **swapped out
//! in the finish transaction** (source-aware), so a Failed retry can't
//! duplicate todos and a *failure* leaves the live board intact.

pub mod tools;

pub mod live;

pub(crate) mod prompts;

use std::sync::{Arc, Mutex};

use harness::{
    Agent, AgentConfig, CompletionRequest, ContentBlock, ContextAssembler, ContextSection,
    LlmProvider, Memory, MemoryStore, Message, Tool, ToolRegistry, ToolSpec, UpdateMemoryTool,
    Usage,
};

use crate::domain::{Session, SessionStatus};
use crate::error::CoreError;
use crate::store::Store;
use tools::{AddItemTool, BuildDocumentTool, UpsertContactTool, WriteReportTool};

/// Maps a session's template key (D4: `landscape`|`property`|`inspection`) to
/// the document's `doc_kind` vocabulary (D2/D5: `estimate`|`report`|
/// `inspection`) that `BuildDocumentTool` and document-number minting use.
/// `None`/unrecognized defaults to `report` — the safest shape (mixed
/// dollar/non-dollar lines, gaps only where explicitly flagged).
pub fn doc_kind_for_template(template: Option<&str>) -> &'static str {
    match template {
        Some("landscape") => "estimate",
        Some("inspection") => "inspection",
        _ => "report",
    }
}

#[derive(Debug)]
pub struct ProcessOutcome {
    pub session: Session,
    pub usage: Usage,
    /// The id of the `document` artifact this run built, if it reached phase B.
    /// `None` when phase B was skipped (empty/whitespace-only transcript).
    /// Callers (the FFI `finish()`) read *this* artifact rather than sweeping
    /// the session's artifacts, so a future non-processing `document` writer
    /// can't be misread as the processing document (Plan 07 D2, carry-note 6).
    pub document_artifact_id: Option<String>,
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
    /// Forced build_document call output budget (phase B, D6: budgeted < 8s
    /// total alongside the extraction pass + summary call).
    pub build_document_max_tokens: u32,
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
            build_document_max_tokens: 1024,
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
        // Phase 0: validate, snapshot the template/existing doc number, sweep
        // prior FAILED-run authoritative leftovers (never the live board), and
        // snapshot the transcript.
        let (transcript, template, existing_doc_number) = {
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
            // D5: a re-process of the same session reuses its already-minted
            // document number rather than minting a new one — read it back
            // from any existing document artifact BEFORE the sweep clears it.
            let existing_doc_number = store
                .latest_document_artifact(session_id)?
                .and_then(|a| serde_json::from_str::<serde_json::Value>(&a.body).ok())
                .and_then(|v| v.get("doc_number").and_then(|n| n.as_u64()));
            // Sweep a prior FAILED attempt's authoritative leftovers (+ artifacts)
            // so repeated retries can't accumulate duplicate todos. Never touches
            // the live board (the safety net) or manual items.
            store.clear_authoritative_outputs(session_id)?;
            (session.transcript, session.template.clone(), existing_doc_number)
        };

        // Empty guard: an empty/whitespace-only transcript would send empty
        // content blocks to the real API (rejected). Skip both LLM phases and
        // process with a placeholder summary; zero usage is correct — no call
        // was made, and the tx helper's contract is status+usage together.
        if transcript.trim().is_empty() {
            let usage = Usage::default();
            let session = self.locked()?.finish_session_processed(
                session_id,
                "(empty session)",
                &usage,
                &[],
            )?;
            return Ok(ProcessOutcome { session, usage, document_artifact_id: None });
        }

        // D5: the document number is minted lazily in phase B
        // (`run_build_document`), only once the model has actually produced a
        // document to write. A run that fails before phase B (extraction or
        // summary) therefore never consumes a number, so a retry can't leave a
        // gap in the sequence. `existing_doc_number` is threaded through so a
        // re-process of the same session reuses its already-minted number
        // instead of minting a fresh one.
        let doc_kind = doc_kind_for_template(template.as_deref());

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

        // Phase 1+2: extraction agent pass, forced summary, forced
        // build_document. The id sink records which items THIS run created,
        // for the finish swap.
        let mut usage = Usage::default();
        let created_ids = Arc::new(Mutex::new(Vec::<String>::new()));
        let result = self
            .run_llm_phases(
                session_id,
                &assembled.text,
                &memory_prompt,
                template.as_deref(),
                doc_kind,
                existing_doc_number,
                &mut usage,
                created_ids.clone(),
            )
            .await;

        // Exit: persist outcome + cost atomically, success or not.
        let store = self.locked()?;
        match result {
            Ok((summary, document_artifact_id)) => {
                let ids = created_ids
                    .lock()
                    .map_err(|_| CoreError::InvalidState("created-ids lock poisoned".into()))?
                    .clone();
                let session = store.finish_session_processed(session_id, &summary, &usage, &ids)?;
                Ok(ProcessOutcome { session, usage, document_artifact_id })
            }
            Err(e) => {
                // Bookkeeping errors are secondary: the original LLM error is
                // what the caller must see — never mask it with a DB failure.
                let _ = store.finish_session_failed(session_id, &usage);
                Err(e.into())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_llm_phases(
        &self,
        session_id: &str,
        assembled_transcript: &str,
        memory_prompt: &str,
        template: Option<&str>,
        doc_kind: &str,
        existing_doc_number: Option<u64>,
        usage: &mut Usage,
        created_ids: Arc<Mutex<Vec<String>>>,
    ) -> Result<(String, Option<String>), harness::HarnessError> {
        let mut registry = ToolRegistry::new();
        registry.register(AddItemTool::authoritative(self.store.clone(), session_id, created_ids));
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
        let summary = summary.ok_or_else(|| {
            harness::HarnessError::Provider("summary response missing write_summary call".into())
        })?;

        // Phase B: forced build_document call — the single most important
        // core addition for a demo-able document (D6). Only reached once the
        // summary succeeded (a doomed session shouldn't also spend on this).
        let document_artifact_id = self
            .run_build_document(
                session_id,
                assembled_transcript,
                memory_prompt,
                template,
                doc_kind,
                existing_doc_number,
                usage,
            )
            .await?;

        Ok((summary, Some(document_artifact_id)))
    }

    /// Forced `build_document` call (mirrors `prompts::summarize`'s one-shot
    /// forced-tool pattern), then executes the tool so the structured document
    /// artifact actually lands (D6).
    #[allow(clippy::too_many_arguments)]
    async fn run_build_document(
        &self,
        session_id: &str,
        assembled_transcript: &str,
        memory_prompt: &str,
        template: Option<&str>,
        doc_kind: &str,
        existing_doc_number: Option<u64>,
        usage: &mut Usage,
    ) -> Result<String, harness::HarnessError> {
        // The tool spec (name/description/schema) is independent of the document
        // number, so build it up front; the number is stamped only at execute.
        let name = BuildDocumentTool::NAME;
        let tool_spec = ToolSpec {
            name: name.to_string(),
            description: BuildDocumentTool::description_str().to_string(),
            input_schema: BuildDocumentTool::input_schema_json(),
        };
        let response = self
            .provider
            .complete(CompletionRequest {
                system: prompts::build_document_prompt(template.unwrap_or("report"), memory_prompt),
                messages: vec![Message::user_text(format!(
                    "Build the document for this session.\n\n{assembled_transcript}"
                ))],
                tools: vec![tool_spec],
                max_tokens: self.build_document_max_tokens,
                tool_choice: Some(name.to_string()),
            })
            .await?;
        usage.add(&response.usage);

        let input = response.content.iter().find_map(|b| match b {
            ContentBlock::ToolUse { name: n, input, .. } if n == name => Some(input.clone()),
            _ => None,
        });
        match input {
            Some(input) => {
                // D5 + carry-note 1 follow-up: the mint lives INSIDE the
                // tool's execute, in the same store transaction as the
                // artifact write — a number is durably consumed if and only
                // if the document lands. Everything that can fail earlier
                // (extraction, summary, the forced call, payload validation)
                // burns nothing. A re-process reuses the number read back
                // from the prior document artifact (`existing_doc_number`).
                let tool = BuildDocumentTool::new(
                    self.store.clone(),
                    session_id,
                    doc_kind,
                    existing_doc_number,
                );
                tool.execute(input).await?;
                // Return the id of the artifact we just wrote so `finish()` can
                // read exactly this run's document (carry-note 6). We just
                // cleared prior documents in phase 0 and wrote one here, so the
                // latest document for the session is unambiguously ours.
                let id = self
                    .store
                    .lock()
                    .map_err(|_| harness::HarnessError::Provider("store lock poisoned".into()))?
                    .latest_document_artifact(session_id)
                    .map_err(|e| harness::HarnessError::Provider(e.to_string()))?
                    .map(|a| a.id)
                    .ok_or_else(|| {
                        harness::HarnessError::Provider(
                            "document artifact missing immediately after build".into(),
                        )
                    })?;
                Ok(id)
            }
            None => Err(harness::HarnessError::Provider(
                "build_document response missing build_document call".into(),
            )),
        }
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

    /// A minimal successful `build_document` response — every successful
    /// `process()` run now makes this forced call as phase B (D6).
    fn document_response() -> CompletionResponse {
        tool_use(
            "build_document",
            serde_json::json!({"total_kind": "sum", "total_label_key": "total", "lines": []}),
        )
    }

    #[tokio::test]
    async fn processes_a_session_end_to_end() {
        let (processor, store, sid) = processor_with(vec![
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
            tool_use("upsert_contact", serde_json::json!({"name": "Dev", "trade": "framer"})),
            end_turn("done"),
            summary_response("Ordered lumber; Dev handles framing."),
            document_response(),
        ]);
        let outcome = processor.process(&sid).await.unwrap();
        assert_eq!(outcome.session.status, SessionStatus::Processed);
        assert_eq!(outcome.session.summary.as_deref(), Some("Ordered lumber; Dev handles framing."));
        // usage: 100+20, 100+20, 50+10 agent + 100+20 summary + 100+20 build_document
        assert_eq!(outcome.usage, Usage { input_tokens: 450, output_tokens: 90 });

        let store = store.lock().unwrap();
        assert_eq!(store.list_items_for_session(&sid).unwrap().len(), 1);
        assert_eq!(store.list_contacts().unwrap().len(), 1);
        let usage_rows = store.list_llm_usage_for_session(&sid).unwrap();
        assert_eq!(usage_rows.len(), 1);
        assert_eq!(usage_rows[0].purpose, "processing");
        assert_eq!(usage_rows[0].input_tokens, 450);
        let artifacts = store.list_artifacts_for_session(&sid).unwrap();
        assert!(artifacts.iter().any(|a| a.kind == "document"), "phase B built a document artifact");
    }

    #[tokio::test]
    async fn processes_and_builds_a_document_artifact() {
        let (processor, store, sid) = processor_with(vec![
            end_turn("nothing to extract"),
            summary_response("Walked the site."),
            document_response(),
        ]);
        processor.process(&sid).await.unwrap();
        let store = store.lock().unwrap();
        let artifacts = store.list_artifacts_for_session(&sid).unwrap();
        assert!(artifacts.iter().any(|a| a.kind == "document"), "phase B built a document artifact");
        // one folded usage row (purpose stays "processing" across all phases)
        let usage_rows = store.list_llm_usage_for_session(&sid).unwrap();
        assert_eq!(usage_rows.len(), 1);
        assert_eq!(usage_rows[0].purpose, "processing");
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
            document_response(),
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
                document_response(),
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
                end_turn("done"), summary_response("Lumber ordered."), document_response(),  // attempt 3 succeeds
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

    /// A document number is a scarce, user-visible sequence (EST-0047). A run
    /// that fails before phase B never built a document, so it must not consume
    /// a number — otherwise the retry shows a gap (EST-0048 with no 0047).
    #[tokio::test]
    async fn failed_attempt_does_not_burn_a_document_number() {
        let (processor, store, sid) = processor_with(vec![
            // attempt 1: extracts an item, then the summary call returns no tool
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
            end_turn("done"),
            end_turn("no summary tool"),
            // attempt 2: succeeds all the way through phase B
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
            end_turn("done"),
            summary_response("Lumber ordered."),
            document_response(),
        ]);
        assert!(processor.process(&sid).await.is_err());
        processor.process(&sid).await.unwrap();
        let store = store.lock().unwrap();
        let doc = store
            .list_artifacts_for_session(&sid)
            .unwrap()
            .into_iter()
            .find(|a| a.kind == "document")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc.body).unwrap();
        assert_eq!(v["doc_number"], 1, "a failed attempt before phase B must not burn a number");
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
        let provider = Arc::new(MockProvider::new(vec![
            end_turn("done"),
            summary_response("s"),
            document_response(),
        ]));
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
        // exact tokens from the one scripted tool_use response
        assert_eq!(usage_rows[0].input_tokens, 100);
        assert_eq!(usage_rows[0].output_tokens, 20);
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

    /// Empty (or whitespace-only) transcripts never reach the LLM — the real
    /// Anthropic API rejects empty content blocks. The session is processed
    /// directly with a placeholder summary and zero usage.
    #[tokio::test]
    async fn empty_transcript_skips_llm_and_processes_with_placeholder() {
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.end_and_record_session(&session.id).unwrap();
        let provider = Arc::new(MockProvider::new(vec![]));
        let processor = SessionProcessor::new(
            provider.clone(),
            Arc::new(Mutex::new(store)),
            Arc::new(Mutex::new(Memory::default())),
            Arc::new(NullMemoryStore),
        );
        let outcome = processor.process(&session.id).await.unwrap();
        assert_eq!(outcome.session.status, SessionStatus::Processed);
        assert_eq!(outcome.session.summary.as_deref(), Some("(empty session)"));
        assert_eq!(outcome.usage, Usage::default());
        assert!(provider.requests().is_empty(), "no LLM calls for an empty session");
    }

    #[tokio::test]
    async fn whitespace_only_transcript_also_skips_llm() {
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        store.append_transcript(&session.id, "  \n\t  ").unwrap();
        store.end_and_record_session(&session.id).unwrap();
        let provider = Arc::new(MockProvider::new(vec![]));
        let processor = SessionProcessor::new(
            provider.clone(),
            Arc::new(Mutex::new(store)),
            Arc::new(Mutex::new(Memory::default())),
            Arc::new(NullMemoryStore),
        );
        let outcome = processor.process(&session.id).await.unwrap();
        assert_eq!(outcome.session.summary.as_deref(), Some("(empty session)"));
        assert!(provider.requests().is_empty());
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
                document_response(),
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
