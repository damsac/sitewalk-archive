use std::sync::{Arc, Mutex};

use crate::error::HarnessError;
use crate::memory::store::MemoryStore;
use crate::memory::{FactSource, Memory};
use crate::tool::Tool;

/// Injectable clock (unix seconds) so tests are deterministic.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// In-session memory updates (spec §7): the agent remembers corrections and new
/// facts as they happen. Every mutation is persisted immediately via the store.
pub struct UpdateMemoryTool {
    memory: Arc<Mutex<Memory>>,
    store: Arc<dyn MemoryStore>,
    clock: Clock,
    session: Option<String>,
}

impl UpdateMemoryTool {
    pub fn new(memory: Arc<Mutex<Memory>>, store: Arc<dyn MemoryStore>) -> Self {
        Self::with_clock(memory, store, Arc::new(system_clock))
    }

    pub fn with_clock(
        memory: Arc<Mutex<Memory>>,
        store: Arc<dyn MemoryStore>,
        clock: Clock,
    ) -> Self {
        UpdateMemoryTool { memory, store, clock, session: None }
    }

    /// Tags every fact this tool records with a session id.
    pub fn for_session(mut self, id: impl Into<String>) -> Self {
        self.session = Some(id.into());
        self
    }

    fn err(message: impl Into<String>) -> HarnessError {
        HarnessError::Tool { name: "update_memory".into(), message: message.into() }
    }
}

#[async_trait::async_trait]
impl Tool for UpdateMemoryTool {
    fn name(&self) -> &str {
        "update_memory"
    }

    fn description(&self) -> &str {
        "Remember or forget one fact about the user, their people, projects, vocabulary, or preferences. Use for corrections and durable facts, not session content."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["remember", "forget"] },
                "section": { "type": "string", "description": "e.g. vocabulary, people, projects, preferences" },
                "text": { "type": "string", "description": "one short fact" },
                "source": { "type": "string", "enum": ["stated", "inferred", "corrected"], "description": "how you know this; default inferred" }
            },
            "required": ["op", "section", "text"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
        let op = input["op"].as_str().ok_or_else(|| Self::err("missing 'op'"))?;
        let section = input["section"].as_str().ok_or_else(|| Self::err("missing 'section'"))?;
        let text = input["text"].as_str().ok_or_else(|| Self::err("missing 'text'"))?;
        let source = match input.get("source").and_then(|s| s.as_str()) {
            None => FactSource::Inferred,
            Some("inferred") => FactSource::Inferred,
            Some("stated") => FactSource::Stated,
            Some("corrected") => FactSource::Corrected,
            Some(other) => return Err(Self::err(format!("unknown source: {other}"))),
        };

        let snapshot = {
            let mut mem = self.memory.lock().map_err(|_| Self::err("memory lock poisoned"))?;
            match op {
                "remember" => {
                    mem.remember_from(section, text, (self.clock)(), source, self.session.clone());
                }
                "forget" => {
                    if !mem.forget(section, text) {
                        return Err(Self::err(format!("no entry in {section} matching: {text}")));
                    }
                }
                other => return Err(Self::err(format!("unknown op: {other}"))),
            }
            mem.clone()
        };
        self.store.save(&snapshot)?;

        Ok(match op {
            "remember" => format!("remembered in {section}: {text}"),
            _ => format!("forgot from {section}: {text}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{FactSource, Memory};
    use crate::tool::Tool;
    use std::sync::Mutex as StdMutex;

    /// In-memory store that records saves.
    struct SpyStore {
        saved: StdMutex<Vec<Memory>>,
    }

    impl SpyStore {
        fn new() -> Arc<Self> {
            Arc::new(SpyStore { saved: StdMutex::new(Vec::new()) })
        }
    }

    impl MemoryStore for SpyStore {
        fn load(&self) -> Result<Memory, HarnessError> {
            Ok(Memory::default())
        }
        fn save(&self, memory: &Memory) -> Result<(), HarnessError> {
            self.saved.lock().unwrap().push(memory.clone());
            Ok(())
        }
    }

    fn tool_with(store: Arc<SpyStore>) -> (UpdateMemoryTool, Arc<Mutex<Memory>>) {
        let memory = Arc::new(Mutex::new(Memory::default()));
        let tool = UpdateMemoryTool::with_clock(memory.clone(), store, Arc::new(|| 777));
        (tool, memory)
    }

    #[tokio::test]
    async fn remember_mutates_and_persists() {
        let store = SpyStore::new();
        let (tool, memory) = tool_with(store.clone());
        let out = tool
            .execute(serde_json::json!({"op": "remember", "section": "people", "text": "Dev — framer"}))
            .await
            .unwrap();
        assert_eq!(out, "remembered in people: Dev — framer");
        let m = memory.lock().unwrap();
        assert_eq!(m.sections["people"][0].last_touched, 777);
        assert_eq!(m.sections["people"][0].source, FactSource::Inferred);
        assert_eq!(store.saved.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn source_and_session_are_recorded() {
        let store = SpyStore::new();
        let memory = Arc::new(Mutex::new(Memory::default()));
        let tool = UpdateMemoryTool::with_clock(memory.clone(), store, Arc::new(|| 5))
            .for_session("s42");
        tool.execute(serde_json::json!({"op": "remember", "section": "people", "text": "Dev", "source": "corrected"}))
            .await
            .unwrap();
        let m = memory.lock().unwrap();
        assert_eq!(m.sections["people"][0].source, FactSource::Corrected);
        assert_eq!(m.sections["people"][0].session.as_deref(), Some("s42"));
    }

    #[tokio::test]
    async fn forget_removes_or_errors() {
        let store = SpyStore::new();
        let (tool, _memory) = tool_with(store.clone());
        tool.execute(serde_json::json!({"op": "remember", "section": "people", "text": "Dave"}))
            .await
            .unwrap();
        let out = tool
            .execute(serde_json::json!({"op": "forget", "section": "people", "text": "Dave"}))
            .await
            .unwrap();
        assert_eq!(out, "forgot from people: Dave");
        let err = tool
            .execute(serde_json::json!({"op": "forget", "section": "people", "text": "Dave"}))
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::Tool { .. }));
    }

    #[tokio::test]
    async fn bad_input_is_a_tool_error() {
        let store = SpyStore::new();
        let (tool, _memory) = tool_with(store);
        let err = tool
            .execute(serde_json::json!({"op": "explode", "section": "x", "text": "y"}))
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::Tool { .. }));
        let err = tool.execute(serde_json::json!({"op": "remember"})).await.unwrap_err();
        assert!(matches!(err, HarnessError::Tool { .. }));
        let err = tool
            .execute(serde_json::json!({"op": "remember", "section": "x", "text": "y", "source": "psychic"}))
            .await
            .unwrap_err();
        assert!(matches!(err, HarnessError::Tool { .. }));
    }
}
