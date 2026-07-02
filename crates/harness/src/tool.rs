use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::HarnessError;
use crate::llm::ToolSpec;

/// A capability the agent can invoke. Implementations convert their own errors into HarnessError at this boundary.
#[async_trait::async_trait]
pub trait Tool: Send + Sync + 'static {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError>;
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a tool under its name; re-registering a name replaces the previous tool.
    pub fn register(&mut self, tool: impl Tool) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    /// Tool specs for the LLM request, in alphabetical order by name (BTreeMap iteration) — deterministic across runs.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.input_schema(),
            })
            .collect()
    }

    /// Dispatches to the named tool, or returns HarnessError::UnknownTool.
    pub async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<String, HarnessError> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| HarnessError::UnknownTool(name.to_string()))?;
        tool.execute(input).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait::async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echoes the input back"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            })
        }
        async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
            Ok(input["text"].as_str().unwrap_or_default().to_string())
        }
    }

    #[tokio::test]
    async fn dispatches_by_name() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let out = reg
            .execute("echo", serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(out, "hi");
    }

    #[tokio::test]
    async fn unknown_tool_is_an_error() {
        let reg = ToolRegistry::new();
        let err = reg.execute("nope", serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, HarnessError::UnknownTool(n) if n == "nope"));
    }

    #[test]
    fn specs_lists_registered_tools() {
        let mut reg = ToolRegistry::new();
        reg.register(Echo);
        let specs = reg.specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "echo");
        assert_eq!(specs[0].description, "echoes the input back");
    }
}
