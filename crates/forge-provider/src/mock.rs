//! A deterministic provider for offline tests and the walking skeleton.
//!
//! Behaviour: on the first turn it asks to read `Cargo.toml` via a `read_file` tool call;
//! once it sees the tool result in the transcript, it returns a final text answer. This
//! exercises the full agent loop (route -> model -> tool -> model -> done) with no network.

use async_trait::async_trait;
use forge_types::{new_id, Message, Role, ToolCall, Usage};
use serde_json::json;

use crate::{EventSink, ModelResponse, Provider, ProviderError, StreamEvent, ToolSpec};

#[derive(Debug, Default)]
pub struct MockProvider;

/// Emit `text` to the sink word by word, simulating streaming.
fn stream_words(text: &str, on_event: &mut EventSink<'_>) {
    for word in text.split_inclusive(' ') {
        on_event(StreamEvent::Text(word.to_string()));
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _model: &str,
        messages: &[Message],
        _tools: &[ToolSpec],
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let already_used_tool = messages.iter().any(|m| m.role == Role::Tool);

        if already_used_tool {
            let content = "Done — I read the project manifest and the workspace looks healthy.";
            stream_words(content, on_event);
            Ok(ModelResponse {
                content: content.to_string(),
                tool_calls: vec![],
                usage: Usage {
                    input_tokens: 42,
                    output_tokens: 18,
                    cost_usd: 0.0,
                },
                quotas: Vec::new(),
            })
        } else {
            let content = "Let me inspect the project manifest.";
            stream_words(content, on_event);
            Ok(ModelResponse {
                content: content.to_string(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "read_file".to_string(),
                    args: json!({ "path": "Cargo.toml" }),
                }],
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 12,
                    cost_usd: 0.0,
                },
                quotas: Vec::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_turn_requests_a_tool() {
        let p = MockProvider;
        let res = p
            .complete(
                "mock",
                &[Message::user("check the project")],
                &[],
                &mut |_| {},
            )
            .await
            .unwrap();
        assert!(res.wants_tools());
        assert_eq!(res.tool_calls[0].name, "read_file");
    }

    #[tokio::test]
    async fn after_tool_result_it_finishes() {
        let p = MockProvider;
        let msgs = vec![
            Message::user("check the project"),
            Message::assistant("Let me inspect the project manifest."),
            Message::new(Role::Tool, "[workspace] ..."),
        ];
        let res = p.complete("mock", &msgs, &[], &mut |_| {}).await.unwrap();
        assert!(!res.wants_tools());
        assert!(!res.content.is_empty());
    }

    #[tokio::test]
    async fn streams_text_to_the_sink() {
        let p = MockProvider;
        let mut streamed = String::new();
        let res = p
            .complete("mock", &[Message::user("check it")], &[], &mut |ev| {
                if let StreamEvent::Text(t) = ev {
                    streamed.push_str(&t)
                }
            })
            .await
            .unwrap();
        assert_eq!(
            streamed, res.content,
            "streamed text deltas reconstruct the full content"
        );
    }
}
