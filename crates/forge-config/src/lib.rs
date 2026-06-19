//! Layered configuration (defaults -> user file -> project file -> `FORGE_*` env) and
//! secret resolution. Secrets are never part of the config surface (ADR-0007): API keys
//! come from environment variables first, then the OS keyring (`forge auth`).

use std::collections::HashMap;
use std::path::PathBuf;

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use forge_types::{
    CreditMode, PermissionDecision, PermissionMode, PermissionRule, RuleSource, TaskTier,
};
use serde::{Deserialize, Serialize};

pub mod agents;
pub mod mcp;
pub mod oauth;
pub mod secret_store;
pub use agents::{load_agents, AgentDef};
pub use mcp::{
    discover_import_sources, import_mcp_json, load_mcp_toml, write_mcp_toml, ImportSource,
    McpAllowlist, McpAuth, McpConfig, McpServerConfig, McpTransport, ParsedServers,
};
pub use oauth::{
    authorize_url, clear_oauth_tokens, load_oauth_tokens, oauth_keyring_key, random_state,
    store_oauth_tokens, AuthServerMetadata, OAuthConfig, OAuthTokens, Pkce,
    ProtectedResourceMetadata,
};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(Box<figment::Error>),
    #[error("no API key found for provider '{0}' (set {1} or run `forge auth {0}`)")]
    MissingKey(String, String),
    #[error("keyring error: {0}")]
    Keyring(String),
    #[error("no per-user config directory available on this platform")]
    NoConfigDir,
    #[error("writing config failed: {0}")]
    Write(String),
}

impl From<figment::Error> for ConfigError {
    fn from(e: figment::Error) -> Self {
        ConfigError::Load(Box::new(e))
    }
}

/// The fully resolved Forge configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Default permission posture for new sessions (ADR-0008).
    pub permission_mode: PermissionMode,
    /// Model Mesh settings (ADR-0006).
    pub mesh: MeshConfig,
    /// Fine-grained allow/ask/deny rules layered on top of the mode (FR-10).
    #[serde(default)]
    pub permissions: PermissionsConfig,
    /// External MCP servers Forge connects to as a client (mcp-client.md). Empty = inert.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Slash-command + skill discovery and trust (command-skill-system.md).
    #[serde(default)]
    pub commands: CommandsConfig,
    /// Native code-intelligence graph (code-intelligence.md).
    #[serde(default)]
    pub lattice: LatticeConfig,
    /// Shell tool behaviour, incl. the AI error interceptor (shell-error-interceptor.md).
    #[serde(default)]
    pub shell: ShellConfig,
    /// Pre/post tool-use shell hooks (hooks.md). Each `[[hooks]]` entry runs a command around a
    /// matching tool call.
    #[serde(default)]
    pub hooks: Vec<HookConfig>,
    /// Git integration settings (co-authoring, hook installation).
    #[serde(default)]
    pub git: GitConfig,
    /// LSP-backed live diagnostics fed back into the turn after edits (lsp.md). Off = inert.
    #[serde(default)]
    pub lsp: LspConfig,
    /// Auto-lint / auto-test self-healing loop after edits (autofix.md). Off = inert.
    #[serde(default)]
    pub autofix: AutofixConfig,
    /// Assay-gated auto-review of write turns before they finish (assay-gate.md). Off = inert.
    #[serde(default)]
    pub assay: AssayConfig,
}

/// When a hook fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before the tool runs; a non-zero exit blocks the call (stderr/stdout = the reason).
    PreToolUse,
    /// After the tool returns; output is surfaced as a note, exit code is advisory.
    PostToolUse,
    /// After the user submits a message, before the agent turn starts.
    /// Hook receives `{"prompt": "<user message>"}` on stdin.
    /// Exit 0 + non-empty stdout → stdout replaces the prompt.
    /// Exit non-zero → turn is blocked; stderr/stdout shown as the reason.
    UserPromptSubmit,
    /// When a session starts (first turn or resume). Observe-only — exit code advisory.
    /// Receives `{"session_id": "<id>", "event": "session_start"}` on stdin.
    SessionStart,
    /// When the session loop exits cleanly. Observe-only.
    /// Receives `{"session_id": "<id>", "event": "session_end"}` on stdin.
    SessionEnd,
}

/// One `[[hooks]]` entry: a shell command run around tool calls matching `matcher`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub event: HookEvent,
    /// Tool-name filter for `pre_tool_use` / `post_tool_use`: absent or `"*"` = every tool;
    /// otherwise a comma-separated list of exact tool names (e.g. `"shell"`).
    /// Ignored for `user_prompt_submit`, `session_start`, `session_end`.
    #[serde(default)]
    pub matcher: Option<String>,
    /// Shell command line. Receives event data as JSON on stdin.
    pub command: String,
    /// Kill the hook after this many seconds (default 30).
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

fn default_hook_timeout() -> u64 {
    30
}

impl HookConfig {
    /// Whether this hook applies to `tool_name`.
    pub fn matches(&self, tool_name: &str) -> bool {
        match self.matcher.as_deref() {
            None | Some("") | Some("*") => true,
            Some(list) => list.split(',').any(|m| m.trim() == tool_name),
        }
    }
}

/// Settings for the Lattice code-intelligence subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatticeConfig {
    /// Build + maintain the structural graph. Off → the `forge lattice` commands are inert.
    #[serde(default = "default_lattice_enabled")]
    pub enabled: bool,
    /// Auto-inject relevant code into each turn (the killer step). Off → the index still builds
    /// and the CLI/tool work, but no system message is injected before the model call.
    #[serde(default = "default_lattice_inject")]
    pub inject: bool,
    /// Token ceiling for the auto-injected context block. Scaled down as the daily budget tightens.
    #[serde(default = "default_inject_budget")]
    pub inject_token_budget: usize,
    /// Watch the working tree and reindex changed files automatically (external editor edits), so
    /// retrieval stays fresh without a manual `forge lattice update`. Off → index only updates on
    /// explicit `update`/agent edits.
    #[serde(default = "default_lattice_watch")]
    pub watch: bool,
    /// Semantic retrieval via embeddings (code-intelligence.md §5.6). On by default with
    /// `backend = "auto"`: embeddings are computed in the background when a backend is reachable
    /// and blended into retrieval, and it's a no-op (zero cost, structural-only) when none is.
    #[serde(default)]
    pub embeddings: EmbeddingsConfig,
    /// Inject the *source body* of the top-ranked retrieved symbols (not just their signature),
    /// so the model can read a function directly from context instead of spending a `read_file`
    /// (which dumps the whole file into the transcript). The single biggest token-saving lever —
    /// see docs/features/lattice-token-savings.md. Off → signature-only (legacy behaviour).
    #[serde(default = "default_inject_bodies")]
    pub inject_bodies: bool,
    /// Per-symbol token ceiling for an injected body. Symbols whose body exceeds this are kept as
    /// a signature line instead (injecting a huge body would cost more than the read it saves).
    #[serde(default = "default_body_max_tokens")]
    pub body_max_tokens: usize,
    /// Future hook for `forge lattice map`: when true, group the map output by importance tier
    /// (high / medium / low pagerank bands) rather than by file path. Not yet wired into the
    /// agent turn loop — present so it can be set in config ahead of the feature landing.
    #[serde(default)]
    pub map_orientation: bool,
}

