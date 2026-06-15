//! Layered configuration (defaults -> user file -> project file -> `FORGE_*` env) and
//! secret resolution. Secrets are never part of the config surface (ADR-0007): API keys
//! come from environment variables first, then the OS keyring (`forge auth`).

use std::collections::HashMap;
use std::path::PathBuf;

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use forge_types::{PermissionDecision, PermissionMode, PermissionRule, RuleSource, TaskTier};
use serde::{Deserialize, Serialize};

pub mod agents;
pub use agents::{load_agents, AgentDef};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to load configuration: {0}")]
    Load(Box<figment::Error>),
    #[error("no API key found for provider '{0}' (set {1} or run `forge auth {0}`)")]
    MissingKey(String, String),
    #[error("keyring error: {0}")]
    Keyring(String),
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
                // catastrophic filesystem / disk
                "rm -rf /",
                "rm -rf ~",
                "rm -rf /*",
                ":(){ :|:& };:",
                "dd of=/dev/*",
                "mkfs*",
                "mkfs.*",
                // remote-to-shell pipe (matched against the raw command line)
                "*| sh",
                "*|sh",
                "*| bash",
                "*|bash",
                "*| zsh",
                "*|zsh",
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
    /// Directory holding named agent-type files (`<name>.md`), relative to the cwd.
    #[serde(default = "default_agents_dir")]
    pub agents_dir: String,
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
fn default_agents_dir() -> String {
    ".forge/agents".to_string()
}

impl Default for SubagentsConfig {
    fn default() -> Self {
        Self {
            enabled: default_subagents_enabled(),
            max_agents: default_max_agents(),
            max_concurrency: default_max_concurrency(),
            agents_dir: default_agents_dir(),
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
    /// Opt-in: ask a cheap model to label the tier (one extra call per turn), falling back
    /// to the heuristic on any error. Off by default (A-2).
    Llm,
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
        let one = |s: &str| OneOrMany::One(s.to_string());
        models.insert(TaskTier::Trivial.as_str().into(), one("ollama::llama3.2"));
        models.insert(
            TaskTier::Standard.as_str().into(),
            one("openai::gpt-4o-mini"),
        );
        models.insert(
            TaskTier::Complex.as_str().into(),
            one("anthropic::claude-opus-4-8"),
        );
        Self {
            permission_mode: PermissionMode::default(),
            mesh: MeshConfig {
                models,
                prefer_subscription: default_prefer_subscription(),
                classifier: ClassifierKind::default(),
                classifier_model: None,
                bridge_mode: BridgeMode::default(),
                daily_budget_usd: None,
                monthly_cap_usd: None,
                warn_threshold: default_warn_threshold(),
                budget: BudgetBehavior::default(),
                pricing: HashMap::new(),
                subagents: SubagentsConfig::default(),
            },
            permissions: PermissionsConfig::default(),
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

/// Load configuration with full layered precedence (lowest -> highest):
/// built-in defaults -> user config -> project `./.forge/config.toml` -> `FORGE_*` env.
pub fn load() -> Result<Config, ConfigError> {
    let mut fig = Figment::from(Serialized::defaults(Config::default()));

    if let Some(dir) = config_dir() {
        fig = fig.merge(Toml::file(dir.join("config.toml")));
    }
    fig = fig.merge(Toml::file("./.forge/config.toml"));
    fig = fig.merge(Env::prefixed("FORGE_").split("__"));

    Ok(fig.extract()?)
}

const KEYRING_SERVICE: &str = "forge";

/// Providers that authenticate with an API key, paired with the environment variable the
/// genai client reads for that provider. The env var names must match genai's
/// `API_KEY_DEFAULT_ENV_NAME` per adapter exactly (note OpenRouter's underscore). Local
/// providers (e.g. ollama) need no key and are intentionally absent.
const PROVIDER_ENV_VARS: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("openrouter", "OPEN_ROUTER_API_KEY"),
];

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
    keyring::Entry::new(KEYRING_SERVICE, provider)
        .and_then(|e| e.get_password())
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
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, provider) {
        if let Ok(key) = entry.get_password() {
            return Ok(key);
        }
    }
    Err(ConfigError::MissingKey(provider.into(), var.into()))
}

/// Securely store a provider API key in the OS keyring.
pub fn store_api_key(provider: &str, key: &str) -> Result<(), ConfigError> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, provider)
        .map_err(|e| ConfigError::Keyring(e.to_string()))?;
    entry
        .set_password(key)
        .map_err(|e| ConfigError::Keyring(e.to_string()))
}

/// Make keyring-stored keys visible to the provider client (genai reads keys from the
/// environment): for each known provider with no env var set, inject the keyring value.
/// Best-effort — providers without a stored key are simply left unset.
pub fn inject_provider_keys() {
    for (provider, var) in PROVIDER_ENV_VARS {
        if std::env::var(var).is_ok() {
            continue;
        }
        if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, provider) {
            if let Ok(key) = entry.get_password() {
                std::env::set_var(var, key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
