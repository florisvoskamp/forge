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
    /// Per-turn recap: a one-line AI-generated summary after each completed turn. On by default;
    /// disable in /config if you find it noisy.
    #[serde(default)]
    pub recap: RecapConfig,
    /// Interactive TUI rendering (chat). Controls inline vs. full-screen (alternate-screen) mode.
    #[serde(default)]
    pub tui: TuiConfig,
    /// Local-LLM runtime (Ollama): which model to auto-start and whether to start it with Forge.
    #[serde(default)]
    pub local: LocalConfig,
    /// Startup update check (GitHub releases). On by default; throttled to once a day.
    #[serde(default)]
    pub update: UpdateConfig,
    /// When true, Forge starts a sub-Forge session as an MCP server so the agent can use
    /// forge_chat / forge_assay as native tools (self-driving mode). Toggle with
    /// `forge self enable` / `forge self disable`. Off by default.
    #[serde(default)]
    pub self_mcp: bool,
    /// Customizable statusline layout (left / center / right widget segments).
    #[serde(default)]
    pub statusline: StatuslineConfig,
    /// User-configurable keybind map (action → key combo). Defaults to the built-in map.
    #[serde(default)]
    pub keybinds: KeybindsConfig,
    /// Runtime-registered providers: custom OpenAI-compatible endpoints (LM Studio, vLLM, llama.cpp
    /// `--server`, text-generation-webui, local/proxy servers) the user adds without recompiling.
    /// Merged with the built-in [`CUSTOM_OPENAI_PROVIDERS`] at startup so they participate in
    /// discovery + routing identically. Empty = inert.
    #[serde(default)]
    pub providers: ProvidersConfig,
}

/// `[providers]` config block. Today just custom OpenAI-compatible endpoints; a home for future
/// runtime provider registration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProvidersConfig {
    /// Each `[[providers.custom]]` entry registers one OpenAI-compatible endpoint at runtime.
    #[serde(default)]
    pub custom: Vec<CustomProviderConfig>,
    /// `[providers.azure]` — an Azure OpenAI resource. Unlike the standard OpenAI shape, Azure scopes
    /// the URL to a deployment and authenticates with an `api-key` header (not `Authorization:
    /// Bearer`), so it needs its own block rather than a `[[providers.custom]]` row. Absent = inert.
    #[serde(default)]
    pub azure: Option<AzureConfig>,
}

/// `[providers.azure]`: an Azure OpenAI resource the user configures without recompiling. Azure's
/// chat URL is `https://<resource>.openai.azure.com/openai/deployments/<deployment>/chat/completions?
/// api-version=<ver>` with an `api-key` header — Forge reaches it through genai's OpenAI adapter via a
/// per-request URL+header override (`AuthData::RequestOverride`), so the request body is standard
/// OpenAI chat-completions (tool calls included). Each model id is `azure::<deployment>`; the
/// deployment name is both the routing key and the body `model`. Example:
/// ```toml
/// [providers.azure]
/// resource    = "my-resource"          # -> https://my-resource.openai.azure.com  (or set `endpoint`)
/// api_version = "2024-10-21"           # optional — defaults to a recent GA version
/// api_key_env = "AZURE_OPENAI_API_KEY" # optional — this is the default
/// deployments = ["gpt-4o", "gpt-4o-mini"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AzureConfig {
    /// Azure resource name → `https://<resource>.openai.azure.com`. Ignored if `endpoint` is set.
    #[serde(default)]
    pub resource: Option<String>,
    /// Full resource endpoint base (e.g. `https://my-resource.openai.azure.com`). Overrides
    /// `resource`; lets sovereign/custom Azure clouds (`*.openai.azure.us`, proxies) be targeted.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Azure REST `api-version` query value. Optional — defaults to [`DEFAULT_AZURE_API_VERSION`].
    #[serde(default)]
    pub api_version: Option<String>,
    /// Env var holding the Azure API key. Optional — defaults to [`AZURE_DEFAULT_KEY_ENV`].
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Deployment names (each becomes an `azure::<deployment>` model id). May be empty (the user pins
    /// `azure::<deployment>` ids manually); Azure has no enumerable deployments endpoint in our flow.
    #[serde(default)]
    pub deployments: Vec<String>,
    /// Whether these deployments are free to call. Azure is metered, so this is `false` by default.
    #[serde(default)]
    pub free: bool,
    /// Human label shown in `forge provider list`. Optional.
    #[serde(default)]
    pub label: Option<String>,
}

/// One user-declared `[[providers.custom]]` endpoint. Mirrors a [`CustomProvider`] row but owned and
/// deserializable; converted (with validation + endpoint normalization) into the static registry at
/// startup. Example:
/// ```toml
/// [[providers.custom]]
/// namespace = "lmstudio"
/// base_url  = "http://localhost:1234/v1"   # trailing /v1 or /v1/ both accepted
/// api_key_env = "LMSTUDIO_API_KEY"          # optional — omit for a keyless local server
/// free = true                                # optional (default false)
/// models = ["qwen2.5-coder-32b"]            # optional — else discovered via /v1/models
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomProviderConfig {
    /// The `provider::` namespace in model ids and the `forge provider` name.
    pub namespace: String,
    /// Full base URL of the OpenAI-compatible server (the part before `chat/completions`).
    pub base_url: String,
    /// Env var holding the API key. Omit for a keyless local server (a placeholder is sent).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Whether this endpoint's models are genuinely free to call (e.g. a local server).
    #[serde(default)]
    pub free: bool,
    /// Explicit model ids (bare, no namespace). Empty → discover live via `/v1/models`.
    #[serde(default)]
    pub models: Vec<String>,
    /// Human label shown in `forge provider list`. Optional.
    #[serde(default)]
    pub label: Option<String>,
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
    /// The agent emitted a user-facing notification (e.g. a tool needs permission). Observe-only.
    /// Claude-Code parity event (`Notification`).
    Notification,
    /// Just before context compaction runs (Claude-Code `PreCompact`). Observe-only.
    PreCompact,
    /// Just after context compaction completes. Observe-only. (Forge extension beyond CC, which
    /// only fires `PreCompact`.)
    PostCompact,
    /// A turn finished — the agent stopped (Claude-Code `Stop`). Observe-only.
    Stop,
    /// A subagent finished (Claude-Code `SubagentStop`). Observe-only.
    SubagentStop,
}

impl HookEvent {
    /// The Claude-Code event name for this hook (`PreToolUse`, `Notification`, …). Used to build
    /// the CC stdin payload's `hook_event_name` and to translate CC settings.json config.
    pub fn cc_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::Notification => "Notification",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStop => "SubagentStop",
        }
    }

    /// Parse a Claude-Code event name into a [`HookEvent`]. `None` for an unknown name.
    pub fn from_cc_name(name: &str) -> Option<HookEvent> {
        Some(match name {
            "PreToolUse" => HookEvent::PreToolUse,
            "PostToolUse" => HookEvent::PostToolUse,
            "UserPromptSubmit" => HookEvent::UserPromptSubmit,
            "SessionStart" => HookEvent::SessionStart,
            "SessionEnd" => HookEvent::SessionEnd,
            "Notification" => HookEvent::Notification,
            "PreCompact" => HookEvent::PreCompact,
            "PostCompact" => HookEvent::PostCompact,
            "Stop" => HookEvent::Stop,
            "SubagentStop" => HookEvent::SubagentStop,
            _ => return None,
        })
    }
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
    /// Claude-Code compatibility mode. When true the hook speaks the CC protocol: it receives the
    /// CC JSON payload on stdin (`{session_id, transcript_path, cwd, hook_event_name, tool_name,
    /// tool_input, …}`) and its output is parsed CC-style (`{"decision":"block|approve","reason":…}`
    /// / `hookSpecificOutput`) with exit-code 2 = block. The `matcher` is interpreted as a CC tool
    /// matcher (`|`-separated names / `*`) against the CC tool-name alias. When false (default) the
    /// hook keeps Forge's native protocol. Set automatically for hooks loaded from a CC settings.json.
    #[serde(default)]
    pub cc_compat: bool,
}

fn default_hook_timeout() -> u64 {
    30
}

/// Map a Forge tool name to its Claude-Code equivalent, so a CC hook matcher written against CC
/// tool names (`Write`, `Edit`, `Bash`, …) still fires on the corresponding Forge tool. Returns the
/// original name when there's no distinct CC alias.
pub fn cc_tool_alias(forge_tool: &str) -> &str {
    match forge_tool {
        "shell" => "Bash",
        "edit_file" | "apply_patch" => "Edit",
        "write_file" | "create_file" => "Write",
        "read_file" => "Read",
        "list_files" | "ls" => "LS",
        "search" | "grep" | "ripgrep" => "Grep",
        "glob" => "Glob",
        "web_fetch" => "WebFetch",
        "web_search" => "WebSearch",
        other => other,
    }
}

/// Map a Claude-Code tool name (`Bash`, `Edit`, `Read`, …) to Forge's tool name, for translating a
/// CC `settings.json` permission entry (`Bash(npm run *)`) into a Forge `[[permissions.rules]]`
/// block. Unknown names (e.g. an MCP tool) pass through unchanged.
pub fn forge_tool_from_cc(cc_tool: &str) -> &str {
    match cc_tool {
        "Bash" => "shell",
        "Edit" | "MultiEdit" => "edit_file",
        "Write" => "write_file",
        "Read" => "read_file",
        "LS" => "list_files",
        "Grep" => "search",
        "Glob" => "glob",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        other => other,
    }
}

impl HookConfig {
    /// Whether this hook applies to `tool_name`. Native hooks use Forge's exact/comma-separated
    /// matcher; CC-compat hooks treat the matcher as a `|`-separated CC matcher tested against both
    /// the Forge tool name and its [`cc_tool_alias`] (so `"Write|Edit"` fires on `edit_file`).
    pub fn matches(&self, tool_name: &str) -> bool {
        match self.matcher.as_deref() {
            None | Some("") | Some("*") => true,
            Some(pattern) => {
                if self.cc_compat {
                    let alias = cc_tool_alias(tool_name);
                    pattern.split('|').map(str::trim).any(|m| {
                        m == "*"
                            || m.eq_ignore_ascii_case(tool_name)
                            || m.eq_ignore_ascii_case(alias)
                    })
                } else {
                    pattern.split(',').any(|m| m.trim() == tool_name)
                }
            }
        }
    }
}

/// Translate a Claude-Code `settings.json` `hooks` object into a flat list of [`HookConfig`]s in
/// CC-compat mode, so existing CC hook scripts run under Forge unmodified. The CC shape is:
/// ```json
/// { "PreToolUse": [ { "matcher": "Write|Edit",
///                     "hooks": [ { "type": "command", "command": "./h.sh", "timeout": 10 } ] } ],
///   "Notification": [ { "hooks": [ { "type": "command", "command": "notify" } ] } ] }
/// ```
/// `value` may be either the top-level settings object (we look up its `hooks` key) or the `hooks`
/// object itself. Unknown event names and non-`command` hook types are skipped.
pub fn cc_hooks_from_settings(value: &serde_json::Value) -> Vec<HookConfig> {
    let hooks_obj = value
        .get("hooks")
        .and_then(|h| h.as_object())
        .or_else(|| value.as_object());
    let Some(obj) = hooks_obj else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (event_name, groups) in obj {
        let Some(event) = HookEvent::from_cc_name(event_name) else {
            continue;
        };
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for group in groups {
            let matcher = group
                .get("matcher")
                .and_then(|m| m.as_str())
                .filter(|m| !m.is_empty() && *m != "*")
                .map(str::to_string);
            let Some(entries) = group.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            for entry in entries {
                // CC only defines `"type": "command"` today; skip anything else.
                if entry.get("type").and_then(|t| t.as_str()) != Some("command") {
                    continue;
                }
                let Some(command) = entry.get("command").and_then(|c| c.as_str()) else {
                    continue;
                };
                let timeout_secs = entry
                    .get("timeout")
                    .and_then(|t| t.as_u64())
                    .unwrap_or_else(default_hook_timeout);
                out.push(HookConfig {
                    event,
                    matcher: matcher.clone(),
                    command: command.to_string(),
                    timeout_secs,
                    cc_compat: true,
                });
            }
        }
    }
    out
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
    /// How many of the top-ranked symbols get their FULL body injected (the rest stay signature
    /// lines). Higher = more aggressive front-loading: the model reads more from context up front
    /// instead of `read_file`/`search`-ing for it. Capped further by `inject_token_budget`.
    #[serde(default = "default_inject_body_hits")]
    pub inject_body_hits: usize,
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
    /// Defaults to `true`: the `auto` backend picks the cheapest available provider (Gemini
    /// free-tier first) and gracefully no-ops if nothing is reachable — so it's safe always-on.
    #[serde(default = "default_embed_enabled")]
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

fn default_embed_enabled() -> bool {
    true
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
            inject_body_hits: default_inject_body_hits(),
            map_orientation: false,
        }
    }
}

fn default_inject_bodies() -> bool {
    true
}

fn default_inject_body_hits() -> usize {
    3
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

/// Per-turn recap: a one-line AI-generated summary shown in scrollback after each completed turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecapConfig {
    /// Emit a `※ recap …` line after each completed turn. Default: true. Disable in /config.
    #[serde(default = "default_recap_enabled")]
    pub enabled: bool,
}

impl Default for RecapConfig {
    fn default() -> Self {
        Self {
            enabled: default_recap_enabled(),
        }
    }
}

fn default_recap_enabled() -> bool {
    true
}

/// Interactive chat TUI rendering mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuiConfig {
    /// Render the chat on the alternate screen (full-screen), with a scrollable transcript and
    /// the panels pinned at the bottom. Default: true. When false, the chat runs inline in the
    /// terminal's native scrollback (a small pinned live region). The `--inline` / `--fullscreen`
    /// flags on `forge chat` override this per-invocation.
    #[serde(default = "default_tui_fullscreen")]
    pub fullscreen: bool,
    /// In full-screen mode, enable minimal mouse reporting (button + wheel, no motion tracking) so
    /// the wheel scrolls the transcript. Default: true. Because motion tracking stays off, the
    /// terminal's native click-drag text selection keeps working. Set false to disable mouse
    /// reporting entirely (e.g. a terminal where any reporting blocks selection). No effect inline.
    #[serde(default = "default_tui_mouse_capture")]
    pub mouse_capture: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            fullscreen: default_tui_fullscreen(),
            mouse_capture: default_tui_mouse_capture(),
        }
    }
}

