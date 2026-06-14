//! `genai`-backed implementation of [`Provider`] (ADR-0003).
//!
//! v0.1 covers text completion across genai's providers (Anthropic / OpenAI / Ollama),
//! reading API keys from the environment. Mapping the model's tool calls through genai's
//! tool API, streaming, and pricing-table cost computation are planned enhancements; this
//! adapter is the seam where they will land without touching any other crate.

use async_trait::async_trait;
use forge_types::{Message, Role, Usage};
use genai::chat::{ChatMessage, ChatRequest};
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

fn to_genai(messages: &[Message]) -> Vec<ChatMessage> {
    messages
        .iter()
        .map(|m| match m.role {
            Role::System => ChatMessage::system(m.content.clone()),
            Role::Assistant => ChatMessage::assistant(m.content.clone()),
            Role::User => ChatMessage::user(m.content.clone()),
            Role::Tool => ChatMessage::user(format!("[tool result] {}", m.content)),
        })
        .collect()
}

#[async_trait]
impl Provider for GenAiProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ModelResponse, ProviderError> {
        // Config uses "provider::model"; genai infers the adapter from the bare model name.
        let model_name = model.rsplit("::").next().unwrap_or(model);

        let req = ChatRequest::new(to_genai(messages));
        let res = self
            .client
            .exec_chat(model_name, req, None)
            .await
            .map_err(|e| ProviderError::Request(e.to_string()))?;

        let usage = Usage {
            input_tokens: res.usage.prompt_tokens.unwrap_or(0).max(0) as u64,
            output_tokens: res.usage.completion_tokens.unwrap_or(0).max(0) as u64,
            cost_usd: 0.0,
        };
        let content = res.into_first_text().unwrap_or_default();

        Ok(ModelResponse {
            content,
            tool_calls: vec![],
            usage,
        })
    }
}