/// Embedding-backed semantic retrieval settings. On by default with `backend = "auto"`, which
/// picks the cheapest available backend and is a zero-cost no-op when none is reachable; node
/// embeddings are computed via the chosen backend and blended into retrieval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    /// Master switch. Off → no embeddings are computed or used; retrieval is structural/lexical.
    #[serde(default)]
    pub enabled: bool,
    /// Embedding backend: `"auto"` (default — pick the cheapest available, see below), `"ollama"`
    /// (local HTTP), or a provider namespace genai can embed with (`"openai"`, `"gemini"`). `auto`
    /// prefers a free/cheap cloud model when a key exists (gemini free-tier → openai), else falls
    /// back to local ollama; if nothing is reachable it costs nothing (retrieval stays structural).
    #[serde(default = "default_embed_backend")]
    pub backend: String,
    /// Embedding model id. Used as-is for `ollama`/explicit-provider backends (e.g. ollama's
    /// `nomic-embed-text`); ignored under `auto`, which picks the model per chosen provider.
    #[serde(default = "default_embed_model")]
    pub model: String,
    /// ollama HTTP API root (only used by the ollama backend / auto's local fallback).
    #[serde(default = "default_embed_endpoint")]
    pub endpoint: String,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: default_embed_backend(),
            model: default_embed_model(),
            endpoint: default_embed_endpoint(),
        }
    }
}

fn default_embed_backend() -> String {
    "auto".to_string()
}

fn default_embed_model() -> String {
    "nomic-embed-text".to_string()
}

fn default_embed_endpoint() -> String {
    "http://localhost:11434".to_string()
}

impl Default for LatticeConfig {
    fn default() -> Self {
        Self {
            enabled: default_lattice_enabled(),
            inject: default_lattice_inject(),
            inject_token_budget: default_inject_budget(),
            watch: default_lattice_watch(),
            embeddings: EmbeddingsConfig::default(),
            inject_bodies: default_inject_bodies(),
            body_max_tokens: default_body_max_tokens(),
            map_orientation: false,
        }
    }
}

fn default_inject_bodies() -> bool {
    true
}

fn default_body_max_tokens() -> usize {
    800
}

fn default_lattice_watch() -> bool {
    true
}

fn default_lattice_enabled() -> bool {
    true
}

fn default_lattice_inject() -> bool {
    true
}

fn default_inject_budget() -> usize {
    // Sized so the top few symbol *bodies* fit (body injection is the token-saving lever): a body
    // costs up to `body_max_tokens` (~800) and we inject up to 3, so ~3000 covers bodies + a tail
    // of signature lines. Prompt-adaptive scaling in retrieval avoids over-injecting on simple
    // prompts. See docs/features/lattice-token-savings.md.
    3000
}

/// Settings for the `shell` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellConfig {
    /// When a shell command fails (non-zero exit, timeout, spawn error), make one cheap
    /// trivial-tier model call to explain the likely cause and suggest a fix, surfaced
    /// alongside the result — no prompt needed (shell-error-interceptor.md). Skipped when the
    /// budget is exhausted.
    #[serde(default = "default_explain_errors")]
    pub explain_errors: bool,
    /// Opt-in OS-level sandbox using Linux Landlock (Linux 5.13+). When true, shell commands
    /// run under a kernel-enforced ruleset that confines filesystem **writes** to the workspace
    /// (cwd) and the system temp directory; reads stay broad so tools continue to work. A clean
    /// no-op on macOS/Windows and on Linux kernels without Landlock support — never hard-fails
    /// a command. Default: false (opt-in).
    ///
    /// TOML key: `shell.sandbox`
    #[serde(default)]
    pub sandbox: bool,
    /// Extra writable paths beyond the workspace (cwd) and the system temp directory. Each
    /// entry is an absolute path (relative entries are resolved against cwd at the time the
    /// shell command runs). Ignored when `sandbox = false`.
    ///
    /// TOML key: `shell.sandbox_writable`
    #[serde(default)]
    pub sandbox_writable: Vec<String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            explain_errors: default_explain_errors(),
            sandbox: false,
            sandbox_writable: Vec::new(),
        }
    }
}

fn default_explain_errors() -> bool {
    true
}

/// Git integration settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitConfig {
    /// When true, `forge git setup` installs a prepare-commit-msg hook that strips
    /// Claude/Codex co-author lines and adds `Co-Authored-By: Forge <noreply@forge.dev>`.
    #[serde(default)]
    pub coauthor: bool,
}

/// LSP-backed live diagnostics. After an edit, Forge asks a language server for diagnostics on the
/// touched file and feeds any errors back into the turn so the model self-corrects. Off = inert;
/// when no server binary is on PATH the path degrades silently to no diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspConfig {
    #[serde(default)]
    pub enabled: bool,
    /// How long to wait for a server's `publishDiagnostics` before giving up.
    #[serde(default = "default_lsp_timeout_ms")]
    pub timeout_ms: u64,
    /// Per-language server command, keyed by language id (`rust`, `typescript`, `python`, …).
    /// Empty = use the built-in defaults for languages whose server binary is found on PATH.
    #[serde(default)]
    pub servers: std::collections::HashMap<String, LspServerEntry>,
}

/// One language server invocation: a binary plus extra args (e.g. `["--stdio"]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerEntry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_ms: default_lsp_timeout_ms(),
            servers: std::collections::HashMap::new(),
        }
    }
}

fn default_lsp_timeout_ms() -> u64 {
    3000
}

/// Auto-lint / auto-test self-healing. After a turn makes edits, run the configured lint and/or
/// test command; on a non-zero exit, feed the output back into the turn so the model fixes it,
/// looping up to `max_iterations` times. Off = inert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutofixConfig {
    #[serde(default)]
    pub auto_lint: bool,
    #[serde(default)]
    pub auto_test: bool,
    /// Lint command line (e.g. `cargo clippy --all-targets` / `npm run lint`). Empty disables lint.
    #[serde(default)]
    pub lint_cmd: String,
    /// Test command line (e.g. `cargo test` / `pytest -x`). Empty disables test.
    #[serde(default)]
    pub test_cmd: String,
    /// Max fix attempts per turn before giving up and surfacing the remaining failures.
    #[serde(default = "default_autofix_iterations")]
    pub max_iterations: u32,
}

impl Default for AutofixConfig {
    fn default() -> Self {
        Self {
            auto_lint: false,
            auto_test: false,
            lint_cmd: String::new(),
            test_cmd: String::new(),
            max_iterations: default_autofix_iterations(),
        }
    }
}

fn default_autofix_iterations() -> u32 {
    3
}

/// Assay-gated auto-review: before a write turn finishes, run the Assay critic crew on the turn's
/// diff and warn (or block) on findings at/above `gate_severity`. Off = inert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssayConfig {
    #[serde(default)]
    pub auto_review: bool,
    /// Minimum severity that triggers the gate: `low` | `medium` | `high`.
    #[serde(default = "default_assay_gate_severity")]
    pub gate_severity: String,
    /// `warn` surfaces findings but lets the turn finish; `block` fails the turn.
    #[serde(default = "default_assay_gate_mode")]
    pub gate_mode: String,
    /// Skip review when the turn's diff is smaller than this (bytes) — trivial edits aren't worth it.
    #[serde(default = "default_assay_min_diff_bytes")]
    pub min_diff_bytes: usize,
}

impl Default for AssayConfig {
    fn default() -> Self {
        Self {
            auto_review: false,
            gate_severity: default_assay_gate_severity(),
            gate_mode: default_assay_gate_mode(),
            min_diff_bytes: default_assay_min_diff_bytes(),
        }
    }
}

fn default_assay_gate_severity() -> String {
    "high".to_string()
}

fn default_assay_gate_mode() -> String {
    "warn".to_string()
}

fn default_assay_min_diff_bytes() -> usize {
    200
}

/// Settings for the slash-command + skill system.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandsConfig {
    /// Trust project-scope (`./.forge/`) commands/skills without a first-run confirmation. A
    /// project file is a *prompt*, not an instruction to the harness — but it can still try to
    /// steer the model, so by default the first use of a project-scope definition is confirmed.
    #[serde(default)]
    pub trust_project: bool,
    /// Max rows the command palette shows at once before overflow.
    #[serde(default = "default_max_palette")]
    pub max_palette: usize,
}

