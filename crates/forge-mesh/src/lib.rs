//! The Model Mesh (ADR-0006): classify a task, then route it to the cheapest configured
//! model that can handle it — adjusting for the remaining budget. Routing is deterministic
//! and adds no model calls. The [`Router`] trait keeps a smarter (e.g. LLM-based)
//! classifier pluggable later without changing callers.

use async_trait::async_trait;
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

/// A routing strategy. `async` so an implementation may consult a model (e.g. the opt-in
/// LLM classifier); the default [`HeuristicRouter`] resolves instantly with no I/O.
#[async_trait]
pub trait Router: Send + Sync {
    async fn route(&self, prompt: &str, budget: BudgetState) -> RoutingDecision;
}

// --- Classification signals (weighted scoring; see `classify`). Capability over length. ---

/// Explicit user hint that forces Complex regardless of anything else (ADR-0006: user hints).
const COMPLEX_HINTS: &[&str] = &[
    "think hard",
    "think deeply",
    "ultrathink",
    "think carefully",
    "step by step",
];
/// Explicit "this is easy" hints — a strong pull toward Trivial.
const TRIVIAL_HINTS: &[&str] = &["quick", "simple", "one-liner", "one liner"];
/// Reasoning / algorithmic / architectural terms — the load is cognitive, not length. A
/// single one can carry a *short* prompt to Complex (the headline fix).
const REASONING_TERMS: &[&str] = &[
    "architect",
    "architecture",
    "refactor",
    "design",
    "debug",
    "why",
    "explain",
    "optimi",
    "concurren",
    "lock-free",
    "lockless",
    "race condition",
    "deadlock",
    "thread-safe",
    "prove",
    "proof",
    "complexity",
    "invariant",
    "distributed",
    "analyze",
    "analyse",
    "trade-off",
    "tradeoff",
    "algorithm",
];
/// Dev-action verbs that imply real (non-trivial) work. Phrases ("add a"/"write a") avoid
/// matching trivial requests like "add a comment" (handled by TRIVIAL_PATTERNS first).
const ACTION_VERBS: &[&str] = &[
    "implement",
    "migrate",
    "integrate",
    "benchmark",
    "profile",
    "parallelize",
    "deploy",
    "wire ",
    "add a ",
    "write a ",
    "create a ",
    "build a ",
];
/// Trivial-edit patterns — a strong pull toward Trivial regardless of length.
const TRIVIAL_PATTERNS: &[&str] = &[
    "typo",
    "rename",
    "bump version",
    "bump the version",
    "reformat",
    "add a comment",
    "fix import",
    "fix the import",
    "whitespace",
    "one-liner",
    "one liner",
];
/// Code-vs-prose markers (besides a fenced ```code block```). Symbol-based on purpose —
/// natural-language words like "function"/"class"/"import" appear in prose and would false-
/// positive ("write a function that…" is not code).
const CODE_TOKENS: &[&str] = &["fn ", "});", "() =>", "();", "{\n", "=> {"];
/// Error / stack-trace markers (a concrete failure usually means real debugging).
const ERROR_MARKERS: &[&str] = &[
    "panic",
    "traceback",
    "stack trace",
    "error[",
    "exception",
    "segfault",
    " at line ",
];

/// The default v0.1 router: deterministic heuristics over cheap local signals (ADR-0006).
pub struct HeuristicRouter {
    config: Config,
    /// A user-pinned model (`--model`) that bypasses classification, subject to the budget
    /// contract. `None` = classify normally.
    pin: Option<String>,
    /// Whether `model`'s provider has a usable key (for provider fallback). Injectable so
    /// tests are deterministic; defaults to a real env/keyring check.
    model_available: fn(&str) -> bool,
    /// Bundled+configured rates, used to rank candidate models by relative cost.
    pricing: pricing::Pricing,
}

fn default_model_available(model: &str) -> bool {
    forge_config::has_api_key(forge_config::provider_of(model))
}

/// A model billed to an already-paid subscription (the CLI bridges) — $0 marginal cost.
fn is_subscription(model: &str) -> bool {
    matches!(forge_config::provider_of(model), "claude-cli" | "codex-cli")
}

/// Tier classification with the human-readable signals that drove it.
struct Classification {
    tier: TaskTier,
    reasons: Vec<&'static str>,
}

