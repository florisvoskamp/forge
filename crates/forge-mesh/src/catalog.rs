//! A live catalog of usable models, discovered from the providers the user has keys for
//! (auto-discovery mesh, docs/features/auto-discovery-mesh.md). This is a plain data holder +
//! ranking; the async *discovery* (querying each provider's model list) lives in the binary
//! (forge-cli), which has the provider client — forge-mesh stays free of that dependency.

use forge_types::TaskTier;

use crate::capability::tier_score;
use crate::pricing::Pricing;

/// Discovered `provider::model` ids the user can actually use right now.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    models: Vec<String>,
}

impl ModelCatalog {
    pub fn new(models: Vec<String>) -> Self {
        Self { models }
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// The discovered models ranked best-first for `tier`, using the capability priors + the
    /// estimated cost from `pricing` (free = $0). Returns at most `top` candidates so the
    /// router's cost-aware pass has a small, strong shortlist.
    pub fn ranked_for(&self, tier: TaskTier, pricing: &Pricing, top: usize) -> Vec<String> {
        let mut scored: Vec<(f64, &String)> = self
            .models
            .iter()
            .map(|m| (tier_score(m, tier, pricing.estimated_cost(m)), m))
            .collect();
        // Best score first; ties broken by id for determinism.
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored
            .into_iter()
            .take(top)
            .map(|(_, m)| m.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> ModelCatalog {
        ModelCatalog::new(vec![
            "groq::llama-3.1-8b-instant".into(),
            "groq::llama-3.3-70b-versatile".into(),
            "anthropic::claude-opus-4-8".into(),
            "ollama::llama3.2".into(),
        ])
    }

    #[test]
    fn ranks_a_small_fast_model_first_for_trivial() {
        let r = catalog().ranked_for(TaskTier::Trivial, &Pricing::default(), 2);
        assert_eq!(r.first().unwrap(), "groq::llama-3.1-8b-instant");
    }

    #[test]
    fn ranks_a_frontier_model_first_for_complex() {
        let r = catalog().ranked_for(TaskTier::Complex, &Pricing::default(), 3);
        // opus (paid, q3) vs groq-70b (free, q3): free bonus tips it to the free 70b.
        assert!(
            r.first().unwrap().contains("70b") || r.first().unwrap().contains("opus"),
            "a frontier-class model leads: {r:?}"
        );
        assert!(
            !r.first().unwrap().contains("8b"),
            "not the tiny model: {r:?}"
        );
    }

    #[test]
    fn empty_catalog_ranks_to_nothing() {
        assert!(ModelCatalog::default()
            .ranked_for(TaskTier::Standard, &Pricing::default(), 3)
            .is_empty());
    }
}
