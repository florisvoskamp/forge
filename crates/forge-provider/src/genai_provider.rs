//! `genai`-backed implementation of [`Provider`] (ADR-0003), covering Anthropic / OpenAI /
//! Ollama with normalized tool calling. Tools are advertised to the model, the model's
//! tool calls are mapped back to Forge [`ToolCall`]s, and prior tool results are replayed
//! as genai tool responses so multi-step tool loops round-trip faithfully.

use async_trait::async_trait;
use forge_types::{Message, Role, ToolCall, Usage};
use genai::chat::{ChatMessage, ChatRequest, Tool, ToolCall as GenAiToolCall, ToolResponse};
use genai::Client;

use crate::{ModelResponse, Provider, ProviderError, ToolSpec};

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

#[async_trait]
impl Provider for GenAiProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ModelResponse, ProviderError> {
        // Config uses "provider::model"; genai infers the adapter from the bare model name.
        let model_name = model.rsplit("::").next().unwrap_or(model);

        let mut req = ChatRequest::new(to_genai_messages(messages));
        if !tools.is_empty() {
            req = req.with_tools(tools.iter().map(to_genai_tool).collect::<Vec<_>>());
        }

        let res = self
            .client
            .exec_chat(model_name, req, None)
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;

        let usage = Usage {
            input_tokens: res.usage.prompt_tokens.unwrap_or(0).max(0) as u64,
            output_tokens: res.usage.completion_tokens.unwrap_or(0).max(0) as u64,
            cost_usd: 0.0, // priced by the mesh from token counts (FR-5)
        };

        let tool_calls: Vec<ToolCall> = res
            .tool_calls()
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.call_id.clone(),
                name: tc.fn_name.clone(),
                args: tc.fn_arguments.clone(),
            })
            .collect();

        let content = res.first_text().unwrap_or_default().to_string();

        Ok(ModelResponse {
            content,
            tool_calls,
            usage,
        })
    }
}
