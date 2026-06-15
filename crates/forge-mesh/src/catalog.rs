//! A live catalog of usable models, discovered from the providers the user has keys for
//! (auto-discovery mesh, docs/features/auto-discovery-mesh.md). This is a plain data holder +
//! ranking; the async *discovery* (querying each provider's model list) lives in the binary
//! (forge-cli), which has the provider client — forge-mesh stays free of that dependency.

use forge_types::TaskTier;

use crate::capability::{is_frontier, tier_score};
use crate::pricing::Pricing;

/// Discovered `provider::model` ids the user can actually use right now.
#[derive(Debug, Clone, Default)]
pub struct ModelCatalog {
    models: Vec<String>,
}

/// The provider prefix of a `provider::model` id (`"groq"` from `"groq::llama-3.1-8b"`).
pub fn provider_of(id: &str) -> &str {
    id.split("::").next().unwrap_or(id)
}

/// A $0-marginal subscription bridge (the locally-installed claude/codex CLI), as opposed to a
/// metered or genuinely-free API. Kept separate from "free" in the overview counts.
pub fn is_subscription(id: &str) -> bool {
    id.starts_with("claude-cli::") || id.starts_with("codex-cli::")
}

/// Whether a model is genuinely free to call. "Free" needs *positive* evidence, not just a missing
/// price: OpenRouter is a paid gateway exposing hundreds of metered models (incl. frontier ones
/// like Claude Opus) that we hold no per-model price for — reading "unpriced" as "free" there is
/// the bug. So for OpenRouter, only its `:free`-suffixed variants count; everything else is paid.
/// Other unpriced providers (local `ollama::`, free-tier `groq`/`cerebras`) are genuinely free.
fn is_free(id: &str, cost: f64, subscription: bool) -> bool {
    if subscription || cost > f64::EPSILON {
        return false;
    }
    if provider_of(id) == "openrouter" {
        return id.contains(":free");
    }
    true
}

/// A discovered model classified for display (the `/models` browser + `forge models`). Pure view
/// data derived from the id + pricing — no health/network state (the caller overlays "benched").
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// Full `provider::model` id.
    pub id: String,
    /// Provider prefix (`anthropic`, `groq`, `claude-cli`, …).
    pub provider: String,
    /// The model name after `::` (empty for a bare bridge id, meaning its default model).
    pub name: String,
    /// Frontier-class by the capability prior (`opus`/`gpt-5`/`-70b`/…).
    pub frontier: bool,
    /// Genuinely free (local/ollama, free-tier APIs, or an OpenRouter `:free` variant) — see
    /// [`is_free`]. NOT merely "unpriced": a paid OpenRouter model is `paid`, not `free`.
    pub free: bool,
    /// Metered: either a known price > 0, or a gateway model with no free evidence (e.g. a paid
    /// OpenRouter model we hold no price for). Mutually exclusive with `free` and `subscription`.
    pub paid: bool,
    /// A $0-marginal subscription CLI bridge (claude-cli/codex-cli).
    pub subscription: bool,
    /// Estimated USD for a nominal turn (0 = subscription/unpriced; a paid model may still be 0
    /// here when we have no per-model rate for it, e.g. an OpenRouter gateway model).
    pub cost: f64,
}

impl ModelInfo {
    fn classify(id: &str, pricing: &Pricing) -> Self {
        let subscription = is_subscription(id);
        let cost = pricing.estimated_cost(id);
        let free = is_free(id, cost, subscription);
        Self {
            id: id.to_string(),
            provider: provider_of(id).to_string(),
            name: id
                .split_once("::")
                .map(|(_, n)| n)
                .unwrap_or("")
                .to_string(),
            frontier: is_frontier(id),
            free,
            paid: !subscription && !free,
            subscription,
            cost,
        }
    }
}

/// Aggregate counts across the whole catalog, for the overview header.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogStats {
    pub total: usize,
    pub providers: usize,
    pub frontier: usize,
    pub free: usize,
    pub subscription: usize,
    pub paid: usize,
}

/// One provider's discovered models, frontier-first then alphabetical.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderGroup {
    pub provider: String,
    pub models: Vec<ModelInfo>,
}

impl ProviderGroup {
    pub fn total(&self) -> usize {
        self.models.len()
    }
    pub fn frontier(&self) -> usize {
        self.models.iter().filter(|m| m.frontier).count()
    }
    pub fn free(&self) -> usize {
        self.models.iter().filter(|m| m.free).count()
    }
    pub fn paid(&self) -> usize {
        self.models.iter().filter(|m| m.paid).count()
    }
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