fn default_tui_fullscreen() -> bool {
    true
}

fn default_tui_mouse_capture() -> bool {
    true
}

/// Local-LLM runtime settings. Forge runs local models through Ollama (exposed in the mesh as
/// `ollama::<tag>`). Off by default — nothing starts unless the user opts in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalConfig {
    /// Start the local runtime + `model` automatically when `forge chat` launches. Default false
    /// (no surprise multi-GB model load / background server); use `forge local start` to run it
    /// on demand.
    #[serde(default)]
    pub autostart: bool,
    /// The Ollama tag to auto-start / treat as the default local model (e.g. `gemma4:12b`).
    #[serde(default)]
    pub model: Option<String>,
    /// Ollama HTTP endpoint. Defaults to the local server.
    #[serde(default = "default_local_endpoint")]
    pub endpoint: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            autostart: false,
            model: None,
            endpoint: default_local_endpoint(),
        }
    }
}

fn default_local_endpoint() -> String {
    "http://localhost:11434".to_string()
}

/// Startup update-check settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateConfig {
    /// Check GitHub releases on startup and notify when a newer version exists. Default: true.
    /// Throttled to once per day; the env var `FORGE_NO_UPDATE_CHECK=1` also disables it.
    #[serde(default = "default_update_check")]
    pub check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: default_update_check(),
        }
    }
}

fn default_update_check() -> bool {
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
    /// Detect lint/test commands from project structure when lint_cmd/test_cmd are empty.
    /// Cargo.toml → `cargo check --all-targets 2>&1`; package.json → `npm run lint 2>&1`.
    /// auto_lint / auto_test must still be true; this only fills in the command.
    #[serde(default = "default_true")]
    pub auto_detect: bool,
}

impl Default for AutofixConfig {
    fn default() -> Self {
        Self {
            auto_lint: false,
            auto_test: false,
            lint_cmd: String::new(),
            test_cmd: String::new(),
            max_iterations: default_autofix_iterations(),
            auto_detect: true,
        }
    }
}

fn default_autofix_iterations() -> u32 {
    3
}

fn default_true() -> bool {
    true
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
    /// Maximum estimated USD cost for one auto-review gate run. When the pre-estimate exceeds
    /// this cap the gate is skipped with a warning instead of running away cost. 0.0 = unlimited.
    #[serde(default = "default_assay_max_cost_usd")]
    pub max_cost_usd: f64,
}

