use std::collections::VecDeque;
use std::sync::Mutex;

use crate::error::HarnessError;
use crate::llm::{CompletionRequest, CompletionResponse, LlmProvider};

/// Scripted LlmProvider for tests: returns queued responses in order and records every request. Ships in the library so downstream crates can use it.
pub struct MockProvider {
    responses: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl MockProvider {
    pub fn new(responses: Vec<CompletionResponse>) -> Self {
        MockProvider {
            responses: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmProvider for MockProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, HarnessError> {
        self.requests.lock().unwrap().push(req);
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| HarnessError::Provider("mock script exhausted".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::*;

    fn text_response(s: &str) -> CompletionResponse {
        CompletionResponse {
            content: vec![ContentBlock::Text { text: s.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage { input_tokens: 1, output_tokens: 1 },
        }
    }

    #[tokio::test]
    async fn returns_scripted_responses_in_order_and_records_requests() {
        let mock = MockProvider::new(vec![text_response("one"), text_response("two")]);
        let req = CompletionRequest {
            system: "sys".into(),
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            max_tokens: 100,
        };
        let r1 = mock.complete(req.clone()).await.unwrap();
        let r2 = mock.complete(req.clone()).await.unwrap();
        assert_eq!(r1.content, vec![ContentBlock::Text { text: "one".into() }]);
        assert_eq!(r2.content, vec![ContentBlock::Text { text: "two".into() }]);
        assert_eq!(mock.requests().len(), 2);
        assert_eq!(mock.requests()[0].system, "sys");
    }

    #[tokio::test]
    async fn errors_when_script_is_exhausted() {
        let mock = MockProvider::new(vec![]);
        let req = CompletionRequest {
            system: String::new(),
            messages: vec![],
            tools: vec![],
            max_tokens: 1,
        };
        let err = mock.complete(req).await.unwrap_err();
        assert!(matches!(err, crate::HarnessError::Provider(_)));
    }
}
