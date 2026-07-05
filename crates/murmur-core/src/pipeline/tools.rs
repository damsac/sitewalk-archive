//! Vocational tools (spec §4): thin adapters from the harness Tool trait onto
//! the Store's writer API. Tools capture their handles (Arc) — no context
//! parameter on execute (Plan 01 review decision). The std Mutex is never
//! held across an await.

use std::sync::{Arc, Mutex};

use harness::{HarnessError, Tool};

use crate::domain::SessionStatus;
use crate::store::Store;

fn tool_err(name: &str, message: impl Into<String>) -> HarnessError {
    HarnessError::Tool { name: name.into(), message: message.into() }
}

fn lock<'a>(
    store: &'a Arc<Mutex<Store>>,
    tool: &str,
) -> Result<std::sync::MutexGuard<'a, Store>, HarnessError> {
    store.lock().map_err(|_| tool_err(tool, "store lock poisoned"))
}

fn req_str<'a>(input: &'a serde_json::Value, key: &str, tool: &str) -> Result<&'a str, HarnessError> {
    match input.get(key) {
        None => Err(tool_err(tool, format!("missing '{key}'"))),
        Some(v) => v
            .as_str()
            .ok_or_else(|| tool_err(tool, format!("'{key}' must be a string"))),
    }
}

/// Required string that must also be non-empty after trimming.
fn req_nonempty_str<'a>(
    input: &'a serde_json::Value,
    key: &str,
    tool: &str,
) -> Result<&'a str, HarnessError> {
    let value = req_str(input, key, tool)?;
    if value.trim().is_empty() {
        return Err(tool_err(tool, format!("'{key}' must not be empty")));
    }
    Ok(value)
}

const VALID_KINDS: [&str; 6] = ["todo", "decision", "note", "safety", "part", "price"];

pub struct AddItemTool {
    store: Arc<Mutex<Store>>,
    session_id: String,
    source: crate::domain::ItemSource,
    /// When set, `execute` only writes if the session's CURRENT status
    /// matches — checked atomically with the insert (see
    /// `Store::add_item_if_status`). Used by `LiveExtractor` to close the
    /// status-gate TOCTOU: a live pass that outlives end-of-session
    /// processing must not insert a stale item (pipeline::live module docs).
    required_status: Option<SessionStatus>,
    /// When set, each inserted item's id is pushed here so the end-of-session
    /// swap knows what THIS run created (Plan 06a design problem 1).
    created_ids: Option<Arc<Mutex<Vec<String>>>>,
}

impl AddItemTool {
    /// Manual, ungated write (source=Manual). Kept for direct/test use.
    pub fn new(store: Arc<Mutex<Store>>, session_id: &str) -> Self {
        AddItemTool {
            store,
            session_id: session_id.to_string(),
            source: crate::domain::ItemSource::Manual,
            required_status: None,
            created_ids: None,
        }
    }

    /// Live in-session write (source=Live), gated to `Recording`.
    pub fn live(store: Arc<Mutex<Store>>, session_id: &str) -> Self {
        AddItemTool {
            store,
            session_id: session_id.to_string(),
            source: crate::domain::ItemSource::Live,
            required_status: Some(SessionStatus::Recording),
            created_ids: None,
        }
    }

    /// Authoritative processing write (source=Authoritative), ungated, records
    /// each new id into `created_ids` for the finish swap.
    pub fn authoritative(
        store: Arc<Mutex<Store>>,
        session_id: &str,
        created_ids: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        AddItemTool {
            store,
            session_id: session_id.to_string(),
            source: crate::domain::ItemSource::Authoritative,
            required_status: None,
            created_ids: Some(created_ids),
        }
    }
}

#[async_trait::async_trait]
impl Tool for AddItemTool {
    fn name(&self) -> &str {
        "add_item"
    }