impl Default for CommandsConfig {
    fn default() -> Self {
        Self {
            trust_project: false,
            max_palette: default_max_palette(),
        }
    }
}

fn default_max_palette() -> usize {
    8
}

/// Fine-grained permission rules (FR-10). Resolution is by specificity/precedence, not file
/// order; see `forge_core::permission`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

/// One TOML rule block: a tool plus exactly one of `allow`/`ask`/`deny` (string or list).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleConfig {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow: Option<OneOrMany>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<OneOrMany>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<OneOrMany>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A TOML scalar-or-array of strings (so `allow = "git *"` and `allow = ["a","b"]` both work).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s],
            OneOrMany::Many(v) => v,
        }
    }

    /// The first entry (the single value, or the head of the list). Empty `Many` → `""`.
    pub fn first(&self) -> &str {
        match self {
            OneOrMany::One(s) => s,
            OneOrMany::Many(v) => v.first().map(String::as_str).unwrap_or(""),
        }
    }

    /// All entries as owned strings (one-element vec for the single form).
    pub fn all(&self) -> Vec<String> {
        match self {
            OneOrMany::One(s) => vec![s.clone()],
            OneOrMany::Many(v) => v.clone(),
        }
    }
}

impl RuleConfig {
    /// Convert to a runtime rule. Deny is highest precedence if more than one is set.
    fn to_rule(&self) -> Option<PermissionRule> {
        let (decision, pats) = if let Some(d) = &self.deny {
            (PermissionDecision::Deny, d.clone())
        } else if let Some(a) = &self.ask {
            (PermissionDecision::Ask, a.clone())
        } else if let Some(a) = &self.allow {
            (PermissionDecision::Allow, a.clone())
        } else {
            return None; // a block with no decision is ignored
        };
        Some(PermissionRule {
            tool: self.tool.clone(),
            patterns: pats.into_vec(),
            decision,
            source: RuleSource::Configured,
            reason: self.reason.clone(),
        })
    }
}

