//! CLI-bridge [`Provider`]: run a turn by spawning the user's locally-installed, already
//! authenticated official agent CLI — `claude` (Claude Code) or `codex` (OpenAI Codex) — and
//! consuming its JSON event stream. This is the ToS-defensible way to use a Claude Pro/Max or
//! ChatGPT subscription: **Forge never reads, stores, or transmits the OAuth token.** The
//! official binary owns its own credentials under its own sanctioned auth; Forge only feeds it
//! a prompt and parses its stdout. See docs/features/provider-integrations.md (Part B) for the
//! honest ToS analysis — this is opt-in and run at the user's discretion.
//!
//! Two modes (RFC cli-bridge-full-harness), selected by `mesh.bridge_mode`:
//! - **harness** (default): claude's built-in tools are disabled (`--tools ""`) and Forge's
//!   own tools are served to it over MCP (`--mcp-config` → `forge mcp-serve`, `--strict-mcp-config`,
//!   `--allowedTools "mcp__forge"`), so every tool/side-effect runs through Forge's registry +
//!   permission gate. The Forge harness on the subscription model.
//! - **text**: claude runs as its own agent with its own tools (`--permission-mode acceptEdits`).
//!
//! codex harness (RFC Phase 3) works the same way via codex's own MCP wiring: the Forge MCP
//! server is registered with `-c mcp_servers.forge.*` and its tools auto-approved
//! (`default_tools_approval_mode="approve"`). codex keeps a `read-only` sandbox, so its own
//! shell can only do read-only recon — every write/side-effect can only go through Forge's
//! gated mcp__forge tools.
//!
//! Either way Forge parses the rich event stream — thinking/reasoning, tool activity, answer
//! text — surfacing each as a [`StreamEvent`]. Each turn is a fresh invocation. Subscription-
//! billed, so usage costs $0 against Forge's USD budget. Forge never reads/transmits the CLI's
//! auth.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use forge_types::{Message, Role, Usage};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::{EventSink, ModelResponse, Provider, ProviderError, StreamEvent, ToolSpec};

/// Which official CLI to bridge to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    /// Anthropic Claude Code (`claude`), Pro/Max subscription.
    ClaudeCode,
    /// OpenAI Codex (`codex`), ChatGPT subscription.
    Codex,
}

impl CliKind {
    /// The Forge model-id prefix that selects this bridge (`claude-cli::…` / `codex-cli::…`).
    pub fn prefix(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude-cli",
            CliKind::Codex => "codex-cli",
        }
    }

    fn default_binary(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude",
            CliKind::Codex => "codex",
        }
    }

    /// All bridge kinds.
    pub fn all() -> [CliKind; 2] {
        [CliKind::ClaudeCode, CliKind::Codex]
    }

    /// Whether this bridge's CLI is installed (its binary resolves on `PATH`). A subscription
    /// bridge that's present is treated as always-available — it doesn't rate-limit like the free
    /// API tiers — so the mesh can fall back to it when metered providers are throttled.
    pub fn available(self) -> bool {
        binary_on_path(self.default_binary())
    }

    /// The bare Forge model id for this bridge (`claude-cli::` / `codex-cli::`), which resolves to
    /// the CLI's own default model.
    pub fn default_model_id(self) -> String {
        format!("{}::", self.prefix())
    }

    /// The model aliases this bridge exposes by default, so auto-discovery surfaces more than just
    /// the CLI's single default — letting the mesh size each turn (haiku/mini for trivial, opus for
    /// complex). The official CLIs publish no machine-readable model list, so these are the
    /// documented `--model` aliases; users can override the set via `[mesh.bridge_models]`, and any
    /// alias that's stale or unavailable just benches itself via failover (never a hard error).
    pub fn default_models(self) -> &'static [&'static str] {
        match self {
            // `claude --model` accepts these aliases (claude 2.x); they span the capability tiers.
            CliKind::ClaudeCode => &["opus", "sonnet", "haiku"],
            // `codex --model` (codex 0.13x model picker). gpt-5.4-mini is the fast/cheap tier.
            CliKind::Codex => &[
                "gpt-5.5",
                "gpt-5.3-codex",
                "gpt-5.2",
                "gpt-5.4",
                "gpt-5.4-mini",
            ],
        }
    }

    /// How to tell the user to make this CLI usable.
    fn setup_hint(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => {
                "install Claude Code and run `claude` once to log in (Pro/Max subscription)"
            }
            CliKind::Codex => "install Codex and run `codex login` (ChatGPT subscription)",
        }
    }
}

/// Whether `bin` resolves to a file on `PATH` (a lightweight `which`, no spawning).
fn binary_on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

/// Forge model ids for every CLI bridge whose CLI is installed — the always-available
/// subscription models the mesh should fall back to (and prefer, being $0). Empty if none.
///
/// Each installed bridge contributes its bare default id (`claude-cli::`, the CLI's own configured
/// default) plus one id per [`CliKind::default_models`] alias (`claude-cli::opus`, …) so the mesh
/// has a model per tier rather than a single mid one. Callers that want a custom set per bridge
/// should build ids from [`CliKind::default_models`] / a config override instead.
pub fn available_bridge_models() -> Vec<String> {
    let mut out = Vec::new();
    for k in CliKind::all().into_iter().filter(|k| k.available()) {
        out.push(k.default_model_id());
        out.extend(
            k.default_models()
                .iter()
                .map(|m| format!("{}::{m}", k.prefix())),
        );
    }
    out
}