/// Score a prompt's difficulty from weighted local signals (deterministic, no I/O). Capability
/// signals (reasoning terms, code, errors) can lift a *short* prompt to Complex; trivial-edit
/// patterns and "quick" hints pull it down. Length is one capped signal, never the decider —
/// this is the fix for the old length-bucket classifier.
fn score_prompt(prompt: &str) -> Classification {
    let lower = prompt.to_lowercase();

    // An explicit "think hard" hint is a hard override — the user told us it's hard.
    if COMPLEX_HINTS.iter().any(|h| lower.contains(h)) {
        return Classification {
            tier: TaskTier::Complex,
            reasons: vec!["explicit 'think hard' hint"],
        };
    }

    let words = prompt.split_whitespace().count();
    let mut pts: i32 = 0;
    let mut reasons: Vec<&'static str> = Vec::new();

    // Length: a single capped nudge, not the decider.
    if words > 120 {
        pts += 3;
        reasons.push("very long prompt");
    } else if words > 40 {
        pts += 1;
        reasons.push("long prompt");
    }

    let has_code = prompt.contains("```") || CODE_TOKENS.iter().any(|t| lower.contains(t));
    if REASONING_TERMS.iter().any(|t| lower.contains(t)) {
        pts += 5;
        reasons.push("reasoning/algorithmic term");
    }
    if has_code {
        pts += 3;
        reasons.push("code present");
    }
    if ACTION_VERBS.iter().any(|v| lower.contains(v)) {
        pts += 2;
        reasons.push("dev-action verb");
    }
    if is_multistep(&lower) {
        pts += 2;
        reasons.push("multi-step scope");
    }
    if lower.contains("test") || lower.contains("benchmark") || lower.contains("edge case") {
        pts += 1;
        reasons.push("tests/edge-cases");
    }
    if ERROR_MARKERS.iter().any(|m| lower.contains(m)) {
        pts += 1;
        reasons.push("error/stack trace");
    }

    // Trivial pulls (strong, regardless of length).
    if TRIVIAL_HINTS.iter().any(|h| lower.contains(h)) {
        pts -= 5;
        reasons.push("explicit 'quick' hint");
    }
    if TRIVIAL_PATTERNS.iter().any(|p| lower.contains(p)) {
        pts -= 4;
        reasons.push("trivial-edit pattern");
    }

    // Thresholds: <=0 Trivial, >=5 Complex, else Standard.
    let tier = if pts <= 0 {
        TaskTier::Trivial
    } else if pts >= 5 {
        TaskTier::Complex
    } else {
        TaskTier::Standard
    };
    if reasons.is_empty() {
        reasons.push(match tier {
            TaskTier::Trivial => "short prompt, no strong signals",
            TaskTier::Standard => "moderate task",
            TaskTier::Complex => "complex task",
        });
    }
    Classification { tier, reasons }
}

fn is_multistep(lower: &str) -> bool {
    lower.contains(" then ")
        || lower.contains("\n- ")
        || lower.contains("\n* ")
        || (lower.contains("1.") && lower.contains("2."))
}

impl HeuristicRouter {
    pub fn new(config: Config) -> Self {
        let pricing = pricing::Pricing::from_config(&config);
        Self {
            config,
            pin: None,
            model_available: default_model_available,
            pricing,
        }
    }

    /// Pin a model (`--model`); empty/`None` clears it.
    pub fn with_pin(mut self, pin: Option<String>) -> Self {
        self.pin = pin.filter(|s| !s.is_empty());
        self
    }

    /// Inject a deterministic provider-availability predicate (tests only).
    #[cfg(test)]
    fn with_availability(mut self, f: fn(&str) -> bool) -> Self {
        self.model_available = f;
        self
    }

    fn classify(prompt: &str) -> (TaskTier, String) {
        let c = score_prompt(prompt);
        (c.tier, c.reasons.join(", "))
    }

    /// Pick the cheapest *usable* model from `candidates` (L1). Usable = the injected
    /// availability predicate passes (key present, or keyless). Ranking key:
    /// `(prefer_subscription && subscription ? 0 : 1, estimated_cost, config_order)` — so a
    /// paid subscription (the $0 CLI bridges) wins when preferred, then lowest est. cost, then
    /// the order the user listed candidates. `None` when none are usable.
    fn cheapest_usable(&self, candidates: &[String]) -> Option<String> {
        let prefer = self.config.mesh.prefer_subscription;
        candidates
            .iter()
            .enumerate()
            .filter(|(_, m)| (self.model_available)(m))
            .min_by(|(ia, a), (ib, b)| {
                let rank = |m: &str| u8::from(!(prefer && is_subscription(m)));
                rank(a)
                    .cmp(&rank(b))
                    .then(
                        self.pricing
                            .estimated_cost(a)
                            .total_cmp(&self.pricing.estimated_cost(b)),
                    )
                    .then(ia.cmp(ib))
            })
            .map(|(_, m)| m.clone())
    }

