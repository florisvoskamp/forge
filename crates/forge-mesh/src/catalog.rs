//! A live catalog of usable models, discovered from the providers the user has keys for
//! (auto-discovery mesh, docs/features/auto-discovery-mesh.md). This is a plain data holder +
//! ranking; the async *discovery* (querying each provider's model list) lives in the binary
//! (forge-cli), which has the provider client — forge-mesh stays free of that dependency.

use forge_types::TaskTier;

use crate::capability::{capability_score, is_frontier};
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

/// A model's cost class for routing: `0` genuinely free (local/free-tier), `1` subscription
/// ($0 marginal but burns the user's plan quota), `2` metered/paid. The mesh prefers low classes
/// for cheap tiers (preserve quota) and the subscription flagship for complex work.
pub(crate) fn cost_class(id: &str, cost: f64) -> u8 {
    if is_subscription(id) {
        1
    } else if is_free(id, cost, false) {
        0
    } else {
        2
    }
}

/// How much a tier *wants* each cost class (added to the capability score). The policy:
/// - Trivial: prefer genuinely-free, so easy tasks don't burn subscription quota.
/// - Standard: subscription ≈ free, a slight subscription edge (use the good $0 models).
/// - Complex: prefer the subscription flagship (strongest reliable, $0 marginal); free as backup.
fn cost_pref(tier: TaskTier, class: u8) -> f64 {
    match (tier, class) {
        (TaskTier::Trivial, 0) => 1.0,
        (TaskTier::Trivial, 1) => 0.3,
        (TaskTier::Trivial, _) => -0.6,
        (TaskTier::Standard, 0) => 0.5,
        (TaskTier::Standard, 1) => 0.6,
        (TaskTier::Standard, _) => -0.4,
        (TaskTier::Complex, 0) => 0.4,
        (TaskTier::Complex, 1) => 0.8,
        (TaskTier::Complex, _) => 0.0,
    }
}

/// A mild, defensible provider prior (a tiebreak nudge, never a hard rule):
/// - code-heavy task → the coding-tuned flagships (codex/claude bridges + their APIs) get a small
///   lift over general models;
/// - trivial non-code → the fast cheap-bulk providers (groq/gemini) get a small lift.
fn code_prior(provider: &str, code_heavy: bool, tier: TaskTier) -> f64 {
    if code_heavy {
        return match provider {
            "codex-cli" | "claude-cli" | "anthropic" | "openai" => 0.3,
            _ => 0.0,
        };
    }
    if tier == TaskTier::Trivial && matches!(provider, "groq" | "gemini") {
        return 0.2;
    }
    0.0
}

/// The full routing score for one model: capability fit + cost-class preference + the mild prior.
fn route_score(id: &str, tier: TaskTier, cost: f64, code_heavy: bool) -> f64 {
    capability_score(id, tier)
        + cost_pref(tier, cost_class(id, cost))
        + code_prior(provider_of(id), code_heavy, tier)
}

/// A per-prompt provider ordering key: hashing `seed:provider` means different prompts rotate
/// which provider wins a genuine score tie, so a workload spreads across equally-good providers
/// (claude ↔ codex) instead of always picking the alphabetically-first one — while staying fully
/// deterministic for a given prompt.
fn provider_rotation(provider: &str, seed: u64) -> u64 {
    stable_hash(&format!("{seed}:{provider}"))
}

/// A fine within-family capability key (the first version number in the id: `gpt-5.5`→5.5,
/// `claude-opus-4-8`→4.8, `gpt-4o-mini`→4.0). Used as a LATE tiebreak — after the provider
/// rotation — so it only orders models of the *same* provider/class: never pick `gpt-5.2` over
/// `gpt-5.5` when both are the same $0 subscription. It never competes across providers (the
/// rotation already separated those), so a higher raw number can't make one provider always win.
fn fine_capability(id: &str) -> f64 {
    let bytes = id.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_digit() {
        i += 1;
    }
    let mut major: u32 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        major = major * 10 + (bytes[i] - b'0') as u32;
        i += 1;
    }
    // An immediately-following `.` or `-` then digits is the minor version (`5.4`, `4-8`).
    let mut frac = 0.0;
    if i < bytes.len()
        && (bytes[i] == b'.' || bytes[i] == b'-')
        && i + 1 < bytes.len()
        && bytes[i + 1].is_ascii_digit()
    {
        i += 1;
        let (mut minor, mut digits) = (0u32, 0i32);
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            minor = minor * 10 + (bytes[i] - b'0') as u32;
            digits += 1;
            i += 1;
        }
        frac = minor as f64 / 10f64.powi(digits);
    }
    major as f64 + frac
}

