//! A structured, human-readable explanation of a single routing decision — the data behind the
//! `/mesh` interactive inspector and `forge mesh explain`. It re-runs the exact production scoring
//! (no parallel logic) and records every step: classification, the per-model scored candidate
//! table, the quota snapshot, the conservation roll, and the final pick + fallback chain. The goal
//! is to make "why did the mesh choose this?" answerable, and to verify the policy is behaving.

use forge_types::{ModelHealth, QuotaStatus, SubscriptionQuota, TaskTier};

use crate::catalog::{self, ConserveDecision, ScoreRow};
use crate::{score_prompt, BudgetState, HeuristicRouter, RouteHints};

/// One model in the ranked candidate table, with the router's usability overlay.
#[derive(Debug, Clone)]
pub struct CandidateRow {
    pub rank: usize,
    pub row: ScoreRow,
    /// Provider key present (or keyless) AND not benched AND not an exhausted subscription.
    pub usable: bool,
    /// The model the mesh actually routed this prompt to.
    pub selected: bool,
}

/// A subscription provider's quota pressure + the spread probability for the explained tier.
#[derive(Debug, Clone)]
pub struct ProviderQuotaView {
    pub provider: String,
    pub status: QuotaStatus,
    pub fraction: f64,
    pub plan: String,
    /// Probability a task of this tier spreads OFF this subscription (the conservation pull).
    pub spread_probability: f64,
}

/// The full explanation of one routing decision.
#[derive(Debug, Clone)]
pub struct RoutingExplanation {
    pub prompt: String,
    /// Tier from prompt classification.
    pub classified_tier: TaskTier,
    /// Tier actually routed (may differ: budget downshift, pin override).
    pub routed_tier: TaskTier,
    pub classify_reasons: Vec<String>,
    pub code_heavy: bool,
    pub seed: u64,
    pub conserve: ConserveDecision,
    pub quota: Vec<ProviderQuotaView>,
    /// Ranked best-first; empty when auto-discovery routing is inactive (manual `[mesh.models]`).
    pub candidates: Vec<CandidateRow>,
    pub pick: String,
    pub fallbacks: Vec<String>,
    pub rationale: String,
    /// Human-readable label for which classifier produced this tier — set by the caller (forge-core)
    /// based on the configured `mesh.classifier`. Defaults to `"heuristic"`.
    pub classifier_label: String,
}

impl HeuristicRouter {
    /// Produce a full [`RoutingExplanation`] for `prompt` — the same decision [`route`](Self::route)
    /// would make, with every intermediate step exposed.
    pub fn explain(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
    ) -> RoutingExplanation {
        let cls = score_prompt(prompt);
        let hints = RouteHints::from_prompt(prompt);
        let tier = cls.tier;

        // The authoritative decision (pin / budget / fallback handling all live here). Compute it
        // FIRST: `decide` can downshift the tier (e.g. budget exhausted → Trivial), and the candidate
        // table + conservation data must describe the tier that ACTUALLY drove the pick, not the
        // classified one — otherwise `/mesh` shows the Trivial pick ranked last among Complex rows
        // with a Complex-tier conservation probability.
        let decision = self.decide(tier, cls.reasons.join(", "), budget, health, hints, quota);
        let routed_tier = decision.tier;

        let (conserve, rows) = if self.auto_active() {
            self.catalog.as_ref().unwrap().ranked_rows(
                routed_tier,
                &self.pricing,
                hints.code_heavy,
                hints.seed,
                quota,
            )
        } else {
            (ConserveDecision::default(), Vec::new())
        };

        let candidates = rows
            .into_iter()
            .enumerate()
            .map(|(i, row)| CandidateRow {
                rank: i + 1,
                usable: self.is_usable(&row.model, health, quota),
                selected: row.model == decision.model,
                row,
            })
            .collect();

        // Quota views for each subscription provider present in the catalog.
        let mut sub_providers: Vec<String> = self
            .catalog
            .as_ref()
            .map(|c| {
                let mut v: Vec<String> = c
                    .models()
                    .iter()
                    .filter(|m| catalog::is_subscription(m))
                    .map(|m| catalog::provider_of(m).to_string())
                    .collect();
                v.sort();
                v.dedup();
                v
            })
            .unwrap_or_default();
        sub_providers.retain(|p| !p.is_empty());
        let quota_views = sub_providers
            .into_iter()
            .map(|p| {
                let fraction = quota.fraction_for(&p);
                let plan = quota.plan_for(&p).to_string();
                ProviderQuotaView {
                    spread_probability: crate::ModelCatalog::spread_probability(
                        routed_tier,
                        fraction,
                        &plan,
                        hints.code_heavy,
                    ),
                    status: quota.status_for(&p),
                    fraction,
                    plan,
                    provider: p,
                }
            })
            .collect();

        RoutingExplanation {
            prompt: prompt.to_string(),
            classified_tier: tier,
            routed_tier: decision.tier,
            classify_reasons: cls.reasons.iter().map(|s| s.to_string()).collect(),
            code_heavy: hints.code_heavy,
            seed: hints.seed,
            conserve,
            quota: quota_views,
            candidates,
            pick: decision.model,
            fallbacks: decision.fallbacks,
            rationale: decision.rationale,
            classifier_label: "heuristic".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HeuristicRouter, ModelCatalog};
    use forge_config::Config;

    fn router() -> HeuristicRouter {
        HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(ModelCatalog::new(vec![
                "claude-cli::opus".into(),
                "codex-cli::gpt-5.5".into(),
                "groq::llama-3.3-70b-versatile".into(),
                "groq::llama-3.1-8b-instant".into(),
            ]))
    }

    #[test]
    fn explanation_pick_matches_the_real_route() {
        let r = router();
        let prompt = "design and prove correct a lock-free queue";
        let e = r.explain(
            prompt,
            BudgetState::default(),
            &ModelHealth::default(),
            &SubscriptionQuota::default(),
        );
        // The explained pick must equal what the selected candidate row says, and the top usable
        // row must be the pick (the table is the decision, made legible).
        let selected = e.candidates.iter().find(|c| c.selected).unwrap();
        assert_eq!(selected.row.model, e.pick);
        assert!(!e.candidates.is_empty());
        assert_eq!(e.classified_tier, TaskTier::Complex);
    }

    #[test]
    fn explanation_surfaces_the_conservation_roll() {
        let r = router();
        let mut fr = std::collections::HashMap::new();
        fr.insert("claude-cli".to_string(), 0.5);
        fr.insert("codex-cli".to_string(), 0.5);
        let quota = SubscriptionQuota::new(std::collections::HashMap::new())
            .with_fractions(fr)
            .with_conserve(true);
        let e = r.explain(
            "design and prove correct a lock-free queue",
            BudgetState::default(),
            &ModelHealth::default(),
            &quota,
        );
        assert!(e.conserve.enabled);
        assert!(e.conserve.eligible, "a free frontier alternative exists");
        assert!(e.conserve.probability > 0.0);
        // When conservation fires, the selected model is non-subscription and carries no penalty,
        // while the demoted subscriptions show the penalty.
        if e.conserve.fired {
            let sel = e.candidates.iter().find(|c| c.selected).unwrap();
            assert!(!sel.row.subscription, "fired → spread to free frontier");
        }
    }
}