const DEFAULT_TIMEOUT_SECS: u64 = 300;
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Cap on captured stderr (for error messages) so a chatty CLI can't blow memory.
const STDERR_CAP: usize = 16 * 1024;

/// A [`Provider`] that delegates the completion to an external agent CLI.
pub struct CliProvider {
    kind: CliKind,
    binary: String,
    timeout: Duration,
    /// Harness mode (RFC cli-bridge-full-harness): the CLI runs Forge's tools via the
    /// `forge mcp-serve` MCP server under Forge's permission gate. When false, the CLI runs as
    /// its own agent with its own tools. Both claude (Phase 2) and codex (Phase 3) support it.
    harness: bool,
}

impl CliProvider {
    pub fn new(kind: CliKind) -> Self {
        Self {
            kind,
            binary: kind.default_binary().to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            harness: true,
        }
    }

    /// Toggle harness mode (Forge-tool MCP bridge) vs Phase-1 self-agent.
    pub fn with_harness(mut self, harness: bool) -> Self {
        self.harness = harness;
        self
    }

    pub fn claude_code() -> Self {
        Self::new(CliKind::ClaudeCode)
    }

    pub fn codex() -> Self {
        Self::new(CliKind::Codex)
    }

    /// Override the binary (path or name). Used by tests to point at a fake CLI.
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Strip the `claude-cli::`/`codex-cli::` prefix to the bare model name the CLI's `--model`
/// flag expects (empty → let the CLI use its own default model).
fn bare_model(model: &str) -> &str {
    model.split_once("::").map(|(_, m)| m).unwrap_or("")
}

/// Build the argv for one invocation. Pure (no I/O, no env, no secrets) so it can be asserted
/// in tests — the boundary guarantee is that Forge passes only the prompt and non-secret flags,
/// never a credential or auth token (the CLI sources those itself).
/// The `--mcp-config` JSON wiring claude to spawn `<forge_exe> mcp-serve` as a stdio MCP
/// server (Forge's tools under Forge's permission gate). No secrets — just the binary path.
fn forge_mcp_config(forge_exe: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"forge":{{"command":{exe},"args":["mcp-serve"]}}}}}}"#,
        exe = serde_json::Value::String(forge_exe.to_string())
    )
}

fn build_args(
    kind: CliKind,
    bare_model: &str,
    prompt: &str,
    harness: bool,
    forge_exe: &str,
) -> Vec<String> {
    let mut args: Vec<String> = match (kind, harness) {
        // Phase 2 harness: Forge serves its tools via `forge mcp-serve`. `--allowedTools
        // "mcp__forge"` permits ONLY Forge's tools, so claude can't use its built-ins (they'd
        // need a permission it can't get headless) — every side-effect goes through Forge's MCP
        // server + permission gate. NOTE: do NOT use --permission-mode bypassPermissions here —
        // it bypasses the allowlist and re-enables the built-ins. No secrets passed.
        (CliKind::ClaudeCode, true) => vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // `--tools ""` disables claude's BUILT-IN tools (incl. auto-permitted read-only
            // Read/Grep/Glob), leaving only the MCP tools from --mcp-config available.
            "--tools".into(),
            "".into(),
            "--mcp-config".into(),
            forge_mcp_config(forge_exe),
            "--strict-mcp-config".into(),
            "--allowedTools".into(),
            "mcp__forge".into(),
        ],
        // Phase 1: claude as its own full agent (its own tools), acceptEdits so it doesn't
        // block on prompts headless. Forge parses its rich event stream either way.
        (CliKind::ClaudeCode, false) => vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "acceptEdits".into(),
        ],
        // Codex harness (RFC Phase 3): serve Forge's tools to codex over MCP, gated by Forge's
        // permission engine. The sandbox stays `read-only`, so codex's OWN shell can only do
        // read-only recon — every write/side-effect can only go through Forge's mcp__forge
        // tools (which run in `forge mcp-serve` under `permission::decide`). We do NOT use
        // `--dangerously-bypass-approvals-and-sandbox` (it would unsandbox codex's shell).
        //
        // Two non-obvious flags, verified live against codex 0.130.0:
        // - `mcp_servers.forge.default_tools_approval_mode="approve"`: codex exec auto-cancels
        //   MCP tool calls non-interactively (openai/codex#16685); "approve" is the only value
        //   that lets them through ("auto" still cancels).
        // - `model_reasoning_summary="detailed"`: makes codex emit `reasoning` items so Forge
        //   can stream the model's thinking (otherwise only a token count is reported).
        (CliKind::Codex, true) => vec![
            "exec".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
            "--sandbox".into(),
            "read-only".into(),
            "-c".into(),
            "approval_policy=\"never\"".into(),
            "-c".into(),
            "model_reasoning_summary=\"detailed\"".into(),
            "-c".into(),
            format!(
                "mcp_servers.forge.command={}",
                serde_json::Value::String(forge_exe.to_string())
            ),
            "-c".into(),
            "mcp_servers.forge.args=[\"mcp-serve\"]".into(),
            "-c".into(),
            "mcp_servers.forge.default_tools_approval_mode=\"approve\"".into(),
        ],
        // Phase-1 text mode: codex as its own read-only agent, no Forge tools.
        (CliKind::Codex, false) => vec![
            "exec".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
            "--sandbox".into(),
            "read-only".into(),
        ],
    };
    if !bare_model.is_empty() {
        args.push("--model".into());
        args.push(bare_model.into());
    }
    // Codex takes the prompt as the trailing positional argument.
    if kind == CliKind::Codex {
        args.push(prompt.into());
    }
    args
}

