//! Layered configuration (defaults -> user file -> project file -> `FORGE_*` env) and
//! secret resolution. Secrets are never part of the config surface (ADR-0007): API keys
//! come from environment variables (keyring storage is a planned enhancement).

use std::collections::HashMap;
use std::path::PathBuf;

use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use forge_types::{PermissionMode, TaskTier};
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshConfig {
    /// Tier -> model id. The heuristic router maps a classified task to one of these.
    pub models: HashMap<String, String>,
    /// Optional daily budget cap in USD; the router downshifts/blocks as it is approached.
    pub daily_budget_usd: Option<f64>,
    /// Per-model pricing overrides (USD per 1k tokens), applied on top of bundled
    /// defaults so a price change needs no release (A-7). Keyed by model id.
    #[serde(default)]
    pub pricing: HashMap<String, PriceOverride>,
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
        models.insert(TaskTier::Trivial.as_str().into(), "ollama::llama3.2".into());
        models.insert(
            TaskTier::Standard.as_str().into(),
            "openai::gpt-4o-mini".into(),
        );
        models.insert(
            TaskTier::Complex.as_str().into(),
            "anthropic::claude-opus-4-8".into(),
        );
        Self {
            permission_mode: PermissionMode::default(),
            mesh: MeshConfig {
                models,
                daily_budget_usd: None,
                pricing: HashMap::new(),
            },
        }
    }
}

impl Config {
    /// Resolve the model id configured for a tier, falling back to the standard tier.
    pub fn model_for(&self, tier: TaskTier) -> Option<&str> {
        self.mesh
            .models
            .get(tier.as_str())
            .or_else(|| self.mesh.models.get(TaskTier::Standard.as_str()))
            .map(String::as_str)
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

/// The conventional environment variable for a provider's API key, if it needs one.
fn env_var_for(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "openai" => Some("OPENAI_API_KEY"),
        _ => None, // local providers (e.g. ollama) need no key
    }
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
    for provider in ["anthropic", "openai"] {
        let Some(var) = env_var_for(provider) else {
            continue;
        };
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
    fn unknown_tier_falls_back_to_standard() {
        let mut c = Config::default();
        c.mesh.models.remove(TaskTier::Trivial.as_str());
        assert_eq!(
            c.model_for(TaskTier::Trivial),
            c.model_for(TaskTier::Standard)
        );
    }
}