    fn description(&self) -> &str {
        "Record one clearly-stated item from the session. Only extract what was actually said — fewer, confident items beat many guessed ones. Never invent assignees, prices, or dates."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string", "enum": ["todo", "decision", "note", "safety", "part", "price"], "minLength": 1 },
                "text": { "type": "string", "minLength": 1, "description": "one short item, in the speaker's own terms" }
            },
            "required": ["kind", "text"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
        let kind = req_str(&input, "kind", "add_item")?;
        if !VALID_KINDS.contains(&kind) {
            return Err(tool_err(
                "add_item",
                format!("invalid kind '{kind}'; must be one of: {}", VALID_KINDS.join(", ")),
            ));
        }
        let text = req_nonempty_str(&input, "text", "add_item")?;
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
            sink.lock()
                .map_err(|_| tool_err("add_item", "created-ids lock poisoned"))?
                .push(item.id.clone());
        }
        Ok(format!("added {kind}: {text}"))
    }
}

pub struct UpsertContactTool {
    store: Arc<Mutex<Store>>,
}

impl UpsertContactTool {
    pub fn new(store: Arc<Mutex<Store>>) -> Self {
        UpsertContactTool { store }
    }
}

#[async_trait::async_trait]
impl Tool for UpsertContactTool {
    fn name(&self) -> &str {
        "upsert_contact"
    }

    fn description(&self) -> &str {
        "Save or update a person mentioned in the session (sub, client, supplier). Match is by exact name; omit fields you don't know rather than guessing."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "minLength": 1 },
                "trade": { "type": "string" },
                "phone": { "type": "string" },
                "notes": { "type": "string" }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
        let name = req_nonempty_str(&input, "name", "upsert_contact")?;
        let trade = input["trade"].as_str();
        let phone = input["phone"].as_str();
        let notes = input["notes"].as_str();
        lock(&self.store, "upsert_contact")?
            .upsert_contact(name, trade, phone, notes)
            .map_err(|e| tool_err("upsert_contact", e.to_string()))?;
        Ok(format!("contact saved: {name}"))
    }
}

pub struct WriteReportTool {
    store: Arc<Mutex<Store>>,
    session_id: String,
}

impl WriteReportTool {
    pub fn new(store: Arc<Mutex<Store>>, session_id: &str) -> Self {
        WriteReportTool { store, session_id: session_id.to_string() }
    }
}

#[async_trait::async_trait]
impl Tool for WriteReportTool {
    fn name(&self) -> &str {
        "write_report"
    }

    fn description(&self) -> &str {
        "Write the session report (markdown). Call at most once, and only when the session has enough substance to report on."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "minLength": 1 },
                "body": { "type": "string", "minLength": 1, "description": "markdown" }
            },
            "required": ["title", "body"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
        let title = req_nonempty_str(&input, "title", "write_report")?;
        let body = req_str(&input, "body", "write_report")?;
        lock(&self.store, "write_report")?
            .add_artifact(&self.session_id, "report", title, body)
            .map_err(|e| tool_err("write_report", e.to_string()))?;
        Ok(format!("report written: {title}"))
    }
}

/// Builds the structured, display-copy-free document artifact (Plan 07 D2).
/// Emits `lines` with `amount_cents` only for amounts actually spoken (R6) —
/// everything unheard is a gap. `is_gap` is template-aware (D2a), NOT derived
/// from `amount_cents == None`: on a dollar template (`estimate`) an unheard
/// amount defaults to a gap when the model omits `is_gap`; on `report`/
/// `inspection` the model's explicit `is_gap` is honored and a normal
/// non-dollar line (an "OK" row, a §-section finding) defaults to NOT a gap.
pub struct BuildDocumentTool {
    store: Arc<Mutex<Store>>,
    session_id: String,
    doc_kind: String,
    /// A previously-minted number to reuse (idempotent re-process, D5). `None`
    /// means "mint one" — which happens atomically with the artifact write in
    /// `execute` (carry-note 1 follow-up: a number is durably consumed if and
    /// only if the document artifact lands).
    existing_doc_number: Option<u64>,
}