/// Flatten a transcript into a single prompt string for a one-shot CLI invocation. System
/// messages become a preamble; the rest is a role-labelled transcript ending on the latest
/// turn so the CLI responds to it.
fn render_prompt(messages: &[Message]) -> String {
    let mut system = Vec::new();
    let mut convo = Vec::new();
    for m in messages {
        match m.role {
            Role::System => system.push(m.content.clone()),
            Role::User => convo.push(format!("User: {}", m.content)),
            Role::Assistant => {
                if !m.content.is_empty() {
                    convo.push(format!("Assistant: {}", m.content));
                }
            }
            // Tools are disabled for the bridge, but include any prior tool results as context.
            Role::Tool => convo.push(format!("Tool result: {}", m.content)),
        }
    }
    let mut out = String::new();
    if !system.is_empty() {
        out.push_str(&system.join("\n\n"));
        out.push_str("\n\n");
    }
    out.push_str(&convo.join("\n\n"));
    out
}

/// One item extracted from a CLI event line. A single line may yield several (e.g. an
/// assistant message with both thinking and a tool call). Control items (Usage/Final/Error)
/// are handled by `complete`; the rest map to [`StreamEvent`]s.
#[derive(Debug, PartialEq)]
enum Parsed {
    Reasoning(String),
    Text(String),
    ToolStarted {
        id: String,
        name: String,
        args: String,
    },
    ToolFinished {
        id: String,
        ok: bool,
        summary: String,
    },
    Usage(Usage),
    /// A subscription quota observation (Claude's `rate_limit_event`) — window/status/reset, for
    /// quota-aware routing (L3). The provider prefix is filled in by `complete`.
    Quota {
        window: String,
        status: forge_types::QuotaStatus,
        resets_at: Option<i64>,
        fraction: Option<f64>,
    },
    /// Authoritative final answer (Claude's `result.result`); used if nothing streamed.
    Final(String),
    Error(String),
}

/// Map a Claude `rate_limit_info` into a coarse [`QuotaStatus`], defensively (the schema is
/// version-volatile). We read the live `status` + `isUsingOverage` (NOT `overageStatus`, which is
/// a setting, not the current state) plus a usage fraction when present. Unknown → `Ok`.
fn quota_status_from(
    status: &str,
    using_overage: bool,
    fraction: Option<f64>,
) -> forge_types::QuotaStatus {
    use forge_types::QuotaStatus;
    let s = status.to_lowercase();
    if s.contains("reject") || s.contains("block") || s.contains("exceed") || s.contains("exhaust")
    {
        return QuotaStatus::Exhausted;
    }
    if let Some(f) = fraction {
        if f >= 0.98 {
            return QuotaStatus::Exhausted;
        }
        if f >= 0.80 {
            return QuotaStatus::Warning;
        }
    }
    if using_overage || s.contains("warn") || s.contains("approach") {
        return QuotaStatus::Warning;
    }
    QuotaStatus::Ok
}

fn usage_from(v: &Value) -> Usage {
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        // Subscription-billed via the user's own CLI — $0 against Forge's USD budget (FR-5).
        cost_usd: 0.0,
    }
}

/// Parse one Claude Code `--output-format stream-json` (NDJSON) line into zero or more items.
/// Field-tolerant: unknown event types and shapes are ignored, so CLI drift degrades gracefully.
fn parse_claude_line(line: &str) -> Vec<Parsed> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match v.get("type").and_then(Value::as_str) {
        // assistant message: content blocks of thinking / text / tool_use.
        Some("assistant") => {
            if let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    out.push(Parsed::Reasoning(t.to_string()));
                                }
                            }
                        }
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    out.push(Parsed::Text(t.to_string()));
                                }
                            }
                        }
                        Some("tool_use") => {
                            let id = b
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let args = b.get("input").map(|i| i.to_string()).unwrap_or_default();
                            out.push(Parsed::ToolStarted { id, name, args });
                        }
                        _ => {}
                    }
                }
            }
        }
        // user message: tool_result blocks (the outcome of a tool the agent ran).
        Some("user") => {
            if let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = b
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let ok = !b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        let summary = tool_result_summary(b.get("content"));
                        out.push(Parsed::ToolFinished { id, ok, summary });
                    }
                }
            }
        }
        Some("result") => {
            if let Some(u) = v.get("usage").map(usage_from) {
                out.push(Parsed::Usage(u));
            }
            let result_text = v.get("result").and_then(Value::as_str).map(str::to_string);
            if let Some(f) = &result_text {
                out.push(Parsed::Final(f.clone()));
            }
            if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                out.push(Parsed::Error(
                    v.get("api_error_status")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(result_text)
                        .unwrap_or_else(|| "claude reported an error".into()),
                ));
            }
        }
        // Subscription quota window (Claude Code stream-json, L3). Defensive: any missing field
        // degrades to Ok / None. `resetsAt` may arrive as secs or ms — normalise ms→secs.
        Some("rate_limit_event") => {
            if let Some(info) = v.get("rate_limit_info") {
                let status = info.get("status").and_then(Value::as_str).unwrap_or("");
                let using_overage = info
                    .get("isUsingOverage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let fraction = info
                    .get("usedFraction")
                    .or_else(|| info.get("fractionUsed"))
                    .and_then(Value::as_f64);
                let resets_at = info.get("resetsAt").and_then(Value::as_i64).map(|t| {
                    if t > 100_000_000_000 {
                        t / 1000
                    } else {
                        t
                    }
                });
                out.push(Parsed::Quota {
                    window: info
                        .get("rateLimitType")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    status: quota_status_from(status, using_overage, fraction),
                    resets_at,
                    fraction,
                });
            }
        }
        _ => {}
    }
    out
}

