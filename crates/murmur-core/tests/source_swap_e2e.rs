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