/// A small deterministic FNV-1a hash (no external deps); used for the seed and provider rotation.
pub fn stable_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
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

    /// The discovered models ranked best-first for `tier` (display / non-prompt callers): the
    /// cost-tiered routing score with a neutral context (not code-heavy, fixed seed). The live
    /// router uses [`ranked_seeded`](Self::ranked_seeded) so genuine ties spread across providers
    /// per prompt instead of always picking the alphabetically-first one.
    pub fn ranked_for(&self, tier: TaskTier, pricing: &Pricing, top: usize) -> Vec<String> {
        self.ranked_seeded(tier, pricing, top, false, 0)
    }

    /// Prompt-aware ranking: cost-tiered capability score, with genuine ties broken by a
    /// per-prompt `seed` rotation across providers (fair spread) then id (stable). `code_heavy`
    /// applies the mild coding-provider prior. The single place the routing policy lives.
    pub fn ranked_seeded(
        &self,
        tier: TaskTier,
        pricing: &Pricing,
        top: usize,
        code_heavy: bool,
        seed: u64,
    ) -> Vec<String> {
        let mut scored: Vec<(f64, u8, u64, f64, &String)> = self
            .models
            .iter()
            .map(|m| {
                let cost = pricing.estimated_cost(m);
                (
                    route_score(m, tier, cost, code_heavy),
                    cost_class(m, cost),
                    provider_rotation(provider_of(m), seed),
                    fine_capability(m),
                    m,
                )
            })
            .collect();
        // Best score first; then cheaper cost-class; then the per-prompt provider rotation
        // (spreads ties ACROSS providers); then — within one provider — the higher-version model
        // (never a lesser sibling); then id for a fully deterministic order.
        scored.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| b.3.total_cmp(&a.3))
                .then_with(|| a.4.cmp(b.4))
        });
        scored
            .into_iter()
            .take(top)
            .map(|(_, _, _, _, m)| m.clone())
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
    fn within_a_subscription_family_the_higher_version_wins() {
        // The gpt-5.2-over-5.5 bug: among same-provider, same-class $0 models, never pick the
        // lesser sibling. fine_capability orders 5.5 > 5.4 > 5.2 (and the mini stays a small/
        // trivial model, not a complex pick).
        let cat = ModelCatalog::new(vec![
            "codex-cli::gpt-5.2".into(),
            "codex-cli::gpt-5.4".into(),
            "codex-cli::gpt-5.5".into(),
            "codex-cli::gpt-5.4-mini".into(),
        ]);
        let r = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 4);
        assert_eq!(
            r[0], "codex-cli::gpt-5.5",
            "highest version leads complex: {r:?}"
        );
        assert!(
            r.iter().position(|m| m == "codex-cli::gpt-5.5").unwrap()
                < r.iter().position(|m| m == "codex-cli::gpt-5.2").unwrap(),
            "5.5 must rank above 5.2: {r:?}"
        );
        // The mini is small-class → it is NOT the complex pick.
        assert_ne!(r[0], "codex-cli::gpt-5.4-mini");
    }

    #[test]
    fn fine_capability_parses_versions() {
        assert!(fine_capability("codex-cli::gpt-5.5") > fine_capability("codex-cli::gpt-5.4"));
        assert!(fine_capability("codex-cli::gpt-5.4") > fine_capability("codex-cli::gpt-5.2"));
        assert!(
            (fine_capability("anthropic::claude-opus-4-8") - 4.8).abs() < 1e-9,
            "4-8 → 4.8"
        );
        assert_eq!(fine_capability("ollama::llama3"), 3.0);
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