/// Collapse a tool_result `content` (string, or array of {type:text,text}) into a short summary.
fn tool_result_summary(content: Option<&Value>) -> String {
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let one_line = text.split('\n').next().unwrap_or("").trim();
    one_line.chars().take(120).collect()
}

fn codex_item_id(item: Option<&Value>) -> String {
    item.and_then(|i| i.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn codex_item_text(item: Option<&Value>) -> Option<String> {
    item.and_then(|i| i.get("text"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn codex_tool_started(item: Option<&Value>) -> Parsed {
    Parsed::ToolStarted {
        id: codex_item_id(item),
        name: item
            .and_then(|i| i.get("tool"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        args: item
            .and_then(|i| i.get("arguments"))
            .map(|a| a.to_string())
            .unwrap_or_default(),
    }
}

fn codex_command_started(item: Option<&Value>) -> Parsed {
    Parsed::ToolStarted {
        id: codex_item_id(item),
        name: "shell".to_string(),
        args: item
            .and_then(|i| i.get("command"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// Parse one Codex `exec --json` (JSONL) line into zero or more items. Handles agent text,
/// reasoning summaries, `mcp_tool_call` (Forge's tools) and `command_execution` (codex's own
/// read-only shell). Field-tolerant: unknown shapes are ignored so CLI drift degrades gracefully.
fn parse_codex_line(line: &str) -> Vec<Parsed> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        // A tool call begins (mcp__forge tool or codex's own read-only shell).
        Some("item.started") => {
            let item = v.get("item");
            match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                Some("mcp_tool_call") => vec![codex_tool_started(item)],
                Some("command_execution") => vec![codex_command_started(item)],
                _ => Vec::new(),
            }
        }
        Some("item.completed") => {
            let item = v.get("item");
            let kind = item.and_then(|i| i.get("type")).and_then(Value::as_str);
            let id = || codex_item_id(item);
            match kind {
                Some("agent_message") => codex_item_text(item)
                    .map(Parsed::Text)
                    .into_iter()
                    .collect(),
                Some("reasoning") => codex_item_text(item)
                    .map(Parsed::Reasoning)
                    .into_iter()
                    .collect(),
                Some("mcp_tool_call") => {
                    let ok = item.and_then(|i| i.get("status")).and_then(Value::as_str)
                        != Some("failed");
                    let summary = item
                        .and_then(|i| i.get("error"))
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            item.and_then(|i| i.get("tool"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_default();
                    vec![Parsed::ToolFinished {
                        id: id(),
                        ok,
                        summary,
                    }]
                }
                Some("command_execution") => {
                    let ok = item
                        .and_then(|i| i.get("exit_code"))
                        .and_then(Value::as_i64)
                        == Some(0);
                    let summary = item
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .chars()
                        .take(120)
                        .collect();
                    vec![Parsed::ToolFinished {
                        id: id(),
                        ok,
                        summary,
                    }]
                }
                _ => Vec::new(),
            }
        }
        Some("turn.completed") => v
            .get("usage")
            .map(usage_from)
            .map(Parsed::Usage)
            .into_iter()
            .collect(),
        Some(t) if t.contains("error") || t.contains("failed") => vec![Parsed::Error(
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or("codex reported an error")
                .to_string(),
        )],
        _ => Vec::new(),
    }
}

fn parse_line(kind: CliKind, line: &str) -> Vec<Parsed> {
    match kind {
        CliKind::ClaudeCode => parse_claude_line(line),
        CliKind::Codex => parse_codex_line(line),
    }
}

#[async_trait]
impl Provider for CliProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        _tools: &[ToolSpec], // harness mode serves Forge's tools via `forge mcp-serve`, which
        // builds its own registry — not from this param; text mode uses the CLI's own tools.
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let prompt = render_prompt(messages);
        // Path to *this* forge binary, so harness mode can spawn `forge mcp-serve`.
        let forge_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "forge".to_string());
        let args = build_args(
            self.kind,
            bare_model(model),
            &prompt,
            self.harness,
            &forge_exe,
        );

        // Harness turns can spawn subagents inside `forge mcp-serve`; give it an out-of-band
        // JSONL sink to report their lifecycle so we can surface them in the TUI (Phase 3c).
        let sink_path: Option<std::path::PathBuf> = if self.harness {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("forge-subagents-{}-{n}.jsonl", std::process::id()));
            // Create it empty so the tailer can open it immediately.
            let _ = std::fs::File::create(&p);
            Some(p)
        } else {
            None
        };

        let mut cmd = Command::new(&self.binary);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(p) = &sink_path {
            cmd.env(SUBAGENT_SINK_ENV, p);
        }
        put_in_own_process_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Request(format!(
                    "`{}` not found — {}",
                    self.binary,
                    self.kind.setup_hint()
                ))
            } else {
                ProviderError::Request(format!("failed to start `{}`: {e}", self.binary))
            }
        })?;

        let pgid = child.id().map(|id| id as i32);
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let err_task = tokio::spawn(read_to_cap(stderr));

        // Tail the subagent sink concurrently (only in harness mode). Events arrive while the
        // CLI is silent waiting on the spawn_agents tool result, so they must be drained live.
        let (sub_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
        let tailer = match &sink_path {
            Some(p) => Some(tokio::spawn(tail_subagent_sink(p.clone(), sub_tx))),
            None => {
                drop(sub_tx); // no sink → close the channel so its select arm is disabled
                None
            }
        };

        let mut content = String::new();
        let mut final_text: Option<String> = None;
        let mut usage = Usage::default();
        let mut quota: Option<forge_types::QuotaHint> = None;
        let mut in_band_error: Option<String> = None;
        // tool_use id → name, so a later tool_result can be labelled.
        let mut tool_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        let read = tokio::time::timeout(self.timeout, async {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                tokio::select! {
                    // Bias toward the CLI's own output; subagent events are supplementary.
                    biased;
                    line = lines.next_line() => {
                        let Some(line) = line? else { break };
                        for item in parse_line(self.kind, &line) {
                            match item {
                                Parsed::Reasoning(t) => on_event(StreamEvent::Reasoning(t)),
                                Parsed::Text(t) => {
                                    content.push_str(&t);
                                    on_event(StreamEvent::Text(t));
                                }
                                Parsed::ToolStarted { id, name, args } => {
                                    tool_names.insert(id, name.clone());
                                    on_event(StreamEvent::ToolStarted { name, args });
                                }
                                Parsed::ToolFinished { id, ok, summary } => {
                                    let name = tool_names.get(&id).cloned().unwrap_or_default();
                                    on_event(StreamEvent::ToolFinished { name, ok, summary });
                                }
                                Parsed::Usage(u) => usage = u,
                                Parsed::Quota {
                                    window,
                                    status,
                                    resets_at,
                                    fraction,
                                } => {
                                    quota = Some(forge_types::QuotaHint {
                                        provider: self.kind.prefix().to_string(),
                                        window,
                                        status,
                                        resets_at,
                                        fraction_used: fraction,
                                    });
                                }
                                Parsed::Final(f) => final_text = Some(f),
                                Parsed::Error(e) => in_band_error = Some(e),
                            }
                        }
                    }
                    Some(ev) = sub_rx.recv() => on_event(ev),
                }
            }
            // Drain any subagent events that landed just before the CLI's stdout closed.
            while let Ok(ev) = sub_rx.try_recv() {
                on_event(ev);
            }
            Ok::<(), std::io::Error>(())
        })
        .await;

        if let Some(t) = tailer {
            t.abort();
        }
        if let Some(p) = &sink_path {
            let _ = std::fs::remove_file(p);
        }

        match read {
            Err(_elapsed) => {
                terminate(&mut child, pgid).await;
                return Err(ProviderError::Request(format!(
                    "`{}` timed out after {}s (killed)",
                    self.binary,
                    self.timeout.as_secs()
                )));
            }
            Ok(Err(e)) => {
                terminate(&mut child, pgid).await;
                return Err(ProviderError::Request(format!(
                    "reading `{}` output failed: {e}",
                    self.binary
                )));
            }
            Ok(Ok(())) => {}
        }

        let status = child.wait().await.ok();
        let stderr_text = err_task.await.unwrap_or_default();

        if let Some(e) = in_band_error {
            return Err(ProviderError::Request(format!(
                "{} error: {e}",
                self.binary
            )));
        }

        let text = if content.is_empty() {
            final_text.unwrap_or_default()
        } else {
            content
        };

        if text.is_empty() {
            let code = status.and_then(|s| s.code());
            if code != Some(0) {
                let tail = stderr_text.trim();
                let detail = if tail.is_empty() {
                    self.kind.setup_hint().to_string()
                } else {
                    tail.to_string()
                };
                return Err(ProviderError::Request(format!(
                    "`{}` exited with {} and no output — {} (is it authenticated? {})",
                    self.binary,
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    detail,
                    self.kind.setup_hint(),
                )));
            }
        }

        Ok(ModelResponse {
            content: text,
            tool_calls: Vec::new(),
            usage,
            quota,
        })
    }
}

/// Env var naming the out-of-band JSONL sink that `forge mcp-serve` writes subagent lifecycle
/// events to; the bridge sets it on the spawned CLI (inherited forge → claude → mcp-serve) and
/// tails it so bridge-spawned subagents surface in the TUI (RFC subagent-orchestration 3c).
pub const SUBAGENT_SINK_ENV: &str = "FORGE_SUBAGENT_SINK";

/// Parse one line of the subagent sink into a [`StreamEvent`]. Field-tolerant.
fn parse_sink_line(line: &str) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match v.get("k").and_then(Value::as_str)? {
        "start" => Some(StreamEvent::SubagentStarted {
            id: s("id"),
            agent: s("agent"),
            task: s("task"),
        }),
        "progress" => Some(StreamEvent::SubagentProgress {
            id: s("id"),
            snippet: s("snippet"),
        }),
        "done" => Some(StreamEvent::SubagentFinished {
            id: s("id"),
            agent: s("agent"),
            ok: v.get("ok").and_then(Value::as_bool).unwrap_or(true),
            summary: s("summary"),
            cost_usd: v.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
        }),
        _ => None,
    }
}

/// Tail the subagent sink file, forwarding each event over `tx` as it is appended. Runs until
/// aborted by the caller (after the CLI process exits). Tolerant of the file not existing yet.
async fn tail_subagent_sink(
    path: std::path::PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) {
    use tokio::io::AsyncBufReadExt;
    // Wait for the file to appear (mcp-serve creates/opens it on first write).
    let file = loop {
        match tokio::fs::File::open(&path).await {
            Ok(f) => break f,
            Err(_) => tokio::time::sleep(Duration::from_millis(40)).await,
        }
    };
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = String::new();
    loop {
        match reader.read_line(&mut buf).await {
            Ok(0) => tokio::time::sleep(Duration::from_millis(40)).await, // EOF: await more
            Ok(_) => {
                if let Some(ev) = parse_sink_line(buf.trim()) {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                buf.clear();
            }
            Err(_) => break,
        }
    }
}

async fn read_to_cap<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = r.read(&mut chunk).await {
        if n == 0 || buf.len() >= STDERR_CAP {
            break;
        }
        let take = n.min(STDERR_CAP - buf.len());
        buf.extend_from_slice(&chunk[..take]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn put_in_own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

async fn terminate(child: &mut Child, pgid: Option<i32>) {
    #[cfg(unix)]
    {
        if let Some(pg) = pgid {
            unsafe { libc::kill(-pg, libc::SIGTERM) };
            tokio::time::sleep(KILL_GRACE).await;
            unsafe { libc::kill(-pg, libc::SIGKILL) };
        }
        let _ = child.wait().await;
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_on_path_detects_present_and_absent() {
        // A ubiquitous binary resolves; a nonsense one does not. (PATH-based, no spawning.)
        let real = if cfg!(windows) { "cmd.exe" } else { "sh" };
        assert!(binary_on_path(real), "{real} should be on PATH");
        assert!(!binary_on_path("forge-definitely-not-a-real-binary-zzz"));
    }

    #[test]
    fn default_model_id_is_the_bare_prefix() {
        assert_eq!(CliKind::ClaudeCode.default_model_id(), "claude-cli::");
        assert_eq!(CliKind::Codex.default_model_id(), "codex-cli::");
    }

    #[test]
    fn default_models_span_tiers_and_namespace_under_the_prefix() {
        // Each bridge exposes more than one model so the mesh can size a turn (haiku/mini ↔ opus).
        let claude = CliKind::ClaudeCode.default_models();
        assert!(
            claude.contains(&"opus") && claude.contains(&"haiku"),
            "{claude:?}"
        );
        let codex = CliKind::Codex.default_models();
        assert!(
            codex.contains(&"gpt-5.5") && codex.contains(&"gpt-5.4-mini"),
            "{codex:?}"
        );
        // A model alias namespaces into a full Forge id under the bridge prefix.
        assert_eq!(
            format!("{}::{}", CliKind::ClaudeCode.prefix(), claude[0]),
            "claude-cli::opus"
        );
    }

    #[test]
    fn sink_lines_parse_into_subagent_events() {
        match parse_sink_line(r#"{"k":"start","id":"x1","agent":"reviewer","task":"review"}"#) {
            Some(StreamEvent::SubagentStarted { id, agent, task }) => {
                assert_eq!(
                    (id.as_str(), agent.as_str(), task.as_str()),
                    ("x1", "reviewer", "review")
                );
            }
            other => panic!("expected SubagentStarted, got {other:?}"),
        }
        assert_eq!(
            parse_sink_line(r#"{"k":"progress","id":"x1","snippet":"reading"}"#),
            Some(StreamEvent::SubagentProgress {
                id: "x1".into(),
                snippet: "reading".into()
            })
        );
        match parse_sink_line(
            r#"{"k":"done","id":"x1","agent":"reviewer","ok":true,"summary":"2 issues","cost":0.01}"#,
        ) {
            Some(StreamEvent::SubagentFinished {
                ok,
                cost_usd,
                summary,
                ..
            }) => {
                assert!(ok && (cost_usd - 0.01).abs() < 1e-9 && summary == "2 issues");
            }
            other => panic!("expected SubagentFinished, got {other:?}"),
        }
        assert!(parse_sink_line("not json").is_none());
        assert!(parse_sink_line(r#"{"k":"unknown"}"#).is_none());
    }

    #[test]
    fn bare_model_strips_cli_prefix() {
        assert_eq!(bare_model("claude-cli::sonnet"), "sonnet");
        assert_eq!(bare_model("codex-cli::gpt-5-codex"), "gpt-5-codex");
        assert_eq!(bare_model("claude-cli"), "");
    }

    #[test]
    fn claude_harness_args_route_tools_through_forge_mcp() {
        let args = build_args(
            CliKind::ClaudeCode,
            "sonnet",
            "hi there",
            true,
            "/bin/forge",
        );
        // Forge owns the tools: strict MCP + only mcp__forge tools.
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(args.contains(&"mcp__forge".to_string()));
        // bypassPermissions must NOT be set — it overrides the allowlist and re-enables built-ins.
        assert!(!args.iter().any(|a| a == "bypassPermissions"));
        let mc = args.iter().position(|a| a == "--mcp-config").unwrap();
        assert!(args[mc + 1].contains("mcp-serve") && args[mc + 1].contains("/bin/forge"));
        // Boundary: no credential/auth material in the argv.
        assert!(!args
            .iter()
            .any(|a| a.contains("API_KEY") || a.contains("token")));
    }

    #[test]
    fn claude_text_mode_runs_a_self_agent_with_accept_edits() {
        let args = build_args(
            CliKind::ClaudeCode,
            "sonnet",
            "hi there",
            false,
            "/bin/forge",
        );
        assert!(!args.iter().any(|a| a == "--strict-mcp-config"));
        let i = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[i + 1], "acceptEdits");
        assert!(args.contains(&"--model".to_string()) && args.contains(&"sonnet".to_string()));
    }

    #[test]
    fn codex_harness_args_wire_forge_mcp_and_approve_tools() {
        let args = build_args(CliKind::Codex, "", "do a thing", true, "/bin/forge");
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        // Sandbox stays read-only: codex's OWN shell can't write, so every write/side-effect
        // can only go through Forge's gated mcp__forge tools.
        assert!(args.contains(&"read-only".to_string()));
        // NEVER bypass the sandbox/approvals — that would unsandbox codex's own shell.
        assert!(!args.iter().any(|a| a.contains("dangerously-bypass")));
        // Forge MCP server wired via -c overrides, and its tools auto-approved ("approve",
        // not "auto" — verified live against codex 0.130: "auto" still cancels).
        let joined = args.join(" ");
        assert!(joined.contains("mcp_servers.forge.command=\"/bin/forge\""));
        assert!(joined.contains("mcp_servers.forge.args=[\"mcp-serve\"]"));
        assert!(joined.contains("mcp_servers.forge.default_tools_approval_mode=\"approve\""));
        // approval_policy never so codex doesn't block on stdin prompts headless.
        assert!(joined.contains("approval_policy=\"never\""));
        // reasoning summaries on so codex emits reasoning items (streamed thinking).
        assert!(joined.contains("model_reasoning_summary"));
        assert_eq!(args.last().unwrap(), "do a thing");
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn codex_text_mode_is_a_plain_read_only_agent() {
        let args = build_args(CliKind::Codex, "", "do a thing", false, "/bin/forge");
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"read-only".to_string()));
        // text mode does NOT wire the Forge MCP server.
        assert!(!args.join(" ").contains("mcp_servers.forge"));
        assert_eq!(args.last().unwrap(), "do a thing");
    }

    #[test]
    fn render_prompt_puts_system_first_then_transcript() {
        let msgs = vec![
            Message::system("be terse"),
            Message::user("hello"),
            Message::assistant("hi"),
            Message::user("explain x"),
        ];
        let p = render_prompt(&msgs);
        assert!(p.starts_with("be terse"));
        assert!(p.contains("User: hello"));
        assert!(p.contains("Assistant: hi"));
        assert!(p.ends_with("User: explain x"));
    }

    // --- Parser fixtures: real-shaped lines from claude 2.1.177 / codex-cli 0.130.0 ---

    fn texts(items: &[Parsed]) -> String {
        items
            .iter()
            .filter_map(|p| match p {
                Parsed::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn claude_assistant_text_block_yields_text() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello world"}]}}"#;
        assert_eq!(texts(&parse_claude_line(line)), "hello world");
    }

    #[test]
    fn claude_thinking_block_yields_reasoning() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"let me think"}]}}"#;
        assert_eq!(
            parse_claude_line(line),
            vec![Parsed::Reasoning("let me think".into())]
        );
    }

    #[test]
    fn claude_tool_use_and_result_round_trip() {
        let use_line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"path":"x"}}]}}"#;
        match &parse_claude_line(use_line)[0] {
            Parsed::ToolStarted { id, name, args } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "Read");
                assert!(args.contains("\"path\""));
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        let res_line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"file body"}]}}"#;
        assert_eq!(
            parse_claude_line(res_line),
            vec![Parsed::ToolFinished {
                id: "t1".into(),
                ok: true,
                summary: "file body".into()
            }]
        );
    }

    #[test]
    fn claude_result_line_yields_usage_and_final_text_zero_cost() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","total_cost_usd":0.125,"usage":{"input_tokens":16612,"output_tokens":4}}"#;
        let items = parse_claude_line(line);
        assert!(items.contains(&Parsed::Usage(Usage {
            input_tokens: 16612,
            output_tokens: 4,
            cost_usd: 0.0
        })));
        assert!(items.contains(&Parsed::Final("hello".into())));
    }

    #[test]
    fn claude_system_and_noise_lines_are_ignored() {
        assert!(parse_claude_line(r#"{"type":"system","subtype":"init"}"#).is_empty());
        assert!(parse_claude_line(r#"{"type":"rate_limit_event"}"#).is_empty());
        assert!(parse_claude_line("not json at all").is_empty());
    }

    #[test]
    fn claude_rate_limit_event_parses_into_a_quota_hint() {
        use forge_types::QuotaStatus;
        // The documented shape: status "allowed", overage not in use → Ok.
        let ok = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1781485800,"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false}}"#;
        match &parse_claude_line(ok)[0] {
            Parsed::Quota {
                window,
                status,
                resets_at,
                ..
            } => {
                assert_eq!(window, "five_hour");
                assert_eq!(*status, QuotaStatus::Ok);
                assert_eq!(*resets_at, Some(1781485800));
            }
            other => panic!("expected Quota, got {other:?}"),
        }
        // Using overage → Warning (near/over the included limit).
        let warn = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"weekly","isUsingOverage":true}}"#;
        assert!(matches!(
            &parse_claude_line(warn)[0],
            Parsed::Quota {
                status: QuotaStatus::Warning,
                ..
            }
        ));
        // A high used-fraction → Exhausted.
        let full = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","usedFraction":0.99}}"#;
        assert!(matches!(
            &parse_claude_line(full)[0],
            Parsed::Quota {
                status: QuotaStatus::Exhausted,
                ..
            }
        ));
    }

    #[test]
    fn claude_error_result_is_surfaced() {
        let line = r#"{"type":"result","is_error":true,"api_error_status":"overloaded"}"#;
        assert!(parse_claude_line(line).contains(&Parsed::Error("overloaded".into())));
    }

    #[test]
    fn codex_agent_message_yields_text_and_reasoning() {
        let msg = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}"#;
        assert_eq!(texts(&parse_codex_line(msg)), "hello");
        let reasoning =
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#;
        assert_eq!(
            parse_codex_line(reasoning),
            vec![Parsed::Reasoning("thinking".into())]
        );
    }

    #[test]
    fn codex_mcp_tool_call_round_trips_started_and_finished() {
        let started = r#"{"type":"item.started","item":{"id":"item_2","type":"mcp_tool_call","server":"forge","tool":"read_file","arguments":{"path":"Cargo.toml"},"status":"in_progress"}}"#;
        match &parse_codex_line(started)[0] {
            Parsed::ToolStarted { id, name, args } => {
                assert_eq!(id, "item_2");
                assert_eq!(name, "read_file");
                assert!(args.contains("Cargo.toml"));
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        let done = r#"{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"forge","tool":"read_file","status":"completed"}}"#;
        assert_eq!(
            parse_codex_line(done),
            vec![Parsed::ToolFinished {
                id: "item_2".into(),
                ok: true,
                summary: "read_file".into()
            }]
        );
        let failed = r#"{"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"forge","tool":"write_file","status":"failed","error":{"message":"denied by Forge permission policy: write_file"}}}"#;
        assert_eq!(
            parse_codex_line(failed),
            vec![Parsed::ToolFinished {
                id: "item_3".into(),
                ok: false,
                summary: "denied by Forge permission policy: write_file".into()
            }]
        );
    }

    #[test]
    fn codex_command_execution_round_trips_as_shell_tool() {
        let started = r#"{"type":"item.started","item":{"id":"item_7","type":"command_execution","command":"rg foo","status":"in_progress"}}"#;
        match &parse_codex_line(started)[0] {
            Parsed::ToolStarted { name, args, .. } => {
                assert_eq!(name, "shell");
                assert!(args.contains("rg foo"));
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        let done = r#"{"type":"item.completed","item":{"id":"item_7","type":"command_execution","command":"rg foo","exit_code":0,"status":"completed"}}"#;
        assert_eq!(
            parse_codex_line(done),
            vec![Parsed::ToolFinished {
                id: "item_7".into(),
                ok: true,
                summary: "rg foo".into()
            }]
        );
    }

    #[test]
    fn codex_turn_completed_yields_usage_zero_cost() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":16927,"output_tokens":23}}"#;
        assert_eq!(
            parse_codex_line(line),
            vec![Parsed::Usage(Usage {
                input_tokens: 16927,
                output_tokens: 23,
                cost_usd: 0.0
            })]
        );
    }

    #[test]
    fn codex_lifecycle_lines_are_ignored() {
        assert!(parse_codex_line(r#"{"type":"thread.started","thread_id":"x"}"#).is_empty());
        assert!(parse_codex_line(r#"{"type":"turn.started"}"#).is_empty());
    }

    #[tokio::test]
    async fn missing_binary_is_a_clean_error_not_a_hang() {
        let provider = CliProvider::claude_code().with_binary("forge-no-such-cli-xyz");
        let mut on_event = |_: StreamEvent| {};
        let err = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect_err("missing binary must error");
        let ProviderError::Request(msg) = err else {
            panic!("expected Request, got {err:?}");
        };
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(
            msg.contains("log in"),
            "should hint how to authenticate: {msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_thinking_text_and_tools_from_a_faked_claude_stream() {
        // A fake `claude` emitting thinking + a tool use/result + text, ignoring its args.
        let fake = make_fake_cli(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"planning"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"path":"x"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"body"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"the answer"}]}}
{"type":"result","is_error":false,"result":"the answer","usage":{"input_tokens":5,"output_tokens":3}}"#,
        );
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut on_event = |ev: StreamEvent| events.push(ev);
        let res = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect("fake stream parses");

        assert!(events.contains(&StreamEvent::Reasoning("planning".into())));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolStarted { name, .. } if name == "Read")));
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::ToolFinished { name, ok, .. } if name == "Read" && *ok)
        ));
        assert!(events.contains(&StreamEvent::Text("the answer".into())));
        assert_eq!(res.content, "the answer", "content is the answer text only");
        assert_eq!(res.usage.input_tokens, 5);
        assert_eq!(res.usage.cost_usd, 0.0);
        assert!(res.tool_calls.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_with_no_output_reports_auth_hint() {
        let fake = make_fake_cli_exit("", 1);
        let provider = CliProvider::codex().with_binary(&fake);
        let mut on_event = |_: StreamEvent| {};
        let err = provider
            .complete(
                "codex-cli::gpt-5",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect_err("nonzero exit, no output → error");
        let ProviderError::Request(msg) = err else {
            panic!("expected Request, got {err:?}");
        };
        assert!(msg.contains("authenticated"), "got: {msg}");
    }

    // Write an executable shell script that prints `stdout` then exits 0.
    #[cfg(unix)]
    fn make_fake_cli(stdout: &str) -> String {
        make_fake_cli_exit(stdout, 0)
    }

    #[cfg(unix)]
    fn make_fake_cli_exit(stdout: &str, code: i32) -> String {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        // Unique path per call without external deps.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("forge-fake-cli-{}-{n}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        // Use a heredoc-free script: printf the payload (escaped) then exit.
        writeln!(f, "#!/bin/sh").unwrap();
        write!(f, "cat <<'FORGE_EOF'\n{stdout}\nFORGE_EOF\n").unwrap();
        writeln!(f, "exit {code}").unwrap();
        f.sync_all().unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        // Wait out ETXTBSY: a *concurrent* test's fork can briefly inherit this file's open
        // write-fd, so exec'ing it would transiently fail with "Text file busy". Probe-exec
        // (retrying past ETXTBSY) until the OS lets us run it — once it does, no writer holds
        // the file, so the provider's real spawn won't flake.
        for _ in 0..200 {
            match std::process::Command::new(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait();
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        path.to_string_lossy().into_owned()
    }
}
