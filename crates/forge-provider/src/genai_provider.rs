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

#[async_trait(?Send)]
impl Provider for GenAiProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_text: &mut TextSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        // Config uses "provider::model"; genai infers the adapter from the bare model name.
        let model_name = model.rsplit("::").next().unwrap_or(model);

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
            .exec_chat_stream(model_name, req, Some(&options))
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