impl Default for AssayConfig {
    fn default() -> Self {
        Self {
            auto_review: false,
            gate_severity: default_assay_gate_severity(),
            gate_mode: default_assay_gate_mode(),
            min_diff_bytes: default_assay_min_diff_bytes(),
            max_cost_usd: default_assay_max_cost_usd(),
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

fn default_assay_max_cost_usd() -> f64 {
    0.50
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
        // `**/.env` does NOT match `.env.local` / `.env.production` / `.env.*` — a real gap, since
        // those dotenv variants are where deploy secrets usually live. Cover them explicitly.
        "**/.env.*",
        "**/*.pem",
        "**/*.key",
        "**/*.p12",
        "**/*.pfx",
        "**/*.keystore",
        "**/*.jks",
        // SSH private keys (all common key types) + the whole ~/.ssh dir.
        "**/id_rsa",
        "**/id_ed25519",
        "**/id_ecdsa",
        "**/id_dsa",
        "**/.ssh/**",
        // Cloud / cluster / registry credentials.
        "**/.aws/credentials",
        "**/.git-credentials",
        "**/.netrc",
        "**/.npmrc",
        "**/.pypirc",
        "**/.kube/config",
        "**/.docker/config.json",
        "**/.config/gcloud/**",
        "**/.gnupg/**",
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
                "cat *.env.*",
                "cat *.pem",
                "cat *.key",
                "cat *id_rsa*",
                "cat *id_ed25519*",
                "cat *id_ecdsa*",
                "cat *id_dsa*",
                "cat */.ssh/*",
                "cat *.aws/credentials*",
                "cat *.git-credentials*",
                "cat *.netrc*",
                "cat *.npmrc*",
                "cat *.pypirc*",
                "cat */.kube/config*",
                "less *.env",
                "less *.env.*",
                "head *.env",
                "head *.env.*",
                "tail *.env",
                "tail *.env.*",
                "cp *.env *",
                "cp *.env.* *",
                "cp */.ssh/* *",
                // Secret reads via the OTHER common non-interactive verbs an agent reaches for when
                // `cat` is the obvious-but-blocked choice: text tools (grep/awk/sed/nl/sort/cut),
                // binary dumps / encoders for exfil (xxd/od/strings/base64), and `source`/`.` which
                // execute a dotenv straight into the environment. Best-effort defense-in-depth — the
                // primary block is the read_file/list_dir tool denylist; this catches the shell-out.
                "grep *.env",
                "grep *.env.*",
                "grep *.pem",
                "grep *.key",
                "grep *id_rsa*",
                "grep */.ssh/*",
                "grep *.aws/credentials*",
                "egrep *.env",
                "rg *.env",
                "awk *.env",
                "awk *.env.*",
                "sed *.env",
                "sed *.env.*",
                "nl *.env",
                "sort *.env",
                "cut *.env",
                "xxd *.env",
                "xxd */.ssh/*",
                "od *.env",
                "strings *.env",
                "strings */.ssh/*",
                "base64 *.env",
                "base64 *.env.*",
                "base64 */.ssh/*",
                "base64 *.pem",
                "base64 *.key",
                "source *.env",
                "source *.env.*",
                ". *.env",
                ". *.env.*",
                // Windows: secret-file reads via type/more
                "type *.env",
                "type *.env.*",
                "type *.pem",
                "type *.key",
                "type *id_rsa*",
                "type */.ssh/*",
                "more *.env",
                "more *.env.*",
                "copy *.env *",
                "copy *.env.* *",
            ],
        ),
        deny("read_file", &secrets),
        deny("list_dir", &secrets),
        // Secrets must also be blocked for write/edit/delete — overwriting a .env or deleting an
        // SSH key is as dangerous as reading it. Also block /etc writes (system config tampering).
        deny("write_file", &{
            let mut v = secrets.to_vec();
            v.extend(["**/.ssh/**", "/etc/**"]);
            v
        }),
        deny("edit_file", &{
            let mut v = secrets.to_vec();
            v.extend(["**/.ssh/**", "/etc/**"]);
            v
        }),
        // delete_file had no deny rules at all — a model could delete .env, SSH keys, etc.
        deny("delete_file", &{
            let mut v = secrets.to_vec();
            v.extend(["**/.ssh/**", "/etc/**"]);
            v
        }),
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
    /// Default bench duration (seconds) when a transient failure (rate-limit / 5xx) gives no
    /// server `Retry-After`. Kept short because free-tier rate limits typically reset per MINUTE
    /// (NVIDIA NIM, Groq, Gemini RPM) — a long bench would strand the best free models and push
    /// routing onto worse or paid ones for minutes. Providers that DO send `Retry-After` use that
    /// exact value instead.
    #[serde(default = "default_failover_cooldown_secs")]
    pub failover_cooldown_secs: u64,
    /// Abort a model stream that goes silent for this many seconds (a half-open/stalled
    /// connection) and fail over, instead of hanging the turn forever. `0` disables the watchdog.
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
    /// Longest rate-limit reset (seconds) Forge will WAIT OUT in-turn to retry the best model rather
    /// than degrade to a lower-ranked one (per-minute free tiers: NIM/Groq/Gemini). A reset longer
    /// than this falls through to failover. `0` disables in-turn waiting entirely.
    #[serde(default = "default_rate_limit_wait_secs")]
    pub rate_limit_wait_secs: u64,
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
    /// Opt-in "max-resolve" mode for CLI-bridge harness turns: append a completeness clause that
    /// makes the model re-verify its change against EVERY requirement before finishing. Measured to
    /// raise SWE-bench resolve (4/10 → 6/10, beating the raw CLI) at ~3× the tokens — so it's OFF by
    /// default and turned on only when solve rate matters more than cost.
    #[serde(default = "default_verify_completeness")]
    pub verify_completeness: bool,
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
    /// Frugal caps output tokens at 2048. Strict caps at 1024 AND restricts auto-routing + failover
    /// to free + subscription models only — a paid, metered model (incl. a priced "free-tier" model
    /// that could bill once its quota runs out) is dropped from the candidate set, so neither the
    /// primary pick nor the failover chain can ever spend API credit without an explicit `--model`
    /// pin. (Enforced in `HeuristicRouter::allowed_under_credit_mode`.)
    #[serde(default)]
    pub credit_mode: CreditMode,
    /// Auto-memory: capture durable facts (preferences/decisions/conventions) at the end of a turn
    /// and recall the most relevant ones into context at the start of a session, scoped per project.
    /// On by default — the built-in cross-session memory. Set false to disable capture + recall (the
    /// `forge memory` command still works for manual entries).
    #[serde(default = "default_auto_memory")]
    pub auto_memory: bool,
    /// Auto-orchestrate: inject the Forge orchestration framework as a standing system instruction
    /// at the start of every session. The model is guided to check skills first, choose the
    /// highest-level tool that fits, and use subagents/MCP/web/Lattice appropriately — without
    /// requiring the user to type `/orchestrate` on each prompt. Off by default (opt-in).
    #[serde(default)]
    pub auto_orchestrate: bool,
    /// Self-review pass (opt-in): after a turn makes edits, the SAME model re-examines its own
    /// changes against the task and may fix them — one bounded round, only on edit turns. OFF by
    /// default: a same-model SWE-bench A/B showed the always-on version REGRESSED (the extra round
    /// over-revised correct fixes). Kept as a lever to refine, not a default-on win.
    #[serde(default = "default_self_review")]
    pub self_review: bool,
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

fn default_verify_completeness() -> bool {
    false
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

fn default_self_review() -> bool {
    // OFF by default: on a same-model SWE-bench A/B the always-on review REGRESSED results
    // (4/6 → 3/6) — the extra round let the model second-guess and break a fix that was already
    // correct. Kept as an opt-in lever; needs a more conservative trigger before it's a net win.
    false
}

fn default_failover_cooldown_secs() -> u64 {
    60
}

fn default_auto_memory() -> bool {
    true
}

fn default_rate_limit_wait_secs() -> u64 {
    // Covers the common per-minute free-tier reset (NIM/Groq/Gemini ~60s) plus slack; a longer
    // (hourly/daily) quota exceeds this and falls through to failover instead of blocking the turn.
    75
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
    /// Max child agents that may run concurrently *on the same provider*. A burst of subagents all
    /// routed to one subscription (claude/codex bridge) or one metered key would otherwise hammer a
    /// single quota in parallel — the global `max_concurrency` doesn't see provider. This sub-cap
    /// throttles per provider so fan-out spreads the load (and protects the subscription thesis).
    /// `0` disables the per-provider cap (global cap only).
    #[serde(default = "default_max_per_provider")]
    pub max_per_provider: usize,
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
fn default_max_per_provider() -> usize {
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
            max_per_provider: default_max_per_provider(),
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

/// A widget shown in the statusline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatuslineWidget {
    /// Spinner + working indicator + model name + tier bracket (compound). Shows spinner while busy.
    Model,
    /// Session cost "◈ $X.XXXX"
    SessionCost,
    /// Reasoning-effort pin label (only renders when effort is pinned)
    Effort,
    /// Operating mode/temper "◆ {label}" (only renders when temper is set)
    Mode,
    /// Per-turn elapsed timer "⧖ Xs"
    TurnElapsed,
    /// Per-turn input tokens "↑N"
    TokensIn,
    /// Per-turn output tokens "↓N"
    TokensOut,
    /// Session total tokens "Σ ↑N ↓N"
    SessionTokens,
    /// Current git branch "⎇ branch"
    GitBranch,
    /// Claude.ai usage % "claude N%"
    QuotaClaude,
    /// Codex usage % "codex N%" (only when data is available)
    QuotaCodex,
    /// MCP server count "⌬ N mcp" (only when servers are connected)
    McpStatus,
    /// Tier bracket only "[tier]"
    Tier,
    /// Static text / env-var substitution
    Custom { text: String },
}

/// Customizable statusline layout (left / center / right segments).
/// Default matches the current built-in layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatuslineConfig {
    /// Left segment widgets (rendered left-aligned).
    #[serde(default = "default_sl_left")]
    pub left: Vec<StatuslineWidget>,
    /// Center segment widgets (centered; empty = no center).
    #[serde(default)]
    pub center: Vec<StatuslineWidget>,
    /// Right segment widgets (rendered right-aligned).
    #[serde(default)]
    pub right: Vec<StatuslineWidget>,
    /// Separator between widgets within a segment.
    #[serde(default = "default_sl_separator")]
    pub separator: String,
}

fn default_sl_left() -> Vec<StatuslineWidget> {
    vec![
        StatuslineWidget::Model,
        StatuslineWidget::SessionCost,
        StatuslineWidget::Effort,
        StatuslineWidget::Mode,
    ]
}

fn default_sl_separator() -> String {
    "  │  ".to_string()
}

impl Default for StatuslineConfig {
    fn default() -> Self {
        Self {
            left: default_sl_left(),
            center: Vec::new(),
            right: Vec::new(),
            separator: default_sl_separator(),
        }
    }
}

/// A key combination: a key name plus modifier flags.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    /// Key name: a single character (e.g. "c", "k", ","), or a named key
    /// ("up", "down", "left", "right", "enter", "esc", "backspace", "delete",
    /// "pageup", "pagedown", "home", "end", "tab", "f1"–"f12").
    pub key: String,
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
}

/// Configurable keybind map: action name → key combination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeybindsConfig {
    #[serde(default)]
    pub binds: std::collections::BTreeMap<String, KeyCombo>,
}

impl Default for KeybindsConfig {
    fn default() -> Self {
        let bind = |key: &str, ctrl: bool, alt: bool, shift: bool| KeyCombo {
            key: key.to_string(),
            ctrl,
            alt,
            shift,
        };
        let mut binds = std::collections::BTreeMap::new();
        binds.insert("interrupt".into(), bind("c", true, false, false));
        binds.insert("command_palette".into(), bind("/", false, false, false));
        binds.insert("skip_model".into(), bind("k", true, false, false));
        binds.insert("tier_up".into(), bind("up", true, false, false));
        binds.insert("tier_down".into(), bind("down", true, false, false));
        binds.insert("toggle_reasoning".into(), bind("r", true, false, false));
        binds.insert("undo".into(), bind("z", true, false, false));
        binds.insert("compact".into(), bind("l", true, false, false));
        binds.insert("model_picker".into(), bind("m", false, true, false));
        binds.insert("effort_cycle".into(), bind("e", false, true, false));
        binds.insert("temper_cycle".into(), bind("t", false, true, false));
        binds.insert("keybind_config".into(), bind(",", true, false, false));
        binds.insert("new_session".into(), bind("n", true, false, false));
        binds.insert("copy_last".into(), bind("c", true, false, true));
        binds.insert("scroll_up".into(), bind("pageup", false, false, false));
        binds.insert("scroll_down".into(), bind("pagedown", false, false, false));
        binds.insert("help".into(), bind("f1", false, false, false));
        binds.insert("checkpoint".into(), bind("s", true, false, false));
        binds.insert("reload".into(), bind("r", false, true, false));
        Self { binds }
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut models = HashMap::new();
        let many = |s: &[&str]| OneOrMany::Many(s.iter().map(|x| x.to_string()).collect());
        // Cost-aware routing (FR-5) picks the cheapest *usable* candidate regardless of list order,
        // so a configured free provider ($0, unlisted in pricing) still wins for the actual route.
        // Order matters only for code paths that take the FIRST candidate (e.g. an architect
        // planner/editor fallback): those key-filter now, but we deliberately DON'T lead any tier
        // with `groq::…` — it needs a key many users don't have, and leading with it made groq the
        // face of every "first candidate" failure. Lead instead with a keyless/bridge option, groq
        // last. Free model ids change over time; edit `[mesh.models]` to taste (free-models.md).
        models.insert(
            TaskTier::Trivial.as_str().into(),
            many(&["ollama::llama3.2", "groq::llama-3.1-8b-instant"]),
        );
        models.insert(
            TaskTier::Standard.as_str().into(),
            many(&[
                "gemini::gemini-2.5-flash",
                "openai::gpt-4o-mini",
                "groq::llama-3.3-70b-versatile",
            ]),
        );
        models.insert(
            TaskTier::Complex.as_str().into(),
            many(&[
                "claude-cli::",
                "anthropic::claude-opus-4-8",
                "groq::llama-3.3-70b-versatile",
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
                rate_limit_wait_secs: default_rate_limit_wait_secs(),
                stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
                max_steps: default_max_steps(),
                subscription_conserve: default_subscription_conserve(),
                benchmark_ranking: default_benchmark_ranking(),
                verify_completeness: default_verify_completeness(),
                bridge_models: HashMap::new(),
                subscriptions: HashMap::new(),
                disabled: Vec::new(),
                max_output_tokens: default_max_output_tokens(),
                credit_mode: CreditMode::Normal,
                auto_memory: default_auto_memory(),
                auto_orchestrate: false,
                self_review: default_self_review(),
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
            recap: RecapConfig::default(),
            tui: TuiConfig::default(),
            local: LocalConfig::default(),
            update: UpdateConfig::default(),
            self_mcp: false,
            statusline: StatuslineConfig::default(),
            keybinds: KeybindsConfig::default(),
            providers: ProvidersConfig::default(),
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

    // Claude-Code-compatible hooks: a `settings.json` (user config dir + project `.forge/`) can
    // declare hooks in CC's event-nested shape. They're appended (CC-compat mode) so existing CC
    // hook scripts run unmodified alongside any native `[[hooks]]`. User scope first, project last.
    let mut cc_paths = Vec::new();
    if let Some(dir) = config_dir() {
        cc_paths.push(dir.join("settings.json"));
    }
    cc_paths.push(PathBuf::from("./.forge/settings.json"));
    for path in cc_paths {
        if let Ok(text) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => config.hooks.extend(cc_hooks_from_settings(&v)),
                Err(e) => tracing::warn!("ignoring malformed {}: {e}", path.display()),
            }
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

/// Persist `self_mcp = true/false` to the global user config, preserving all other keys.
/// Active on the next session start.
pub fn write_self_mcp(enabled: bool) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    let mut root: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    root.insert("self_mcp".to_string(), toml::Value::Boolean(enabled));
    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(path)
}

/// Persist `[statusline]` to the user config TOML, preserving all other keys.
pub fn write_statusline_config(cfg: &StatuslineConfig) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    let mut root: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    let val = toml::Value::try_from(cfg).map_err(|e| ConfigError::Write(e.to_string()))?;
    root.insert("statusline".to_string(), val);
    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    std::fs::write(&path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(path)
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

/// Persist a single keybind to `[keybinds.binds]` in the user config TOML, preserving all other keys.
pub fn write_keybind(action: &str, combo: &KeyCombo) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    let mut root: toml::Table = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();
    let keybinds = root
        .entry("keybinds".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if let toml::Value::Table(kb) = keybinds {
        let binds = kb
            .entry("binds".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let toml::Value::Table(binds_t) = binds {
            let mut combo_t = toml::Table::new();
            combo_t.insert("key".to_string(), toml::Value::String(combo.key.clone()));
            combo_t.insert("ctrl".to_string(), toml::Value::Boolean(combo.ctrl));
            combo_t.insert("alt".to_string(), toml::Value::Boolean(combo.alt));
            combo_t.insert("shift".to_string(), toml::Value::Boolean(combo.shift));
            binds_t.insert(action.to_string(), toml::Value::Table(combo_t));
        }
    }
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

// ----------------------------------------------------------------------------------------------
// Dynamic config editor backing (`/config`). The settable surface is *discovered* by walking the
// serialized Config — every scalar field appears automatically, and a newly-added field needs no
// extra code here. Complex sections (lists/maps: hooks, mcp, permission rules) are excluded; they
// have dedicated commands (`/hooks`, `/mcp`, …).
// ----------------------------------------------------------------------------------------------

/// Where a `/config` edit is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigScope {
    /// `~/.config/forge/config.toml` — applies everywhere.
    User,
    /// `./.forge/config.toml` — repo-local override.
    Project,
}

/// A single editable scalar setting: its dotted path and current value/type.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingLeaf {
    pub path: String,
    pub value: SettingValue,
}

/// The typed value of a [`SettingLeaf`] (only scalars are editable here).
#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// An unset optional (serialized `null`) — edited as text, empty clears it.
    Unset,
}

impl SettingValue {
    /// A short type tag for the editor UI.
    pub fn type_tag(&self) -> &'static str {
        match self {
            SettingValue::Bool(_) => "bool",
            SettingValue::Int(_) => "int",
            SettingValue::Float(_) => "float",
            SettingValue::Str(_) | SettingValue::Unset => "text",
        }
    }

    /// How the current value renders in the editor.
    pub fn display(&self) -> String {
        match self {
            SettingValue::Bool(b) => b.to_string(),
            SettingValue::Int(i) => i.to_string(),
            SettingValue::Float(f) => f.to_string(),
            SettingValue::Str(s) => s.clone(),
            SettingValue::Unset => String::new(),
        }
    }
}

/// Top-level sections that are NOT scalar-editable here — each has its own command/flow.
const COMPLEX_SECTIONS: &[&str] = &["hooks", "mcp", "permissions"];

/// The complex (table/array) config sections the flat `/config` editor can't surface as scalars.
/// They're listed read-only there with an "edit in $EDITOR" jump so they're at least discoverable.
pub fn complex_sections() -> &'static [&'static str] {
    COMPLEX_SECTIONS
}

/// One-line description of a complex section, for its read-only `/config` row.
pub fn complex_section_help(section: &str) -> &'static str {
    match section {
        "hooks" => "pre/post tool-use shell hooks — structured TOML, edit in $EDITOR",
        "mcp" => "external MCP servers — edit in $EDITOR (or .mcp.json / .forge/mcp.toml)",
        "permissions" => "allow/deny tool rules — structured TOML, edit in $EDITOR",
        _ => "structured section — edit in $EDITOR",
    }
}

/// Importance order for the editor: these path prefixes sort first (in this order); everything else
/// follows alphabetically. New fields therefore appear automatically, just lower in the list until
/// curated here.
const PRIORITY_PREFIXES: &[&str] = &[
    "permission_mode",
    "mesh.credit_mode",
    "mesh.daily_cap_usd",
    "mesh.monthly_cap_usd",
    "mesh.weekly_cap_usd",
    "local.autostart",
    "local.model",
    "tui.fullscreen",
    "tui.mouse_capture",
    "recap.enabled",
    "mesh",
    "local",
    "tui",
];

/// Discover every scalar setting from the *effective* config (defaults + user + project), as
/// importance-ordered dotted-path leaves. Arrays and the complex sections are skipped.
pub fn config_leaves() -> Vec<SettingLeaf> {
    let cfg = load().unwrap_or_default();
    let value = serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null);
    let mut out = Vec::new();
    flatten_value("", &value, &mut out);
    out.sort_by(|a, b| {
        priority_rank(&a.path)
            .cmp(&priority_rank(&b.path))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn priority_rank(path: &str) -> usize {
    // Most specific matching prefix wins (so `mesh.credit_mode` beats the `mesh` catch-all).
    PRIORITY_PREFIXES
        .iter()
        .enumerate()
        .filter(|(_, p)| path == **p || path.starts_with(&format!("{p}.")))
        .map(|(i, _)| i)
        .min()
        .unwrap_or(usize::MAX)
}

/// The editing control a setting should use in the `/config` UI.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingKind {
    /// On/off — toggled, never typed.
    Bool,
    Int,
    Float,
    /// A fixed set of valid values — cycled / picked, never typed.
    Enum(Vec<&'static str>),
    Text,
}

/// A fully-described setting for the friendly `/config` editor: friendly label + group, help, the
/// editing control, the current/default values, whether it's been overridden, and from where.
#[derive(Debug, Clone)]
pub struct SettingDescriptor {
    pub path: String,
    /// Section header it groups under (e.g. "Mesh & Cost").
    pub group: String,
    /// Friendly display name (e.g. "Daily spend cap").
    pub label: String,
    pub help: Option<String>,
    pub kind: SettingKind,
    /// Current effective value.
    pub value: SettingValue,
    /// Built-in default value.
    pub default: SettingValue,
    /// True when set in a config file (overrides the default).
    pub modified: bool,
    /// Where the effective value comes from: "project" | "user" | "default".
    pub source: &'static str,
}

/// Valid values for an enum-typed setting (so the editor can cycle them instead of free text).
pub fn setting_options(path: &str) -> Option<Vec<&'static str>> {
    Some(match path {
        "permission_mode" => vec!["default", "accept-edits", "bypass", "plan"],
        "mesh.credit_mode" => vec!["normal", "frugal", "strict"],
        "mesh.classifier" => vec!["heuristic", "llm"],
        "mesh.default_effort" => vec!["low", "medium", "high", "xhigh", "max"],
        "lattice.embeddings.backend" => vec!["auto", "ollama", "openai", "gemini"],
        _ => return None,
    })
}

/// The section group and friendly label for a setting path. Curated for common settings; anything
/// else derives a sensible group (from the first path segment) + label (humanized last segment), so
/// new fields still slot in nicely without code changes.
pub fn setting_group_and_label(path: &str) -> (String, String) {
    let curated = match path {
        "permission_mode" => Some(("Safety", "Permission mode")),
        "mesh.credit_mode" => Some(("Mesh & Cost", "Credit conservation")),
        "mesh.daily_cap_usd" => Some(("Mesh & Cost", "Daily spend cap (USD)")),
        "mesh.weekly_cap_usd" => Some(("Mesh & Cost", "Weekly spend cap (USD)")),
        "mesh.monthly_cap_usd" => Some(("Mesh & Cost", "Monthly spend cap (USD)")),
        "mesh.classifier" => Some(("Mesh & Cost", "Task classifier")),
        "mesh.classifier_model" => Some(("Mesh & Cost", "Classifier model")),
        "mesh.prefer_subscription" => Some(("Mesh & Cost", "Prefer subscriptions")),
        "mesh.max_output_tokens" => Some(("Mesh & Cost", "Max output tokens")),
        "mesh.architect_mode" => Some(("Mesh & Cost", "Architect mode")),
        "mesh.architect_model" => Some(("Mesh & Cost", "Architect model")),
        "mesh.editor_model" => Some(("Mesh & Cost", "Editor model")),
        "mesh.self_review" => Some(("Mesh & Cost", "Self-review writes")),
        "mesh.default_effort" => Some(("Mesh & Cost", "Default reasoning effort")),
        "local.autostart" => Some(("Local LLM", "Auto-start on launch")),
        "local.model" => Some(("Local LLM", "Model (Ollama tag)")),
        "local.endpoint" => Some(("Local LLM", "Ollama endpoint")),
        "tui.fullscreen" => Some(("Interface", "Full-screen TUI")),
        "tui.mouse_capture" => Some(("Interface", "Mouse wheel scroll")),
        "recap.enabled" => Some(("Interface", "Per-turn recap")),
        "update.check" => Some(("Interface", "Check for updates")),
        "shell.explain_errors" => Some(("Shell", "Explain failed commands")),
        "lattice.enabled" => Some(("Code Intelligence", "Enabled")),
        "lattice.inject" => Some(("Code Intelligence", "Auto-inject context")),
        "lattice.watch" => Some(("Code Intelligence", "Watch & reindex")),
        "lattice.embeddings.backend" => Some(("Code Intelligence", "Embeddings backend")),
        "autofix.enabled" => Some(("Autofix", "Enabled")),
        "autofix.max_iterations" => Some(("Autofix", "Max iterations")),
        "autofix.auto_detect" => Some(("Autofix", "Auto-detect commands")),
        "assay.gate_enabled" => Some(("Assay", "Review gate")),
        "assay.max_cost_usd" => Some(("Assay", "Max cost (USD)")),
        "git.coauthor" => Some(("Git", "Co-author commits")),
        "lsp.enabled" => Some(("Code Intelligence", "LSP diagnostics")),
        "mesh.auto_orchestrate" => Some(("Behaviour", "Auto-orchestrate")),
        _ => None,
    };
    if let Some((g, l)) = curated {
        return (g.to_string(), l.to_string());
    }
    // Fallback: group from the top segment, label humanized from the last segment.
    let top = path.split('.').next().unwrap_or(path);
    let last = path.rsplit('.').next().unwrap_or(path);
    (humanize(top), humanize(last))
}

fn humanize(s: &str) -> String {
    let mut out = String::new();
    for (i, word) in s.split('_').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut cs = word.chars();
        if let Some(c) = cs.next() {
            out.extend(c.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

/// Build the full descriptor list for the friendly `/config` editor: every scalar setting with its
/// group, label, help, control kind, value, default, modified flag, and source — importance-ordered.
pub fn config_descriptors() -> Vec<SettingDescriptor> {
    // Effective leaves (defaults + user + project).
    let leaves = config_leaves();
    // Default-only values, for the "default" column + modified detection.
    let default_value = serde_json::to_value(Config::default()).unwrap_or(serde_json::Value::Null);
    let mut default_leaves = Vec::new();
    flatten_value("", &default_value, &mut default_leaves);
    let defaults: std::collections::HashMap<String, SettingValue> = default_leaves
        .into_iter()
        .map(|l| (l.path, l.value))
        .collect();
    // Which file set each path (for source + modified).
    let user_table = read_table(scope_path(ConfigScope::User).ok().as_deref());
    let project_table = read_table(Some(std::path::Path::new("./.forge/config.toml")));

    let mut descriptors: Vec<SettingDescriptor> = leaves
        .into_iter()
        .map(|l| {
            let (group, label) = setting_group_and_label(&l.path);
            let kind = match setting_options(&l.path) {
                Some(opts) => SettingKind::Enum(opts),
                None => match l.value {
                    SettingValue::Bool(_) => SettingKind::Bool,
                    SettingValue::Int(_) => SettingKind::Int,
                    SettingValue::Float(_) => SettingKind::Float,
                    _ => SettingKind::Text,
                },
            };
            let in_project = project_table
                .as_ref()
                .is_some_and(|t| dotted_present(t, &l.path));
            let in_user = user_table
                .as_ref()
                .is_some_and(|t| dotted_present(t, &l.path));
            let source = if in_project {
                "project"
            } else if in_user {
                "user"
            } else {
                "default"
            };
            let default = defaults
                .get(&l.path)
                .cloned()
                .unwrap_or(SettingValue::Unset);
            SettingDescriptor {
                help: setting_help(&l.path).map(str::to_string),
                kind,
                value: l.value,
                modified: in_project || in_user,
                default,
                group,
                label,
                source,
                path: l.path,
            }
        })
        .collect();
    // Group rows so each section is contiguous; sections ordered by the importance of their first
    // member (descriptors are already importance-ordered), rows kept in that order within a group.
    let mut group_order: Vec<String> = Vec::new();
    for d in &descriptors {
        if !group_order.contains(&d.group) {
            group_order.push(d.group.clone());
        }
    }
    descriptors.sort_by_key(|d| {
        group_order
            .iter()
            .position(|g| g == &d.group)
            .unwrap_or(usize::MAX)
    });
    descriptors
}

fn read_table(path: Option<&std::path::Path>) -> Option<toml::Table> {
    let p = path?;
    std::fs::read_to_string(p).ok()?.parse().ok()
}

fn dotted_present(table: &toml::Table, path: &str) -> bool {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = table;
    for p in &parts[..parts.len() - 1] {
        match cur.get(*p).and_then(|v| v.as_table()) {
            Some(t) => cur = t,
            None => return false,
        }
    }
    cur.contains_key(parts[parts.len() - 1])
}

fn flatten_value(prefix: &str, value: &serde_json::Value, out: &mut Vec<SettingLeaf>) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                // Skip complex top-level sections entirely (their own commands own them).
                if prefix.is_empty() && COMPLEX_SECTIONS.contains(&k.as_str()) {
                    continue;
                }
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_value(&path, v, out);
            }
        }
        // Arrays are complex — excluded here.
        Value::Array(_) => {}
        Value::Bool(b) => out.push(leaf(prefix, SettingValue::Bool(*b))),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push(leaf(prefix, SettingValue::Int(i)));
            } else if let Some(f) = n.as_f64() {
                out.push(leaf(prefix, SettingValue::Float(f)));
            }
        }
        Value::String(s) => out.push(leaf(prefix, SettingValue::Str(s.clone()))),
        Value::Null => out.push(leaf(prefix, SettingValue::Unset)),
    }
}