/// Built-in safety deny rules — present even with zero config, unoverridable (`Builtin`),
/// active in every mode including `bypass`.
pub fn builtin_deny_rules() -> Vec<PermissionRule> {
    let deny = |tool: &str, pats: &[&str]| PermissionRule {
        tool: tool.to_string(),
        patterns: pats.iter().map(|s| s.to_string()).collect(),
        decision: PermissionDecision::Deny,
        source: RuleSource::Builtin,
        reason: Some("built-in safety rule".into()),
    };
    let secrets = [
        "**/.env",
        "**/*.pem",
        "**/id_rsa",
        "**/id_ed25519",
        "**/.ssh/**",
        "**/.aws/credentials",
        "**/.git-credentials",
    ];
    vec![
        deny(
            "shell",
            &[
                // catastrophic filesystem / disk (Unix)
                "rm -rf /",
                "rm -rf ~",
                "rm -rf /*",
                ":(){ :|:& };:",
                "dd of=/dev/*",
                "mkfs*",
                "mkfs.*",
                // catastrophic filesystem / disk (Windows)
                "del /s *",
                "del /f /s *",
                "del /q /s *",
                "rd /s *",
                "rmdir /s *",
                "format ?:*",
                // remote-to-shell pipe (matched against the raw command line)
                "*| sh",
                "*|sh",
                "*| bash",
                "*|bash",
                "*| zsh",
                "*|zsh",
                // Windows: catastrophic filesystem / disk (cmd.exe + PowerShell)
                "rd /s /q *\\",
                "rd /s /q /",
                "rmdir /s /q *\\",
                "rmdir /s /q /",
                "del /f /s /q *\\*",
                "format c:*",
                "format *: /q*",
                "Remove-Item -Recurse -Force /*",
                "Remove-Item -Recurse -Force C:\\*",
                "rm -Recurse -Force /*",
                // secret-file reads via common verbs
                "cat *.env",
                "cat *.pem",
                "cat *id_rsa*",
                "cat *id_ed25519*",
                "cat */.ssh/*",
                "cat *.aws/credentials*",
                "cat *.git-credentials*",
                "less *.env",
                "head *.env",
                "tail *.env",
                "cp *.env *",
                "cp */.ssh/* *",
                // Windows: secret-file reads via type/more
                "type *.env",
                "type *.pem",
                "type *id_rsa*",
                "type */.ssh/*",
                "more *.env",
                "copy *.env *",
            ],
        ),
        deny("read_file", &secrets),
        deny("list_dir", &secrets),
        deny("write_file", &["**/.ssh/**", "/etc/**"]),
        deny("edit_file", &["**/.ssh/**", "/etc/**"]),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    /// Tier -> model id, or an ordered list of candidate model ids for that tier. With a
    /// list, the router picks the cheapest *usable* candidate (cost-aware routing); a single
    /// string behaves as a one-element list (back-compat).
    pub models: HashMap<String, OneOrMany>,
    /// Prefer an already-paid subscription (the `claude-cli::`/`codex-cli::` bridges, $0
    /// marginal cost) over a metered API model when both are usable candidates. Default true.
    #[serde(default = "default_prefer_subscription")]
    pub prefer_subscription: bool,
    /// Daily spend cap in USD across all sessions (FR-5). `daily_cap_usd` is the preferred
    /// key; `daily_budget_usd` is kept as a backward-compatible alias.
    #[serde(alias = "daily_cap_usd")]
    pub daily_budget_usd: Option<f64>,
    /// Monthly spend cap in USD across all sessions. Absent = unlimited.
    #[serde(default)]
    pub monthly_cap_usd: Option<f64>,
    /// Weekly spend cap in USD (Monday 00:00 local → Sunday). Absent = unlimited.
    #[serde(default)]
    pub weekly_budget_usd: Option<f64>,
    /// Fraction of a cap that triggers a warning (default 0.8).
    #[serde(default = "default_warn_threshold")]
    pub warn_threshold: f64,
    /// Which task classifier the mesh uses (ADR-0006). Default = deterministic heuristic.
    #[serde(default)]
    pub classifier: ClassifierKind,
    /// Model id the `llm` classifier calls to label the tier (a cheap/$0 model, e.g. a local
    /// `ollama::` or a `claude-cli::`/`codex-cli::` subscription bridge). Ignored for the
    /// heuristic classifier. Falls back to the trivial-tier model when unset.
    #[serde(default)]
    pub classifier_model: Option<String>,
    /// How a CLI-bridge (`claude-cli::`/`codex-cli::`) turn runs (RFC cli-bridge-full-harness):
    /// `harness` (default) routes the model's tools through Forge's own MCP server + permission
    /// gate; `text` runs the CLI as its own agent with its own tools.
    #[serde(default)]
    pub bridge_mode: BridgeMode,
    /// Enforcement behavior once a cap is reached.
    #[serde(default)]
    pub budget: BudgetBehavior,
    /// Per-model pricing overrides (USD per 1k tokens), applied on top of bundled
    /// defaults so a price change needs no release (A-7). Keyed by model id.
    #[serde(default)]
    pub pricing: HashMap<String, PriceOverride>,
    /// Subagent orchestration (RFC subagent-orchestration): the `spawn_agents` tool.
    #[serde(default)]
    pub subagents: SubagentsConfig,
    /// Auto-discovery routing (docs/features/auto-discovery-mesh.md): when true (default), the
    /// mesh discovers the models the user can actually use and ranks the best per tier itself,
    /// rather than relying on the `[mesh.models]` lists. Set false to route strictly from the
    /// configured `[mesh.models]` candidates.
    #[serde(default = "default_auto_discover")]
    pub auto_discover: bool,
    /// Failover (docs/features/model-health-failover.md): when true (default), a model that
    /// errors with a retryable failure (rate-limit / unavailable / auth) is benched and the
    /// turn transparently retries on the next-best healthy model. Set false for single-shot.
    #[serde(default = "default_failover")]
    pub failover: bool,
    /// Default bench duration (seconds) when a rate-limited provider gives no `Retry-After`.
    #[serde(default = "default_failover_cooldown_secs")]
    pub failover_cooldown_secs: u64,
    /// Abort a model stream that goes silent for this many seconds (a half-open/stalled
    /// connection) and fail over, instead of hanging the turn forever. `0` disables the watchdog.
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    /// Max model↔tool rounds in a single turn. This is a *runaway guard*, not a functional limit:
    /// like Claude Code / Codex, the agent loop runs until the model stops calling tools — a turn
    /// should normally finish well under this. Hitting it pauses the turn with a visible warning
    /// (type `continue` to resume), never a silent stop. Raise for very long agentic turns.
    #[serde(default = "default_max_steps")]
    pub max_steps: usize,
    /// Proactively spread complex/standard tasks off the subscription bridges (claude-cli/
    /// codex-cli) onto the free-frontier pool, scaling with how full the weekly/session window is
    /// and how much headroom the plan has (subscription-conservation routing). When true (default)
    /// a fraction of complex/standard tasks route to a free frontier model even while the
    /// subscription is fresh, so a complex-heavy workload doesn't exhaust the plan. Set false for
    /// the old greedy behaviour (always the subscription flagship until the hard limit).
    #[serde(default = "default_subscription_conserve")]
    pub subscription_conserve: bool,
    /// Rank models on REAL measured performance (Artificial Analysis benchmark indices, ADR-0011)
    /// instead of the family-name heuristic, when benchmark data is available. Default true; a
    /// no-op without a cached dataset / API key (falls back to the heuristic).
    #[serde(default = "default_benchmark_ranking")]
    pub benchmark_ranking: bool,
    /// Which subscription plan backs each CLI bridge (`claude-cli` → "max-20x", `codex-cli` →
    /// "plus"), captured by `forge init`. Records the usage headroom the user has: the
    /// subscription-conservation layer reads it so a larger plan (more headroom) is spent more
    /// freely than a smaller one. Also shown by `forge init`/`forge models`.
    #[serde(default)]
    pub subscriptions: HashMap<String, String>,
    /// Override which models a CLI bridge exposes to auto-discovery, keyed by bridge prefix
    /// (`claude-cli` / `codex-cli`); each value is a list of model aliases/ids the CLI's `--model`
    /// flag accepts (e.g. `["opus","sonnet","haiku"]`). Empty/absent → the bridge's built-in
    /// defaults. The CLIs expose no machine-readable model list, so this is how a user pins the
    /// exact set; a stale alias just benches itself via failover.
    #[serde(default)]
    pub bridge_models: HashMap<String, Vec<String>>,
    /// Models/providers excluded from discovery + routing. Each entry is either a full model id
    /// (`provider::model`) or a bare provider prefix (`provider`, matching every `provider::*`).
    /// Use it to drop a flaky or unwanted model without deleting its key (known-issues.md). A
    /// disabled model never enters the catalog, so the mesh won't route to or fail over onto it.
    #[serde(default)]
    pub disabled: Vec<String>,
    /// Cap on the output tokens requested per completion. Providers otherwise default to a
    /// model's full max (often 65k), which a free / low-credit account can't afford — OpenRouter
    /// then returns HTTP 402 ("requested 65536 tokens, can only afford 669") and the model looks
    /// "down". Capping keeps free-tier models usable and bounds runaway generations. `0` = no cap
    /// (use the provider default).
    #[serde(default = "default_max_output_tokens")]
    pub max_output_tokens: u32,
    /// How aggressively to conserve metered API credits. Default = Normal (no restriction).
    /// Frugal caps output tokens at 2048; Strict caps at 1024 and routes to free/sub only.
    #[serde(default)]
    pub credit_mode: CreditMode,
    /// Architect mode (dual-model pipeline): when true, each turn runs a plan phase on a strong
    /// model then an apply phase on a cheaper one. Off = single-model turns (default).
    #[serde(default)]
    pub architect_mode: bool,
    /// Planner model id for architect mode. Empty → mesh-route at the Complex tier.
    #[serde(default)]
    pub architect_model: Option<String>,
    /// Editor (apply) model id for architect mode. Empty → mesh-route at the Standard tier.
    #[serde(default)]
    pub editor_model: Option<String>,
    /// Default reasoning effort sent to API providers (`low`|`medium`|`high`|`xhigh`). Absent →
    /// no effort param (provider default). Overridable per session with `/effort`.
    #[serde(default)]
    pub default_effort: Option<String>,
}

impl MeshConfig {
    /// Effective per-completion output token cap, accounting for credit_mode overrides.
    pub fn effective_max_output_tokens(&self) -> u32 {
        match self.credit_mode {
            CreditMode::Normal => self.max_output_tokens,
            CreditMode::Frugal => self.max_output_tokens.min(2048),
            CreditMode::Strict => self.max_output_tokens.min(1024),
        }
    }
}

/// Whether `model_id` is excluded by a `[mesh] disabled` list — exact id match or a bare provider
/// prefix matching `provider::*`. Pure so it's unit-testable.
pub fn is_model_disabled(model_id: &str, disabled: &[String]) -> bool {
    disabled.iter().any(|d| {
        let d = d.trim();
        !d.is_empty() && (model_id == d || model_id.starts_with(&format!("{d}::")))
    })
}

fn default_auto_discover() -> bool {
    true
}

/// Default per-completion output cap. 8192 is comfortably above any single agent step's real
/// output yet small enough that a free / low-credit account can afford it (avoids the 402 churn).
fn default_max_output_tokens() -> u32 {
    8192
}

/// Default per-turn step cap (runaway guard). 100 model↔tool rounds is far above what a normal
/// agentic turn needs — the loop ends naturally when the model stops calling tools — while still
/// bounding a model stuck in a tool-call loop. Configurable via `mesh.max_steps`.
fn default_max_steps() -> usize {
    100
}

fn default_subscription_conserve() -> bool {
    true
}

fn default_benchmark_ranking() -> bool {
    true
}

/// The Artificial Analysis Data API key (ADR-0011), for benchmark-driven ranking. Read from
/// `ARTIFICIALANALYSIS_API_KEY` first, then the `artificialanalysis` keyring/file entry. `None`
/// disables the live fetch (ranking falls back to a cached dataset, then the heuristic).
pub fn benchmark_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("ARTIFICIALANALYSIS_API_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    secret_store::get("artificialanalysis").filter(|k| !k.is_empty())
}

fn default_failover() -> bool {
    true
}

fn default_failover_cooldown_secs() -> u64 {
    300
}

fn default_stream_idle_timeout_secs() -> u64 {
    // Long enough to never trip during normal generation (incl. slow reasoning models and a
    // bridge running a slow tool), short enough to recover from a genuine stall in reasonable time.
    120
}

/// Subagent orchestration settings (RFC subagent-orchestration).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentsConfig {
    /// Advertise the `spawn_agents` tool to the top-level model. Default true.
    #[serde(default = "default_subagents_enabled")]
    pub enabled: bool,
    /// Max child agents per `spawn_agents` call (hard cap).
    #[serde(default = "default_max_agents")]
    pub max_agents: usize,
    /// Max child agents running concurrently (parallel fan-out is Phase 2).
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    /// How deep subagents may nest (1 = a top-level turn may spawn children, but those children
    /// may not spawn their own). Bounds total fan-out; the per-call `max_agents`/`max_concurrency`
    /// caps still apply at every level (RFC subagent-orchestration Phase 3c).
    #[serde(default = "default_max_depth")]
    pub max_depth: usize,
    /// Directory holding named agent-type files (`<name>.md`), relative to the cwd.
    #[serde(default = "default_agents_dir")]
    pub agents_dir: String,
    /// Give each write-capable child its own git worktree so concurrent edits can't corrupt the
    /// shared working tree; changes are merged back after the child finishes. Read-only children
    /// always skip this. Off by default (requires the repo to be a git work tree).
    #[serde(default)]
    pub worktree_isolation: bool,
}

