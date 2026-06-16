//! Backend selection for Lattice embeddings (code-intelligence.md §5.6). The [`Embedder`] trait
//! and the local `OllamaEmbedder` live in `forge-index`; this module adds a genai-backed embedder
//! (so any provider genai can embed with — OpenAI, Gemini — works, keyed the same way as chat) and
//! [`select_embedder`], which turns an [`EmbeddingsConfig`] into the right backend, including the
//! `"auto"` policy that picks the cheapest available so embeddings need zero manual setup.

use async_trait::async_trait;
use forge_config::EmbeddingsConfig;
use forge_index::{Embedder, LatticeError, OllamaEmbedder};
use genai::Client;

use crate::genai_provider::{build_client, to_genai_model};

/// genai-backed embedder. `model` is a Forge `provider::model` id (e.g. `gemini::text-embedding-004`);
/// genai resolves the adapter + endpoint + API-key env var from the namespace, exactly like chat.
pub struct GenaiEmbedder {
    client: Client,
    model: String,
}

impl GenaiEmbedder {
    pub fn new(model: &str) -> Self {
        Self {
            client: build_client(),
            model: model.to_string(),
        }
    }
}

#[async_trait]
impl Embedder for GenaiEmbedder {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LatticeError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = to_genai_model(&self.model);
        let resp = self
            .client
            .embed_batch(model.as_str(), texts.to_vec(), None)
            .await
            .map_err(|e| LatticeError::Io(format!("embed via {}: {e}", self.model)))?;
        let vecs = resp.into_vectors();
        if vecs.len() != texts.len() {
            return Err(LatticeError::Io(format!(
                "embed via {}: got {} vectors for {} inputs",
                self.model,
                vecs.len(),
                texts.len()
            )));
        }
        Ok(vecs)
    }
}

/// Cloud embedding candidates for `backend = "auto"`, cheapest/freest first. Both providers are in
/// `PROVIDER_ENV_VARS` (so `has_api_key` is meaningful) and have genai embedding adapters. Gemini's
/// `text-embedding-004` is free-tier; OpenAI's `text-embedding-3-small` is ~$0.02/M tokens.
const AUTO_CANDIDATES: &[(&str, &str)] = &[
    ("gemini", "gemini::text-embedding-004"),
    ("openai", "openai::text-embedding-3-small"),
];

/// Resolve an embedder from config, or `None` when embeddings are off or no backend is available.
/// Returns the backend plus a human label (the chosen `provider::model`, for status output).
///
/// - `auto` (default): first cloud candidate whose key is set ([`AUTO_CANDIDATES`], free-first),
///   else local ollama. With no cloud key + no ollama running it still returns the ollama backend,
///   which degrades to a no-op at call time — so default-on embeddings never error and never spend
///   when nothing is configured.
/// - `ollama`: the local HTTP backend at `endpoint`/`model`.
/// - any other value: treated as a genai provider namespace; `model` is namespaced if it isn't
///   already. Returns `None` if that provider has no API key (nothing to authenticate with).
pub fn select_embedder(cfg: &EmbeddingsConfig) -> Option<(Box<dyn Embedder>, String)> {
    if !cfg.enabled {
        return None;
    }
    match cfg.backend.as_str() {
        "auto" => Some(auto_embedder(cfg)),
        "ollama" => Some((
            Box::new(OllamaEmbedder::new(&cfg.endpoint, &cfg.model)),
            format!("ollama ({})", cfg.model),
        )),
        provider => {
            if !forge_config::has_api_key(provider) {
                return None;
            }
            let model = if cfg.model.contains("::") {
                cfg.model.clone()
            } else {
                format!("{provider}::{}", cfg.model)
            };
            let label = model.clone();
            Some((Box::new(GenaiEmbedder::new(&model)), label))
        }
    }
}

fn auto_embedder(cfg: &EmbeddingsConfig) -> (Box<dyn Embedder>, String) {
    match auto_candidate(forge_config::has_api_key) {
        Some(model) => (Box::new(GenaiEmbedder::new(model)), model.to_string()),
        None => (
            Box::new(OllamaEmbedder::new(&cfg.endpoint, &cfg.model)),
            format!("ollama ({})", cfg.model),
        ),
    }
}

/// The first `auto` cloud candidate whose key `has_key` reports present, or `None` (→ ollama).
/// Pure (the keyring/env check is injected) so the free-first ordering is deterministically tested.
fn auto_candidate(has_key: impl Fn(&str) -> bool) -> Option<&'static str> {
    AUTO_CANDIDATES
        .iter()
        .find(|(provider, _)| has_key(provider))
        .map(|(_, model)| *model)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, backend: &str) -> EmbeddingsConfig {
        EmbeddingsConfig {
            enabled,
            backend: backend.to_string(),
            model: "nomic-embed-text".to_string(),
            endpoint: "http://localhost:11434".to_string(),
        }
    }

    #[test]
    fn disabled_yields_none() {
        assert!(select_embedder(&cfg(false, "auto")).is_none());
    }

    #[test]
    fn ollama_backend_labels_model() {
        let (_, label) = select_embedder(&cfg(true, "ollama")).unwrap();
        assert_eq!(label, "ollama (nomic-embed-text)");
    }

    #[test]
    fn auto_prefers_free_gemini_over_paid_openai() {
        // Both keys present → gemini wins (free-tier, listed first).
        assert_eq!(auto_candidate(|_| true), Some("gemini::text-embedding-004"));
    }

    #[test]
    fn auto_picks_openai_when_only_openai_keyed() {
        assert_eq!(
            auto_candidate(|p| p == "openai"),
            Some("openai::text-embedding-3-small")
        );
    }

    #[test]
    fn auto_no_cloud_key_means_ollama_fallback() {
        assert_eq!(auto_candidate(|_| false), None);
    }
}