fn leaf(path: &str, value: SettingValue) -> SettingLeaf {
    SettingLeaf {
        path: path.to_string(),
        value,
    }
}

/// One-line help for a setting path, shown in the `/config` editor. `None` for paths without a
/// curated description (they still appear and are editable — just without help text).
pub fn setting_help(path: &str) -> Option<&'static str> {
    Some(match path {
        "permission_mode" => "Tool-safety posture for new sessions: default · accept-edits · bypass · plan.",
        "mesh.credit_mode" => "Subscription conservation: normal · frugal · strict (spread work off paid plans).",
        "mesh.daily_cap_usd" => "Hard daily spend cap (USD) across sessions; the mesh downshifts/stops near it.",
        "mesh.weekly_cap_usd" => "Hard weekly spend cap (USD). Empty = unlimited.",
        "mesh.monthly_cap_usd" => "Hard monthly spend cap (USD). Empty = unlimited.",
        "mesh.classifier" => "Task-tier classifier: heuristic (instant, no call) or llm (a cheap model labels each turn).",
        "mesh.classifier_model" => "Model the llm classifier calls — pick a $0/local one (e.g. ollama::… or a CLI bridge).",
        "mesh.prefer_subscription" => "Prefer $0 CLI-bridge subscriptions over a metered API model on a tie.",
        "mesh.max_output_tokens" => "Cap on tokens a model may generate per call.",
        "mesh.architect_mode" => "Use a stronger 'architect' model to plan, a cheaper one to edit.",
        "mesh.architect_model" => "Model used for the architect/planning pass when architect_mode is on.",
        "mesh.editor_model" => "Model used to apply edits when architect_mode is on.",
        "mesh.self_review" => "After a write turn, have the model review its own diff before finishing.",
        "mesh.default_effort" => "Default reasoning effort for models that support it (low/medium/high/…).",
        "local.autostart" => "Start the local Ollama model automatically when `forge chat` launches.",
        "local.model" => "Ollama tag to auto-start (e.g. gemma4:12b). Set it via `forge local install`.",
        "local.endpoint" => "Ollama HTTP endpoint (default http://localhost:11434).",
        "tui.fullscreen" => "Full-screen TUI on the alternate screen. Off = inline in native scrollback.",
        "tui.mouse_capture" => "Wheel scrolls the transcript in full-screen mode (minimal button+wheel reporting, no motion tracking — native click-drag text selection still works). Default on. Off disables mouse reporting entirely; scroll with PgUp/PgDn/Home/End.",
        "recap.enabled" => "Show a one-line AI recap after each completed turn.",
        "update.check" => "Check GitHub for a newer Forge release on startup (throttled to once a day).",
        "shell.explain_errors" => "When a shell command fails, the AI explains the likely cause + a fix.",
        "lattice.enabled" => "Build/maintain the code-intelligence graph (`forge lattice`).",
        "lattice.inject" => "Auto-inject relevant code into each turn before the model call.",
        "lattice.watch" => "Reindex changed files automatically as you edit.",
        "autofix.enabled" => "After edits, run lint/test and feed failures back so the model self-heals.",
        "autofix.max_iterations" => "Max self-heal passes before giving up.",
        "autofix.auto_detect" => "Detect lint/test commands from project structure when lint_cmd/test_cmd are empty (Cargo.toml → cargo check; package.json → npm run lint).",
        "assay.gate_enabled" => "Run an Assay review on write turns before they finish.",
        "assay.max_cost_usd" => "Per-run cost ceiling for the Assay critic crew.",
        "git.coauthor" => "Install a commit hook stamping Co-Authored-By: Forge and stripping CLI co-authors.",
        "lsp.enabled" => "Feed language-server diagnostics back into the turn after edits.",
        "mesh.auto_orchestrate" => "Inject the orchestration framework every session: skills first, highest-level tool, subagents/MCP/web/Lattice — no need to /orchestrate manually.",
        _ => return None,
    })
}

/// The config file path for a scope.
pub fn scope_path(scope: ConfigScope) -> Result<PathBuf, ConfigError> {
    match scope {
        ConfigScope::User => Ok(config_dir()
            .ok_or(ConfigError::NoConfigDir)?
            .join("config.toml")),
        ConfigScope::Project => Ok(PathBuf::from("./.forge/config.toml")),
    }
}

/// Set a dotted-path scalar in the given scope's `config.toml`, preserving every other key. `raw` is
/// coerced to the leaf's existing type (bool/int/float/text); an empty value on an optional clears
/// it. The result is validated by re-extracting the whole `Config` — a bad value (e.g. an invalid
/// enum) is rejected and the file is left untouched.
pub fn set_config_value(scope: ConfigScope, path: &str, raw: &str) -> Result<(), ConfigError> {
    let leaves = config_leaves();
    let existing = leaves.iter().find(|l| l.path == path);
    let coerced = coerce_value(raw, existing.map(|l| &l.value))?;

    let file = scope_path(scope)?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ConfigError::Write(e.to_string()))?;
    }
    let mut root: toml::Table = std::fs::read_to_string(&file)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    match coerced {
        Some(v) => set_dotted(&mut root, path, v),
        None => remove_dotted(&mut root, path), // empty → clear the optional
    }

    // Validate: the file must still extract to a Config layered over the defaults.
    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    Figment::from(Serialized::defaults(Config::default()))
        .merge(Toml::string(&body))
        .extract::<Config>()
        .map_err(|e| ConfigError::Write(format!("invalid value for {path}: {e}")))?;

    std::fs::write(&file, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(())
}

/// Reset a setting to its default by removing it from the given scope's `config.toml` (and, when
/// resetting user scope, also from the project file if present, so the default actually takes
/// effect). No-op if absent. The remaining file is validated.
pub fn reset_config_value(scope: ConfigScope, path: &str) -> Result<(), ConfigError> {
    let file = scope_path(scope)?;
    let Some(text) = std::fs::read_to_string(&file).ok() else {
        return Ok(()); // nothing written → already default
    };
    let mut root: toml::Table = text.parse().unwrap_or_default();
    remove_dotted(&mut root, path);
    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    Figment::from(Serialized::defaults(Config::default()))
        .merge(Toml::string(&body))
        .extract::<Config>()
        .map_err(|e| ConfigError::Write(format!("invalid config after reset of {path}: {e}")))?;
    std::fs::write(&file, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(())
}

/// Coerce raw input to a TOML value matching the existing leaf's type. `None` = clear (empty input
/// on an optional/text). Errors on a malformed bool/number.
fn coerce_value(
    raw: &str,
    existing: Option<&SettingValue>,
) -> Result<Option<toml::Value>, ConfigError> {
    let t = raw.trim();
    match existing {
        Some(SettingValue::Bool(_)) => {
            let b = match t.to_ascii_lowercase().as_str() {
                "true" | "on" | "yes" | "1" => true,
                "false" | "off" | "no" | "0" => false,
                _ => {
                    return Err(ConfigError::Write(format!(
                        "expected a boolean, got '{raw}'"
                    )))
                }
            };
            Ok(Some(toml::Value::Boolean(b)))
        }
        Some(SettingValue::Int(_)) => t
            .parse::<i64>()
            .map(|i| Some(toml::Value::Integer(i)))
            .map_err(|_| ConfigError::Write(format!("expected an integer, got '{raw}'"))),
        Some(SettingValue::Float(_)) => t
            .parse::<f64>()
            .map(|f| Some(toml::Value::Float(f)))
            .map_err(|_| ConfigError::Write(format!("expected a number, got '{raw}'"))),
        // Text or unset/optional: empty clears, otherwise a string.
        _ => {
            if t.is_empty() {
                Ok(None)
            } else {
                Ok(Some(toml::Value::String(t.to_string())))
            }
        }
    }
}

fn set_dotted(root: &mut toml::Table, path: &str, val: toml::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for p in &parts[..parts.len() - 1] {
        let entry = cur
            .entry(p.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if !entry.is_table() {
            *entry = toml::Value::Table(toml::Table::new());
        }
        cur = entry.as_table_mut().unwrap();
    }
    cur.insert(parts[parts.len() - 1].to_string(), val);
}

fn remove_dotted(root: &mut toml::Table, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut cur = root;
    for p in &parts[..parts.len() - 1] {
        match cur.get_mut(*p).and_then(|v| v.as_table_mut()) {
            Some(t) => cur = t,
            None => return,
        }
    }
    cur.remove(parts[parts.len() - 1]);
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
// matches the name the genai adapter reads — so a key set here is picked up end-to-end. Every
// provider here has a NATIVE genai adapter. Providers genai has no adapter for live in
// [`CUSTOM_OPENAI_PROVIDERS`] (OpenAI-compatible endpoints Forge wires via a custom resolver);
// the key/discovery accessors below chain both tables so adding either kind is one row.
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
    ("cohere", "COHERE_API_KEY"), // native adapter — Command A (218B), free trial tier
    // Enterprise gateways with a NATIVE genai 0.6 adapter (Bearer/key auth). They route fine but are
    // marked non-listable in `forge-provider::is_discoverable` (no enumerable models endpoint for our
    // flow) so discovery skips them quietly; users pin `bedrock::…` / `vertex::…` model ids.
    //   • Bedrock → genai's `bedrock_api` adapter (`forge-provider::normalize_namespace` maps it).
    //     AWS Bedrock Converse with a long-lived Bedrock API key (Bearer). SigV4 auth is NOT wired.
    //   • Vertex → genai's `vertex` adapter. ALSO needs `VERTEX_PROJECT_ID` (+ optional
    //     `VERTEX_LOCATION`) exported in the environment — genai reads those directly; Forge only
    //     manages the `VERTEX_API_KEY`. See docs / `forge provider list`.
    ("bedrock", "BEDROCK_API_KEY"),
    ("vertex", "VERTEX_API_KEY"),
];

/// Enterprise gateways Forge has config/CLI scaffolding for but CANNOT route to with the pinned genai
/// version — shown by `forge provider list` with the reason, never entered into routing (honest: not
/// faked). `(namespace, why)`. Empty now that Azure OpenAI is wired through genai's per-request
/// URL+header override (see [`AzureConfig`] / `[providers.azure]`); kept as the home for the next
/// gateway that genai can't yet reach.
pub const UNWIRED_ENTERPRISE_PROVIDERS: &[(&str, &str)] = &[];

/// Provider namespace for Azure OpenAI model ids (`azure::<deployment>`).
pub const AZURE_NS: &str = "azure";

/// Default env var Forge reads for the Azure OpenAI API key (overridable via `api_key_env`).
pub const AZURE_DEFAULT_KEY_ENV: &str = "AZURE_OPENAI_API_KEY";

/// Default Azure REST `api-version` when the config omits one. A recent GA version that supports
/// tool/function calling; the user can pin any version their resource exposes via `api_version`.
pub const DEFAULT_AZURE_API_VERSION: &str = "2024-10-21";

/// A resolved, validated Azure OpenAI provider (from `[providers.azure]`). The genai client builds a
/// per-request `AuthData::RequestOverride` from this to retarget the OpenAI adapter at Azure's
/// deployment-scoped URL with an `api-key` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureProvider {
    /// Resource endpoint base, no trailing slash (e.g. `https://my-resource.openai.azure.com`).
    pub endpoint: String,
    /// Azure REST `api-version` query value.
    pub api_version: String,
    /// Env var holding the API key.
    pub env_var: String,
    /// Deployment names → `azure::<deployment>` model ids.
    pub deployments: Vec<String>,
    /// Whether these deployments are free (Azure is metered, so normally false).
    pub free: bool,
    /// Human label for `forge provider list`.
    pub label: String,
}

impl AzureProvider {
    /// The full Azure chat-completions URL for a deployment, including the `api-version` query. This
    /// is what the genai OpenAI adapter's request is redirected to via `AuthData::RequestOverride`.
    pub fn chat_completions_url(&self, deployment: &str) -> String {
        format!(
            "{}/openai/deployments/{}/chat/completions?api-version={}",
            self.endpoint.trim_end_matches('/'),
            deployment,
            self.api_version
        )
    }
}

impl AzureConfig {
    /// Resolve the endpoint base (no trailing slash) from `endpoint` (preferred) or `resource`.
    fn resolved_endpoint(&self) -> Option<String> {
        if let Some(ep) = self
            .endpoint
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            return Some(ep.trim_end_matches('/').to_string());
        }
        let res = self
            .resource
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())?;
        Some(format!("https://{res}.openai.azure.com"))
    }

    /// Validate + resolve into an [`AzureProvider`]. Requires `endpoint` or `resource`; the
    /// endpoint must be `http(s)://`. `api_version`/`api_key_env` fall back to the defaults.
    pub fn into_provider(self) -> Result<AzureProvider, String> {
        let endpoint = self
            .resolved_endpoint()
            .ok_or_else(|| "[providers.azure] needs `resource` or `endpoint`".to_string())?;
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err(format!(
                "azure endpoint '{endpoint}' must start with http(s)://"
            ));
        }
        let api_version = self
            .api_version
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_AZURE_API_VERSION.to_string());
        let env_var = self
            .api_key_env
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| AZURE_DEFAULT_KEY_ENV.to_string());
        Ok(AzureProvider {
            endpoint,
            api_version,
            env_var,
            deployments: self
                .deployments
                .into_iter()
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty())
                .collect(),
            free: self.free,
            label: self.label.unwrap_or_default(),
        })
    }
}