fn default_subagents_enabled() -> bool {
    true
}
fn default_max_agents() -> usize {
    8
}
fn default_max_concurrency() -> usize {
    4
}
fn default_max_depth() -> usize {
    2
}
fn default_agents_dir() -> String {
    ".forge/agents".to_string()
}

impl Default for SubagentsConfig {
    fn default() -> Self {
        Self {
            enabled: default_subagents_enabled(),
            max_agents: default_max_agents(),
            max_concurrency: default_max_concurrency(),
            max_depth: default_max_depth(),
            agents_dir: default_agents_dir(),
            worktree_isolation: false,
        }
    }
}

fn default_warn_threshold() -> f64 {
    0.8
}

fn default_prefer_subscription() -> bool {
    true
}

/// How a CLI-bridge turn runs (RFC cli-bridge-full-harness).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum BridgeMode {
    /// Forge serves its own tools to the CLI via `forge mcp-serve` (MCP) and gates them with
    /// the permission engine; the CLI's built-in tools are disabled. The full Forge harness.
    #[default]
    Harness,
    /// The CLI runs as its own agent with its own tools (no Forge tools/permission gate).
    Text,
}

/// How the mesh decides a task's tier (ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClassifierKind {
    /// Deterministic weighted-signal heuristic — zero added cost/latency (default).
    #[default]
    Heuristic,
    /// Ask a cheap model to label the tier on every turn, falling back to the heuristic on
    /// any error. One extra round-trip per turn regardless of how obvious the task is.
    Llm,
    /// Best of both: run the heuristic first; only call the LLM when the heuristic score is
    /// near a tier boundary (score −3…7, i.e. the uncertain middle). Clear Trivial or
    /// strongly-signalled Complex tasks skip the LLM entirely — zero added latency for them.
    /// Recommended when a fast $0 model (subscription bridge or local ollama) is available.
    Hybrid,
}

/// What Forge does once a budget cap is reached (FR-5).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BudgetBehavior {
    /// Refuse model calls once a cap is exceeded (overridable per-turn via
    /// `FORGE_BUDGET_OVERRIDE=1`). Default true.
    pub hard_stop: bool,
    /// A cap downshifts/stops even an explicitly pinned model. Default true. (Model pinning
    /// is not yet a feature; this is forward-compatible config.)
    pub cap_overrides_pin: bool,
}

impl Default for BudgetBehavior {
    fn default() -> Self {
        Self {
            hard_stop: true,
            cap_overrides_pin: true,
        }
    }
}

/// A user-supplied price for one model (USD per 1,000 tokens).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PriceOverride {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
}

impl Default for Config {
    fn default() -> Self {
        let mut models = HashMap::new();
        let many = |s: &[&str]| OneOrMany::Many(s.iter().map(|x| x.to_string()).collect());
        // Free models lead each tier: cost-aware routing (FR-5) picks the cheapest *usable*
        // candidate, and free providers cost $0 (unlisted in pricing), so a free model with a
        // configured key wins — otherwise the mesh falls back down the list. Free model ids
        // change over time; edit `[mesh.models]` to taste (see docs/features/free-models.md).
        models.insert(
            TaskTier::Trivial.as_str().into(),
            many(&["groq::llama-3.1-8b-instant", "ollama::llama3.2"]),
        );
        models.insert(
            TaskTier::Standard.as_str().into(),
            many(&[
                "groq::llama-3.3-70b-versatile",
                "gemini::gemini-2.5-flash",
                "openai::gpt-4o-mini",
            ]),
        );
        models.insert(
            TaskTier::Complex.as_str().into(),
            many(&[
                "groq::llama-3.3-70b-versatile",
                "claude-cli::",
                "anthropic::claude-opus-4-8",
            ]),
        );
        Self {
            permission_mode: PermissionMode::AcceptEdits,
            mesh: MeshConfig {
                models,
                prefer_subscription: default_prefer_subscription(),
                classifier: ClassifierKind::default(),
                classifier_model: None,
                bridge_mode: BridgeMode::default(),
                daily_budget_usd: None,
                monthly_cap_usd: None,
                weekly_budget_usd: None,
                warn_threshold: default_warn_threshold(),
                budget: BudgetBehavior::default(),
                pricing: HashMap::new(),
                subagents: SubagentsConfig::default(),
                auto_discover: default_auto_discover(),
                failover: default_failover(),
                failover_cooldown_secs: default_failover_cooldown_secs(),
                stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
                max_steps: default_max_steps(),
                subscription_conserve: default_subscription_conserve(),
                benchmark_ranking: default_benchmark_ranking(),
                bridge_models: HashMap::new(),
                subscriptions: HashMap::new(),
                disabled: Vec::new(),
                max_output_tokens: default_max_output_tokens(),
                credit_mode: CreditMode::Normal,
                architect_mode: false,
                architect_model: None,
                editor_model: None,
                default_effort: None,
            },
            permissions: PermissionsConfig::default(),
            mcp: McpConfig::default(),
            commands: CommandsConfig::default(),
            lattice: LatticeConfig::default(),
            shell: ShellConfig::default(),
            hooks: Vec::new(),
            git: GitConfig::default(),
            lsp: LspConfig::default(),
            autofix: AutofixConfig::default(),
            assay: AssayConfig::default(),
        }
    }
}

impl Config {
    /// Resolve the primary model id for a tier (the single value, or the first candidate),
    /// falling back to the standard tier.
    pub fn model_for(&self, tier: TaskTier) -> Option<&str> {
        self.mesh
            .models
            .get(tier.as_str())
            .or_else(|| self.mesh.models.get(TaskTier::Standard.as_str()))
            .map(OneOrMany::first)
    }

    /// All candidate model ids configured for a tier (one element for the single-string form),
    /// falling back to the standard tier. The cost-aware router ranks these.
    pub fn candidates_for(&self, tier: TaskTier) -> Vec<String> {
        self.mesh
            .models
            .get(tier.as_str())
            .or_else(|| self.mesh.models.get(TaskTier::Standard.as_str()))
            .map(OneOrMany::all)
            .unwrap_or_default()
    }

    /// The full ordered rule set the broker resolves against: built-in safety denies first,
    /// then configured rules. Precedence is decided in `forge_core::permission`, not order.
    pub fn permission_rules(&self) -> Vec<PermissionRule> {
        let mut rules = builtin_deny_rules();
        rules.extend(
            self.permissions
                .rules
                .iter()
                .filter_map(RuleConfig::to_rule),
        );
        rules
    }
}

/// Per-OS config directory: `<config>/forge`.
pub fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "forge", "forge").map(|d| d.config_dir().to_path_buf())
}

/// Per-OS data directory: `<data>/forge` (e.g. `~/.local/share/forge`). The session + usage store
/// lives here so spend/budget and history persist across restarts and are shared regardless of the
/// directory `forge` is launched from (FR-5 budget is global, not per-project).
pub fn data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dev", "forge", "forge").map(|d| d.data_dir().to_path_buf())
}

/// Claude Code's home directory (`~/.claude`), source for `forge import claude`. `None` if no
/// home directory resolves on this platform.
pub fn claude_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().join(".claude"))
}