    /// Count usable candidates (for the rationale).
    fn usable_count(&self, candidates: &[String]) -> usize {
        candidates
            .iter()
            .filter(|m| (self.model_available)(m))
            .count()
    }

    /// Cross-tier fallback: the cheapest usable candidate across tiers, most capable first.
    /// Returns `original` unchanged if nothing anywhere is usable (it errors downstream with
    /// the actionable MissingKey message — same as today).
    fn cross_tier_cheapest(&self, original: &str, why: &mut String) -> String {
        for tier in [TaskTier::Complex, TaskTier::Standard, TaskTier::Trivial] {
            if let Some(m) = self.cheapest_usable(&self.config.candidates_for(tier)) {
                why.push_str(&format!(
                    " — fell back to {m} (no usable key for {original})"
                ));
                return m;
            }
        }
        why.push_str(&format!(
            " — warning: no usable key for {original} and no fallback"
        ));
        original.to_string()
    }
}

impl HeuristicRouter {
    /// Given an already-decided tier (from the heuristic OR an external classifier) + the
    /// reason it was chosen, apply pin / budget pressure / cost-aware candidate selection.
    /// Pure + sync, so any [`Router`] (incl. the LLM one) can reuse the whole selection path.
    pub fn decide(
        &self,
        classified_tier: TaskTier,
        classify_reason: String,
        budget: BudgetState,
    ) -> RoutingDecision {
        let exhausted = budget.status() == BudgetStatus::Exhausted;
        let cap_overrides_pin = self.config.mesh.budget.cap_overrides_pin;

        // A pin bypasses classification unless an exhausted budget may override it.
        if let Some(pin) = self
            .pin
            .as_ref()
            .filter(|_| !(exhausted && cap_overrides_pin))
        {
            let mut why = "pinned via --model".to_string();
            let model = if (self.model_available)(pin) {
                pin.clone()
            } else {
                self.cross_tier_cheapest(pin, &mut why)
            };
            return RoutingDecision {
                tier: classified_tier,
                model,
                rationale: why,
            };
        }

        // Apply budget pressure (FR-5).
        let mut tier = classified_tier;
        let mut why = if self.pin.is_some() {
            // pin was set but an exhausted budget overrode it (see filter above)
            tier = TaskTier::Trivial;
            "budget cap reached — pin overridden, trivial tier".to_string()
        } else if exhausted && tier != TaskTier::Trivial {
            tier = TaskTier::Trivial;
            "budget cap reached — downshifted to trivial tier".to_string()
        } else {
            classify_reason
        };

        // Cost-aware selection among the tier's usable candidates (L1/L2).
        let candidates = self.config.candidates_for(tier);
        let model = match self.cheapest_usable(&candidates) {
            Some(m) => {
                let n = self.usable_count(&candidates);
                if n > 1 {
                    why.push_str(&format!(
                        " — cheapest of {n} usable {} models: {m}",
                        tier.as_str()
                    ));
                }
                if self.config.mesh.prefer_subscription && is_subscription(&m) {
                    why.push_str(" (paid subscription)");
                }
                m
            }
            None => {
                let original = candidates
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                self.cross_tier_cheapest(&original, &mut why)
            }
        };

        RoutingDecision {
            tier,
            model,
            rationale: why,
        }
    }
}

