//! The Model Mesh (ADR-0006): classify a task, then route it to the cheapest configured
//! model that can handle it — adjusting for the remaining budget. Routing is deterministic
//! and adds no model calls. The [`Router`] trait keeps a smarter (e.g. LLM-based)
//! classifier pluggable later without changing callers.

use forge_config::Config;
use forge_types::TaskTier;

pub mod pricing;

/// Live budget context the router considers when choosing a tier.
#[derive(Debug, Clone, Copy, Default)]
pub struct BudgetState {
    pub spent_today_usd: f64,
    pub daily_budget_usd: Option<f64>,
}

/// Where spending sits relative to the configured daily cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStatus {
    /// No cap, or comfortably under it.
    Ok,
    /// At or past the warn threshold (default 80% of the cap), not yet over.
    Warning,
    /// At or over the cap — the router downshifts to the cheapest tier.
    Exhausted,
}

/// Fraction of the cap at which to warn the user.
const WARN_FRACTION: f64 = 0.8;

impl BudgetState {
    /// Classify current spending against the cap.
    pub fn status(&self) -> BudgetStatus {
        match self.daily_budget_usd {
            Some(cap) if self.spent_today_usd >= cap => BudgetStatus::Exhausted,
            Some(cap) if self.spent_today_usd >= cap * WARN_FRACTION => BudgetStatus::Warning,
            _ => BudgetStatus::Ok,
        }
    }
}

/// The Mesh's decision for one task, including *why* (recorded + shown to the user).
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    pub tier: TaskTier,
    pub model: String,
    pub rationale: String,
}

/// A routing strategy. Implement this to add a new classifier (e.g. an LLM-based one).
pub trait Router {
    fn route(&self, prompt: &str, budget: BudgetState) -> RoutingDecision;
}

const COMPLEX_KEYWORDS: &[&str] = &[
    "architecture",
    "refactor",
    "design",
    "debug",
    "why",
    "explain",
    "optimi",
    "concurren",
];

/// The default v0.1 router: deterministic heuristics over cheap local signals.
pub struct HeuristicRouter {
    config: Config,
}

impl HeuristicRouter {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    fn classify(prompt: &str) -> (TaskTier, &'static str) {
        let len = prompt.chars().count();
        let lower = prompt.to_lowercase();
        if COMPLEX_KEYWORDS.iter().any(|k| lower.contains(k)) || len > 600 {
            (
                TaskTier::Complex,
                "matched complex signal (keyword or long prompt)",
            )
        } else if len < 80 {
            (TaskTier::Trivial, "short prompt, no complex signals")
        } else {
            (TaskTier::Standard, "medium prompt, no complex signals")
        }
    }
}

impl Router for HeuristicRouter {
    fn route(&self, prompt: &str, budget: BudgetState) -> RoutingDecision {
        let (mut tier, mut rationale) = Self::classify(prompt);
        let mut why = rationale.to_string();

        // Budget pressure downshifts to the cheapest tier (FR-5).
        if budget.status() == BudgetStatus::Exhausted && tier != TaskTier::Trivial {
            tier = TaskTier::Trivial;
            rationale = "budget cap reached — downshifted to trivial tier";
            why = rationale.to_string();
        }

        let model = self.config.model_for(tier).unwrap_or("unknown").to_string();

        RoutingDecision {
            tier,
            model,
            rationale: why,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> HeuristicRouter {
        HeuristicRouter::new(Config::default())
    }

    #[test]
    fn short_prompt_is_trivial() {
        let d = router().route("fix typo", BudgetState::default());
        assert_eq!(d.tier, TaskTier::Trivial);
    }

    #[test]
    fn budget_status_thresholds() {
        let mk = |spent| BudgetState {
            spent_today_usd: spent,
            daily_budget_usd: Some(10.0),
        };
        assert_eq!(mk(0.0).status(), BudgetStatus::Ok);
        assert_eq!(mk(7.99).status(), BudgetStatus::Ok);
        assert_eq!(mk(8.0).status(), BudgetStatus::Warning); // 80% of cap
        assert_eq!(mk(9.5).status(), BudgetStatus::Warning);
        assert_eq!(mk(10.0).status(), BudgetStatus::Exhausted);
        assert_eq!(mk(99.0).status(), BudgetStatus::Exhausted);
    }

    #[test]
    fn no_cap_is_always_ok() {
        let b = BudgetState {
            spent_today_usd: 1000.0,
            daily_budget_usd: None,
        };
        assert_eq!(b.status(), BudgetStatus::Ok);
    }

    #[test]
    fn keyword_forces_complex() {
        let d = router().route("refactor the auth module", BudgetState::default());
        assert_eq!(d.tier, TaskTier::Complex);
    }

    #[test]
    fn medium_prompt_is_standard() {
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = router().route(&prompt, BudgetState::default());
        assert_eq!(d.tier, TaskTier::Standard);
    }

    #[test]
    fn exhausted_budget_downshifts() {
        let budget = BudgetState {
            spent_today_usd: 5.0,
            daily_budget_usd: Some(5.0),
        };
        let d = router().route("refactor the whole architecture", budget);
        assert_eq!(d.tier, TaskTier::Trivial);
        assert!(d.rationale.contains("budget"));
    }
}