impl BuildDocumentTool {
    /// The forced tool name. Exposed as an associated const so the processor
    /// can build the `ToolSpec` (name/description/schema — none of which depend
    /// on the document number) BEFORE minting a number (D5: mint only when a
    /// document is actually about to be written).
    pub const NAME: &'static str = "build_document";

    pub fn new(
        store: Arc<Mutex<Store>>,
        session_id: &str,
        doc_kind: &str,
        existing_doc_number: Option<u64>,
    ) -> Self {
        BuildDocumentTool {
            store,
            session_id: session_id.to_string(),
            doc_kind: doc_kind.to_string(),
            existing_doc_number,
        }
    }

    /// Number-independent description — see [`NAME`](Self::NAME).
    pub fn description_str() -> &'static str {
        "Build the structured job document from this session. Put an amount only on a line \
         whose number was actually spoken — never guess. If a quantity or price was not said, \
         omit amount_cents. On a priced template an unheard amount is a gap; on a report or \
         inspection, only mark a line a gap when it was genuinely left open — a normal 'OK' row \
         or a section finding with no dollar figure is not a gap."
    }

    /// Number-independent input schema — see [`NAME`](Self::NAME).
    pub fn input_schema_json() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "total_kind": { "type": "string", "enum": ["sum", "static"] },
                "total_label_key": { "type": "string", "minLength": 1 },
                "static_total_cents": { "type": "integer" },
                "lines": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "minLength": 1 },
                            "detail": { "type": "string" },
                            "qty": { "type": "string" },
                            "amount_cents": { "type": "integer" },
                            "section": { "type": "string" },
                            "is_gap": { "type": "boolean" }
                        },
                        "required": ["title"]
                    }
                }
            },
            "required": ["total_kind", "total_label_key", "lines"]
        })
    }
}

