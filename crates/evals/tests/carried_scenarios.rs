//! Carried from the Plan 05 final review: characterization tests that PIN known
//! gaps (not fixes). Each documents current behavior so a later plan's fix has a
//! baseline to move. All hermetic (MockProvider).

use std::sync::{Arc, Mutex};

use harness::{CompletionResponse, ContentBlock, MockProvider, Memory, StopReason, Usage};
use murmur_core::{ItemSource, LiveExtractor, SessionProcessor, SessionStatus, Store};
// `evals::run::NullMemoryStore` is public (Task 6 Step 1) — reuse it, don't redeclare.

fn tool_use(name: &str, input: serde_json::Value) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::ToolUse { id: "tu".into(), name: name.into(), input }],
        stop_reason: StopReason::ToolUse,
        usage: Usage { input_tokens: 30, output_tokens: 8 },
    }
}

fn end_turn(t: &str) -> CompletionResponse {
    CompletionResponse {
        content: vec![ContentBlock::Text { text: t.into() }],
        stop_reason: StopReason::EndTurn,
        usage: Usage { input_tokens: 10, output_tokens: 2 },
    }
}

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
    assert_eq!(after[0].source, ItemSource::Live);
    assert_eq!(store.lock().unwrap().get_session(&sid).unwrap().status, SessionStatus::Failed);
}

/// 4b — restart-mid-session at a large item count. A fresh LiveExtractor starts
/// at cursor 0; the already-captured dedup list is budget-capped
/// (`already_captured_budget_tokens` = 400 tokens ⇒ `budget_chars(400)` = 1600
/// chars), and `format_already_captured` renders newest-first, so the assembler
/// truncates the OLDEST lines. An item that has scrolled out of that window is
/// INVISIBLE to the restart pass — the dedup blindspot — so the model re-adds it.
///
/// Budget arithmetic (why 80 items, not 64): each line "- [todo] task number NN"
/// is ~23 chars; N items render ≈ 23·N + (N−1) chars. At **64** items that is
/// ≈ 1_575 chars — UNDER the 1_600-char budget, so NOTHING is evicted and the
/// test would be vacuous (the reviewer's finding). At **80** items it is ≈ 1_930
/// chars — comfortably over budget — so the tail (oldest) is cut. The oldest item
/// is given a unique marker so the assertions can't alias another line.
///
/// CHARACTERIZATION for the Plan 06 dedup fix. Not a fix.
#[tokio::test]
async fn restart_after_many_items_re_adds_an_evicted_item() {
    let store = Store::open_in_memory("dev").unwrap();
    let session = store.start_session(None).unwrap();
    store.append_transcript(&session.id, "long live session, eighty-plus captured tasks and more talk").unwrap();
    let sid = session.id.clone();
    // Oldest item first (insertion = id order); unique marker so no substring alias.
    store.add_item(&sid, "todo", "oldest evicted marker task").unwrap();
    for n in 1..80 { store.add_item(&sid, "todo", &format!("task number {n:02}")).unwrap(); }
    let store = Arc::new(Mutex::new(store));

    // Fresh extractor (app restart) at cursor 0. Because the oldest item is no
    // longer in the (truncated) already-captured list, the pass re-extracts it.
    let provider = Arc::new(MockProvider::new(vec![
        tool_use("add_item", serde_json::json!({"kind":"todo","text":"oldest evicted marker task"})),
        end_turn("re-captured what looked new"),
    ]));
    let mut live = LiveExtractor::new(
        provider.clone(), store.clone(), Arc::new(Mutex::new(Memory::default())), &sid);
    live.min_new_chars = 1;
    live.maybe_extract().await.unwrap();

    // PRIMARY — the blindspot itself: assert against what was actually SENT. The
    // oldest item's text is ABSENT from the already-captured section of the
    // request (evicted by budget truncation) even though it IS on the board; a
    // recent item survives. This is the defect, independent of add_item dedup.
    let reqs = provider.requests();
    let user_text = match &reqs[0].messages[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        other => panic!("expected user text block, got {other:?}"),
    };
    assert!(user_text.contains("already captured"), "the dedup section is present");
    assert!(user_text.contains("task number 79"), "a recent item survives truncation");
    assert!(
        !user_text.contains("oldest evicted marker task"),
        "oldest item was evicted from the dedup window — the restart blindspot"
    );

    // SECONDARY consequence: the board now holds the oldest item twice.
    let items = store.lock().unwrap().list_items_for_session(&sid).unwrap();
    let dupes = items.iter().filter(|i| i.text == "oldest evicted marker task").count();
    assert_eq!(dupes, 2, "the evicted item was re-added → duplicate on the board");
}

/// 4c — long single-tick transcript exceeding the live window budget. One
/// maybe_extract() covers only its clamped window; the cursor advances by the
/// window, so catching up to the full transcript takes MULTIPLE passes.
/// CHARACTERIZATION: assert multi-pass catch-up (cursor < len after one pass,
/// reaches len after enough passes).
#[tokio::test]
async fn long_transcript_needs_multiple_live_passes_to_catch_up() {
    let store = Store::open_in_memory("dev").unwrap();
    let session = store.start_session(None).unwrap();
    let big = "word ".repeat(4000); // ~20k chars, well over the 2000-token window
    store.append_transcript(&session.id, &big).unwrap();
    let sid = session.id.clone();
    let total_chars = big.chars().count();
    let store = Arc::new(Mutex::new(store));

    // Enough end_turn responses for several passes.
    let responses: Vec<_> = (0..8).map(|_| end_turn("nothing new")).collect();
    let mut live = LiveExtractor::new(Arc::new(MockProvider::new(responses)),
        store.clone(), Arc::new(Mutex::new(Memory::default())), &sid);
    live.min_new_chars = 1;

    live.maybe_extract().await.unwrap();
    assert!(live.cursor() < total_chars, "one pass covers only the clamped window");
    // Drive to catch-up.
    let mut passes = 1;
    while live.cursor() < total_chars && passes < 8 {
        live.maybe_extract().await.unwrap();
        passes += 1;
    }
    assert_eq!(live.cursor(), total_chars, "cursor reaches transcript end after multi-pass catch-up");
    assert!(passes > 1, "a window-exceeding transcript required {passes} passes");
}
