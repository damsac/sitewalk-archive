//! Bridge-level e2e (Plan 07 Task 8): begin_walk -> append -> live snapshot ->
//! finish -> document payload, over the public FFI-facing surface (minus
//! actually crossing FFI — that's Task 9/11's job). Mirrors
//! `murmur-core`'s `live_extraction_e2e` one layer up. Only `ffi::` public
//! API is used (plus the `#[doc(hidden)]` test-support constructor).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use harness::{
    CompletionRequest, CompletionResponse, ContentBlock, HarnessError, LlmProvider, Memory,
    MemoryStore, MockProvider, StopReason, Usage,
};
use murmur_core::Store;

use ffi::{MurmurEngine, Providers, WalkEvent, WalkEventListener};

struct NullMemoryStore;
impl MemoryStore for NullMemoryStore {
    fn load(&self) -> Result<Memory, HarnessError> {
        Ok(Memory::default())
    }
    fn save(&self, _m: &Memory) -> Result<(), HarnessError> {
        Ok(())
    }
}

/// Collects every delivered `WalkEvent` into a shared `Vec` (std `Mutex` —
/// `on_event` is a plain sync callback, never awaited).
struct CollectingListener(StdMutex<Vec<WalkEvent>>);
impl WalkEventListener for CollectingListener {
    fn on_event(&self, event: WalkEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn tool_use(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse { id: "tu".into(), name: name.into(), input }],
        stop_reason: StopReason::ToolUse,
        usage: Usage { input_tokens: 10, output_tokens: 5 },
    }
}

fn end_turn(text: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text { text: text.into() }],
        stop_reason: StopReason::EndTurn,
        usage: Usage { input_tokens: 10, output_tokens: 5 },
    }
}

fn summary_response(text: &str) -> CompletionResponse {
    tool_use("write_summary", serde_json::json!({"summary": text}))
}

fn document_response_with_a_gap() -> CompletionResponse {
    tool_use(
        "build_document",
        serde_json::json!({
            "total_kind": "sum",
            "total_label_key": "total",
            "lines": [
                {"title": "Mulch", "qty": "3 CU YD", "amount_cents": 28500},
                {"title": "Haul & disposal", "qty": "× 1"}
            ]
        }),
    )
}

#[tokio::test]
async fn begin_append_live_snapshot_finish_document_with_gap() {
    let store = Store::open_in_memory("device-a").unwrap();
    let engine = MurmurEngine::with_providers(
        store,
        Memory::default(),
        Arc::new(NullMemoryStore),
        Providers {
            live: Arc::new(MockProvider::new(vec![
                tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
                end_turn("captured"),
            ])),
            processing: Arc::new(MockProvider::new(vec![
                tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order 12 2x10 joists"})),
                end_turn("done"),
                summary_response("Landscape walk: mulch and haul planned."),
                document_response_with_a_gap(),
            ])),
            reflection: Arc::new(MockProvider::new(vec![])),
        },
    );

    let session = engine.begin_walk(None, "landscape".into());
    let listener = Arc::new(CollectingListener(StdMutex::new(Vec::new())));
    session.clone().set_event_listener(listener.clone());

    // Default min_new_chars (120) — pad past it so the live tick fires.
    let long_text = "order twelve two by tens for the deck framing today. ".repeat(3);
    session.clone().append_transcript(long_text);

    // Poll for the live snapshot (fire-and-forget tick).
    let live_snapshot = wait_for(&listener, 1).await;
    let WalkEvent::BoardUpdated { items } = &live_snapshot[0];
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, "order lumber");

    let payload = session.finish().await;

    assert_eq!(payload.doc_kind, "estimate", "landscape template maps to the estimate doc_kind");
    assert_eq!(payload.doc_number, 1);
    assert!(!payload.queued);
    assert_eq!(payload.lines.len(), 2);
    assert_eq!(payload.lines[0].amount_cents, Some(28500));
    assert!(!payload.lines[0].is_gap);
    assert_eq!(payload.lines[1].amount_cents, None);
    assert!(payload.lines[1].is_gap, "unheard amount on a dollar template is a gap (R6)");

    // Terminal snapshot: the authoritative item replaced the live one (swap).
    let events = listener.0.lock().unwrap().clone();
    let WalkEvent::BoardUpdated { items: terminal } = events.last().unwrap();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].text, "order 12 2x10 joists");
}

