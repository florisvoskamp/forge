//! The Model Mesh (ADR-0006): classify a task, then route it to the cheapest configured
//! model that can handle it — adjusting for the remaining budget. Routing is deterministic
//! and adds no model calls. The [`Router`] trait keeps a smarter (e.g. LLM-based)
//! classifier pluggable later without changing callers.

use forge_config::Config;
use forge_types::TaskTier;

pub mod pricing;

/// Live budget context the router considers when choosing a tier. Carries both the daily
/// and monthly axes (FR-5); the stricter of the two governs.
#[derive(Debug, Clone, Copy)]
pub struct BudgetState {
    pub spent_today_usd: f64,
    pub daily_cap_usd: Option<f64>,
    pub spent_month_usd: f64,
    pub monthly_cap_usd: Option<f64>,
    /// Fraction of a cap at which to warn (e.g. 0.8 = 80%).
    pub warn_fraction: f64,
}

impl Default for BudgetState {
    fn default() -> Self {
        Self {
            spent_today_usd: 0.0,
            daily_cap_usd: None,
            spent_month_usd: 0.0,
            monthly_cap_usd: None,
            warn_fraction: DEFAULT_WARN_FRACTION,
        }
    }
}

/// Where spending sits relative to a cap. Ordered `Ok < Warning < Exhausted` so the stricter
/// of two axes can be taken with `.max()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetStatus {
    /// No cap, or comfortably under it.
    Ok,
    /// At or past the warn threshold (default 80% of the cap), not yet over.
    Warning,
    /// At or over the cap — the router downshifts to the cheapest tier.
    Exhausted,
}

/// Default fraction of the cap at which to warn the user.
pub const DEFAULT_WARN_FRACTION: f64 = 0.8;

impl BudgetState {
    fn axis(spent: f64, cap: Option<f64>, warn: f64) -> BudgetStatus {
        match cap {
            Some(c) if spent >= c => BudgetStatus::Exhausted,
            Some(c) if spent >= c * warn => BudgetStatus::Warning,
            _ => BudgetStatus::Ok,
        }
    }

    /// Classify current spending: the stricter of the daily and monthly axes wins.
    pub fn status(&self) -> BudgetStatus {
        Self::axis(self.spent_today_usd, self.daily_cap_usd, self.warn_fraction).max(Self::axis(
            self.spent_month_usd,
            self.monthly_cap_usd,
            self.warn_fraction,
        ))
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
pub trait Router: Send {
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
            daily_cap_usd: Some(10.0),
            ..Default::default()
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
            ..Default::default()
        };
        assert_eq!(b.status(), BudgetStatus::Ok);
    }

    #[test]
    fn stricter_axis_wins() {
        // day Ok, month Exhausted -> Exhausted (AC-8).
        let b = BudgetState {
            spent_today_usd: 1.0,
            daily_cap_usd: Some(100.0),
            spent_month_usd: 80.0,
            monthly_cap_usd: Some(80.0),
            warn_fraction: DEFAULT_WARN_FRACTION,
        };
        assert_eq!(b.status(), BudgetStatus::Exhausted);
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
            daily_cap_usd: Some(5.0),
            ..Default::default()
        };
        let d = router().route("refactor the whole architecture", budget);
        assert_eq!(d.tier, TaskTier::Trivial);
        assert!(d.rationale.contains("budget"));
    }
}
