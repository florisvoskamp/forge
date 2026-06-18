//! The Forge `Provider` trait: a minimal, provider-neutral model interface that every
//! other crate depends on instead of any concrete SDK (ADR-0003). v0.1 ships one real
//! implementation (`GenAiProvider`, backed by the `genai` crate, covering Anthropic /
//! OpenAI / Ollama) plus a deterministic `MockProvider` for offline tests and the
//! walking skeleton.

use async_trait::async_trait;
use forge_types::{Message, QuotaHint, ToolCall, Usage};

mod cli_provider;
mod embedder;
mod genai_provider;
mod mock;

pub use cli_provider::{available_bridge_models, CliKind, CliProvider, SUBAGENT_SINK_ENV};
pub use embedder::{select_embedder, GenaiEmbedder};
pub use genai_provider::{list_models, GenAiProvider};
pub use mock::MockProvider;

/// Normalize legacy underscore-prefixed bridge ids to the canonical hyphen form so
/// `codex_cli::gpt-5.4-mini` and `claude_cli::opus` work identically to their hyphen forms.
pub fn normalize_model_id(model: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = model.strip_prefix("claude_cli::") {
        return std::borrow::Cow::Owned(format!("claude-cli::{rest}"));
    }
    if let Some(rest) = model.strip_prefix("codex_cli::") {
        return std::borrow::Cow::Owned(format!("codex-cli::{rest}"));
    }
    std::borrow::Cow::Borrowed(model)
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// A non-retryable failure: bad request, malformed response, context-length, etc. It
    /// would fail the same way on any model, so the mesh must NOT fail over on it.
    #[error("provider request failed: {0}")]
    Request(String),
    /// Rate-limited / out of quota (HTTP 429, `RESOURCE_EXHAUSTED`). Retryable on another
    /// model; `retry_after` carries the server's cooldown when it told us one.
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<std::time::Duration>,
    },
    /// The provider is down / the stream dropped (5xx, connection/timeout). Retryable.
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    /// Authentication failed (HTTP 401/403) — the key is bad or lacks access. Retryable in
    /// the sense that *another provider* may work; the bad one is benched.
    #[error("provider auth failed: {0}")]
    Auth(String),
    /// A PERMANENT, model-specific incapability: this model can't serve Forge's (tool-using)
    /// turns at all — it rejects function calling, has no tool-supporting endpoint, mangles tool
    /// params, or the account can't afford it (HTTP 402 / "requires more credits"). Failing over
    /// to *another* model is correct, but retrying THIS one will fail identically every time, so
    /// the mesh excludes it (a long bench window) rather than benching it on a short cooldown.
    #[error("model unsupported: {0}")]
    Capability(String),
}

impl ProviderError {
    /// Whether the mesh should bench this model and fail over to another. True for
    /// rate-limit / unavailable / auth; false for [`Request`](Self::Request) (would fail
    /// identically everywhere).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. } | Self::Unavailable(_) | Self::Auth(_) | Self::Capability(_)
        )
    }

    /// Whether this failure is PERMANENT for the model: it will recur on every call, so the model
    /// should be *excluded* (a long bench window + periodic re-probe), not benched on the short
    /// transient cooldown. True only for [`Capability`](Self::Capability).
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Capability(_))
    }

    /// How long to bench the model: the server-provided `retry_after` when present,
    /// otherwise `default`.
    pub fn cooldown(&self, default: std::time::Duration) -> std::time::Duration {
        match self {
            Self::RateLimited {
                retry_after: Some(d),
                ..
            } => *d,
            _ => default,
        }
    }

    /// A short reason string for the health record / UI ("rate-limited (429)", …).
    pub fn reason(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "rate-limited",
            Self::Unavailable(_) => "unavailable",
            Self::Auth(_) => "auth failed",
            Self::Request(_) => "request error",
            Self::Capability(_) => "unsupported (no tool calling / unaffordable)",
        }
    }
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
    /// Subscription quota observations surfaced by a CLI bridge this turn (Claude's
    /// `rate_limit_event` / Codex rollout). Empty for API providers / when the bridge
    /// reported nothing. Multiple entries when both the 5h and weekly windows were observed.
    pub quotas: Vec<QuotaHint>,
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
    /// A subagent was spawned. Emitted by the CLI bridge by tailing the out-of-band event sink
    /// that `forge mcp-serve` writes (so bridge-spawned subagents are visible in the TUI just
    /// like native ones — RFC subagent-orchestration Phase 3c).
    SubagentStarted {
        id: String,
        agent: String,
        task: String,
    },
    /// A live activity snippet from a still-running subagent (CLI bridge only).
    SubagentProgress { id: String, snippet: String },
    /// A subagent finished (CLI bridge only).
    SubagentFinished {
        id: String,
        agent: String,
        ok: bool,
        summary: String,
        cost_usd: f64,
    },
    /// The task list changed inside a bridged turn (the bridge model called `update_tasks` in the
    /// `mcp-serve` process). Tailed from the out-of-band sink so the TUI's sticky task panel
    /// updates LIVE during the turn, not only on completion (CLI bridge only).
    Tasks(Vec<forge_types::TodoItem>),
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
    /// `harness` = run CLI-bridge turns through Forge's MCP tool server + permission gate
    /// (RFC Phase 2); `false` runs the CLI as its own agent (Phase 1).
    pub fn new(harness: bool) -> Self {
        Self {
            genai: GenAiProvider::new(),
            claude_cli: CliProvider::claude_code().with_harness(harness),
            codex_cli: CliProvider::codex().with_harness(harness),
            notice: std::sync::Once::new(),
        }
    }

    /// Cap output tokens on the genai (API-provider) path. `0` disables the cap. The CLI bridges
    /// manage their own output, so this only affects the genai backend.
    pub fn with_max_output_tokens(mut self, cap: u32) -> Self {
        self.genai = self.genai.with_max_output_tokens(cap);
        self
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
        Self::new(true)
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
        let model = normalize_model_id(model);
        let model = model.as_ref();
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