/// A provider that always errors — models `finish()` on no network (D9).
struct FailingProvider;
#[async_trait::async_trait]
impl LlmProvider for FailingProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, HarnessError> {
        Err(HarnessError::Provider("network unreachable".into()))
    }
}

#[tokio::test]
async fn finish_degrades_to_a_partial_queued_document_when_offline() {
    let store = Store::open_in_memory("device-a").unwrap();
    let engine = MurmurEngine::with_providers(
        store,
        Memory::default(),
        Arc::new(NullMemoryStore),
        Providers {
            live: Arc::new(MockProvider::new(vec![
                tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
                end_turn("captured"),
            ])),
            processing: Arc::new(FailingProvider),
            reflection: Arc::new(MockProvider::new(vec![])),
        },
    );

    let session = engine.begin_walk(None, "landscape".into());
    let listener = Arc::new(CollectingListener(StdMutex::new(Vec::new())));
    session.clone().set_event_listener(listener.clone());

    let long_text = "order twelve two by tens for the deck framing today. ".repeat(3);
    session.clone().append_transcript(long_text);
    wait_for(&listener, 1).await;

    let payload = session.finish().await;

    assert!(payload.queued, "offline finish returns a queued partial document (D9)");
    assert_eq!(payload.lines.len(), 1, "built from the live board — capture is never lost");
    assert_eq!(payload.lines[0].title, "order lumber");
    assert_eq!(payload.lines[0].amount_cents, None);
    assert!(payload.lines[0].is_gap, "an offline partial document is all gaps");
}

/// A processing provider whose first call blocks on a barrier — used to
/// prove the tick/finish exclusion end-to-end through the public surface.
struct BarrierProvider {
    barrier: Arc<tokio::sync::Barrier>,
    responses: StdMutex<VecDeque<CompletionResponse>>,
    first: AtomicBool,
}
#[async_trait::async_trait]
impl LlmProvider for BarrierProvider {
    async fn complete(&self, _req: CompletionRequest) -> Result<CompletionResponse, HarnessError> {
        if self.first.swap(false, Ordering::SeqCst) {
            self.barrier.wait().await;
        }
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| HarnessError::Provider("mock script exhausted".into()))
    }
}

#[tokio::test]
async fn a_tick_mid_finish_never_observes_an_empty_board() {
    let store = Store::open_in_memory("device-a").unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let engine = MurmurEngine::with_providers(
        store,
        Memory::default(),
        Arc::new(NullMemoryStore),
        Providers {
            live: Arc::new(MockProvider::new(vec![
                tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order lumber"})),
                end_turn("captured"),
            ])),
            processing: Arc::new(BarrierProvider {
                barrier: barrier.clone(),
                responses: StdMutex::new(VecDeque::from(vec![
                    tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order 12 2x10s"})),
                    end_turn("done"),
                    summary_response("Lumber ordered."),
                    tool_use(
                        "build_document",
                        serde_json::json!({"total_kind": "sum", "total_label_key": "total", "lines": []}),
                    ),
                ])),
                first: AtomicBool::new(true),
            }),
            reflection: Arc::new(MockProvider::new(vec![])),
        },
    );

    let session = engine.begin_walk(None, "landscape".into());
    let listener = Arc::new(CollectingListener(StdMutex::new(Vec::new())));
    session.clone().set_event_listener(listener.clone());

    let long_text = "order twelve two by tens for the deck framing today. ".repeat(3);
    session.clone().append_transcript(long_text);
    wait_for(&listener, 1).await;

    let finishing = session.clone();
    let finish_task = tokio::spawn(async move { finishing.finish().await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Fires while finish() holds the extractor mutex — must queue behind it.
    session.clone().append_transcript("more talk mid finish.".into());

    barrier.wait().await;
    finish_task.await.unwrap();

    for event in listener.0.lock().unwrap().iter() {
        let WalkEvent::BoardUpdated { items } = event;
        assert!(!items.is_empty(), "no snapshot the UI can observe is ever an empty board (D3b)");
    }
}

async fn wait_for(listener: &Arc<CollectingListener>, count: usize) -> Vec<WalkEvent> {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            {
                let events = listener.0.lock().unwrap();
                if events.len() >= count {
                    return events.clone();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("event did not arrive in time")
}
