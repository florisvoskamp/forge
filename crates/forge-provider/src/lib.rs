//! The Forge `Provider` trait: a minimal, provider-neutral model interface that every
//! other crate depends on instead of any concrete SDK (ADR-0003). v0.1 ships one real
//! implementation (`GenAiProvider`, backed by the `genai` crate, covering Anthropic /
//! OpenAI / Ollama) plus a deterministic `MockProvider` for offline tests and the
//! walking skeleton.

use async_trait::async_trait;
use forge_types::{Message, ToolCall, Usage};

mod cli_provider;
mod genai_provider;
mod mock;

pub use cli_provider::{CliKind, CliProvider};
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

/// A streamed event produced by a provider during a completion. Lets the UI animate not just
/// the answer but the model's *reasoning* and (for the agentic CLI bridge) its tool activity.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// A delta of the assistant's answer text (accumulates into [`ModelResponse::content`]).
    Text(String),
    /// A delta of the model's reasoning/thinking — shown live but NOT part of the final answer.
    Reasoning(String),
    /// The agent started a tool call. Emitted by the CLI bridge, whose agent loop runs tools
    /// itself; genai providers leave tool execution to forge-core and don't emit this.
    ToolStarted { name: String, args: String },
    /// A tool call finished (CLI bridge only).
    ToolFinished {
        name: String,
        ok: bool,
        summary: String,
    },
}

/// A sink for [`StreamEvent`]s as they arrive (text, reasoning, tool activity).
pub type EventSink<'a> = dyn FnMut(StreamEvent) + Send + 'a;

/// A model backend. Implement this trait (and nothing in the core) to add a provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Run one completion against `model` given the transcript and the available tools.
    /// Streamed events (text, reasoning, tool activity) are delivered to `on_event` as they
    /// arrive; the full answer text is also returned in [`ModelResponse::content`].
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError>;
}

/// Routes each turn to a backend by the model id's `provider::` prefix: `claude-cli::…` /
/// `codex-cli::…` go to the subscription CLI bridge; everything else goes to the genai-backed
/// API providers. This is the single `Provider` the CLI installs for a real session.
pub struct DispatchProvider {
    genai: GenAiProvider,
    claude_cli: CliProvider,
    codex_cli: CliProvider,
    /// One-time CLI-bridge ToS/discretion notice (FR-Part-B AC-B8).
    notice: std::sync::Once,
}

impl DispatchProvider {
    pub fn new() -> Self {
        Self {
            genai: GenAiProvider::new(),
            claude_cli: CliProvider::claude_code(),
            codex_cli: CliProvider::codex(),
            notice: std::sync::Once::new(),
        }
    }

    fn cli_notice(&self) {
        self.notice.call_once(|| {
            tracing::warn!(
                "CLI-bridge runs your locally-installed claude/codex; Forge never sees your \
                 login. Using subscription CLIs from third-party tools may be restricted by \
                 Anthropic/OpenAI terms — you run this at your own discretion. See \
                 docs/features/provider-integrations.md."
            );
        });
    }
}

impl Default for DispatchProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for DispatchProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        if model.starts_with("claude-cli::") {
            self.cli_notice();
            self.claude_cli
                .complete(model, messages, tools, on_event)
                .await
        } else if model.starts_with("codex-cli::") {
            self.cli_notice();
            self.codex_cli
                .complete(model, messages, tools, on_event)
                .await
        } else {
            self.genai.complete(model, messages, tools, on_event).await
        }
    }
}
