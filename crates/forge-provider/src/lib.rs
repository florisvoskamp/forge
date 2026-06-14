//! The Forge `Provider` trait: a minimal, provider-neutral model interface that every
//! other crate depends on instead of any concrete SDK (ADR-0003). v0.1 ships one real
//! implementation (`GenAiProvider`, backed by the `genai` crate, covering Anthropic /
//! OpenAI / Ollama) plus a deterministic `MockProvider` for offline tests and the
//! walking skeleton.

use async_trait::async_trait;
use forge_types::{Message, ToolCall, Usage};

mod genai_provider;
mod mock;

pub use genai_provider::GenAiProvider;
pub use mock::MockProvider;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider request failed: {0}")]
    Request(String),
}

/// A tool advertised to the model so it can choose to call it.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub schema: serde_json::Value,
}

/// The result of a single model completion: text, any requested tool calls, and usage.
#[derive(Debug, Clone, Default)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

impl ModelResponse {
    pub fn wants_tools(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// A model backend. Implement this trait (and nothing in the core) to add a provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Run one completion against `model` given the transcript and the available tools.
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ModelResponse, ProviderError>;
}
