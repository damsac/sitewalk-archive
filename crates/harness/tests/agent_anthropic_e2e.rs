use std::sync::Arc;

use harness::{
    Agent, AgentConfig, AnthropicProvider, ContentBlock, HarnessError, Message, Tool,
    ToolRegistry,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

struct SaveItem;

#[async_trait::async_trait]
impl Tool for SaveItem {
    fn name(&self) -> &str {
        "save_item"
    }
    fn description(&self) -> &str {
        "saves a captured item"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"]
        })
    }
    async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
        Ok(format!("saved: {}", input["title"].as_str().unwrap_or("?")))
    }
}

struct Script;

impl Respond for Script {
    fn respond(&self, req: &wiremock::Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&req.body)
            .expect("Script: failed to parse request body as JSON");
        let has_tool_result = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .any(|b| b["type"] == "tool_result");

        if has_tool_result {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "logged the mulch"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 30, "output_tokens": 10}
            }))
        } else {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "tool_use", "id": "tu_1", "name": "save_item",
                             "input": {"title": "bark mulch — front beds"}}],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 20, "output_tokens": 15}
            }))
        }
    }
}

#[tokio::test]
async fn full_round_trip_through_real_provider_wire_format() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(Script)
        .expect(2)
        .mount(&server)
        .await;

    let provider =
        AnthropicProvider::new("sk-test", "claude-haiku-4-5-20251001").with_base_url(server.uri());
    let mut tools = ToolRegistry::new();
    tools.register(SaveItem);

    let agent = Agent::new(
        Arc::new(provider),
        tools,
        AgentConfig {
            system_prompt: "extract items from field transcripts".into(),
            max_turns: 4,
            max_tokens: 512,
        },
    );

    let out = agent
        .run(vec![Message::user_text("front beds need mulch, call it three yards")])
        .await
        .unwrap();

    assert_eq!(out.text, "logged the mulch");
    assert_eq!(out.usage.input_tokens, 50);
    assert_eq!(out.usage.output_tokens, 25);
    assert!(out
        .messages
        .iter()
        .any(|m| m.content.iter().any(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content == "saved: bark mulch — front beds"))));
}