    /// Every discovered model classified for display (id order preserved).
    pub fn infos(&self, pricing: &Pricing) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|m| ModelInfo::classify(m, pricing))
            .collect()
    }

    /// Headline counts across the catalog (total / providers / frontier / free / subscription /
    /// paid) for the overview.
    pub fn stats(&self, pricing: &Pricing) -> CatalogStats {
        let infos = self.infos(pricing);
        let mut providers: Vec<&str> = infos.iter().map(|m| m.provider.as_str()).collect();
        providers.sort_unstable();
        providers.dedup();
        CatalogStats {
            total: infos.len(),
            providers: providers.len(),
            frontier: infos.iter().filter(|m| m.frontier).count(),
            free: infos.iter().filter(|m| m.free).count(),
            subscription: infos.iter().filter(|m| m.subscription).count(),
            paid: infos.iter().filter(|m| m.paid).count(),
        }
    }

    /// Models grouped by provider for the drill-in browser. Providers are ordered by model count
    /// (richest first), ties by name; within a group, frontier models lead, then alphabetical.
    pub fn by_provider(&self, pricing: &Pricing) -> Vec<ProviderGroup> {
        let mut groups: Vec<ProviderGroup> = Vec::new();
        for info in self.infos(pricing) {
            match groups.iter_mut().find(|g| g.provider == info.provider) {
                Some(g) => g.models.push(info),
                None => groups.push(ProviderGroup {
                    provider: info.provider.clone(),
                    models: vec![info],
                }),
            }
        }
        for g in &mut groups {
            g.models.sort_by(|a, b| {
                b.frontier
                    .cmp(&a.frontier)
                    .then_with(|| a.name.cmp(&b.name))
                    .then_with(|| a.id.cmp(&b.id))
            });
        }
        groups.sort_by(|a, b| {
            b.models
                .len()
                .cmp(&a.models.len())
                .then_with(|| a.provider.cmp(&b.provider))
        });
        groups
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

    fn overview_catalog() -> ModelCatalog {
        ModelCatalog::new(vec![
            "anthropic::claude-opus-4-8".into(),            // frontier, paid
            "openai::gpt-4o-mini".into(),                   // small, paid
            "groq::llama-3.1-8b-instant".into(),            // small, free (unpriced free-tier)
            "groq::llama-3.3-70b-versatile".into(),         // frontier, free
            "ollama::llama3.2".into(),                      // free, local
            "claude-cli::".into(),                          // subscription bridge
            "openrouter::anthropic/claude-opus-4".into(), // frontier, PAID gateway (no price, no :free)
            "openrouter::deepseek/deepseek-r1:free".into(), // frontier, free (:free variant)
        ])
    }

    #[test]
    fn openrouter_unpriced_models_are_paid_unless_free_suffixed() {
        let infos = overview_catalog().infos(&Pricing::default());
        // A paid OpenRouter frontier model we hold no price for must NOT read as free (the bug).
        let opus = infos
            .iter()
            .find(|m| m.id == "openrouter::anthropic/claude-opus-4")
            .unwrap();
        assert!(opus.frontier && opus.paid && !opus.free, "{opus:?}");
        // Its `:free` sibling is correctly free.
        let r1 = infos.iter().find(|m| m.id.contains(":free")).unwrap();
        assert!(r1.free && !r1.paid, "{r1:?}");
    }

    #[test]
    fn paid_free_and_subscription_are_mutually_exclusive() {
        for m in overview_catalog().infos(&Pricing::default()) {
            let n = [m.free, m.paid, m.subscription]
                .iter()
                .filter(|b| **b)
                .count();
            assert_eq!(n, 1, "exactly one category per model: {m:?}");
        }
    }

    #[test]
    fn classifies_frontier_free_and_subscription() {
        let infos = overview_catalog().infos(&Pricing::default());
        let opus = infos.iter().find(|m| m.id.contains("opus")).unwrap();
        assert!(opus.frontier && !opus.free && !opus.subscription && opus.cost > 0.0);

        let g70 = infos.iter().find(|m| m.id.contains("70b")).unwrap();
        assert!(g70.frontier && g70.free, "free frontier groq model");

        let local = infos.iter().find(|m| m.provider == "ollama").unwrap();
        assert!(local.free && !local.frontier && local.cost == 0.0);

        let bridge = infos.iter().find(|m| m.provider == "claude-cli").unwrap();
        assert!(
            bridge.subscription && !bridge.free,
            "subscription bridge is not counted as free"
        );
        assert_eq!(
            bridge.name, "",
            "bare bridge id → default model (empty name)"
        );
    }

    #[test]
    fn stats_count_each_category() {
        let s = overview_catalog().stats(&Pricing::default());
        assert_eq!(s.total, 8);
        assert_eq!(s.providers, 6); // anthropic, openai, groq, ollama, claude-cli, openrouter
        assert_eq!(s.frontier, 4); // anthropic-opus, groq-70b, or-opus, or-deepseek-r1
        assert_eq!(s.subscription, 1); // claude-cli
        assert_eq!(s.free, 4); // groq-8b, groq-70b, ollama, or-deepseek-r1:free
        assert_eq!(s.paid, 3); // anthropic-opus, gpt-4o-mini, or-opus
    }

    #[test]
    fn groups_by_provider_richest_first_frontier_leads() {
        let groups = overview_catalog().by_provider(&Pricing::default());
        // groq has 2 models → it leads.
        assert_eq!(groups[0].provider, "groq");
        assert_eq!(groups[0].total(), 2);
        // within groq, the frontier 70b sorts before the 8b.
        assert!(groups[0].models[0].id.contains("70b"));
        assert_eq!(groups[0].frontier(), 1);
        assert_eq!(groups[0].free(), 2);
    }
}