/// Codex CLI's home directory (`~/.codex`), source for `forge import codex`. Custom prompts live
/// under `~/.codex/prompts/*.md` (plain markdown slash-command templates). `None` if no home
/// directory resolves on this platform.
pub fn codex_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().join(".codex"))
}

/// Cursor AI's home directory (`~/.cursor`), source for `forge import cursor`. Rules live under
/// `~/.cursor/rules/*.mdc`. `None` if no home directory resolves on this platform.
pub fn cursor_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().join(".cursor"))
}

/// Home directory, `None` if not resolvable. Used by `forge import aider` to locate convention
/// files that don't follow a fixed tool-specific directory structure.
pub fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// The command/skill discovery sources, scope-tagged: user scope (`<config>/forge/{commands,
/// skills}`, present only when a config dir resolves) then project scope (`./.forge/{commands,
/// skills}`). Project wins on a name collision (see `forge_skills::Catalog`).
pub fn command_sources() -> forge_skills::Sources {
    use forge_skills::{Scope, ScopedDir};
    let mut commands = Vec::new();
    let mut skills = Vec::new();
    if let Some(dir) = config_dir() {
        commands.push(ScopedDir {
            scope: Scope::User,
            path: dir.join("commands"),
        });
        skills.push(ScopedDir {
            scope: Scope::User,
            path: dir.join("skills"),
        });
    }
    commands.push(ScopedDir {
        scope: Scope::Project,
        path: PathBuf::from("./.forge/commands"),
    });
    skills.push(ScopedDir {
        scope: Scope::Project,
        path: PathBuf::from("./.forge/skills"),
    });
    forge_skills::Sources { commands, skills }
}

/// Load configuration with full layered precedence (lowest -> highest):
/// built-in defaults -> user config -> project `./.forge/config.toml` -> `FORGE_*` env.
pub fn load() -> Result<Config, ConfigError> {
    let mut fig = Figment::from(Serialized::defaults(Config::default()));

    if let Some(dir) = config_dir() {
        fig = fig.merge(Toml::file(dir.join("config.toml")));
    }
    fig = fig.merge(Toml::file("./.forge/config.toml"));
    fig = fig.merge(Env::prefixed("FORGE_").split("__"));

    let mut config: Config = fig.extract()?;
    // Project-local `.forge/mcp.toml` is the dedicated home for MCP server declarations; when
    // present it sets the whole `[mcp]` section (overriding any `[mcp]` in config.toml). Keeping
    // it a separate file matches Claude-Code's `.mcp.json` convention and keeps server lists out
    // of the main config.
    if let Ok(text) = std::fs::read_to_string("./.forge/mcp.toml") {
        match toml::from_str::<McpConfig>(&text) {
            Ok(mcp) => config.mcp = mcp,
            Err(e) => tracing::warn!("ignoring malformed .forge/mcp.toml: {e}"),
        }
    }
    Ok(config)
}

/// Whether the user has a persisted config file (the onboarding "first run" signal — combined
/// with "no provider keys / no bridges" by the caller).
pub fn user_config_exists() -> bool {
    config_dir().is_some_and(|d| d.join("config.toml").exists())
}

/// Persist `permission_mode` and `mesh.credit_mode` into the user config TOML, preserving all
/// other keys. Returns the path written.
pub fn write_settings(
    permission: PermissionMode,
    credit_mode: CreditMode,
) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    write_settings_at(&path, permission, credit_mode)?;
    Ok(path)
}

fn write_settings_at(
    path: &std::path::Path,
    permission: PermissionMode,
    credit_mode: CreditMode,
) -> Result<(), ConfigError> {
    let mut root: toml::Table = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    // permission_mode uses kebab-case serde names
    let perm_str = match permission {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Bypass => "bypass",
        PermissionMode::Plan => "plan",
    };
    root.insert(
        "permission_mode".to_string(),
        toml::Value::String(perm_str.to_string()),
    );

    let mesh = root
        .entry("mesh".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(mesh_t) = mesh {
        let credit_str = match credit_mode {
            CreditMode::Normal => "normal",
            CreditMode::Frugal => "frugal",
            CreditMode::Strict => "strict",
        };
        mesh_t.insert(
            "credit_mode".to_string(),
            toml::Value::String(credit_str.to_string()),
        );
    }

    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    std::fs::write(path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(())
}

/// Persist only `permission_mode` to the user config TOML, preserving every other key (notably
/// `mesh.credit_mode`, which [`write_settings`] would also rewrite). Used when the temper is
/// switched at runtime (SHIFT+TAB / `/mode`) so the chosen posture becomes the default next launch.
pub fn write_permission_mode(permission: PermissionMode) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    let mut root: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    let perm_str = match permission {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept-edits",
        PermissionMode::Bypass => "bypass",
        PermissionMode::Plan => "plan",
    };
    root.insert(
        "permission_mode".to_string(),
        toml::Value::String(perm_str.to_string()),
    );
    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(path)
}

/// Append a `[[permissions.rules]]` allow entry for `tool` to the project `.forge/config.toml`.
/// Creates the file (and `.forge/` dir) if absent. Idempotent at the file level — duplicate
/// entries are harmless (first match wins in the permission broker).
pub fn append_allow_rule(tool: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(".forge")?;
    let entry = format!("\n[[permissions.rules]]\ntool = \"{tool}\"\nallow = \"*\"\n");
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(".forge/config.toml")?;
    f.write_all(entry.as_bytes())
}

/// Persist the CLI-bridge subscription plans into the user `config.toml`, preserving every other
/// key already in the file (`forge init`). Returns the path written. Set `[mesh.subscriptions]`
/// without disturbing the rest of the config — secrets are NEVER written here (keys go to the
/// keyring; ADR-0007).
pub fn write_subscriptions(subs: &HashMap<String, String>) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    write_subscriptions_at(&path, subs)?;
    Ok(path)
}

/// The file half of [`write_subscriptions`] against an explicit path: set `[mesh.subscriptions]`
/// in the TOML at `path`, preserving every other key. Split out so it can be tested without
/// touching the real per-user config directory.
fn write_subscriptions_at(
    path: &std::path::Path,
    subs: &HashMap<String, String>,
) -> Result<(), ConfigError> {
    let mut root: toml::Table = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let mesh = root
        .entry("mesh".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(mesh_t) = mesh {
        let sub_t: toml::Table = subs
            .iter()
            .map(|(k, v)| (k.clone(), toml::Value::String(v.clone())))
            .collect();
        mesh_t.insert("subscriptions".to_string(), toml::Value::Table(sub_t));
    }
    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    std::fs::write(path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(())
}

/// Providers that authenticate with an API key, paired with the environment variable the
/// genai client reads for that provider. The env var names must match genai's
/// `API_KEY_DEFAULT_ENV_NAME` per adapter exactly (note OpenRouter's underscore). Local
/// providers (e.g. ollama) need no key and are intentionally absent.
// Provider prefix -> API-key env var. The prefix matches the `provider::` namespace in model
// ids (and, except `openrouter`→`open_router`, the genai adapter namespace), and the env var
// matches the name the genai adapter reads — so a key set here is picked up end-to-end. The
// lower block is free / free-tier providers (genai 0.6 has native adapters for all of these
// except Cerebras, which Forge wires via a custom endpoint resolver).
const PROVIDER_ENV_VARS: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("openrouter", "OPEN_ROUTER_API_KEY"),
    // Free / free-tier providers.
    ("groq", "GROQ_API_KEY"),
    ("opencode_go", "OPENCODE_GO_API_KEY"), // OpenCode Zen free curated coding models
    ("github_copilot", "GITHUB_TOKEN"),     // GitHub Models free inference
    ("mimo", "MIMO_API_KEY"),               // Xiaomi MiMo
    ("minimax", "MINIMAX_API_KEY"),
    ("cerebras", "CEREBRAS_API_KEY"), // no native genai adapter — custom endpoint resolver
];

// Search-API providers for the `web_search` tool. Kept separate from PROVIDER_ENV_VARS so
// they never enter model discovery / the mesh — they authenticate a tool, not a model.
const SEARCH_ENV_VARS: &[(&str, &str)] = &[("brave", "BRAVE_API_KEY")];

/// Search providers Forge can authenticate (`forge auth brave`).
pub fn known_search_providers() -> impl Iterator<Item = &'static str> {
    SEARCH_ENV_VARS.iter().map(|(name, _)| *name)
}

