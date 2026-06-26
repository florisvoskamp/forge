//! The Forge `Provider` trait: a minimal, provider-neutral model interface that every
//! other crate depends on instead of any concrete SDK (ADR-0003). v0.1 ships one real
//! implementation (`GenAiProvider`, backed by the `genai` crate, covering Anthropic /
//! OpenAI / Ollama) plus a deterministic `MockProvider` for offline tests and the
//! walking skeleton.

use async_trait::async_trait;
use forge_types::{EffortLevel, Message, QuotaHint, ToolCall, Usage};

mod cli_provider;
mod embedder;
mod genai_provider;
mod mock;
mod tool_recovery;

pub use cli_provider::{available_bridge_models, CliKind, CliProvider, SUBAGENT_SINK_ENV};
pub use embedder::{select_embedder, GenaiEmbedder};
pub use genai_provider::{bundled_http_client, is_discoverable, list_models, GenAiProvider};
pub use mock::MockProvider;
pub use tool_recovery::{looks_like_unexecuted_tool_call, recover_text_tool_calls};

/// Normalize legacy underscore-prefixed bridge ids to the canonical hyphen form so
/// `codex_cli::gpt-5.4-mini` and `claude_cli::opus` work identically to their hyphen forms.
pub fn normalize_model_id(model: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = model.strip_prefix("claude_cli::") {
        return std::borrow::Cow::Owned(format!("claude-cli::{rest}"));
    }
    if let Some(rest) = model.strip_prefix("codex_cli::") {
        return std::borrow::Cow::Owned(format!("codex-cli::{rest}"));
    }
    if let Some(rest) = model.strip_prefix("agy_cli::") {
        return std::borrow::Cow::Owned(format!("agy-cli::{rest}"));
    }
    std::borrow::Cow::Borrowed(model)
}

