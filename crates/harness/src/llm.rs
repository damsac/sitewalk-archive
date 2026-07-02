use serde::{Deserialize, Serialize};

use crate::error::HarnessError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompletionRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, HarnessError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_serializes_to_anthropic_wire_shape() {
        let block = ContentBlock::ToolUse {
            id: "tu_1".into(),
            name: "create_item".into(),
            input: serde_json::json!({"title": "mulch beds"}),
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "tool_use");
        assert_eq!(v["name"], "create_item");
        let back: ContentBlock = serde_json::from_value(v).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn tool_result_round_trips() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "tu_1".into(),
            content: "ok".into(),
            is_error: false,
        };
        let v = serde_json::to_value(&block).unwrap();
        assert_eq!(v["type"], "tool_result");
        let back: ContentBlock = serde_json::from_value(v).unwrap();
        assert_eq!(back, block);
    }

    #[test]
    fn usage_adds() {
        let mut u = Usage { input_tokens: 10, output_tokens: 5 };
        u.add(&Usage { input_tokens: 3, output_tokens: 7 });
        assert_eq!(u, Usage { input_tokens: 13, output_tokens: 12 });
    }
}