#[async_trait]
impl Router for HeuristicRouter {
    async fn route(&self, prompt: &str, budget: BudgetState) -> RoutingDecision {
        let (tier, reason) = Self::classify(prompt);
        self.decide(tier, reason, budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn router() -> HeuristicRouter {
        // Treat every provider as available so tier-classification tests are deterministic
        // (no dependence on ambient env/keyring) and exercise no fallback.
        HeuristicRouter::new(Config::default()).with_availability(|_| true)
    }

    #[tokio::test]
    async fn short_prompt_is_trivial() {
        let d = router().route("fix typo", BudgetState::default()).await;
        assert_eq!(d.tier, TaskTier::Trivial);
    }

    // --- Scoring classifier: capability over length (the headline fix) ---

    #[test]
    fn hard_short_prompt_is_complex_despite_length() {
        // "design a lock-free queue" is 24 chars — the old <80 rule called this Trivial.
        assert_eq!(
            score_prompt("design a lock-free queue").tier,
            TaskTier::Complex
        );
        assert_eq!(
            score_prompt("prove this sort is stable").tier,
            TaskTier::Complex
        );
        assert_eq!(score_prompt("debug this deadlock").tier, TaskTier::Complex);
    }

    #[test]
    fn trivial_edit_stays_trivial_even_with_a_path() {
        assert_eq!(
            score_prompt("rename foo to bar in utils.rs").tier,
            TaskTier::Trivial
        );
        assert_eq!(score_prompt("fix typo").tier, TaskTier::Trivial);
        assert_eq!(
            score_prompt("bump version to 1.2.0").tier,
            TaskTier::Trivial
        );
    }

    #[test]
    fn action_and_multistep_is_standard_not_complex() {
        let p = "write a function that validates email addresses against the RFC rules and \
                 returns which inputs were rejected, then wire it into the signup handler";
        assert_eq!(score_prompt(p).tier, TaskTier::Standard); // AC-A3
    }

    #[test]
    fn long_prose_without_signals_is_not_auto_complex() {
        // Length alone is a capped nudge — 200 plain words must not force Complex.
        let p = "word ".repeat(200);
        assert_ne!(score_prompt(&p).tier, TaskTier::Complex); // AC-A7
    }

    #[test]
    fn every_decision_names_a_signal() {
        for p in [
            "fix typo",
            "design a lock-free queue",
            "add a logging helper module",
        ] {
            assert!(!score_prompt(p).reasons.is_empty(), "no reason for {p:?}");
        }
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

    #[tokio::test]
    async fn keyword_forces_complex() {
        let d = router()
            .route("refactor the auth module", BudgetState::default())
            .await;
        assert_eq!(d.tier, TaskTier::Complex);
    }

    #[tokio::test]
    async fn medium_prompt_is_standard() {
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = router().route(&prompt, BudgetState::default()).await;
        assert_eq!(d.tier, TaskTier::Standard);
    }

    #[tokio::test]
    async fn exhausted_budget_downshifts() {
        let budget = BudgetState {
            spent_today_usd: 5.0,
            daily_cap_usd: Some(5.0),
            ..Default::default()
        };
        let d = router()
            .route("refactor the whole architecture", budget)
            .await;
        assert_eq!(d.tier, TaskTier::Trivial);
        assert!(d.rationale.contains("budget"));
    }

    // --- New: richer signals (AC-5, AC-6, AC-7) ---

    #[tokio::test]
    async fn explicit_think_hard_hint_forces_complex() {
        let d = router()
            .route(
                "rename x; but think hard about edge cases",
                BudgetState::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex); // AC-6
    }

    #[tokio::test]
    async fn fenced_code_is_at_least_standard_despite_short_length() {
        let d = router()
            .route("```rust\nlet x=1;\n```", BudgetState::default())
            .await;
        assert_eq!(d.tier, TaskTier::Standard); // AC-5
    }

    #[tokio::test]
    async fn dev_verb_lifts_short_prompt_to_standard() {
        let d = router()
            .route("integrate the parser", BudgetState::default())
            .await;
        assert_eq!(d.tier, TaskTier::Standard);
    }

    #[tokio::test]
    async fn fix_typo_stays_trivial_no_regression() {
        let d = router().route("fix typo", BudgetState::default()).await;
        assert_eq!(d.tier, TaskTier::Trivial); // AC-7
    }

    // --- New: pin / override (AC-1, AC-2) ---

    #[tokio::test]
    async fn pin_overrides_classification() {
        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_pin(Some("openai::gpt-4o".into()));
        let d = r.route("fix typo", BudgetState::default()).await;
        assert_eq!(d.model, "openai::gpt-4o"); // AC-1
        assert!(d.rationale.contains("pinned"));
    }

    #[tokio::test]
    async fn exhausted_budget_overrides_pin() {
        // hard_stop is enforced pre-routing in core; here cap_overrides_pin governs.
        let mut config = Config::default();
        config.mesh.budget.cap_overrides_pin = true;
        let r = HeuristicRouter::new(config)
            .with_availability(|_| true)
            .with_pin(Some("anthropic::claude-opus-4-8".into()));
        let budget = BudgetState {
            spent_today_usd: 5.0,
            daily_cap_usd: Some(5.0),
            ..Default::default()
        };
        let d = r.route("design a system", budget).await;
        // pin ignored; trivial-tier model chosen (AC-2)
        assert_eq!(
            d.model,
            Config::default().model_for(TaskTier::Trivial).unwrap()
        );
        assert_ne!(d.model, "anthropic::claude-opus-4-8");
    }

    // --- New: provider fallback (AC-3, AC-4) ---

    #[tokio::test]
    async fn falls_back_to_an_available_model_when_key_missing() {
        // Only ollama (the trivial-tier default) is "available"; complex (anthropic) is not.
        let r =
            HeuristicRouter::new(Config::default()).with_availability(|m| m.starts_with("ollama"));
        let d = r
            .route("design the architecture", BudgetState::default())
            .await;
        assert_eq!(d.tier, TaskTier::Complex, "tier still reflects difficulty");
        assert!(
            d.model.starts_with("ollama"),
            "fell back to a usable model: {}",
            d.model
        );
        assert!(d.rationale.contains("fell back"), "{}", d.rationale);
    }

    #[tokio::test]
    async fn no_usable_model_keeps_original_and_warns() {
        // Nothing available → keep the routed model (errors downstream as today).
        let r = HeuristicRouter::new(Config::default()).with_availability(|_| false);
        let d = r
            .route("design the architecture", BudgetState::default())
            .await;
        assert_eq!(
            d.model,
            Config::default().model_for(TaskTier::Complex).unwrap()
        ); // AC-4
        assert!(d.rationale.contains("no usable key"));
    }

    // --- Cost-aware selection (L1) + subscription-first (L2) ---

    fn list_config(tier: &str, models: &[&str]) -> Config {
        let mut c = Config::default();
        c.mesh.models.insert(
            tier.to_string(),
            forge_config::OneOrMany::Many(models.iter().map(|s| s.to_string()).collect()),
        );
        c
    }

    #[test]
    fn cheapest_usable_picks_lowest_estimated_cost() {
        // gpt-4o-mini (~$0.00045/turn) is cheaper than deepseek-chat (~$0.00082/turn).
        let r = HeuristicRouter::new(Config::default()).with_availability(|_| true);
        let cands = vec![
            "deepseek::deepseek-chat".to_string(),
            "openai::gpt-4o-mini".to_string(),
        ];
        assert_eq!(r.cheapest_usable(&cands).unwrap(), "openai::gpt-4o-mini"); // AC-L1a
    }

    #[test]
    fn cheapest_usable_skips_models_without_a_key() {
        // ollama is "cheapest" ($0) but unavailable here → the usable openai wins.
        let r =
            HeuristicRouter::new(Config::default()).with_availability(|m| !m.starts_with("ollama"));
        let cands = vec![
            "ollama::free".to_string(),
            "openai::gpt-4o-mini".to_string(),
        ];
        assert_eq!(r.cheapest_usable(&cands).unwrap(), "openai::gpt-4o-mini"); // AC-L1b
    }

    #[tokio::test]
    async fn route_picks_cheapest_standard_candidate_with_rationale() {
        let c = list_config(
            "standard",
            &["deepseek::deepseek-chat", "openai::gpt-4o-mini"],
        );
        let r = HeuristicRouter::new(c).with_availability(|_| true);
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = r.route(&prompt, BudgetState::default()).await;
        assert_eq!(d.tier, TaskTier::Standard);
        assert_eq!(d.model, "openai::gpt-4o-mini");
        assert!(d.rationale.contains("cheapest of 2"), "{}", d.rationale);
    }

    #[tokio::test]
    async fn legacy_single_string_tier_routes_unchanged() {
        // AC-L1c: the single-string form behaves as a one-candidate list.
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = router().route(&prompt, BudgetState::default()).await;
        assert_eq!(d.model, "openai::gpt-4o-mini");
    }

    #[tokio::test]
    async fn subscription_is_preferred_when_enabled() {
        // AC-L2a: a $0 paid subscription (CLI bridge) wins over a metered API model.
        let r = HeuristicRouter::new(list_config(
            "complex",
            &["anthropic::claude-opus-4-8", "claude-cli::"],
        ))
        .with_availability(|_| true);
        let d = r
            .route(
                "design the system architecture carefully",
                BudgetState::default(),
            )
            .await;
        assert_eq!(d.model, "claude-cli::");
        assert!(d.rationale.contains("paid subscription"), "{}", d.rationale);
    }

    #[tokio::test]
    async fn subscription_still_cheapest_when_preference_disabled() {
        // prefer_subscription off → pure cost ranking; the $0 bridge is still cheapest, but the
        // rationale no longer flags it as a subscription.
        let mut c = list_config("complex", &["anthropic::claude-opus-4-8", "claude-cli::"]);
        c.mesh.prefer_subscription = false;
        let r = HeuristicRouter::new(c).with_availability(|_| true);
        let d = r
            .route(
                "design the system architecture carefully",
                BudgetState::default(),
            )
            .await;
        assert_eq!(d.model, "claude-cli::");
        assert!(!d.rationale.contains("paid subscription"));
    }
}