#[async_trait::async_trait]
impl Tool for BuildDocumentTool {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        Self::description_str()
    }

    fn input_schema(&self) -> serde_json::Value {
        Self::input_schema_json()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
        let total_kind = req_nonempty_str(&input, "total_kind", "build_document")?;
        let total_label_key = req_nonempty_str(&input, "total_label_key", "build_document")?;
        let static_total_cents = input.get("static_total_cents").and_then(|v| v.as_i64());
        let lines_in = input
            .get("lines")
            .and_then(|v| v.as_array())
            .ok_or_else(|| tool_err("build_document", "missing 'lines'"))?;

        let mut lines = Vec::with_capacity(lines_in.len());
        for (idx, line) in lines_in.iter().enumerate() {
            let title = line
                .get("title")
                .and_then(|v| v.as_str())
                .ok_or_else(|| tool_err("build_document", format!("lines[{idx}] missing 'title'")))?;
            let detail = line.get("detail").and_then(|v| v.as_str()).unwrap_or("");
            let qty = line.get("qty").and_then(|v| v.as_str()).unwrap_or("");
            let amount_cents = line.get("amount_cents").and_then(|v| v.as_i64());
            let section = line.get("section").and_then(|v| v.as_str());
            let is_gap = match line.get("is_gap").and_then(|v| v.as_bool()) {
                Some(explicit) => explicit,
                // D2a: only the dollar template auto-derives a gap from a missing
                // amount. report/inspection lines default to NOT a gap — a normal
                // "OK" row or a §-finding with no dollar figure is not a gap.
                None => self.doc_kind == "estimate" && amount_cents.is_none(),
            };
            lines.push(serde_json::json!({
                "id": crate::ids::new_id(),
                "title": title,
                "detail": detail,
                "qty": qty,
                "amount_cents": amount_cents,
                "section": section,
                "is_gap": is_gap,
            }));
        }

        let guard = lock(&self.store, "build_document")?;
        let session = guard
            .get_session(&self.session_id)
            .map_err(|e| tool_err("build_document", e.to_string()))?;
        // Payload WITHOUT doc_number — the store stamps it inside the same
        // transaction that mints it, so mint + write succeed or neither does
        // (carry-note 1 follow-up). All validation above ran before any mint.
        let payload = serde_json::json!({
            "doc_kind": self.doc_kind,
            "job_date_unix": session.started_at,
            "total_kind": total_kind,
            "total_label_key": total_label_key,
            "static_total_cents": static_total_cents,
            "lines": lines,
            "queued": false,
        });
        let artifact = guard
            .mint_document_number_and_add_artifact(
                &self.session_id,
                &self.doc_kind,
                self.existing_doc_number,
                payload,
            )
            .map_err(|e| tool_err("build_document", e.to_string()))?;
        Ok(format!("document built: {}", artifact.title))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use harness::{HarnessError, Tool};

    use crate::store::Store;

    fn shared_store_with_session() -> (Arc<Mutex<Store>>, String) {
        let store = Store::open_in_memory("device-a").unwrap();
        let session = store.start_session(None).unwrap();
        (Arc::new(Mutex::new(store)), session.id)
    }

    #[tokio::test]
    async fn add_item_writes_through_store() {
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::new(store.clone(), &sid);
        let out = tool
            .execute(serde_json::json!({"kind": "todo", "text": "order lumber"}))
            .await
            .unwrap();
        assert_eq!(out, "added todo: order lumber");
        let items = store.lock().unwrap().list_items_for_session(&sid).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].kind, "todo");
    }

    #[tokio::test]
    async fn add_item_rejects_bad_input() {
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::new(store, &sid);
        let err = tool.execute(serde_json::json!({"kind": "todo"})).await.unwrap_err();
        assert!(matches!(err, HarnessError::Tool { .. }));
    }

    #[tokio::test]
    async fn upsert_contact_writes_through_store() {
        let (store, _sid) = shared_store_with_session();
        let tool = super::UpsertContactTool::new(store.clone());
        let out = tool
            .execute(serde_json::json!({"name": "Dev", "trade": "framer"}))
            .await
            .unwrap();
        assert_eq!(out, "contact saved: Dev");
        let contacts = store.lock().unwrap().list_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].trade.as_deref(), Some("framer"));
    }

    #[tokio::test]
    async fn write_report_creates_artifact() {
        let (store, sid) = shared_store_with_session();
        let tool = super::WriteReportTool::new(store.clone(), &sid);
        let out = tool
            .execute(serde_json::json!({"title": "Johnson walk", "body": "## Summary\nDeck."}))
            .await
            .unwrap();
        assert_eq!(out, "report written: Johnson walk");
        let artifacts = store.lock().unwrap().list_artifacts_for_session(&sid).unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].kind, "report");
    }

    #[tokio::test]
    async fn store_errors_surface_as_tool_errors() {
        let store = Arc::new(Mutex::new(Store::open_in_memory("device-a").unwrap()));
        let tool = super::AddItemTool::new(store, "no-such-session");
        let err = tool
            .execute(serde_json::json!({"kind": "todo", "text": "x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::Tool { .. }));
    }

    #[tokio::test]
    async fn wrong_typed_field_names_the_type_error() {
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::new(store, &sid);
        let err = tool
            .execute(serde_json::json!({"kind": 42, "text": "x"}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HarnessError::Tool { message, .. } if message.contains("must be a string")),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn invalid_kind_names_the_valid_kinds() {
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::new(store.clone(), &sid);
        let err = tool
            .execute(serde_json::json!({"kind": "vibe", "text": "x"}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HarnessError::Tool { message, .. }
                if message.contains("todo") && message.contains("price")),
            "error should name the valid kinds, got: {err}"
        );
        assert!(store.lock().unwrap().list_items_for_session(&sid).unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_text_is_rejected() {
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::new(store.clone(), &sid);
        let err = tool
            .execute(serde_json::json!({"kind": "todo", "text": "  "}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HarnessError::Tool { message, .. } if message.contains("must not be empty")),
            "got: {err}"
        );
        assert!(store.lock().unwrap().list_items_for_session(&sid).unwrap().is_empty());
    }

    #[tokio::test]
    async fn gated_add_item_writes_when_status_matches() {
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::live(store.clone(), &sid);
        let out = tool
            .execute(serde_json::json!({"kind": "todo", "text": "order lumber"}))
            .await
            .unwrap();
        assert_eq!(out, "added todo: order lumber");
        assert_eq!(store.lock().unwrap().list_items_for_session(&sid).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gated_add_item_errors_and_writes_nothing_when_status_changed() {
        let (store, sid) = shared_store_with_session();
        store.lock().unwrap().end_and_record_session(&sid).unwrap(); // Recording -> AwaitingProcessing
        let tool = super::AddItemTool::live(store.clone(), &sid);
        let err = tool
            .execute(serde_json::json!({"kind": "todo", "text": "order lumber"}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HarnessError::Tool { message, .. } if message.contains("no longer recording")),
            "got: {err}"
        );
        assert!(store.lock().unwrap().list_items_for_session(&sid).unwrap().is_empty());
    }

    #[tokio::test]
    async fn live_tool_writes_live_source_when_recording() {
        use crate::domain::ItemSource;
        let (store, sid) = shared_store_with_session();
        let tool = super::AddItemTool::live(store.clone(), &sid);
        tool.execute(serde_json::json!({"kind":"todo","text":"order lumber"})).await.unwrap();
        let items = store.lock().unwrap().list_items_for_session(&sid).unwrap();
        assert_eq!(items[0].source, ItemSource::Live);
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

    /// The number and the artifact are one transaction (carry-note 1 follow-up):
    /// a payload that fails validation must not durably consume a document
    /// number — the mint happens only inside the validated write path.
    #[tokio::test]
    async fn malformed_build_document_payload_does_not_burn_a_number() {
        let (store, sid) = shared_store_with_session();
        let tool = super::BuildDocumentTool::new(store.clone(), &sid, "estimate", None);
        // missing total_kind -> validation error, no mint
        let err = tool
            .execute(serde_json::json!({"total_label_key": "total", "lines": []}))
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::Tool { .. }));
        assert_eq!(
            store.lock().unwrap().mint_document_number("estimate").unwrap(),
            1,
            "the failed build must not have consumed a number"
        );
    }

    #[tokio::test]
    async fn build_document_mints_when_no_number_exists() {
        let (store, sid) = shared_store_with_session();
        let tool = super::BuildDocumentTool::new(store.clone(), &sid, "estimate", None);
        tool.execute(serde_json::json!({
            "total_kind": "sum", "total_label_key": "total",
            "lines": [{"title": "Mulch", "amount_cents": 28500}]
        }))
        .await
        .unwrap();
        let store = store.lock().unwrap();
        let doc = store.latest_document_artifact(&sid).unwrap().unwrap();
        let v: serde_json::Value = serde_json::from_str(&doc.body).unwrap();
        assert_eq!(v["doc_number"], 1, "first mint for this kind");
        assert_eq!(store.mint_document_number("estimate").unwrap(), 2, "sequence advanced");
    }

    #[tokio::test]
    async fn build_document_writes_structured_json_artifact_with_gaps() {
        let (store, sid) = shared_store_with_session();
        let tool = super::BuildDocumentTool::new(store.clone(), &sid, "estimate", Some(47));
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
        let tool = super::BuildDocumentTool::new(store.clone(), &sid, "inspection", Some(389));
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

    #[tokio::test]
    async fn empty_name_and_title_are_rejected() {
        let (store, sid) = shared_store_with_session();
        let contact = super::UpsertContactTool::new(store.clone());
        let err = contact.execute(serde_json::json!({"name": ""})).await.unwrap_err();
        assert!(
            matches!(&err, HarnessError::Tool { message, .. } if message.contains("must not be empty"))
        );
        let report = super::WriteReportTool::new(store, &sid);
        let err = report
            .execute(serde_json::json!({"title": " ", "body": "b"}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, HarnessError::Tool { message, .. } if message.contains("must not be empty"))
        );
    }
}
