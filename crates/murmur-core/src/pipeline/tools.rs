//! Vocational tools (spec §4): thin adapters from the harness Tool trait onto
//! the Store's writer API. Tools capture their handles (Arc) — no context
//! parameter on execute (Plan 01 review decision). The std Mutex is never
//! held across an await.

use std::sync::{Arc, Mutex};

use harness::{HarnessError, Tool};

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
}

impl AddItemTool {
    pub fn new(store: Arc<Mutex<Store>>, session_id: &str) -> Self {
        AddItemTool { store, session_id: session_id.to_string() }
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
        lock(&self.store, "add_item")?
            .add_item(&self.session_id, kind, text)
            .map_err(|e| tool_err("add_item", e.to_string()))?;
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
