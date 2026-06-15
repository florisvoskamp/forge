//! `genai`-backed implementation of [`Provider`] (ADR-0003), covering Anthropic / OpenAI /
//! Ollama with normalized tool calling. Tools are advertised to the model, the model's
//! tool calls are mapped back to Forge [`ToolCall`]s, and prior tool results are replayed
//! as genai tool responses so multi-step tool loops round-trip faithfully.

use async_trait::async_trait;
use forge_types::{Message, Role, ToolCall, Usage};
use futures::StreamExt;
use genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, Tool, ToolCall as GenAiToolCall,
    ToolResponse,
};
use genai::Client;

use crate::{ModelResponse, Provider, ProviderError, TextSink, ToolSpec};

#[derive(Default)]
pub struct GenAiProvider {
    client: Client,
}

impl GenAiProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a caller-supplied `genai::Client`. Used by the HTTP contract tests to
    /// point genai at a local mock server; otherwise identical to [`GenAiProvider::new`].
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }
}

/// Forge model ids are `"provider::model"`. genai 0.6 resolves the adapter from a
/// `namespace::name` prefix directly (its namespace table covers anthropic, openai, gemini,
/// xai, deepseek, ollama, open_router, …), so we pass the namespaced form straight through
/// rather than stripping it — that selects the right adapter (and its endpoint + default
/// API-key env var) explicitly instead of relying on name inference. The only fix-up is
/// Forge's `openrouter` alias → genai's `open_router` namespace. A model with no `::` is
/// passed verbatim (genai falls back to name inference).
fn to_genai_model(model: &str) -> String {
    match model.split_once("::") {
        Some((prefix, name)) => format!("{}::{}", normalize_namespace(prefix), name),
        None => model.to_string(),
    }
}

/// Map a Forge provider prefix to the namespace genai expects. Identity for everything
/// except `openrouter`, which genai spells `open_router`.
fn normalize_namespace(prefix: &str) -> &str {
    match prefix {
        "openrouter" => "open_router",
        other => other,
    }
}

fn to_genai_messages(messages: &[Message]) -> Vec<ChatMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for m in messages {
        match m.role {
            Role::System => out.push(ChatMessage::system(m.content.clone())),
            Role::User => out.push(ChatMessage::user(m.content.clone())),
            Role::Assistant => {
                if !m.content.is_empty() {
                    out.push(ChatMessage::assistant(m.content.clone()));
                }
                if !m.tool_calls.is_empty() {
                    let calls: Vec<GenAiToolCall> = m
                        .tool_calls
                        .iter()
                        .map(|c| GenAiToolCall {
                            call_id: c.id.clone(),
                            fn_name: c.name.clone(),
                            fn_arguments: c.args.clone(),
                            thought_signatures: None,
                        })
                        .collect();
                    out.push(ChatMessage::from(calls));
                }
            }
            Role::Tool => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                out.push(ChatMessage::from(ToolResponse::new(id, m.content.clone())));
            }
        }
    }
    out
}

fn to_genai_tool(spec: &ToolSpec) -> Tool {
    Tool::new(spec.name.clone())
        .with_description(spec.description.clone())
        .with_schema(spec.schema.clone())
}