/// The resolved Azure provider, cached process-wide (build-once, like the custom-provider registry).
/// `None` when no valid `[providers.azure]` block is configured.
static AZURE_REGISTRY: std::sync::OnceLock<Option<AzureProvider>> = std::sync::OnceLock::new();

fn azure_registry() -> Option<&'static AzureProvider> {
    AZURE_REGISTRY
        .get_or_init(|| {
            let cfg = load().ok().and_then(|c| c.providers.azure)?;
            match cfg.into_provider() {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!("ignoring invalid [providers.azure]: {e}");
                    None
                }
            }
        })
        .as_ref()
}

/// The configured Azure OpenAI provider, if `[providers.azure]` is present and valid. Used by the
/// provider client to build the per-request Azure override, and by discovery to seed deployments.
pub fn azure_provider() -> Option<&'static AzureProvider> {
    azure_registry()
}

/// An OpenAI-compatible API provider that genai has **no native adapter** for. Forge reaches it
/// by retargeting genai's OpenAI adapter at `endpoint` with the key from `env_var` (see the
/// service-target resolver in `forge-provider`). These providers expose a standard
/// `/chat/completions` but cannot be model-LISTED through genai, so the mesh seeds `seed_models`
/// for them at discovery instead of enumerating live.
///
/// Adding a new OpenAI-compatible provider is a single row here: that wires auth (`forge auth
/// <namespace>`), env injection, routing, mesh discovery, and the free/paid flag end-to-end.
#[derive(Debug, Clone, Copy)]
pub struct CustomProvider {
    /// The `provider::` namespace in model ids and the `forge auth` name.
    pub namespace: &'static str,
    /// Full base URL, trailing slash included (genai appends `chat/completions`).
    pub endpoint: &'static str,
    /// Environment variable holding the API key.
    pub env_var: &'static str,
    /// Whether the provider's models are genuinely free to call (standing free tier).
    pub free: bool,
    /// Human label + tier hint shown in `forge init` / `forge auth`.
    pub label: &'static str,
    /// Curated bare model ids (no namespace) seeded into the mesh when a key is present, since
    /// these providers have no live model-listing API. Users can pin any `namespace::model`.
    pub seed_models: &'static [&'static str],
}

/// OpenAI-compatible providers with no native genai adapter, reached via the custom endpoint
/// resolver in `forge-provider`. Single source of truth for their endpoint, key env var, free
/// flag, and curated seed models — every key/discovery accessor chains this with
/// [`PROVIDER_ENV_VARS`]. Add a provider by appending one row.
pub const CUSTOM_OPENAI_PROVIDERS: &[CustomProvider] = &[
    CustomProvider {
        namespace: "cerebras",
        endpoint: "https://api.cerebras.ai/v1/",
        env_var: "CEREBRAS_API_KEY",
        free: true,
        label: "Cerebras — free tier (very fast)",
        seed_models: &["llama-3.3-70b", "gpt-oss-120b", "qwen-3-coder-480b"],
    },
    CustomProvider {
        namespace: "nvidia",
        endpoint: "https://integrate.api.nvidia.com/v1/",
        env_var: "NVIDIA_API_KEY",
        free: true,
        label: "NVIDIA NIM — free developer tier (100+ models)",
        seed_models: &[
            "deepseek-ai/deepseek-r1",
            "meta/llama-3.1-405b-instruct",
            "meta/llama-3.3-70b-instruct",
            "qwen/qwen2.5-coder-32b-instruct",
            "nvidia/llama-3.1-nemotron-70b-instruct",
        ],
    },
    CustomProvider {
        namespace: "sambanova",
        endpoint: "https://api.sambanova.ai/v1/",
        env_var: "SAMBANOVA_API_KEY",
        free: true,
        label: "SambaNova — free tier (fast, frontier OSS)",
        seed_models: &[
            "DeepSeek-V3.1",
            "DeepSeek-R1",
            "Meta-Llama-3.3-70B-Instruct",
            "Llama-4-Maverick-17B-128E-Instruct",
        ],
    },
    CustomProvider {
        namespace: "mistral",
        endpoint: "https://api.mistral.ai/v1/",
        env_var: "MISTRAL_API_KEY",
        free: true,
        label: "Mistral — free Experiment tier (La Plateforme)",
        seed_models: &[
            "mistral-large-latest",
            "mistral-small-latest",
            "codestral-latest",
            "magistral-medium-latest",
        ],
    },
    // Popular OSS gateways — all OpenAI-compatible, reached via the same custom resolver. Paid
    // (metered), so `free: false`: priced-by-token, not a standing free tier.
    CustomProvider {
        namespace: "together",
        endpoint: "https://api.together.xyz/v1/",
        env_var: "TOGETHER_API_KEY",
        free: false,
        label: "Together AI — gateway (OSS frontier, metered)",
        seed_models: &[
            "deepseek-ai/DeepSeek-V3",
            "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            "Qwen/Qwen2.5-Coder-32B-Instruct",
        ],
    },
    CustomProvider {
        namespace: "fireworks",
        endpoint: "https://api.fireworks.ai/inference/v1/",
        env_var: "FIREWORKS_API_KEY",
        free: false,
        label: "Fireworks AI — gateway (fast OSS, metered)",
        seed_models: &[
            "accounts/fireworks/models/deepseek-v3",
            "accounts/fireworks/models/llama-v3p3-70b-instruct",
            "accounts/fireworks/models/qwen2p5-coder-32b-instruct",
        ],
    },
    CustomProvider {
        namespace: "perplexity",
        endpoint: "https://api.perplexity.ai/",
        env_var: "PERPLEXITY_API_KEY",
        free: false,
        label: "Perplexity — Sonar (online + reasoning, metered)",
        seed_models: &[
            "sonar",
            "sonar-pro",
            "sonar-reasoning",
            "sonar-reasoning-pro",
        ],
    },
];

/// Owned form of a runtime-registered custom provider (from a `[[providers.custom]]` block), after
/// validation + endpoint normalization. Leaked into a `'static` [`CustomProvider`] by
/// [`build_custom_registry`] so it joins the built-ins transparently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCustomProvider {
    pub namespace: String,
    pub endpoint: String,
    /// Env var holding the key; `""` = keyless (a placeholder is sent by the provider client).
    pub env_var: String,
    pub free: bool,
    pub label: String,
    pub seed_models: Vec<String>,
}

/// The merged custom-provider registry: built-in [`CUSTOM_OPENAI_PROVIDERS`] + any runtime
/// `[[providers.custom]]` entries. Initialized once (lazily from config, or explicitly via
/// [`register_custom_providers`]) and then immutable for the process — matching the build-once
/// nature of the genai client and the const it replaces.
static CUSTOM_PROVIDER_REGISTRY: std::sync::OnceLock<Vec<CustomProvider>> =
    std::sync::OnceLock::new();

/// The active custom-provider registry. On first access, lazily merges the built-ins with the
/// runtime `[[providers.custom]]` entries read from the loaded config (zero call-site wiring), unless
/// [`register_custom_providers`] already seeded it. Process-lifetime `'static`.
fn custom_provider_registry() -> &'static [CustomProvider] {
    CUSTOM_PROVIDER_REGISTRY
        .get_or_init(|| build_custom_registry(&load_runtime_custom_providers()))
        .as_slice()
}

/// Read + validate the runtime `[[providers.custom]]` entries from the loaded config. Best-effort:
/// a malformed entry is skipped (logged), never fatal.
fn load_runtime_custom_providers() -> Vec<RuntimeCustomProvider> {
    let custom = load().map(|c| c.providers.custom).unwrap_or_default();
    custom
        .into_iter()
        .filter_map(|c| match c.clone().into_runtime() {
            Ok(rp) => Some(rp),
            Err(e) => {
                tracing::warn!(
                    "ignoring invalid [[providers.custom]] '{}': {e}",
                    c.namespace
                );
                None
            }
        })
        .collect()
}

/// Merge built-in custom providers with `runtime` ones, leaking the owned runtime strings to
/// `'static`. A runtime entry whose namespace collides with a built-in custom provider OR a native
/// adapter is dropped — built-ins always win, so a config typo can't shadow a first-class provider.
fn build_custom_registry(runtime: &[RuntimeCustomProvider]) -> Vec<CustomProvider> {
    let mut out: Vec<CustomProvider> = CUSTOM_OPENAI_PROVIDERS.to_vec();
    for rp in runtime {
        let collides = out.iter().any(|p| p.namespace == rp.namespace)
            || PROVIDER_ENV_VARS.iter().any(|(n, _)| *n == rp.namespace);
        if collides {
            tracing::warn!(
                "[[providers.custom]] '{}' collides with a built-in provider — ignored",
                rp.namespace
            );
            continue;
        }
        let seeds: Vec<&'static str> = rp.seed_models.iter().cloned().map(leak_str).collect();
        let label = if rp.label.is_empty() {
            format!("{} — custom OpenAI endpoint", rp.namespace)
        } else {
            rp.label.clone()
        };
        out.push(CustomProvider {
            namespace: leak_str(rp.namespace.clone()),
            endpoint: leak_str(rp.endpoint.clone()),
            env_var: leak_str(rp.env_var.clone()),
            free: rp.free,
            label: leak_str(label),
            seed_models: Box::leak(seeds.into_boxed_slice()),
        });
    }
    out
}

/// Leak a `String` to `&'static str`. Only ever called on the bounded, build-once provider registry
/// (a few entries for the whole process), so the leak is intentional and negligible.
fn leak_str(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Explicitly seed the custom-provider registry (built-ins + `runtime`). No-op if already
/// initialized (lazily or by a prior call). Primarily for tests / explicit startup wiring; normal
/// runs initialize lazily from config on first [`custom_provider`]/[`custom_providers`] access.
pub fn register_custom_providers(runtime: &[RuntimeCustomProvider]) {
    let _ = CUSTOM_PROVIDER_REGISTRY.set(build_custom_registry(runtime));
}

/// The custom OpenAI-compatible provider registered under `namespace` (built-in or runtime), if any.
pub fn custom_provider(namespace: &str) -> Option<&'static CustomProvider> {
    custom_provider_registry()
        .iter()
        .find(|p| p.namespace == namespace)
}

/// All custom OpenAI-compatible providers (built-in + runtime-registered).
pub fn custom_providers() -> impl Iterator<Item = &'static CustomProvider> {
    custom_provider_registry().iter()
}

/// Normalize an OpenAI-compatible base URL to the form the resolver + `/models` listing expect: a
/// trailing slash (so `{endpoint}models` / `{endpoint}chat/completions` join correctly). Accepts
/// `http://h/v1` and `http://h/v1/` identically.
pub fn normalize_endpoint(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    }
}

impl CustomProviderConfig {
    /// Validate + normalize into a [`RuntimeCustomProvider`]. Rejects an empty/odd namespace or a
    /// non-HTTP base URL; an absent `api_key_env` means keyless (`env_var = ""`).
    pub fn into_runtime(self) -> Result<RuntimeCustomProvider, String> {
        let namespace = self.namespace.trim().to_string();
        if namespace.is_empty()
            || !namespace
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "namespace '{namespace}' must be non-empty and only [A-Za-z0-9_-]"
            ));
        }
        let endpoint = normalize_endpoint(&self.base_url);
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err(format!(
                "base_url '{}' must start with http(s)://",
                self.base_url
            ));
        }
        let env_var = self
            .api_key_env
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        Ok(RuntimeCustomProvider {
            namespace,
            endpoint,
            env_var,
            free: self.free,
            label: self.label.unwrap_or_default(),
            seed_models: self
                .models
                .into_iter()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .collect(),
        })
    }
}