/// Whether a search-API key is configured (env var or keyring) for `provider`. Unlike
/// [`has_api_key`], this checks the `SEARCH_ENV_VARS` table (search keys are not model
/// providers, so `has_api_key` would wrongly treat them as keyless).
pub fn has_search_key(provider: &str) -> bool {
    let Some((_, var)) = SEARCH_ENV_VARS.iter().find(|(n, _)| *n == provider) else {
        return false;
    };
    if std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false) {
        return true;
    }
    secret_store::get(provider)
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

/// A human label + hint for a search provider, shown in `forge init` / `/config`.
pub fn search_provider_label(provider: &str) -> &'static str {
    match provider {
        "brave" => "Brave Search (web_search) — metered, ~$0.005/query",
        _ => "Web search API key",
    }
}

/// Export keyring-stored search keys into the environment so the `web_search` tool (which
/// reads `BRAVE_API_KEY`) sees them. Mirrors [`inject_provider_keys`]; best-effort.
pub fn inject_search_keys() {
    for (provider, var) in SEARCH_ENV_VARS {
        if std::env::var(var).is_ok() {
            continue;
        }
        if let Some(key) = secret_store::get(provider) {
            std::env::set_var(var, key);
        }
    }
}

/// The conventional environment variable for a provider's API key, if it needs one.
fn env_var_for(provider: &str) -> Option<&'static str> {
    PROVIDER_ENV_VARS
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, var)| *var)
}

/// Provider names Forge knows how to authenticate (for `forge auth` validation/help).
pub fn known_key_providers() -> impl Iterator<Item = &'static str> {
    PROVIDER_ENV_VARS.iter().map(|(name, _)| *name)
}

/// The provider prefix of a `"provider::model"` id (the part before the first `::`), or `""`
/// when the id is unprefixed.
pub fn provider_of(model: &str) -> &str {
    model.split_once("::").map(|(p, _)| p).unwrap_or("")
}

/// Whether a usable API key is available for `model`'s provider *without* erroring. True for
/// keyless providers (local `ollama::`, the `claude-cli::`/`codex-cli::` bridges that own
/// their own auth, and unprefixed ids we can't classify). For key-based providers, true iff
/// the env var is set or the keyring holds an entry. The mesh uses this for provider fallback.
pub fn has_api_key(provider: &str) -> bool {
    let Some(var) = env_var_for(provider) else {
        return true; // keyless / unknown -> don't block routing on it
    };
    if std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false) {
        return true;
    }
    secret_store::get(provider)
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

