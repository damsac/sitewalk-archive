//! Live board → end-of-session swap (spec Rev 2 §2): live extraction populates
//! the board during recording; `process()` tombstones those items and
//! re-creates the authoritative set. Only public `murmur_core::` API is used.

use std::sync::{Arc, Mutex};

use harness::{
    CompletionResponse, ContentBlock, HarnessError, Memory, MemoryStore, MockProvider, StopReason,
    Usage,
};
use murmur_core::{LiveExtractOutcome, LiveExtractor, SessionProcessor, SessionStatus, Store};

struct NullMemoryStore;
impl MemoryStore for NullMemoryStore {
    fn load(&self) -> Result<Memory, HarnessError> {
        Ok(Memory::default())
    }
    fn save(&self, _m: &Memory) -> Result<(), HarnessError> {
        Ok(())
    }
}

fn tool_use(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse { id: "tu".into(), name: name.into(), input }],
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

#[tokio::test]
async fn live_board_is_swapped_by_end_of_session_processing() {
    let store = Store::open_in_memory("marcos-phone").unwrap();
    let session = store.start_session(None).unwrap();
    store
        .append_transcript(&session.id, "order twelve two-by-tens for the deck framing today")
        .unwrap();
    let sid = session.id;
    let store = Arc::new(Mutex::new(store));
    let memory = Arc::new(Mutex::new(Memory::default()));

    // Live pass: a CHEAP provider extracts one item onto the board.
    let mut extractor = LiveExtractor::new(
        Arc::new(MockProvider::new(vec![
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order twelve 2x10s"})),
            end_turn("captured"),
        ])),
        store.clone(),
        memory.clone(),
        &sid,
    );
    extractor.min_new_chars = 1;
    let outcome = extractor.maybe_extract().await.unwrap();
    assert!(matches!(outcome, LiveExtractOutcome::Extracted { items_added: 1, .. }));

    // The live board shows the item; capture its id to prove the swap.
    let live_ids: Vec<String> = {
        let s = store.lock().unwrap();
        s.list_items_for_session(&sid).unwrap().into_iter().map(|i| i.id).collect()
    };
    assert_eq!(live_ids.len(), 1);

    // End recording → queue → process with the (stronger) end-of-session provider.
    store.lock().unwrap().end_and_record_session(&sid).unwrap();
    let processor = SessionProcessor::new(
        Arc::new(MockProvider::new(vec![
            tool_use("add_item", serde_json::json!({"kind": "todo", "text": "order 12 2x10 joists"})),
            tool_use("add_item", serde_json::json!({"kind": "safety", "text": "verify ledger attachment"})),
            end_turn("done"),
            tool_use("write_summary", serde_json::json!({"summary": "Deck framing: lumber ordered."})),
        ])),
        store.clone(),
        memory.clone(),
        Arc::new(NullMemoryStore),
    );
    let processed = processor.process(&sid).await.unwrap();
    assert_eq!(processed.session.status, SessionStatus::Processed);

    // Contract: live items are tombstoned and REPLACED by the authoritative set.
    let s = store.lock().unwrap();
    let after = s.list_items_for_session(&sid).unwrap();
    assert_eq!(after.len(), 2, "authoritative pass re-created the board");
    for item in &after {
        assert!(!live_ids.contains(&item.id), "live item ids must not survive the swap");
    }
    // Both passes are billed to the session under distinct purposes (R9).
    let purposes: Vec<String> = s
        .list_llm_usage_for_session(&sid)
        .unwrap()
        .into_iter()
        .map(|u| u.purpose)
        .collect();
    assert!(purposes.contains(&"live_extraction".to_string()));
    assert!(purposes.contains(&"processing".to_string()));
}