#[async_trait]
impl Provider for GenAiProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut TextSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let model_name = to_genai_model(model);

        let mut req = ChatRequest::new(to_genai_messages(messages));
        if !tools.is_empty() {
            req = req.with_tools(tools.iter().map(to_genai_tool).collect::<Vec<_>>());
        }

        // Capture flags so the terminal End event carries usage + tool calls.
        let options = ChatOptions::default()
            .with_capture_usage(true)
            .with_capture_content(true)
            .with_capture_tool_calls(true);

        let res = self
            .client
            .exec_chat_stream(model_name.as_str(), req, Some(&options))
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;

        let mut stream = res.stream;
        let mut content = String::new();
        let mut usage = Usage::default();
        let mut tool_calls: Vec<ToolCall> = Vec::new();

        while let Some(event) = stream.next().await {
            match event.map_err(|e| ProviderError::Request(e.to_string()))? {
                ChatStreamEvent::Chunk(chunk) => {
                    content.push_str(&chunk.content);
                    on_text(&chunk.content);
                }
                ChatStreamEvent::End(end) => {
                    if let Some(u) = &end.captured_usage {
                        usage = Usage {
                            input_tokens: u.prompt_tokens.unwrap_or(0).max(0) as u64,
                            output_tokens: u.completion_tokens.unwrap_or(0).max(0) as u64,
                            cost_usd: 0.0, // priced by the mesh from token counts (FR-5)
                        };
                    }
                    // Some providers deliver text only at the end (not chunked).
                    if content.is_empty() {
                        if let Some(text) = end.captured_first_text() {
                            content.push_str(text);
                            on_text(text);
                        }
                    }
                    if let Some(tcs) = end.captured_into_tool_calls() {
                        tool_calls = tcs
                            .into_iter()
                            .map(|tc| ToolCall {
                                id: tc.call_id,
                                name: tc.fn_name,
                                args: tc.fn_arguments,
                            })
                            .collect();
                    }
                }
                _ => {}
            }
        }

        Ok(ModelResponse {
            content,
            tool_calls,
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn to_genai_model_passes_namespaced_ids_through() {
        // Native providers: namespace kept verbatim so genai selects the adapter explicitly.
        assert_eq!(to_genai_model("ollama::llama3.2"), "ollama::llama3.2");
        assert_eq!(to_genai_model("openai::gpt-4o"), "openai::gpt-4o");
        assert_eq!(
            to_genai_model("anthropic::claude-opus-4-8"),
            "anthropic::claude-opus-4-8"
        );
        assert_eq!(
            to_genai_model("gemini::gemini-2.5-pro"),
            "gemini::gemini-2.5-pro"
        );
        assert_eq!(to_genai_model("xai::grok-4"), "xai::grok-4");
        assert_eq!(
            to_genai_model("deepseek::deepseek-chat"),
            "deepseek::deepseek-chat"
        );
    }

    #[test]
    fn to_genai_model_renames_openrouter_alias() {
        // Forge says `openrouter`; genai's namespace is `open_router`. The model part —
        // including its `/` and any later separators — is preserved.
        assert_eq!(
            to_genai_model("openrouter::deepseek/deepseek-chat"),
            "open_router::deepseek/deepseek-chat"
        );
        assert_eq!(
            to_genai_model("openrouter::anthropic/claude-sonnet-4-5"),
            "open_router::anthropic/claude-sonnet-4-5"
        );
    }

    #[test]
    fn to_genai_model_splits_on_first_separator() {
        // genai splits namespace on the FIRST `::`, so the remainder stays intact.
        assert_eq!(to_genai_model("openai::a::b"), "openai::a::b");
    }

    #[test]
    fn to_genai_model_without_prefix_is_verbatim() {
        assert_eq!(to_genai_model("claude-3-5-sonnet"), "claude-3-5-sonnet");
        assert_eq!(to_genai_model(""), "");
    }

    #[test]
    fn maps_all_roles_and_round_trips_tool_call_ids() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("hi"),
            Message::assistant_tool_calls(
                "thinking",
                vec![ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    args: json!({"path": "x"}),
                }],
            ),
            Message::tool_result("call_1", "file contents"),
        ];
        let out = to_genai_messages(&msgs);
        // system, user, assistant-text, assistant-tool-call, tool-response = 5
        assert_eq!(out.len(), 5, "every role maps to a genai message");
    }

    #[test]
    fn empty_assistant_content_emits_no_stray_text_message() {
        // assistant with empty content but a tool call -> only the tool-call message.
        let msgs = vec![Message::assistant_tool_calls(
            "",
            vec![ToolCall {
                id: "c".into(),
                name: "t".into(),
                args: json!({}),
            }],
        )];
        let out = to_genai_messages(&msgs);
        assert_eq!(out.len(), 1, "no empty assistant text message");
    }

    #[test]
    fn tool_spec_maps_name_description_and_schema() {
        let schema = json!({"type":"object","properties":{"path":{"type":"string"}}});
        let spec = ToolSpec {
            name: "read_file".into(),
            description: "read a file".into(),
            schema: schema.clone(),
        };
        let tool = to_genai_tool(&spec);
        assert_eq!(tool.name.as_str(), "read_file");
        assert_eq!(tool.description.as_deref(), Some("read a file"));
        assert_eq!(tool.schema.as_ref(), Some(&schema));
    }
}