/// True when `model` routes to a subscription CLI bridge (`claude-cli::…` / `codex-cli::…`). A
/// bridge runs its OWN internal tool loop and returns the finished turn as a single text response
/// (no tool calls surface to the parent), so the parent must treat a bridge response as terminal —
/// it must NOT nudge it to "keep calling tools," which only re-runs the whole bridge in confusion.
pub fn is_cli_bridge(model: &str) -> bool {
    let m = normalize_model_id(model);
    m.starts_with("claude-cli::") || m.starts_with("codex-cli::") || m.starts_with("agy-cli::")
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
    /// Authentication failed (HTTP 401/403) — the key is bad, missing, or lacks access. Failing
    /// over to *another* provider is correct, but the bad credential won't fix itself mid-session,
    /// so retrying THIS model auth-fails identically every turn (the per-turn failover churn). Like
    /// [`Capability`](Self::Capability) it's treated as PERMANENT: excluded on the long window +
    /// periodic re-probe (so it recovers automatically once the user fixes the key).
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
    /// transient cooldown. True for [`Capability`](Self::Capability) (the model can't serve
    /// tool-using turns) and [`Auth`](Self::Auth) (the credential is bad/missing and won't fix
    /// itself mid-session) — both auth-fail/incapability-fail identically every turn otherwise.
    pub fn is_permanent(&self) -> bool {
        matches!(self, Self::Capability(_) | Self::Auth(_))
    }

    /// Whether this is a rate-limit / quota-exhaustion failure (HTTP 429, `RESOURCE_EXHAUSTED`).
    /// Used by the failover loop to lazily skip the *same provider's* remaining chain entries
    /// after one of its models 429s — a rate limit is usually provider-wide, so the siblings would
    /// 429 too. Every other failure mode keeps strict mesh-rank failover order.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
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

    /// Heuristic: whether this failure is a context-length OVERFLOW (the prompt exceeded the
    /// model's window) rather than a genuine outage. Providers surface overflow inconsistently —
    /// often as a 4xx/5xx the generic classifier files under [`Unavailable`](Self::Unavailable) or
    /// [`Request`](Self::Request) — so we sniff the message. The correct response is to SHRINK the
    /// input (compact/trim) and retry the SAME model, not to bench a healthy model and fail over.
    pub fn is_context_overflow(&self) -> bool {
        let msg = match self {
            Self::Unavailable(m) | Self::Request(m) => m,
            Self::RateLimited { message, .. } => message,
            _ => return false,
        };
        let m = msg.to_lowercase();
        [
            "context length",
            "context window",
            "context_length",
            "maximum context",
            "maximum number of tokens",
            "too many tokens",
            "reduce the length",
            "prompt is too long",
            "input is too large",
            "exceeds the maximum",
            "string too long",
        ]
        .iter()
        .any(|k| m.contains(k))
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

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn is_cli_bridge_detects_both_forms() {
        assert!(is_cli_bridge("claude-cli::opus"));
        assert!(is_cli_bridge("codex-cli::gpt-5.5"));
        assert!(is_cli_bridge("claude_cli::opus"), "legacy underscore form");
        assert!(
            is_cli_bridge("codex_cli::gpt-5.5"),
            "legacy underscore form"
        );
        assert!(is_cli_bridge("agy-cli::gemini-3.5-flash"), "antigravity");
        assert!(
            is_cli_bridge("agy_cli::gemini-3.1-pro"),
            "antigravity legacy underscore form"
        );
        assert!(!is_cli_bridge("openrouter::google/gemini-3.5-flash"));
        assert!(!is_cli_bridge("gemini::gemini-3.5-flash"));
        assert!(!is_cli_bridge("ollama::llama3.2"));
    }

    #[test]
    fn is_context_overflow_sniffs_the_message_but_not_plain_outages() {
        // Providers surface overflow as Unavailable/Request/RateLimited with a telltale message.
        assert!(ProviderError::Unavailable(
            "This model's maximum context length is 128000 tokens".into()
        )
        .is_context_overflow());
        assert!(
            ProviderError::Request("input is too large for the context window".into())
                .is_context_overflow()
        );
        // A genuine outage / rate-limit is NOT an overflow — it must fail over, not compact.
        assert!(!ProviderError::Unavailable("502 bad gateway".into()).is_context_overflow());
        assert!(!ProviderError::RateLimited {
            message: "429 slow down".into(),
            retry_after: None
        }
        .is_context_overflow());
        assert!(!ProviderError::Auth("401".into()).is_context_overflow());
    }

    #[test]
    fn auth_and_capability_are_permanent_transient_outages_are_not() {
        // Permanent → excluded (long window + re-probe), never re-tried at the top of every turn.
        // A bad/missing credential and a tool-incapable model both recur identically every call.
        assert!(ProviderError::Auth("401 unauthorized".into()).is_permanent());
        assert!(ProviderError::Capability("no tool calling".into()).is_permanent());
        // Transient → short bench + fail over (the provider may recover on its own).
        assert!(!ProviderError::Unavailable("502".into()).is_permanent());
        assert!(!ProviderError::RateLimited {
            message: "429".into(),
            retry_after: None
        }
        .is_permanent());
        // All four still fail over to another model; only Request("…") is non-retryable.
        assert!(ProviderError::Auth("403".into()).is_retryable());
        assert!(!ProviderError::Request("malformed".into()).is_retryable());
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
    /// The bridge model proposed a plan (`present_plan` in the `mcp-serve` process). Tailed from
    /// the out-of-band sink so the parent renders the plan card and runs the approval flow at turn
    /// end, exactly like the in-process path (CLI bridge only).
    Plan(forge_types::PlanProposal),
}

/// A sink for [`StreamEvent`]s as they arrive (text, reasoning, tool activity).
pub type EventSink<'a> = dyn FnMut(StreamEvent) + Send + 'a;

/// Per-completion options that extend the base [`Provider::complete`] signature without breaking
/// existing call sites. Passed via [`Provider::complete_with`]; the base `complete` ignores it.
#[derive(Debug, Clone, Default)]
pub struct CompletionOptions {
    /// Reasoning / thinking intensity hint forwarded to the model. `None` = provider default.
    pub effort: Option<EffortLevel>,
    /// Sampling temperature. `None` = provider default; coding turns set a low value so edits and
    /// patches are deterministic rather than creatively varied.
    pub temperature: Option<f32>,
}

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

    /// Like [`complete`] but accepts extra per-call options (e.g. effort / thinking intensity).
    /// The default implementation ignores `opts` and delegates to [`complete`], so existing
    /// backends need not change. Override in providers that support the options.
    async fn complete_with(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        opts: &CompletionOptions,
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let _ = opts;
        self.complete(model, messages, tools, on_event).await
    }
}

/// Routes each turn to a backend by the model id's `provider::` prefix: `claude-cli::…` /
/// `codex-cli::…` go to the subscription CLI bridge; everything else goes to the genai-backed
/// API providers. This is the single `Provider` the CLI installs for a real session.
pub struct DispatchProvider {
    genai: GenAiProvider,
    claude_cli: CliProvider,
    codex_cli: CliProvider,
    /// Google Antigravity (`agy`) — text-mode only (no MCP), so always built `with_harness(false)`.
    agy_cli: CliProvider,
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
            // agy has no MCP/`--tools` wiring → always text mode, never the Forge-MCP harness.
            agy_cli: CliProvider::antigravity().with_harness(false),
            notice: std::sync::Once::new(),
        }
    }

    /// Cap output tokens on the genai (API-provider) path. `0` disables the cap. The CLI bridges
    /// manage their own output, so this only affects the genai backend.
    pub fn with_max_output_tokens(mut self, cap: u32) -> Self {
        self.genai = self.genai.with_max_output_tokens(cap);
        self
    }

    /// Opt into completeness re-verification on the CLI bridges (`mesh.verify_completeness`):
    /// higher resolve rate on under-scoped fixes, ~3× tokens. Only affects the harness-mode bridges.
    pub fn with_verify_completeness(mut self, on: bool) -> Self {
        self.claude_cli = self.claude_cli.with_verify_completeness(on);
        self.codex_cli = self.codex_cli.with_verify_completeness(on);
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
        } else if model.starts_with("agy-cli::") {
            self.cli_notice();
            self.agy_cli
                .complete(model, messages, tools, on_event)
                .await
        } else {
            self.genai.complete(model, messages, tools, on_event).await
        }
    }

    async fn complete_with(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        opts: &CompletionOptions,
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
        } else if model.starts_with("agy-cli::") {
            self.cli_notice();
            self.agy_cli
                .complete(model, messages, tools, on_event)
                .await
        } else {
            self.genai
                .complete_with(model, messages, tools, opts, on_event)
                .await
        }
    }
}