/// Persist a `[[providers.custom]]` block to the user `config.toml`, replacing any existing entry
/// with the same namespace and preserving every other key. Validates first (so a bad endpoint fails
/// before writing) and that the whole file still extracts to a [`Config`]. Returns the path written.
/// Active on the next session start (the registry is build-once per process).
pub fn add_custom_provider(p: &CustomProviderConfig) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    add_custom_provider_at(&path, p)?;
    Ok(path)
}

/// The file half of [`add_custom_provider`] against an explicit path — split out so it's testable
/// without touching the real per-user config directory (mirrors `write_subscriptions_at`).
fn add_custom_provider_at(
    path: &std::path::Path,
    p: &CustomProviderConfig,
) -> Result<(), ConfigError> {
    // Validate the entry (and that it doesn't collide with a native/built-in provider).
    let rp = p.clone().into_runtime().map_err(ConfigError::Write)?;
    if PROVIDER_ENV_VARS.iter().any(|(n, _)| *n == rp.namespace)
        || CUSTOM_OPENAI_PROVIDERS
            .iter()
            .any(|c| c.namespace == rp.namespace)
    {
        return Err(ConfigError::Write(format!(
            "'{}' is a built-in provider — pick another namespace",
            rp.namespace
        )));
    }
    let mut root: toml::Table = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let providers = root
        .entry("providers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !providers.is_table() {
        *providers = toml::Value::Table(toml::Table::new());
    }
    let custom = providers
        .as_table_mut()
        .unwrap()
        .entry("custom".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    if !custom.is_array() {
        *custom = toml::Value::Array(Vec::new());
    }
    let arr = custom.as_array_mut().unwrap();
    arr.retain(|v| v.get("namespace").and_then(|n| n.as_str()) != Some(rp.namespace.as_str()));

    let mut entry = toml::Table::new();
    entry.insert(
        "namespace".into(),
        toml::Value::String(rp.namespace.clone()),
    );
    entry.insert(
        "base_url".into(),
        toml::Value::String(p.base_url.trim().to_string()),
    );
    if let Some(env) = &p.api_key_env {
        if !env.trim().is_empty() {
            entry.insert(
                "api_key_env".into(),
                toml::Value::String(env.trim().to_string()),
            );
        }
    }
    if p.free {
        entry.insert("free".into(), toml::Value::Boolean(true));
    }
    if !rp.seed_models.is_empty() {
        entry.insert(
            "models".into(),
            toml::Value::Array(
                rp.seed_models
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
    }
    if let Some(label) = &p.label {
        if !label.trim().is_empty() {
            entry.insert(
                "label".into(),
                toml::Value::String(label.trim().to_string()),
            );
        }
    }
    arr.push(toml::Value::Table(entry));

    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    Figment::from(Serialized::defaults(Config::default()))
        .merge(Toml::string(&body))
        .extract::<Config>()
        .map_err(|e| ConfigError::Write(format!("invalid config after add: {e}")))?;
    std::fs::write(path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(())
}

/// Remove a user-registered `[[providers.custom]]` entry by namespace from the user `config.toml`.
/// `Ok(true)` if one was removed, `Ok(false)` if absent (idempotent). Built-ins can't be removed.
pub fn remove_custom_provider(namespace: &str) -> Result<bool, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    let path = dir.join("config.toml");
    remove_custom_provider_at(&path, namespace)
}

fn remove_custom_provider_at(path: &std::path::Path, namespace: &str) -> Result<bool, ConfigError> {
    let Some(text) = std::fs::read_to_string(path).ok() else {
        return Ok(false);
    };
    let mut root: toml::Table = text.parse().unwrap_or_default();
    let mut removed = false;
    if let Some(arr) = root
        .get_mut("providers")
        .and_then(|p| p.as_table_mut())
        .and_then(|p| p.get_mut("custom"))
        .and_then(|c| c.as_array_mut())
    {
        let before = arr.len();
        arr.retain(|v| v.get("namespace").and_then(|n| n.as_str()) != Some(namespace));
        removed = arr.len() != before;
    }
    if removed {
        let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
        std::fs::write(path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    }
    Ok(removed)
}

/// The user-declared `[[providers.custom]]` entries from config (for `forge provider list`). Empty if
/// none / unreadable. Distinct from [`custom_providers`], which also includes the built-ins.
pub fn user_custom_providers() -> Vec<CustomProviderConfig> {
    load().map(|c| c.providers.custom).unwrap_or_default()
}

/// The raw `[providers.azure]` block from config (unresolved), for `forge provider list` to show what
/// the user set. `None` if absent. [`azure_provider`] returns the validated, resolved form.
pub fn user_azure_config() -> Option<AzureConfig> {
    load().ok().and_then(|c| c.providers.azure)
}

/// Persist a `[providers.azure]` block to the user `config.toml`, validating first (a bad
/// resource/endpoint fails before writing) and that the whole file still extracts to a [`Config`].
/// Returns the path written. Active on the next session start (the registry is build-once).
pub fn add_azure_provider(cfg: &AzureConfig) -> Result<PathBuf, ConfigError> {
    let dir = config_dir().ok_or(ConfigError::NoConfigDir)?;
    std::fs::create_dir_all(&dir).map_err(|e| ConfigError::Write(e.to_string()))?;
    let path = dir.join("config.toml");
    add_azure_provider_at(&path, cfg)?;
    Ok(path)
}

/// The file half of [`add_azure_provider`] against an explicit path — testable without the real
/// per-user config dir (mirrors [`add_custom_provider_at`]).
fn add_azure_provider_at(path: &std::path::Path, cfg: &AzureConfig) -> Result<(), ConfigError> {
    // Validate (resource/endpoint present + well-formed) before touching the file.
    cfg.clone().into_provider().map_err(ConfigError::Write)?;

    let mut root: toml::Table = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default();

    let providers = root
        .entry("providers".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !providers.is_table() {
        *providers = toml::Value::Table(toml::Table::new());
    }
    let azure = providers
        .as_table_mut()
        .unwrap()
        .entry("azure".to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    let mut entry = toml::Table::new();
    let mut put = |k: &str, v: Option<&String>| {
        if let Some(s) = v.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            entry.insert(k.into(), toml::Value::String(s.to_string()));
        }
    };
    put("resource", cfg.resource.as_ref());
    put("endpoint", cfg.endpoint.as_ref());
    put("api_version", cfg.api_version.as_ref());
    put("api_key_env", cfg.api_key_env.as_ref());
    put("label", cfg.label.as_ref());
    if cfg.free {
        entry.insert("free".into(), toml::Value::Boolean(true));
    }
    let deps: Vec<toml::Value> = cfg
        .deployments
        .iter()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .map(toml::Value::String)
        .collect();
    if !deps.is_empty() {
        entry.insert("deployments".into(), toml::Value::Array(deps));
    }
    *azure = toml::Value::Table(entry);

    let body = toml::to_string_pretty(&root).map_err(|e| ConfigError::Write(e.to_string()))?;
    Figment::from(Serialized::defaults(Config::default()))
        .merge(Toml::string(&body))
        .extract::<Config>()
        .map_err(|e| ConfigError::Write(format!("invalid config after azure add: {e}")))?;
    std::fs::write(path, body).map_err(|e| ConfigError::Write(e.to_string()))?;
    Ok(())
}

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

/// Whether a `provider::model` id is a NON-chat model — image/video/audio generation, embeddings,
/// reranking, OCR, moderation/safety, or extraction/detection — that can't serve `/chat/completions`
/// and so should be excluded from the routable mesh catalog (otherwise it pollutes routing, never
/// gets a chat-intelligence benchmark, and shows as a heuristic "—"). Conservative: matches only
/// unambiguously non-conversational families. Multimodal CHAT models (…-vision, …-vl, flash) are
/// kept — "vision"/"vl" are intentionally NOT in the deny list.
pub fn is_non_chat_model(id: &str) -> bool {
    let l = id.to_ascii_lowercase();
    const DENY: &[&str] = &[
        // Embedding / reranking.
        "embed",
        "embedqa",
        "rerank",
        "/bge",
        "reranker",
        // Image / video generation.
        "imagen",
        "veo-",
        "veo3",
        "lyria",
        "nano-banana",
        "diffusion",
        "stable-diffusion",
        "flux",
        "text-to-image",
        "image-generation",
        // Audio / speech / TTS.
        "whisper",
        "voxtral",
        "orpheus",
        "-tts",
        "tts-",
        "text-to-speech",
        "speech-to-text",
        // OCR / document parsing / extraction / detection.
        "-ocr",
        "ocr-",
        "mistral-ocr",
        "deplot",
        "-parse",
        "parse-",
        "gliner",
        "detector",
        // Moderation / safety classifiers.
        "moderation",
        "content-safety",
        "llama-guard",
        "shieldgemma",
        "guard-",
    ];
    DENY.iter().any(|p| l.contains(p))
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

/// The conventional environment variable for a provider's API key, if it needs one. Chains the
/// native-adapter table and the custom OpenAI-compatible registry.
fn env_var_for(provider: &str) -> Option<&'static str> {
    PROVIDER_ENV_VARS
        .iter()
        .find(|(name, _)| *name == provider)
        .map(|(_, var)| *var)
        // A custom provider with an empty `env_var` is KEYLESS (local server) — `None` here makes
        // `has_api_key` treat it as not-key-blocked and `api_key` return `Ok("")` without erroring.
        .or_else(|| {
            custom_provider(provider).and_then(|p| (!p.env_var.is_empty()).then_some(p.env_var))
        })
        // Azure carries a configurable key env var (default `AZURE_OPENAI_API_KEY`); the resolved
        // provider is build-once `'static`, so the borrowed name is `'static` too.
        .or_else(|| {
            (provider == AZURE_NS)
                .then(azure_registry)
                .flatten()
                .map(|a| a.env_var.as_str())
        })
}

/// Provider names Forge knows how to authenticate (for `forge auth` validation/help). Includes
/// native-adapter providers and the custom OpenAI-compatible registry (built-in + runtime).
pub fn known_key_providers() -> impl Iterator<Item = &'static str> {
    PROVIDER_ENV_VARS
        .iter()
        .map(|(name, _)| *name)
        .chain(custom_provider_registry().iter().map(|p| p.namespace))
        // Azure is a known keyed provider only when configured (so `forge auth azure` is valid then).
        .chain(azure_registry().map(|_| AZURE_NS))
}

/// Conventional / legacy env-var aliases accepted IN ADDITION to the canonical name in
/// [`PROVIDER_ENV_VARS`]. OpenRouter's own docs and most users use `OPENROUTER_API_KEY`, but the
/// canonical var (the one genai's `open_router` adapter reads) is `OPEN_ROUTER_API_KEY` — so a user
/// who exported the conventional name was silently treated as keyless, the mesh skipped OpenRouter
/// discovery, and routing fell back to the built-in groq defaults. Accept both: read either name,
/// and [`inject_provider_keys`] copies an alias into the canonical var genai authenticates with.
const PROVIDER_ENV_ALIASES: &[(&str, &str)] = &[("openrouter", "OPENROUTER_API_KEY")];

/// Accepted env-var aliases for `provider` (besides its canonical [`env_var_for`] name).
fn env_aliases_for(provider: &str) -> impl Iterator<Item = &'static str> + '_ {
    PROVIDER_ENV_ALIASES
        .iter()
        .filter(move |(p, _)| *p == provider)
        .map(|(_, v)| *v)
}

/// Whether an env var is set to a non-empty value.
fn env_set(var: &str) -> bool {
    std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false)
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
    if env_set(var) || env_aliases_for(provider).any(env_set) {
        return true;
    }
    secret_store::get(provider)
        .map(|k| !k.is_empty())
        .unwrap_or(false)
}

/// Resolve a single API key for a provider (the first usable one). Environment first, then the OS
/// keyring. Used wherever exactly one key is needed (model listing, balance probes); for rotation
/// across all configured keys see [`api_keys`].
pub fn api_key(provider: &str) -> Result<String, ConfigError> {
    if let Some(first) = api_keys(provider).into_iter().next() {
        return Ok(first);
    }
    // Keyless / unknown providers (e.g. local `ollama`) have no env var → not an error, just "".
    let Some(var) = env_var_for(provider) else {
        return Ok(String::new());
    };
    Err(ConfigError::MissingKey(provider.into(), var.into()))
}

/// All usable API keys configured for a provider, in priority order, de-duplicated. Sources: the
/// canonical env var and its numbered siblings (`VAR`, `VAR_2`, `VAR_3`, …), comma-separated values
/// within any of those, accepted env aliases (+ their numbered siblings), and the OS keyring entry
/// (a newline-separated list appended by repeated `forge auth`). Multiple keys let the provider
/// client round-robin across them to multiply a free tier's per-key rate limit and fail over within
/// one provider on a 429. Empty for keyless/unknown providers.
pub fn api_keys(provider: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        for part in raw.split([',', '\n']) {
            let k = part.trim();
            if !k.is_empty() && !out.iter().any(|e| e == k) {
                out.push(k.to_string());
            }
        }
    };
    if let Some(var) = env_var_for(provider) {
        let vars =
            std::iter::once(var.to_string()).chain(env_aliases_for(provider).map(str::to_string));
        for base in vars {
            if let Ok(v) = std::env::var(&base) {
                push(&v);
            }
            for n in 2..=MAX_NUMBERED_KEY_ENV {
                if let Ok(v) = std::env::var(format!("{base}_{n}")) {
                    push(&v);
                }
            }
        }
    }
    if let Some(stored) = secret_store::get(provider) {
        push(&stored);
    }
    out
}

/// Upper bound when scanning numbered env-var siblings (`VAR_2`..=`VAR_N`). Gaps are tolerated.
const MAX_NUMBERED_KEY_ENV: u8 = 16;

/// The Forge provider that authenticates with environment variable `var`, if any. Reverse of
/// [`env_var_for`]; the provider client uses it to map a genai `AuthData::FromEnv` back to a
/// provider so it can substitute a rotated key. Aliases canonicalize to the primary var.
pub fn provider_for_env_var(var: &str) -> Option<&'static str> {
    if var.is_empty() {
        return None; // keyless custom providers carry an empty env_var — never match on it
    }
    PROVIDER_ENV_VARS
        .iter()
        .find(|(_, v)| *v == var)
        .map(|(p, _)| *p)
        .or_else(|| {
            custom_provider_registry()
                .iter()
                .find(|p| p.env_var == var)
                .map(|p| p.namespace)
        })
        .or_else(|| {
            azure_registry()
                .filter(|a| a.env_var == var)
                .map(|_| AZURE_NS)
        })
}

/// Store a provider API key, REPLACING any existing key(s) with this single one
/// (`forge auth <p> --replace`). For additive multi-key setup use [`add_api_key`].
pub fn store_api_key(provider: &str, key: &str) -> Result<(), ConfigError> {
    secret_store::set(provider, key.trim())
}

/// Append a key to a provider's keyring list (idempotent — a duplicate is ignored). Returns the
/// number of keys in the keyring afterwards. Default `forge auth` behaviour, so repeated runs
/// accumulate keys to rotate across.
pub fn add_api_key(provider: &str, key: &str) -> Result<usize, ConfigError> {
    let key = key.trim();
    let mut keys: Vec<String> = secret_store::get(provider)
        .map(|s| {
            s.split('\n')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if !keys.iter().any(|k| k == key) {
        keys.push(key.to_string());
    }
    secret_store::set(provider, &keys.join("\n"))?;
    Ok(keys.len())
}

/// Masked fingerprints (`…last4`) of every configured key for a provider, for `forge auth --list`.
/// Never returns the keys themselves.
pub fn api_key_fingerprints(provider: &str) -> Vec<String> {
    api_keys(provider)
        .iter()
        .map(|k| {
            let tail: String = k
                .chars()
                .rev()
                .take(4)
                .collect::<Vec<char>>()
                .into_iter()
                .rev()
                .collect();
            format!("…{tail}")
        })
        .collect()
}

/// Delete a provider's stored API key(s). `Ok(true)` if an entry was removed, `Ok(false)` if there
/// was nothing stored (so `forge auth --remove` is idempotent). Removes the whole keyring list;
/// env-var keys are not touched (Forge doesn't own the environment).
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
/// each known provider with no env var set, inject the PRIMARY stored key. Only the first key is
/// injected — the keyring may hold a newline-separated list of several, and the env var must be a
/// single key; the provider client rotates across the full list separately (see `api_keys`).
/// Best-effort — providers without a stored key are simply left unset.
pub fn inject_provider_keys() {
    let native = PROVIDER_ENV_VARS.iter().copied();
    let custom = custom_provider_registry()
        .iter()
        .map(|p| (p.namespace, p.env_var));
    let azure = azure_registry()
        .into_iter()
        .map(|a| (AZURE_NS, a.env_var.as_str()));
    for (provider, var) in native.chain(custom).chain(azure) {
        // Keyless custom providers (local servers) carry an empty env_var — nothing to inject.
        if var.is_empty() || env_set(var) {
            continue;
        }
        // A conventional alias the user exported (e.g. OPENROUTER_API_KEY) → copy into the
        // canonical var genai's adapter actually reads (OPEN_ROUTER_API_KEY), so it authenticates.
        if let Some(key) =
            env_aliases_for(provider).find_map(|a| std::env::var(a).ok().filter(|s| !s.is_empty()))
        {
            std::env::set_var(var, key);
            continue;
        }
        if let Some(key) = secret_store::get(provider) {
            let primary = key.split('\n').next().unwrap_or(&key).trim();
            if !primary.is_empty() {
                std::env::set_var(var, primary);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keybinds_default_has_all_actions() {
        let kb = KeybindsConfig::default();
        assert_eq!(kb.binds.len(), 19);
        assert_eq!(
            kb.binds["interrupt"],
            KeyCombo {
                key: "c".into(),
                ctrl: true,
                alt: false,
                shift: false
            }
        );
        assert_eq!(
            kb.binds["copy_last"],
            KeyCombo {
                key: "c".into(),
                ctrl: true,
                alt: false,
                shift: true
            }
        );
        assert_eq!(
            kb.binds["tier_up"],
            KeyCombo {
                key: "up".into(),
                ctrl: true,
                alt: false,
                shift: false
            }
        );
        assert_eq!(
            kb.binds["help"],
            KeyCombo {
                key: "f1".into(),
                ctrl: false,
                alt: false,
                shift: false
            }
        );
    }

    /// A partial `[keybinds.binds]` override in the user TOML must DEEP-MERGE over the serialized
    /// defaults (keeping the other 18 binds), not replace the whole map. `write_keybind` writes a
    /// single bind, so a replace would unbind everything else — this guards that regression.
    #[test]
    fn partial_keybind_override_deep_merges_over_defaults() {
        use figment::providers::{Format, Toml};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(
            &path,
            "[keybinds.binds.toggle_reasoning]\nkey = \"x\"\nctrl = true\nalt = false\nshift = false\n",
        )
        .unwrap();
        let fig = Figment::from(Serialized::defaults(Config::default())).merge(Toml::file(&path));
        let cfg: Config = fig.extract().unwrap();
        // The override took effect…
        assert_eq!(cfg.keybinds.binds["toggle_reasoning"].key, "x");
        // …and every other default bind survived the merge.
        assert_eq!(cfg.keybinds.binds.len(), 19);
        assert_eq!(cfg.keybinds.binds["interrupt"].key, "c");
        assert_eq!(cfg.keybinds.binds["help"].key, "f1");
    }

    #[test]
    fn config_leaves_discovers_scalars_and_skips_complex_sections() {
        let value = serde_json::to_value(Config::default()).unwrap();
        let mut leaves = Vec::new();
        flatten_value("", &value, &mut leaves);
        let paths: Vec<&str> = leaves.iter().map(|l| l.path.as_str()).collect();
        // Known scalars are discovered automatically.
        assert!(paths.contains(&"tui.fullscreen"));
        assert!(paths.contains(&"local.autostart"));
        assert!(paths.contains(&"recap.enabled"));
        // Complex sections are excluded (their own commands own them).
        assert!(!paths.iter().any(|p| p.starts_with("hooks")));
        assert!(!paths.iter().any(|p| p.starts_with("mcp")));
        assert!(!paths.iter().any(|p| p.starts_with("permissions")));
    }

    #[test]
    fn architect_mode_stays_off_when_config_omits_it() {
        use figment::providers::{Format, Serialized, Toml};
        // Mirror load()'s merge order (built-in defaults <- user toml) with a minimal user config
        // that sets a few unrelated fields but NOT architect_mode. It must stay false: a user on a
        // "default config" must never silently get the architect dual-model pipeline (which led the
        // planner to the keyless groq default and auth-failed every turn). Guards against a stray
        // serde-default or a deserialization quirk flipping it on.
        let user_toml = r#"
            permission_mode = "accept-edits"
            [mesh]
            prefer_subscription = true
            failover = true
        "#;
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(user_toml))
            .extract()
            .expect("minimal config should load");
        assert!(
            !cfg.mesh.architect_mode,
            "architect_mode must stay false when the config omits it"
        );
    }

    #[test]
    fn priority_orders_important_settings_first() {
        // permission_mode outranks an arbitrary deep field; most-specific prefix wins.
        assert!(priority_rank("permission_mode") < priority_rank("lattice.embeddings.model"));
        assert!(priority_rank("mesh.credit_mode") < priority_rank("mesh.disabled"));
        assert_eq!(priority_rank("some.unlisted.field"), usize::MAX);
    }

    #[test]
    fn coerce_respects_existing_type() {
        assert_eq!(
            coerce_value("on", Some(&SettingValue::Bool(false))).unwrap(),
            Some(toml::Value::Boolean(true))
        );
        assert_eq!(
            coerce_value("42", Some(&SettingValue::Int(0))).unwrap(),
            Some(toml::Value::Integer(42))
        );
        // Bad bool/int are rejected, not silently stringified.
        assert!(coerce_value("maybe", Some(&SettingValue::Bool(false))).is_err());
        assert!(coerce_value("3.x", Some(&SettingValue::Int(0))).is_err());
        // Empty on an optional clears it.
        assert_eq!(coerce_value("", Some(&SettingValue::Unset)).unwrap(), None);
    }

    #[test]
    fn set_and_remove_dotted_paths() {
        let mut root = toml::Table::new();
        set_dotted(
            &mut root,
            "local.model",
            toml::Value::String("gemma4:12b".into()),
        );
        assert_eq!(root["local"]["model"].as_str(), Some("gemma4:12b"));
        remove_dotted(&mut root, "local.model");
        assert!(root["local"].as_table().unwrap().get("model").is_none());
    }

    #[test]
    fn hook_matcher_filters_by_tool_name() {
        let mk = |m: Option<&str>| HookConfig {
            event: HookEvent::PreToolUse,
            matcher: m.map(String::from),
            command: "true".into(),
            timeout_secs: 30,
            cc_compat: false,
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
    fn cc_hooks_parse_from_settings_json_and_match_by_alias() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{ "hooks": {
                   "PreToolUse": [
                     { "matcher": "Write|Edit",
                       "hooks": [ { "type": "command", "command": "./guard.sh", "timeout": 7 } ] }
                   ],
                   "Notification": [
                     { "hooks": [ { "type": "command", "command": "notify-send forge" } ] }
                   ]
                 } }"#,
        )
        .unwrap();
        let hooks = cc_hooks_from_settings(&v);
        assert_eq!(hooks.len(), 2);
        let pre = hooks
            .iter()
            .find(|h| h.event == HookEvent::PreToolUse)
            .unwrap();
        assert!(pre.cc_compat);
        assert_eq!(pre.timeout_secs, 7);
        // The CC matcher "Write|Edit" fires on Forge's `edit_file`/`write_file` via the alias.
        assert!(pre.matches("edit_file"), "Edit alias");
        assert!(pre.matches("write_file"), "Write alias");
        assert!(!pre.matches("shell"), "Bash not in matcher");
        let note = hooks
            .iter()
            .find(|h| h.event == HookEvent::Notification)
            .unwrap();
        assert!(note.matcher.is_none(), "no matcher = all");
        assert!(note.matches("anything"));
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
    fn openrouter_accepts_the_conventional_env_var_alias() {
        // OpenRouter's own docs + most users export OPENROUTER_API_KEY, but the canonical var genai's
        // adapter reads is OPEN_ROUTER_API_KEY. Both must be recognised — else the mesh treats the
        // user as keyless, skips OpenRouter discovery, and falls back to the built-in groq defaults.
        std::env::remove_var("OPEN_ROUTER_API_KEY");
        std::env::set_var("OPENROUTER_API_KEY", "sk-or-conventional");
        assert!(
            has_api_key("openrouter"),
            "conventional alias counts as a key"
        );
        assert_eq!(api_key("openrouter").unwrap(), "sk-or-conventional");
        // inject copies the alias into the canonical var genai authenticates with.
        inject_provider_keys();
        assert_eq!(
            std::env::var("OPEN_ROUTER_API_KEY").unwrap(),
            "sk-or-conventional"
        );
        std::env::remove_var("OPENROUTER_API_KEY");
        std::env::remove_var("OPEN_ROUTER_API_KEY");
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
        assert_eq!(env_var_for("cohere"), Some("COHERE_API_KEY"));
        // Custom OpenAI-compatible providers resolve their key env var via the registry chain.
        assert_eq!(env_var_for("nvidia"), Some("NVIDIA_API_KEY"));
        assert_eq!(env_var_for("sambanova"), Some("SAMBANOVA_API_KEY"));
        assert_eq!(env_var_for("mistral"), Some("MISTRAL_API_KEY"));
        // provider_of pulls the right prefix from a namespaced model id.
        assert_eq!(provider_of("groq::llama-3.3-70b-versatile"), "groq");
        assert_eq!(provider_of("opencode_go::deepseek-v4-flash"), "opencode_go");
        // Slash-bearing NIM ids still split on the FIRST `::` only.
        assert_eq!(
            provider_of("nvidia::meta/llama-3.1-405b-instruct"),
            "nvidia"
        );
    }

    #[test]
    fn non_chat_models_are_detected_but_chat_models_kept() {
        for id in [
            "gemini::imagen-4.0-generate-001",
            "gemini::veo-3.0-generate-001",
            "gemini::gemini-embedding-001",
            "mistral::mistral-ocr-latest",
            "mistral::voxtral-mini-2507",
            "mistral::mistral-moderation-2411",
            "groq::canopylabs/orpheus-v1-english",
            "nvidia::baai/bge-m3",
            "nvidia::nvidia/nemotron-parse",
            "openrouter::meta-llama/llama-guard-4-12b",
        ] {
            assert!(is_non_chat_model(id), "{id} should be non-chat");
        }
        // Real chat / coding / multimodal-CHAT models are kept.
        for id in [
            "nvidia::minimaxai/minimax-m3",
            "nvidia::deepseek-ai/deepseek-v4-pro",
            "nvidia::meta/llama-3.3-70b-instruct",
            "nvidia::google/gemma-4-31b-it",
            "gemini::gemini-3.5-flash",
            "nvidia::meta/llama-3.2-90b-vision-instruct",
            "openrouter::qwen/qwen3.5-397b-a17b",
        ] {
            assert!(!is_non_chat_model(id), "{id} is a chat model, must be kept");
        }
    }

    #[test]
    fn custom_providers_are_known_and_seed_models() {
        let providers: Vec<_> = known_key_providers().collect();
        for p in ["nvidia", "sambanova", "mistral", "cerebras"] {
            assert!(providers.contains(&p), "{p} should be a known key provider");
            let cp = custom_provider(p).unwrap_or_else(|| panic!("{p} registered"));
            assert!(cp.free, "{p} seeded as free");
            assert!(!cp.seed_models.is_empty(), "{p} has seed models");
            assert!(
                cp.endpoint.ends_with('/'),
                "{p} endpoint has trailing slash"
            );
        }
        // cohere is a native adapter, not a custom-endpoint provider.
        assert!(custom_provider("cohere").is_none());
    }

    #[test]
    fn provider_for_env_var_reverses_the_mapping() {
        assert_eq!(provider_for_env_var("GROQ_API_KEY"), Some("groq"));
        assert_eq!(provider_for_env_var("NVIDIA_API_KEY"), Some("nvidia"));
        assert_eq!(provider_for_env_var("MISTRAL_API_KEY"), Some("mistral"));
        assert_eq!(
            provider_for_env_var("OPEN_ROUTER_API_KEY"),
            Some("openrouter")
        );
        assert_eq!(provider_for_env_var("NOT_A_KEY"), None);
    }

    #[test]
    fn api_keys_reads_numbered_and_comma_separated_env_vars() {
        // Unique provider for this test to avoid env races with other env-touching tests.
        std::env::set_var("XAI_API_KEY", " k1 , k2 ");
        std::env::set_var("XAI_API_KEY_2", "k3");
        let keys = api_keys("xai");
        std::env::remove_var("XAI_API_KEY");
        std::env::remove_var("XAI_API_KEY_2");
        for k in ["k1", "k2", "k3"] {
            assert!(
                keys.contains(&k.to_string()),
                "{k} should be parsed; got {keys:?}"
            );
        }
    }

    #[test]
    fn fingerprints_mask_to_last_four_chars() {
        std::env::set_var("DEEPSEEK_API_KEY", "supersecretKEY1,anotherKEY2");
        let fps = api_key_fingerprints("deepseek");
        std::env::remove_var("DEEPSEEK_API_KEY");
        assert!(
            fps.iter().all(|f| f.starts_with('…')),
            "all masked: {fps:?}"
        );
        assert!(fps.contains(&"…KEY1".to_string()));
        assert!(fps.contains(&"…KEY2".to_string()));
    }

    #[test]
    fn self_review_is_off_by_default() {
        // OFF by default (a same-model A/B showed the always-on version regressed); opt-in only.
        assert!(!Config::default().mesh.self_review, "Rust default");
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(figment::providers::Toml::string(
                "[mesh]\nfailover = true\n",
            ))
            .extract()
            .unwrap();
        assert!(
            !cfg.mesh.self_review,
            "absent in TOML → serde default false"
        );
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
    fn builtin_denies_block_secret_writes_and_deletes() {
        let rules = Config::default().permission_rules();
        for tool in ["write_file", "edit_file", "delete_file"] {
            assert!(
                rules
                    .iter()
                    .any(|r| r.tool == tool && r.patterns.iter().any(|p| p == "**/.env")),
                "{tool} must deny .env writes/deletes by default"
            );
            assert!(
                rules
                    .iter()
                    .any(|r| r.tool == tool && r.patterns.iter().any(|p| p == "**/.env.*")),
                "{tool} must deny .env.* writes/deletes by default"
            );
            assert!(
                rules
                    .iter()
                    .any(|r| r.tool == tool && r.patterns.iter().any(|p| p == "**/id_rsa")),
                "{tool} must deny SSH key writes/deletes by default"
            );
        }
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

    #[test]
    fn statusline_config_default_layout() {
        let cfg = StatuslineConfig::default();
        assert_eq!(
            cfg.left,
            vec![
                StatuslineWidget::Model,
                StatuslineWidget::SessionCost,
                StatuslineWidget::Effort,
                StatuslineWidget::Mode,
            ]
        );
        assert!(cfg.center.is_empty());
        assert!(cfg.right.is_empty());
        assert_eq!(cfg.separator, "  │  ");
    }

    #[test]
    fn statusline_config_roundtrips_toml() {
        let cfg = StatuslineConfig {
            left: vec![StatuslineWidget::Model, StatuslineWidget::GitBranch],
            center: vec![],
            right: vec![StatuslineWidget::McpStatus],
            separator: " | ".to_string(),
        };
        let serialized = toml::to_string(&cfg).unwrap();
        let parsed: StatuslineConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed.left, cfg.left);
        assert_eq!(parsed.right, cfg.right);
        assert_eq!(parsed.separator, cfg.separator);
    }

    #[test]
    fn config_default_includes_statusline() {
        let cfg = Config::default();
        assert_eq!(
            cfg.statusline.left,
            vec![
                StatuslineWidget::Model,
                StatuslineWidget::SessionCost,
                StatuslineWidget::Effort,
                StatuslineWidget::Mode,
            ]
        );
    }

    // --- Runtime custom OpenAI-compatible providers ---

    #[test]
    fn custom_provider_config_parses_normalizes_and_keyless() {
        // A keyless local server (no api_key_env): base_url without trailing slash gets one;
        // env_var becomes "" (keyless).
        let toml_src = r#"
            [[providers.custom]]
            namespace = "lmstudio"
            base_url  = "http://localhost:1234/v1"
            free = true
            models = ["qwen2.5-coder-32b", "  ", "llama-3.3-70b"]
        "#;
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml_src))
            .extract()
            .unwrap();
        assert_eq!(cfg.providers.custom.len(), 1);
        let rp = cfg.providers.custom[0].clone().into_runtime().unwrap();
        assert_eq!(rp.namespace, "lmstudio");
        assert_eq!(
            rp.endpoint, "http://localhost:1234/v1/",
            "trailing slash added"
        );
        assert_eq!(rp.env_var, "", "no api_key_env → keyless");
        assert!(rp.free);
        assert_eq!(rp.seed_models, vec!["qwen2.5-coder-32b", "llama-3.3-70b"]);
    }

    #[test]
    fn custom_provider_config_rejects_bad_namespace_and_url() {
        let bad_ns = CustomProviderConfig {
            namespace: "has space".into(),
            base_url: "http://x/v1".into(),
            api_key_env: None,
            free: false,
            models: vec![],
            label: None,
        };
        assert!(bad_ns.into_runtime().is_err());
        let bad_url = CustomProviderConfig {
            namespace: "x".into(),
            base_url: "localhost:1234".into(),
            api_key_env: None,
            free: false,
            models: vec![],
            label: None,
        };
        assert!(bad_url.into_runtime().is_err());
    }

    #[test]
    fn build_custom_registry_merges_builtins_with_runtime_and_drops_collisions() {
        let runtime = vec![
            RuntimeCustomProvider {
                namespace: "lmstudio".into(),
                endpoint: "http://localhost:1234/v1/".into(),
                env_var: "".into(),
                free: true,
                label: "".into(),
                seed_models: vec!["local-model".into()],
            },
            // Collides with a built-in custom provider — must be dropped, built-in wins.
            RuntimeCustomProvider {
                namespace: "cerebras".into(),
                endpoint: "http://evil/".into(),
                env_var: "X".into(),
                free: false,
                label: "".into(),
                seed_models: vec![],
            },
            // Collides with a NATIVE provider — also dropped.
            RuntimeCustomProvider {
                namespace: "openai".into(),
                endpoint: "http://evil/".into(),
                env_var: "X".into(),
                free: false,
                label: "".into(),
                seed_models: vec![],
            },
        ];
        let reg = build_custom_registry(&runtime);
        let find = |ns: &str| reg.iter().find(|p| p.namespace == ns);
        // Built-ins present (discovery iterates exactly this list).
        assert!(find("cerebras").is_some());
        assert!(find("together").is_some());
        // The runtime keyless local server merged in with a synthesized label + normalized endpoint.
        let lm = find("lmstudio").expect("runtime provider merged into registry");
        assert_eq!(lm.endpoint, "http://localhost:1234/v1/");
        assert_eq!(lm.env_var, "");
        assert!(lm.free);
        assert!(lm.label.contains("lmstudio"));
        assert_eq!(lm.seed_models, &["local-model"]);
        // The cerebras collision did NOT overwrite the built-in endpoint.
        assert_eq!(
            find("cerebras").unwrap().endpoint,
            "https://api.cerebras.ai/v1/"
        );
        // The native-provider collision was rejected (not added as a custom row).
        assert!(find("openai").is_none());
    }

    #[test]
    fn together_fireworks_perplexity_rows_resolve() {
        for (ns, ep) in [
            ("together", "https://api.together.xyz/v1/"),
            ("fireworks", "https://api.fireworks.ai/inference/v1/"),
            ("perplexity", "https://api.perplexity.ai/"),
        ] {
            let cp = custom_provider(ns).unwrap_or_else(|| panic!("{ns} missing"));
            assert_eq!(cp.endpoint, ep);
            assert!(!cp.seed_models.is_empty());
        }
        // These are paid gateways, not standing free tiers.
        assert!(!custom_provider("together").unwrap().free);
    }

    #[test]
    fn bedrock_and_vertex_are_native_keyed_providers() {
        let known: Vec<_> = known_key_providers().collect();
        assert!(known.contains(&"bedrock"));
        assert!(known.contains(&"vertex"));
        assert_eq!(env_var_for("bedrock"), Some("BEDROCK_API_KEY"));
        assert_eq!(env_var_for("vertex"), Some("VERTEX_API_KEY"));
        assert_eq!(provider_for_env_var("BEDROCK_API_KEY"), Some("bedrock"));
        assert_eq!(provider_for_env_var("VERTEX_API_KEY"), Some("vertex"));
    }

    #[test]
    fn keyless_custom_provider_is_not_key_blocked() {
        // env_var_for is None for an empty env var → has_api_key treats it as not-blocked, and the
        // reverse map never matches the empty string.
        let reg = build_custom_registry(&[RuntimeCustomProvider {
            namespace: "lmstudio".into(),
            endpoint: "http://localhost:1234/v1/".into(),
            env_var: "".into(),
            free: true,
            label: "".into(),
            seed_models: vec![],
        }]);
        let lm = reg.iter().find(|p| p.namespace == "lmstudio").unwrap();
        assert!(lm.env_var.is_empty());
        assert_eq!(provider_for_env_var(""), None);
    }

    #[test]
    fn add_and_remove_custom_provider_round_trips_through_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let p = CustomProviderConfig {
            namespace: "lmstudio".into(),
            base_url: "http://localhost:1234/v1".into(),
            api_key_env: Some("LMSTUDIO_API_KEY".into()),
            free: true,
            models: vec!["qwen2.5-coder-32b".into()],
            label: None,
        };
        add_custom_provider_at(&path, &p).unwrap();
        // The written file parses back into a Config with the entry intact.
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(&path))
            .extract()
            .unwrap();
        assert_eq!(cfg.providers.custom.len(), 1);
        assert_eq!(cfg.providers.custom[0].namespace, "lmstudio");
        assert_eq!(cfg.providers.custom[0].base_url, "http://localhost:1234/v1");

        // Adding the same namespace again replaces (no duplicate).
        add_custom_provider_at(&path, &p).unwrap();
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(&path))
            .extract()
            .unwrap();
        assert_eq!(
            cfg.providers.custom.len(),
            1,
            "same namespace replaced, not duplicated"
        );

        // A built-in namespace is rejected.
        let collide = CustomProviderConfig {
            namespace: "cerebras".into(),
            ..p.clone()
        };
        assert!(add_custom_provider_at(&path, &collide).is_err());

        // Remove is idempotent.
        assert!(remove_custom_provider_at(&path, "lmstudio").unwrap());
        assert!(!remove_custom_provider_at(&path, "lmstudio").unwrap());
    }

    #[test]
    fn azure_is_no_longer_in_the_unwired_list() {
        // Azure is wired now (genai per-request URL+header override), so it must NOT appear as a
        // scaffolded-but-unwired gateway.
        assert!(
            !UNWIRED_ENTERPRISE_PROVIDERS
                .iter()
                .any(|(ns, _)| *ns == "azure"),
            "azure should be wired, not listed as unwired"
        );
    }

    #[test]
    fn azure_config_resolves_resource_and_builds_deployment_url() {
        let cfg = AzureConfig {
            resource: Some("my-resource".into()),
            deployments: vec!["gpt-4o".into(), "  ".into(), "gpt-4o-mini".into()],
            ..Default::default()
        };
        let p = cfg.into_provider().unwrap();
        assert_eq!(p.endpoint, "https://my-resource.openai.azure.com");
        // Defaults applied when omitted.
        assert_eq!(p.api_version, DEFAULT_AZURE_API_VERSION);
        assert_eq!(p.env_var, AZURE_DEFAULT_KEY_ENV);
        // Blank deployments filtered out.
        assert_eq!(p.deployments, vec!["gpt-4o", "gpt-4o-mini"]);
        // The deployment-scoped URL with the api-version query is the Azure REST shape.
        assert_eq!(
            p.chat_completions_url("gpt-4o"),
            format!(
                "https://my-resource.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version={DEFAULT_AZURE_API_VERSION}"
            )
        );
    }

    #[test]
    fn azure_explicit_endpoint_overrides_resource_and_strips_trailing_slash() {
        let cfg = AzureConfig {
            resource: Some("ignored".into()),
            endpoint: Some("https://sovereign.openai.azure.us/".into()),
            api_version: Some("2025-01-01".into()),
            api_key_env: Some("MY_AZURE_KEY".into()),
            ..Default::default()
        };
        let p = cfg.into_provider().unwrap();
        assert_eq!(p.endpoint, "https://sovereign.openai.azure.us");
        assert_eq!(p.api_version, "2025-01-01");
        assert_eq!(p.env_var, "MY_AZURE_KEY");
        assert!(p.chat_completions_url("d").starts_with(
            "https://sovereign.openai.azure.us/openai/deployments/d/chat/completions?"
        ));
    }

    #[test]
    fn azure_config_requires_resource_or_endpoint() {
        let err = AzureConfig::default().into_provider().unwrap_err();
        assert!(err.contains("resource") || err.contains("endpoint"));
    }

    #[test]
    fn azure_config_block_parses_from_toml() {
        let toml = r#"
[providers.azure]
resource = "acme"
api_version = "2024-10-21"
deployments = ["gpt-4o", "gpt-4o-mini"]
"#;
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(toml))
            .extract()
            .unwrap();
        let az = cfg.providers.azure.expect("azure block parsed");
        assert_eq!(az.resource.as_deref(), Some("acme"));
        assert_eq!(az.deployments.len(), 2);
        // And it resolves to a usable provider.
        let p = az.into_provider().unwrap();
        assert_eq!(p.endpoint, "https://acme.openai.azure.com");
    }

    #[test]
    fn add_azure_provider_round_trips_through_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AzureConfig {
            resource: Some("acme".into()),
            api_version: Some("2024-10-21".into()),
            deployments: vec!["gpt-4o".into()],
            ..Default::default()
        };
        add_azure_provider_at(&path, &cfg).unwrap();
        let parsed: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(&path))
            .extract()
            .unwrap();
        let az = parsed.providers.azure.expect("azure persisted");
        assert_eq!(az.resource.as_deref(), Some("acme"));
        assert_eq!(az.deployments, vec!["gpt-4o"]);

        // Re-adding replaces the block (no duplicate / merge cruft).
        let cfg2 = AzureConfig {
            endpoint: Some("https://acme.openai.azure.com".into()),
            deployments: vec!["o3-mini".into()],
            ..Default::default()
        };
        add_azure_provider_at(&path, &cfg2).unwrap();
        let parsed: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file(&path))
            .extract()
            .unwrap();
        let az = parsed.providers.azure.unwrap();
        assert_eq!(az.deployments, vec!["o3-mini"]);

        // An invalid block (no resource/endpoint) is rejected before writing.
        assert!(add_azure_provider_at(&path, &AzureConfig::default()).is_err());
    }
}