/// Resolve an API key for a provider: environment variable first, then the OS keyring.
pub fn api_key(provider: &str) -> Result<String, ConfigError> {
    let Some(var) = env_var_for(provider) else {
        return Ok(String::new());
    };
    if let Ok(key) = std::env::var(var) {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    if let Some(key) = secret_store::get(provider) {
        return Ok(key);
    }
    Err(ConfigError::MissingKey(provider.into(), var.into()))
}

/// Securely store a provider API key (OS keyring, encrypted-file fallback).
pub fn store_api_key(provider: &str, key: &str) -> Result<(), ConfigError> {
    secret_store::set(provider, key)
}

/// Delete a provider API key. Returns `Ok(true)` if an entry was removed, `Ok(false)` if there
/// was nothing stored (so `forge auth --remove` is idempotent).
pub fn remove_api_key(provider: &str) -> Result<bool, ConfigError> {
    secret_store::delete(provider)
}

/// Store an arbitrary secret (e.g. an MCP server token, keyed `mcp:<server>`) in the OS keyring
/// under the `forge` service. Cross-platform via the `keyring` crate's native backends (macOS
/// Keychain, Windows Credential Manager, Linux Secret Service). ADR-0007: secrets live in the
/// keyring, never in config or logs. `forge mcp import` uses this to persist captured tokens.
pub fn store_secret(key: &str, value: &str) -> Result<(), ConfigError> {
    secret_store::set(key, value)
}

/// Read a stored secret by key (keyring, then the encrypted-file fallback). Used for MCP tokens.
pub fn load_secret(key: &str) -> Option<String> {
    secret_store::get(key)
}

/// Make stored keys visible to the provider client (genai reads keys from the environment): for
/// each known provider with no env var set, inject the stored value. Best-effort — providers
/// without a stored key are simply left unset.
pub fn inject_provider_keys() {
    for (provider, var) in PROVIDER_ENV_VARS {
        if std::env::var(var).is_ok() {
            continue;
        }
        if let Some(key) = secret_store::get(provider) {
            std::env::set_var(var, key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_matcher_filters_by_tool_name() {
        let mk = |m: Option<&str>| HookConfig {
            event: HookEvent::PreToolUse,
            matcher: m.map(String::from),
            command: "true".into(),
            timeout_secs: 30,
        };
        assert!(mk(None).matches("shell"), "no matcher = all tools");
        assert!(mk(Some("*")).matches("anything"));
        assert!(mk(Some("shell")).matches("shell"));
        assert!(!mk(Some("shell")).matches("edit_file"));
        assert!(
            mk(Some("edit_file,write_file")).matches("write_file"),
            "comma list"
        );
    }

    #[test]
    fn hooks_parse_from_toml() {
        let cfg: Config = figment::Figment::from(figment::providers::Serialized::defaults(
            Config::default(),
        ))
        .merge(figment::providers::Toml::string(
            "[[hooks]]\nevent = \"pre_tool_use\"\nmatcher = \"shell\"\ncommand = \"rtk $TOOL\"\n",
        ))
        .extract()
        .unwrap();
        assert_eq!(cfg.hooks.len(), 1);
        assert_eq!(cfg.hooks[0].event, HookEvent::PreToolUse);
        assert_eq!(cfg.hooks[0].timeout_secs, 30, "default applied");
    }

    #[test]
    fn defaults_have_a_model_per_tier() {
        let c = Config::default();
        assert!(c.model_for(TaskTier::Trivial).is_some());
        assert!(c.model_for(TaskTier::Standard).is_some());
        assert!(c.model_for(TaskTier::Complex).is_some());
    }

    #[test]
    fn api_key_prefers_the_environment() {
        std::env::set_var("OPENAI_API_KEY", "sk-env-precedence");
        assert_eq!(api_key("openai").unwrap(), "sk-env-precedence");
        std::env::remove_var("OPENAI_API_KEY");
    }

    #[test]
    fn local_providers_need_no_key() {
        assert_eq!(api_key("ollama").unwrap(), "");
    }

    #[test]
    fn env_var_mapping_covers_all_key_providers() {
        // Names must match genai's per-adapter API_KEY_DEFAULT_ENV_NAME exactly.
        assert_eq!(env_var_for("anthropic"), Some("ANTHROPIC_API_KEY"));
        assert_eq!(env_var_for("openai"), Some("OPENAI_API_KEY"));
        assert_eq!(env_var_for("gemini"), Some("GEMINI_API_KEY"));
        assert_eq!(env_var_for("xai"), Some("XAI_API_KEY"));
        assert_eq!(env_var_for("deepseek"), Some("DEEPSEEK_API_KEY"));
        // Forge's `openrouter` alias maps to genai's underscored env var.
        assert_eq!(env_var_for("openrouter"), Some("OPEN_ROUTER_API_KEY"));
        assert_eq!(env_var_for("ollama"), None);
    }

    #[test]
    fn missing_key_error_names_the_env_var_and_auth_command() {
        std::env::remove_var("DEEPSEEK_API_KEY");
        let err = api_key("deepseek").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("DEEPSEEK_API_KEY"), "got: {msg}");
        assert!(msg.contains("forge auth deepseek"), "got: {msg}");
    }

    #[test]
    fn write_subscriptions_sets_the_section_and_preserves_other_keys() {
        let dir = std::env::temp_dir().join(format!("forge-cfg-{}", forge_types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // A pre-existing config with an unrelated key that must survive the write.
        std::fs::write(&path, "[mesh]\nprefer_subscription = true\n").unwrap();

        let mut subs = HashMap::new();
        subs.insert("claude-cli".to_string(), "max-20x".to_string());
        subs.insert("codex-cli".to_string(), "plus".to_string());
        write_subscriptions_at(&path, &subs).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Table = written.parse().unwrap();
        let mesh = parsed["mesh"].as_table().unwrap();
        assert_eq!(
            mesh["prefer_subscription"].as_bool(),
            Some(true),
            "existing key preserved"
        );
        let s = mesh["subscriptions"].as_table().unwrap();
        assert_eq!(s["claude-cli"].as_str(), Some("max-20x"));
        assert_eq!(s["codex-cli"].as_str(), Some("plus"));

        // And it round-trips through the typed config.
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(&path))
            .extract()
            .unwrap();
        assert_eq!(
            cfg.mesh.subscriptions.get("claude-cli").map(String::as_str),
            Some("max-20x")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn known_key_providers_lists_the_new_providers() {
        let providers: Vec<_> = known_key_providers().collect();
        for p in [
            "anthropic",
            "openai",
            "gemini",
            "xai",
            "deepseek",
            "openrouter",
        ] {
            assert!(providers.contains(&p), "{p} should be a known key provider");
        }
    }

    #[test]
    fn free_providers_map_to_genai_env_vars() {
        // Names must match genai's per-adapter API_KEY_DEFAULT_ENV_NAME exactly so a key set
        // via `forge auth`/env is picked up end-to-end.
        assert_eq!(env_var_for("groq"), Some("GROQ_API_KEY"));
        assert_eq!(env_var_for("opencode_go"), Some("OPENCODE_GO_API_KEY"));
        assert_eq!(env_var_for("github_copilot"), Some("GITHUB_TOKEN"));
        assert_eq!(env_var_for("mimo"), Some("MIMO_API_KEY"));
        assert_eq!(env_var_for("minimax"), Some("MINIMAX_API_KEY"));
        assert_eq!(env_var_for("cerebras"), Some("CEREBRAS_API_KEY"));
        // provider_of pulls the right prefix from a namespaced model id.
        assert_eq!(provider_of("groq::llama-3.3-70b-versatile"), "groq");
        assert_eq!(provider_of("opencode_go::deepseek-v4-flash"), "opencode_go");
    }

    #[test]
    fn default_tiers_lead_with_free_models() {
        // Each tier offers a free candidate so cost-aware routing uses $0 models when keyed,
        // and falls back otherwise (these are candidate lists, not single pins).
        let c = Config::default();
        let trivial = c.candidates_for(TaskTier::Trivial);
        assert!(
            trivial.iter().any(|m| m.starts_with("groq::")),
            "trivial offers a free Groq model: {trivial:?}"
        );
        assert!(
            trivial.iter().any(|m| m.starts_with("ollama::")),
            "trivial keeps a keyless local fallback: {trivial:?}"
        );
        assert!(c
            .candidates_for(TaskTier::Standard)
            .iter()
            .any(|m| m.starts_with("groq::") || m.starts_with("gemini::")));
    }

    #[test]
    fn unknown_tier_falls_back_to_standard() {
        let mut c = Config::default();
        c.mesh.models.remove(TaskTier::Trivial.as_str());
        assert_eq!(
            c.model_for(TaskTier::Trivial),
            c.model_for(TaskTier::Standard)
        );
    }

    #[test]
    fn builtin_denies_present_with_empty_config() {
        let rules = Config::default().permission_rules();
        assert!(
            rules.iter().any(|r| r.source == RuleSource::Builtin
                && r.decision == PermissionDecision::Deny
                && r.tool == "shell"
                && r.patterns.iter().any(|p| p == "rm -rf /")),
            "shell rm -rf / deny must ship by default"
        );
        assert!(
            rules
                .iter()
                .any(|r| r.tool == "read_file" && r.patterns.iter().any(|p| p == "**/.env")),
            "secret-read deny must ship by default"
        );
    }

    #[test]
    fn rules_parse_from_toml_and_layer_over_builtins() {
        let toml = r#"
[[permissions.rules]]
tool = "shell"
allow = ["git *", "cargo *"]

[[permissions.rules]]
tool = "shell"
deny = "sudo *"
reason = "no privilege escalation"
"#;
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
            .unwrap();
        assert_eq!(cfg.permissions.rules.len(), 2);
        let configured: Vec<_> = cfg
            .permissions
            .rules
            .iter()
            .filter_map(RuleConfig::to_rule)
            .collect();
        assert_eq!(configured[0].decision, PermissionDecision::Allow);
        assert_eq!(configured[0].patterns, vec!["git *", "cargo *"]);
        assert_eq!(configured[1].decision, PermissionDecision::Deny);
        assert_eq!(
            configured[1].reason.as_deref(),
            Some("no privilege escalation")
        );
        // builtins still present in the full set
        assert!(cfg
            .permission_rules()
            .iter()
            .any(|r| r.source == RuleSource::Builtin));
    }

    #[test]
    fn disabled_matches_exact_id_and_provider_prefix() {
        let disabled = vec!["claude-cli::opus".to_string(), "gemini".to_string()];
        // Exact model id.
        assert!(is_model_disabled("claude-cli::opus", &disabled));
        // Bare provider prefix matches all its models...
        assert!(is_model_disabled("gemini::flash", &disabled));
        assert!(is_model_disabled("gemini::pro", &disabled));
        // ...but not a different provider or a non-disabled sibling model.
        assert!(!is_model_disabled("claude-cli::sonnet", &disabled));
        assert!(!is_model_disabled("openai::gpt-4o", &disabled));
        // A prefix must match on the `::` boundary, not a substring.
        assert!(!is_model_disabled("geminix::pro", &disabled));
        // Empty list / empty entries disable nothing.
        assert!(!is_model_disabled("gemini::flash", &[]));
        assert!(!is_model_disabled("gemini::flash", &["".to_string()]));
    }

    #[test]
    fn append_allow_rule_creates_valid_toml_entry() {
        let dir = tempfile::tempdir().unwrap();
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        std::fs::create_dir_all(".forge").unwrap();

        append_allow_rule("shell").unwrap();
        append_allow_rule("write_file").unwrap();

        let text = std::fs::read_to_string(".forge/config.toml").unwrap();
        // Parseable as TOML
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(&text))
            .extract()
            .unwrap();
        assert_eq!(cfg.permissions.rules.len(), 2);
        let rules: Vec<_> = cfg
            .permissions
            .rules
            .iter()
            .filter_map(RuleConfig::to_rule)
            .collect();
        assert!(rules
            .iter()
            .any(|r| r.tool == "shell" && r.decision == PermissionDecision::Allow));
        assert!(rules
            .iter()
            .any(|r| r.tool == "write_file" && r.decision == PermissionDecision::Allow));

        std::env::set_current_dir(orig).unwrap();
    }
}
