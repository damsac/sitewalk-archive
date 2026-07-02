use std::sync::Arc;

use crate::error::HarnessError;
use crate::llm::{
    CompletionRequest, ContentBlock, LlmProvider, Message, Role, ToolSpec, Usage,
};
use crate::tool::ToolRegistry;

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub system_prompt: String,
    pub max_turns: usize,
    pub max_tokens: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    /// Concatenated text of the final (tool-free) assistant response.
    pub text: String,
    /// Full transcript including tool_use/tool_result messages appended during the run.
    pub messages: Vec<Message>,
    /// Token usage accumulated across every provider call in this run.
    pub usage: Usage,
}

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: ToolRegistry,
    config: AgentConfig,
}

impl Agent {
    pub fn new(provider: Arc<dyn LlmProvider>, tools: ToolRegistry, config: AgentConfig) -> Self {
        Agent { provider, tools, config }
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools.specs()
    }

    pub async fn run(&self, mut messages: Vec<Message>) -> Result<TurnOutcome, HarnessError> {
        let mut usage = Usage::default();

        for _ in 0..self.config.max_turns {
            let response = self
                .provider
                .complete(CompletionRequest {
                    system: self.config.system_prompt.clone(),
                    messages: messages.clone(),
                    tools: self.tool_specs(),
                    max_tokens: self.config.max_tokens,
                })
                .await?;
            usage.add(&response.usage);

            let tool_uses: Vec<(String, String, serde_json::Value)> = response
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() {
                let text = response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(Message { role: Role::Assistant, content: response.content });
                return Ok(TurnOutcome { text, messages, usage });
            }

            messages.push(Message { role: Role::Assistant, content: response.content });

            let mut results = Vec::with_capacity(tool_uses.len());
            for (id, name, input) in tool_uses {
                let block = match self.tools.execute(&name, input).await {
                    Ok(content) => ContentBlock::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error: false,
                    },
                    Err(e) => ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: e.to_string(),
                        is_error: true,
                    },
                };
                results.push(block);
            }
            messages.push(Message { role: Role::User, content: results });
        }

        Err(HarnessError::MaxTurns(self.config.max_turns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::*;
    use crate::mock::MockProvider;
    use crate::tool::{Tool, ToolRegistry};
    use std::sync::{Arc, Mutex};

    fn usage1() -> Usage {
        Usage { input_tokens: 10, output_tokens: 20 }
    }

    fn text_end(s: &str) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::Text { text: s.into() }],
            stop_reason: StopReason::EndTurn,
            usage: usage1(),
        }
    }

    fn tool_call(name: &str, input: serde_json::Value) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: name.into(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage: usage1(),
        }
    }

    struct Recorder {
        calls: Arc<Mutex<Vec<serde_json::Value>>>,
        reply: Result<String, String>,
    }

    #[async_trait::async_trait]
    impl Tool for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn description(&self) -> &str {
            "records calls"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, input: serde_json::Value) -> Result<String, HarnessError> {
            self.calls.lock().unwrap().push(input);
            self.reply.clone().map_err(|m| HarnessError::Tool {
                name: "recorder".into(),
                message: m,
            })
        }
    }

    fn agent_with(
        responses: Vec<CompletionResponse>,
        tools: ToolRegistry,
    ) -> (Agent, Arc<MockProvider>) {
        let provider = Arc::new(MockProvider::new(responses));
        let agent = Agent::new(
            provider.clone(),
            tools,
            AgentConfig {
                system_prompt: "you are a field agent".into(),
                max_turns: 5,
                max_tokens: 1000,
            },
        );
        (agent, provider)
    }

    #[tokio::test]
    async fn text_only_response_ends_the_loop() {
        let (agent, provider) = agent_with(vec![text_end("done")], ToolRegistry::new());
        let out = agent.run(vec![Message::user_text("hi")]).await.unwrap();
        assert_eq!(out.text, "done");
        assert_eq!(out.usage, usage1());
        let reqs = provider.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].system, "you are a field agent");
    }

    #[tokio::test]
    async fn tool_call_executes_and_result_feeds_back() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut reg = ToolRegistry::new();
        reg.register(Recorder { calls: calls.clone(), reply: Ok("saved".into()) });

        let (agent, provider) = agent_with(
            vec![
                tool_call("recorder", serde_json::json!({"x": 1})),
                text_end("all done"),
            ],
            reg,
        );
        let out = agent.run(vec![Message::user_text("go")]).await.unwrap();

        assert_eq!(out.text, "all done");
        assert_eq!(calls.lock().unwrap().as_slice(), &[serde_json::json!({"x": 1})]);
        // usage accumulated over two provider calls
        assert_eq!(out.usage, Usage { input_tokens: 20, output_tokens: 40 });

        // second request must carry assistant tool_use then user tool_result
        let reqs = provider.requests();
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];
        let assistant = &second.messages[second.messages.len() - 2];
        let user = &second.messages[second.messages.len() - 1];
        assert_eq!(assistant.role, Role::Assistant);
        assert!(matches!(&assistant.content[0], ContentBlock::ToolUse { name, .. } if name == "recorder"));
        assert_eq!(
            user.content[0],
            ContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: "saved".into(),
                is_error: false,
            }
        );
    }

    #[tokio::test]
    async fn failing_tool_becomes_error_result_not_abort() {
        let mut reg = ToolRegistry::new();
        reg.register(Recorder {
            calls: Arc::new(Mutex::new(Vec::new())),
            reply: Err("disk full".into()),
        });
        let (agent, provider) = agent_with(
            vec![tool_call("recorder", serde_json::json!({})), text_end("recovered")],
            reg,
        );
        let out = agent.run(vec![Message::user_text("go")]).await.unwrap();
        assert_eq!(out.text, "recovered");
        let reqs = provider.requests();
        let user = reqs[1].messages.last().unwrap();
        assert!(matches!(
            &user.content[0],
            ContentBlock::ToolResult { is_error: true, content, .. } if content.contains("disk full")
        ));
    }

    #[tokio::test]
    async fn unknown_tool_becomes_error_result() {
        let (agent, provider) = agent_with(
            vec![tool_call("ghost", serde_json::json!({})), text_end("ok")],
            ToolRegistry::new(),
        );
        let out = agent.run(vec![Message::user_text("go")]).await.unwrap();
        assert_eq!(out.text, "ok");
        let reqs = provider.requests();
        let user = reqs[1].messages.last().unwrap();
        assert!(matches!(
            &user.content[0],
            ContentBlock::ToolResult { is_error: true, content, .. } if content.contains("ghost")
        ));
    }

    #[tokio::test]
    async fn max_turns_aborts() {
        let mut reg = ToolRegistry::new();
        reg.register(Recorder {
            calls: Arc::new(Mutex::new(Vec::new())),
            reply: Ok("again".into()),
        });
        // always answers with a tool call; max_turns = 5 → 5 responses then error
        let responses = (0..5)
            .map(|_| tool_call("recorder", serde_json::json!({})))
            .collect();
        let (agent, provider) = agent_with(responses, reg);
        let err = agent.run(vec![Message::user_text("go")]).await.unwrap_err();
        assert!(matches!(err, HarnessError::MaxTurns(5)));
        assert_eq!(provider.requests().len(), 5);
    }
}
