//! The Model Mesh (ADR-0006): classify a task, then route it to the cheapest configured
//! model that can handle it — adjusting for the remaining budget. Routing is deterministic
//! and adds no model calls. The [`Router`] trait keeps a smarter (e.g. LLM-based)
//! classifier pluggable later without changing callers.

use async_trait::async_trait;
use forge_config::Config;
use forge_types::{ModelHealth, SubscriptionQuota, TaskTier};

pub mod bench;
pub mod capability;
pub mod catalog;
pub mod explain;
pub mod pricing;

pub use bench::{BenchScore, BenchmarkScores};
pub use catalog::{
    CatalogStats, ConserveDecision, ModelCatalog, ModelInfo, ProviderGroup, ScoreRow,
};
pub use explain::{CandidateRow, ProviderQuotaView, RoutingExplanation};

/// Live budget context the router considers when choosing a tier. Carries daily, weekly, and
/// monthly axes (FR-5); the stricter of all configured axes governs.
#[derive(Debug, Clone, Copy)]
pub struct BudgetState {
    pub spent_today_usd: f64,
    pub daily_cap_usd: Option<f64>,
    pub spent_week_usd: f64,
    pub weekly_cap_usd: Option<f64>,
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
            spent_week_usd: 0.0,
            weekly_cap_usd: None,
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

    /// Classify current spending: the stricter of all configured axes wins.
    pub fn status(&self) -> BudgetStatus {
        Self::axis(self.spent_today_usd, self.daily_cap_usd, self.warn_fraction)
            .max(Self::axis(
                self.spent_week_usd,
                self.weekly_cap_usd,
                self.warn_fraction,
            ))
            .max(Self::axis(
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
    /// Ordered, already-filtered (available + healthy) alternatives to try if `model` fails
    /// mid-turn — most-preferred first, the routed tier's runners-up then cross-tier picks.
    /// Empty when nothing else is usable.
    pub fallbacks: Vec<String>,
}

/// A routing strategy. `async` so an implementation may consult a model (e.g. the opt-in
/// LLM classifier); the default [`HeuristicRouter`] resolves instantly with no I/O. `health`
/// is the set of currently-benched models to route around (failover).
#[async_trait]
pub trait Router: Send + Sync {
    async fn route(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
    ) -> RoutingDecision;

    /// Route with an optional tier hint from an invoked command/skill (`tier:` frontmatter).
    /// The default ignores the hint and delegates to [`Router::route`]; classifying routers
    /// override this to pin the tier (an explicit user `--model` pin still wins, handled in
    /// `decide`). A `None` hint is exactly today's behaviour.
    async fn route_hinted(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
        _tier_override: Option<TaskTier>,
    ) -> RoutingDecision {
        self.route(prompt, budget, health, quota).await
    }
}

// --- Classification signals (weighted scoring; see `classify`). Capability over length. ---

/// Explicit user hint that forces Complex regardless of anything else (ADR-0006: user hints).
const COMPLEX_HINTS: &[&str] = &[
    "think hard",
    "think deeply",
    "ultrathink",
    "think carefully",
    "step by step",
    "in depth",
    "in-depth",
    "deep dive",
    "comprehensive",
    "thorough",
];
/// Explicit "this is easy" hints — a strong pull toward Trivial (-5 pts).
const TRIVIAL_HINTS: &[&str] = &[
    "quick",
    "simple",
    "one-liner",
    "one liner",
    "minor",
    "briefly",
    "small fix",
    "small change",
];
/// Reasoning / algorithmic / architectural terms — cognitive load, not length. A single one
/// carries a short prompt to Complex (+5 pts, threshold is 5).
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
    "investigate",
    "audit",
    "diagnose",
    "evaluate",
    "vulnerabilit",
    "memory leak",
];
/// Medium-weight analytical signals (+3 pts each). A single term lifts to Standard; two
/// or one + another signal reach Complex.
const ANALYSIS_TERMS: &[&str] = &[
    "performance",
    "security",
    "compare",
    "review",
    "bottleneck",
    "scalab",
    "understand",
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
    "port ",
    "convert ",
    "add a ",
    "write a ",
    "create a ",
    "build a ",
];
/// Trivial-edit patterns — a strong pull toward Trivial (-4 pts) regardless of length.
const TRIVIAL_PATTERNS: &[&str] = &[
    "typo",
    "rename",
    "bump version",
    "bump the version",
    "update the version",
    "change the version",
    "reformat",
    "add a comment",
    "fix import",
    "fix the import",
    "whitespace",
    "one-liner",
    "one liner",
    "delete this line",
    "remove this line",
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
    /// Live catalog of usable models (auto-discovery). When present and `mesh.auto_discover` is
    /// on, the router ranks the best discovered model per tier instead of the configured lists.
    catalog: Option<ModelCatalog>,
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
    /// Raw weighted score. ≤0 → Trivial, ≥5 → Complex, else Standard. Exposed so callers
    /// can measure confidence: a score far from both boundaries is a high-confidence call;
    /// a near-boundary score means an LLM classifier should be consulted.
    score: i32,
    reasons: Vec<&'static str>,
}

/// Prompt-derived context for model selection (beyond the tier): whether the task is code-heavy
/// (mild coding-provider prior) and a stable per-prompt seed (so genuine ties spread across
/// equally-good providers instead of always the alphabetically-first one). `Default` = a neutral
/// context for callers that have no prompt.
#[derive(Debug, Clone, Copy, Default)]
pub struct RouteHints {
    pub code_heavy: bool,
    pub seed: u64,
}

impl RouteHints {
    pub fn from_prompt(prompt: &str) -> Self {
        Self {
            code_heavy: is_code_heavy(prompt),
            seed: catalog::stable_hash(prompt),
        }
    }
}

/// Whether a prompt reads as a coding task (code fences, code tokens, or a dev-action verb) — the
/// signal behind the mild coding-provider prior.
fn is_code_heavy(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    prompt.contains("```")
        || CODE_TOKENS.iter().any(|t| lower.contains(t))
        || ACTION_VERBS.iter().any(|v| lower.contains(v))
}

/// Score a prompt's difficulty from weighted local signals (deterministic, no I/O). Capability
/// signals (reasoning terms, code, errors) can lift a *short* prompt to Complex; trivial-edit
/// patterns and "quick" hints pull it down. Length is one capped signal, never the decider —
/// this is the fix for the old length-bucket classifier.
fn score_prompt(prompt: &str) -> Classification {
    let lower = prompt.to_lowercase();

    // An explicit "think hard" hint is a hard override — the user told us it's hard.
    // score=i32::MAX signals "certain Complex" so hybrid mode never second-guesses it.
    if COMPLEX_HINTS.iter().any(|h| lower.contains(h)) {
        return Classification {
            tier: TaskTier::Complex,
            score: i32::MAX,
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
    let analysis_hits = ANALYSIS_TERMS.iter().filter(|t| lower.contains(*t)).count() as i32;
    if analysis_hits > 0 {
        pts += analysis_hits * 3;
        reasons.push("analytical signal");
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
    Classification {
        tier,
        score: pts,
        reasons,
    }
}

fn is_multistep(lower: &str) -> bool {
    lower.contains(" then ")
        || lower.contains("\n- ")
        || lower.contains("\n* ")
        || (lower.contains("1.") && lower.contains("2."))
        || (lower.contains("1)") && lower.contains("2)"))
        || lower.contains("after that")
}

impl HeuristicRouter {
    pub fn new(config: Config) -> Self {
        let pricing = pricing::Pricing::from_config(&config);
        Self {
            config,
            pin: None,
            model_available: default_model_available,
            pricing,
            catalog: None,
        }
    }

    /// Pin a model (`--model`); empty/`None` clears it.
    pub fn with_pin(mut self, pin: Option<String>) -> Self {
        self.pin = pin.filter(|s| !s.is_empty());
        self
    }

    /// Attach a discovered model catalog for auto-discovery routing (no-op when empty).
    pub fn with_catalog(mut self, catalog: ModelCatalog) -> Self {
        self.catalog = Some(catalog);
        self
    }

    /// Whether auto-discovery routing is active (enabled + a non-empty catalog attached).
    fn auto_active(&self) -> bool {
        self.config.mesh.auto_discover && self.catalog.as_ref().is_some_and(|c| !c.is_empty())
    }

    /// Candidate models for a tier: the auto-discovered, capability-ranked shortlist when
    /// [`auto_active`](Self::auto_active); otherwise the configured `[mesh.models]` candidates
    /// (the manual/override path, and the offline/no-catalog default).
    fn candidates_for_tier(
        &self,
        tier: TaskTier,
        hints: RouteHints,
        quota: &SubscriptionQuota,
    ) -> Vec<String> {
        if self.auto_active() {
            let ranked = self.catalog.as_ref().unwrap().ranked_seeded(
                tier,
                &self.pricing,
                5,
                hints.code_heavy,
                hints.seed,
                quota,
            );
            if !ranked.is_empty() {
                return ranked;
            }
        }
        self.config.candidates_for(tier)
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

    /// Like [`classify`] but also reports whether the heuristic is confident enough that an
    /// LLM second-opinion would add little value. High confidence means the score is far from
    /// both tier boundaries (≤−4 for Trivial, ≥8 for Complex) OR a COMPLEX_HINTS hard-override
    /// fired. A near-boundary score (−3…7) is "uncertain" — hybrid classifiers should call an
    /// LLM to decide. This is the hook that makes [`ClassifierKind::Hybrid`] cheap: obvious
    /// Trivial / strongly-signalled Complex skip the extra model call entirely.
    pub fn classify_confident(prompt: &str) -> (TaskTier, bool, String) {
        let c = score_prompt(prompt);
        // score == i32::MAX → COMPLEX_HINTS hard override (always confident).
        // score ≤ −4 → strong Trivial pull (TRIVIAL_PATTERNS or double TRIVIAL_HINTS).
        // score ≥ 8  → two or more strong Complex signals (REASONING_TERM + something else).
        let confident = c.score == i32::MAX || c.score <= -4 || c.score >= 8;
        (c.tier, confident, c.reasons.join(", "))
    }

    /// A model is usable if its provider key is present (or it's keyless) AND it isn't
    /// currently benched (rate-limited / unavailable — failover).
    fn is_usable(&self, m: &str, health: &ModelHealth, quota: &SubscriptionQuota) -> bool {
        (self.model_available)(m)
            && !health.is_benched(m)
            // An exhausted subscription is routed around entirely (L3), like a benched model.
            && !(is_subscription(m) && quota.is_exhausted(forge_config::provider_of(m)))
    }

    /// Pick the cheapest *usable* model from `candidates` (L1). Ranking key:
    /// `(prefer_subscription && subscription ? 0 : 1, estimated_cost, config_order)` — so a
    /// paid subscription (the $0 CLI bridges) wins when preferred, then lowest est. cost, then
    /// the order the user listed candidates. `None` when none are usable. The production path
    /// uses [`ordered_usable_for_tier`](Self::ordered_usable_for_tier); this stays for the
    /// cost-ranking unit tests.
    #[cfg(test)]
    fn cheapest_usable(&self, candidates: &[String], health: &ModelHealth) -> Option<String> {
        let quota = SubscriptionQuota::default();
        candidates
            .iter()
            .enumerate()
            .filter(|(_, m)| self.is_usable(m, health, &quota))
            .min_by(|(ia, a), (ib, b)| self.cost_rank(a).cmp(&self.cost_rank(b)).then(ia.cmp(ib)))
            .map(|(_, m)| m.clone())
    }

    /// Comparable cost ranking key for one model: `(not-preferred-subscription, est_cost)`.
    fn cost_rank(&self, m: &str) -> (u8, CostKey) {
        let prefer = self.config.mesh.prefer_subscription;
        (
            u8::from(!(prefer && is_subscription(m))),
            CostKey(self.pricing.estimated_cost(m)),
        )
    }

    /// Count usable candidates (for the rationale).
    fn usable_count(
        &self,
        candidates: &[String],
        health: &ModelHealth,
        quota: &SubscriptionQuota,
    ) -> usize {
        candidates
            .iter()
            .filter(|m| self.is_usable(m, health, quota))
            .count()
    }

    /// Usable candidates for one tier, in preference order: the auto-discovered capability
    /// ranking (cost folded in) when auto is active, else cheapest-first over the configured
    /// candidates.
    fn ordered_usable_for_tier(
        &self,
        tier: TaskTier,
        health: &ModelHealth,
        hints: RouteHints,
        quota: &SubscriptionQuota,
    ) -> Vec<String> {
        let candidates = self.candidates_for_tier(tier, hints, quota);
        let mut usable: Vec<String> = candidates
            .iter()
            .filter(|m| self.is_usable(m, health, quota))
            .cloned()
            .collect();
        if !self.auto_active() {
            // Configured path: cost-aware order (auto path keeps the ranked order verbatim).
            usable.sort_by_key(|m| self.cost_rank(m));
        }
        // Demote a near-limit subscription (Warning, L3) to the back — still a fallback, but the
        // mesh tries everything else first. Stable, so it preserves the order within each group.
        usable.sort_by_key(|m| quota.is_pressured(forge_config::provider_of(m)));
        usable
    }

    /// Build the ordered failover chain for the routed tier: that tier's usable models first,
    /// then the other tiers (Complex → Standard → Trivial) as cross-tier fallbacks, deduped.
    fn build_chain(
        &self,
        routed: TaskTier,
        health: &ModelHealth,
        hints: RouteHints,
        quota: &SubscriptionQuota,
    ) -> Vec<String> {
        let mut chain = self.ordered_usable_for_tier(routed, health, hints, quota);
        for tier in [TaskTier::Complex, TaskTier::Standard, TaskTier::Trivial] {
            if tier == routed {
                continue;
            }
            for m in self.ordered_usable_for_tier(tier, health, hints, quota) {
                if !chain.contains(&m) {
                    chain.push(m);
                }
            }
        }
        chain
    }
}

/// A `(u8, f64)`-comparable cost key. `f64` isn't `Ord`, so wrap it for use inside tuple
/// `.cmp()`; NaN (no price → treated as a stable max) can't occur here as costs are finite.
#[derive(PartialEq)]
struct CostKey(f64);
impl Eq for CostKey {}
impl PartialOrd for CostKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for CostKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
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
        health: &ModelHealth,
        hints: RouteHints,
        quota: &SubscriptionQuota,
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
            // Fallbacks even for a pin: if the pinned model is rate-limited/down mid-turn we
            // still want to keep working.
            let mut chain = self.build_chain(classified_tier, health, hints, quota);
            let model = if self.is_usable(pin, health, quota) {
                pin.clone()
            } else {
                match chain.first().cloned() {
                    Some(m) => {
                        why.push_str(&format!(" — fell back to {m} (no usable key for {pin})"));
                        m
                    }
                    None => {
                        why.push_str(&format!(
                            " — warning: no usable key for {pin} and no fallback"
                        ));
                        pin.clone()
                    }
                }
            };
            chain.retain(|m| m != &model);
            return RoutingDecision {
                tier: classified_tier,
                model,
                rationale: why,
                fallbacks: chain,
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

        // The failover chain: usable models for the routed tier first, then cross-tier picks.
        // `routed_usable` lets us tell a same-tier pick (normal rationale) from a cross-tier
        // fallback ("fell back …") for the message.
        let auto = self.auto_active();
        let routed_usable = self.ordered_usable_for_tier(tier, health, hints, quota);
        let mut chain = self.build_chain(tier, health, hints, quota);
        match chain.first().cloned() {
            Some(model) => {
                if routed_usable.contains(&model) {
                    let n = self.usable_count(
                        &self.candidates_for_tier(tier, hints, quota),
                        health,
                        quota,
                    );
                    if auto {
                        why.push_str(&format!(
                            " — auto-selected best of {n} usable {} models: {model}",
                            tier.as_str()
                        ));
                    } else if n > 1 {
                        why.push_str(&format!(
                            " — cheapest of {n} usable {} models: {model}",
                            tier.as_str()
                        ));
                    }
                } else {
                    let original = self
                        .candidates_for_tier(tier, hints, quota)
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "unknown".into());
                    why.push_str(&format!(
                        " — fell back to {model} (no usable key for {original})"
                    ));
                }
                if self.config.mesh.prefer_subscription && is_subscription(&model) {
                    why.push_str(" (paid subscription)");
                }
                chain.retain(|m| m != &model);
                RoutingDecision {
                    tier,
                    model,
                    rationale: why,
                    fallbacks: chain,
                }
            }
            None => {
                let original = self
                    .candidates_for_tier(tier, hints, quota)
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                why.push_str(&format!(
                    " — warning: no usable key for {original} and no fallback"
                ));
                RoutingDecision {
                    tier,
                    model: original,
                    rationale: why,
                    fallbacks: Vec::new(),
                }
            }
        }
    }
}

#[async_trait]
impl Router for HeuristicRouter {
    async fn route(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
    ) -> RoutingDecision {
        let (tier, reason) = Self::classify(prompt);
        self.decide(
            tier,
            reason,
            budget,
            health,
            RouteHints::from_prompt(prompt),
            quota,
        )
    }

    async fn route_hinted(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
        tier_override: Option<TaskTier>,
    ) -> RoutingDecision {
        match tier_override {
            // A command/skill tier hint replaces classification but goes through the same
            // selection path (pin, budget pressure, cost-aware candidates all still apply).
            Some(tier) => self.decide(
                tier,
                format!("tier hint: {}", tier.as_str()),
                budget,
                health,
                RouteHints::from_prompt(prompt),
                quota,
            ),
            None => self.route(prompt, budget, health, quota).await,
        }
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

    /// A realistic mixed catalog mirroring a user with claude+codex CLIs, local ollama, and
    /// keys for free-tier groq + metered gemini — the setup the routing policy targets.
    fn mixed_catalog() -> ModelCatalog {
        ModelCatalog::new(vec![
            "claude-cli::".into(),
            "claude-cli::opus".into(),
            "claude-cli::sonnet".into(),
            "claude-cli::haiku".into(),
            "codex-cli::".into(),
            "codex-cli::gpt-5.5".into(),
            "codex-cli::gpt-5.3-codex".into(),
            "codex-cli::gpt-5.4".into(),
            "codex-cli::gpt-5.4-mini".into(),
            "ollama::qwen3-coder:30b".into(),
            "ollama::llama3.2".into(),
            "groq::llama-3.1-8b-instant".into(),
            "groq::llama-3.3-70b-versatile".into(),
            "gemini::gemini-2.5-pro".into(),
            "gemini::gemini-2.5-flash".into(),
        ])
    }

    fn mixed_router() -> HeuristicRouter {
        HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(mixed_catalog())
    }

    async fn route_model(r: &HeuristicRouter, prompt: &str) -> String {
        r.route(
            prompt,
            BudgetState::default(),
            &ModelHealth::default(),
            &SubscriptionQuota::default(),
        )
        .await
        .model
    }

    async fn route_model_q(r: &HeuristicRouter, prompt: &str, q: &SubscriptionQuota) -> String {
        r.route(prompt, BudgetState::default(), &ModelHealth::default(), q)
            .await
            .model
    }

    /// A conservation-enabled quota: both subs at `frac` of their window, given plan slugs, Ok
    /// status (so we isolate proactive spreading from the hard Warning/Exhausted backstops).
    fn conserve_quota(frac: f64, plan_claude: &str, plan_codex: &str) -> SubscriptionQuota {
        let mut fr = std::collections::HashMap::new();
        fr.insert("claude-cli".to_string(), frac);
        fr.insert("codex-cli".to_string(), frac);
        let mut pl = std::collections::HashMap::new();
        pl.insert("claude-cli".to_string(), plan_claude.to_string());
        pl.insert("codex-cli".to_string(), plan_codex.to_string());
        SubscriptionQuota::new(std::collections::HashMap::new())
            .with_fractions(fr)
            .with_plans(pl)
            .with_conserve(true)
    }

    /// Distinct complex prompts (varying the seed) for measuring routing spread.
    fn complex_workload(n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                format!(
                    "prove the correctness and analyze the asymptotic complexity of this \
                     distributed consensus approach, scenario {i}"
                )
            })
            .collect()
    }

    async fn subscription_share(
        r: &HeuristicRouter,
        q: &SubscriptionQuota,
        prompts: &[String],
    ) -> usize {
        let mut sub = 0;
        for p in prompts {
            if is_subscription(&route_model_q(r, p, q).await) {
                sub += 1;
            }
        }
        sub
    }

    #[tokio::test]
    async fn conservation_spreads_some_complex_off_subscriptions_while_fresh() {
        // The core ask: even with subscriptions fresh, NOT every complex task hits the best-2
        // subscriptions — a share spreads to the free-frontier pool to preserve the plan.
        let r = mixed_router();
        let prompts = complex_workload(80);
        let q = conserve_quota(0.0, "plus", "plus");
        let sub = subscription_share(&r, &q, &prompts).await;
        let free = prompts.len() - sub;
        assert!(
            free > 0,
            "some complex tasks must spread to free frontier: free={free}"
        );
        assert!(
            sub > free,
            "but subscriptions still take the majority while fresh: sub={sub} free={free}"
        );
    }

    #[tokio::test]
    async fn conservation_grows_as_the_weekly_window_fills() {
        let r = mixed_router();
        let prompts = complex_workload(80);
        let fresh_free = prompts.len()
            - subscription_share(&r, &conserve_quota(0.0, "plus", "plus"), &prompts).await;
        let full_free = prompts.len()
            - subscription_share(&r, &conserve_quota(0.7, "plus", "plus"), &prompts).await;
        assert!(
            full_free > fresh_free,
            "more tasks must spread off subscriptions as the window fills: fresh={fresh_free} full={full_free}"
        );
    }

    #[tokio::test]
    async fn bigger_plan_is_spent_more_than_a_smaller_one() {
        // A larger plan has more headroom → conserved less → used more. (Consumes the initializer
        // subscription type.)
        let r = mixed_router();
        let prompts = complex_workload(80);
        let big =
            subscription_share(&r, &conserve_quota(0.5, "max-20x", "max-20x"), &prompts).await;
        let small = subscription_share(&r, &conserve_quota(0.5, "plus", "plus"), &prompts).await;
        assert!(
            big > small,
            "the bigger plan should be used for more complex tasks: max-20x={big} plus={small}"
        );
    }

    #[tokio::test]
    async fn conservation_disabled_keeps_the_greedy_flagship() {
        // Opt-out (config.mesh.subscription_conserve = false): old behaviour, always the flagship.
        let r = mixed_router();
        let prompts = complex_workload(40);
        let q = SubscriptionQuota::default(); // conserve = false
        let sub = subscription_share(&r, &q, &prompts).await;
        assert_eq!(
            sub,
            prompts.len(),
            "with conservation off every complex task uses a subscription"
        );
    }

    #[tokio::test]
    async fn conservation_never_drops_a_complex_task_onto_a_weak_model() {
        // Guard: when the only frontier-calibre option IS the subscription (no frontier free
        // alternative), conservation must not fire — quality wins over conservation.
        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(ModelCatalog::new(vec![
                "claude-cli::opus".into(),
                "codex-cli::gpt-5.5".into(),
                "groq::llama-3.1-8b-instant".into(), // small, NOT a frontier alternative
            ]));
        let prompts = complex_workload(30);
        let q = conserve_quota(0.7, "plus", "plus"); // high pressure + conserve on
        let sub = subscription_share(&r, &q, &prompts).await;
        assert_eq!(
            sub,
            prompts.len(),
            "no frontier alternative → keep using the subscription"
        );
    }

    #[tokio::test]
    async fn route_hinted_pins_the_given_tier_over_classification() {
        let r = mixed_router();
        // A SHORT prompt the heuristic would classify Trivial, forced Complex by a skill hint.
        let d = r
            .route_hinted(
                "fix typo",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
                Some(TaskTier::Complex),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex);
        assert!(d.rationale.contains("tier hint"));
        // A None hint behaves exactly like plain route().
        let plain = route_model(&r, "fix typo").await;
        let none_hint = r
            .route_hinted(
                "fix typo",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
                None,
            )
            .await
            .model;
        assert_eq!(plain, none_hint);
    }

    #[tokio::test]
    async fn trivial_tasks_use_a_free_model_to_preserve_subscription_quota() {
        let r = mixed_router();
        for p in [
            "fix this typo in the readme",
            "rename foo to bar",
            "format this file",
        ] {
            let m = route_model(&r, p).await;
            assert!(
                !is_subscription(&m),
                "trivial '{p}' should route to a free model, not burn subscription: got {m}"
            );
        }
    }

    #[tokio::test]
    async fn complex_tasks_use_the_subscription_flagship() {
        let r = mixed_router();
        for p in [
            "design a lock-free queue and prove it is correct",
            "refactor the auth module to use the new token store",
        ] {
            let d = r
                .route(
                    p,
                    BudgetState::default(),
                    &ModelHealth::default(),
                    &SubscriptionQuota::default(),
                )
                .await;
            assert_eq!(d.tier, TaskTier::Complex, "{p}");
            assert!(
                is_subscription(&d.model),
                "complex '{p}' should use the subscription flagship: got {}",
                d.model
            );
        }
    }

    #[tokio::test]
    async fn routing_spreads_across_providers_not_only_claude() {
        // The regression this fixes: every task went to claude-cli (alphabetical tie-break).
        let r = mixed_router();
        let prompts = [
            "fix this typo",
            "rename the variable",
            "write a function that validates an email and wire it into signup",
            "add a unit test for the parser",
            "implement a retry wrapper around the http client",
            "refactor the auth module to use the new token store",
            "design a lock-free queue and prove it is correct",
            "debug why the scheduler stalls under load",
            "optimize the hot path in the parser",
            "explain how tokio's scheduler works",
        ];
        let mut providers = std::collections::HashSet::new();
        for p in prompts {
            providers.insert(forge_config::provider_of(&route_model(&r, p).await).to_string());
        }
        // Must use more than one provider, and specifically both subscription bridges + a free one.
        assert!(
            providers.len() >= 3,
            "routing should spread across providers, got {providers:?}"
        );
        assert!(
            providers.contains("claude-cli") && providers.contains("codex-cli"),
            "both subscription bridges should be used across a workload, got {providers:?}"
        );
        assert!(
            providers
                .iter()
                .any(|p| p == "groq" || p == "ollama" || p == "gemini"),
            "a free provider should be used for the easy tasks, got {providers:?}"
        );
    }

    #[tokio::test]
    async fn code_heavy_complex_prefers_a_coding_provider() {
        let r = mixed_router();
        // A code-heavy complex task should land on a coding-tuned provider (codex/claude), not
        // a general free model, via the mild prior + complex subscription preference.
        let m = route_model(
            &r,
            "refactor the auth module and add tests for the token store",
        )
        .await;
        assert!(
            forge_config::provider_of(&m) == "codex-cli"
                || forge_config::provider_of(&m) == "claude-cli",
            "code-heavy complex should use a coding provider: got {m}"
        );
    }

    #[tokio::test]
    async fn exhausted_subscription_is_routed_around() {
        // L3: a subscription at its limit is skipped entirely, like a benched model.
        let r = mixed_router();
        let mut map = std::collections::HashMap::new();
        map.insert(
            "claude-cli".to_string(),
            forge_types::QuotaStatus::Exhausted,
        );
        map.insert("codex-cli".to_string(), forge_types::QuotaStatus::Exhausted);
        let quota = SubscriptionQuota::new(map);
        let d = r
            .route(
                "design a lock-free queue and prove it is correct",
                BudgetState::default(),
                &ModelHealth::default(),
                &quota,
            )
            .await;
        assert!(
            !is_subscription(&d.model),
            "both subs exhausted → {}",
            d.model
        );
        assert!(
            !d.fallbacks.iter().any(|m| is_subscription(m)),
            "exhausted subs absent from the chain too: {:?}",
            d.fallbacks
        );
    }

    #[tokio::test]
    async fn near_limit_subscription_is_demoted_below_alternatives() {
        // L3: a Warning subscription is still usable but ranks behind everything else.
        let r = mixed_router();
        let mut map = std::collections::HashMap::new();
        map.insert("claude-cli".to_string(), forge_types::QuotaStatus::Warning);
        map.insert("codex-cli".to_string(), forge_types::QuotaStatus::Warning);
        let quota = SubscriptionQuota::new(map);
        let d = r
            .route(
                "design a lock-free queue and prove it is correct",
                BudgetState::default(),
                &ModelHealth::default(),
                &quota,
            )
            .await;
        // Complex normally picks the subscription flagship; under quota pressure a non-subscription
        // model leads instead, with the subscription kept only as a later fallback.
        assert!(
            !is_subscription(&d.model),
            "near-limit subs demoted below alternatives: got {}",
            d.model
        );
    }

    #[tokio::test]
    async fn weekly_warning_complex_picks_the_best_other_frontier() {
        // User scenario: claude & codex ~80% weekly → a complex task uses the best OTHER
        // available FRONTIER model, not merely any non-subscription model. (80% → Warning.)
        let r = mixed_router();
        let mut map = std::collections::HashMap::new();
        map.insert("claude-cli".to_string(), forge_types::QuotaStatus::Warning);
        map.insert("codex-cli".to_string(), forge_types::QuotaStatus::Warning);
        let quota = SubscriptionQuota::new(map);
        let d = r
            .route(
                "design a lock-free queue and prove it is correct",
                BudgetState::default(),
                &ModelHealth::default(),
                &quota,
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex);
        assert!(
            !is_subscription(&d.model),
            "demoted off subscription: {}",
            d.model
        );
        assert!(
            crate::capability::is_frontier(&d.model),
            "complex under weekly pressure must still pick a FRONTIER alternative: got {}",
            d.model
        );
    }

    #[tokio::test]
    async fn fully_exhausted_routes_around_subscriptions_for_every_tier() {
        // User scenario: both subs at 100% weekly/session → use the best other available model
        // for ALL tasks, not just complex ones.
        let r = mixed_router();
        let mut map = std::collections::HashMap::new();
        map.insert(
            "claude-cli".to_string(),
            forge_types::QuotaStatus::Exhausted,
        );
        map.insert("codex-cli".to_string(), forge_types::QuotaStatus::Exhausted);
        let quota = SubscriptionQuota::new(map);
        for p in [
            "fix this typo",                                 // trivial
            "write a function that validates an email",      // standard
            "design a lock-free queue and prove it correct", // complex
        ] {
            let d = r
                .route(p, BudgetState::default(), &ModelHealth::default(), &quota)
                .await;
            assert!(
                !is_subscription(&d.model),
                "'{p}' ({:?}) must route around exhausted subs: got {}",
                d.tier,
                d.model
            );
            assert!(
                !d.fallbacks.iter().any(|m| is_subscription(m)),
                "'{p}': exhausted subs must be absent from the failover chain too: {:?}",
                d.fallbacks
            );
        }
    }

    // DIAGNOSTIC (ignored): print what the mesh routes to across a realistic catalog.
    // Run: cargo test -p forge-mesh routing_distribution_diagnostic -- --nocapture --ignored
    #[ignore]
    #[tokio::test]
    async fn routing_distribution_diagnostic() {
        let cat = ModelCatalog::new(vec![
            "claude-cli::".into(),
            "claude-cli::opus".into(),
            "claude-cli::sonnet".into(),
            "claude-cli::haiku".into(),
            "codex-cli::".into(),
            "codex-cli::gpt-5.5".into(),
            "codex-cli::gpt-5.3-codex".into(),
            "codex-cli::gpt-5.4".into(),
            "codex-cli::gpt-5.4-mini".into(),
            "ollama::qwen3-coder:30b".into(),
            "ollama::llama3.2".into(),
            "groq::llama-3.1-8b-instant".into(),
            "groq::llama-3.3-70b-versatile".into(),
            "gemini::gemini-2.5-pro".into(),
            "gemini::gemini-2.5-flash".into(),
        ]);
        let pricing = crate::pricing::Pricing::default();
        println!("\n=== ranked_for (top 6) per tier ===");
        for tier in [TaskTier::Trivial, TaskTier::Standard, TaskTier::Complex] {
            println!(
                "{:<9} {:?}",
                tier.as_str(),
                cat.ranked_for(tier, &pricing, 6)
            );
        }

        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(cat);
        let prompts = [
            "fix this typo in the readme",
            "rename the variable foo to bar",
            "format this file",
            "write a function that validates an email address and wire it into the signup handler",
            "add a unit test for the parser",
            "refactor the auth module to use the new token store",
            "design a lock-free queue and prove it is correct",
            "debug why the mesh routes everything to one provider and propose a fix",
            "explain how tokio's scheduler works",
        ];
        println!("\n=== route() per prompt ===");
        for p in prompts {
            let d = r
                .route(
                    p,
                    BudgetState::default(),
                    &ModelHealth::default(),
                    &SubscriptionQuota::default(),
                )
                .await;
            println!("[{:?}] {} -> {}", d.tier, &p[..p.len().min(46)], d.model);
        }
        println!();
    }

    #[tokio::test]
    async fn short_prompt_is_trivial() {
        let d = router()
            .route(
                "fix typo",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
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
            spent_week_usd: 0.0,
            weekly_cap_usd: None,
            spent_month_usd: 80.0,
            monthly_cap_usd: Some(80.0),
            warn_fraction: DEFAULT_WARN_FRACTION,
        };
        assert_eq!(b.status(), BudgetStatus::Exhausted);
    }

    #[tokio::test]
    async fn keyword_forces_complex() {
        let d = router()
            .route(
                "refactor the auth module",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex);
    }

    #[tokio::test]
    async fn medium_prompt_is_standard() {
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = router()
            .route(
                &prompt,
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
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
            .route(
                "refactor the whole architecture",
                budget,
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
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
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex); // AC-6
    }

    #[tokio::test]
    async fn fenced_code_is_at_least_standard_despite_short_length() {
        let d = router()
            .route(
                "```rust\nlet x=1;\n```",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Standard); // AC-5
    }

    #[tokio::test]
    async fn dev_verb_lifts_short_prompt_to_standard() {
        let d = router()
            .route(
                "integrate the parser",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Standard);
    }

    #[tokio::test]
    async fn fix_typo_stays_trivial_no_regression() {
        let d = router()
            .route(
                "fix typo",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Trivial); // AC-7
    }

    // --- New: pin / override (AC-1, AC-2) ---

    #[tokio::test]
    async fn pin_overrides_classification() {
        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_pin(Some("openai::gpt-4o".into()));
        let d = r
            .route(
                "fix typo",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
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
        let d = r
            .route(
                "design a system",
                budget,
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
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
            .route(
                "design the architecture",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
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
            .route(
                "design the architecture",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
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
        assert_eq!(
            r.cheapest_usable(&cands, &ModelHealth::default()).unwrap(),
            "openai::gpt-4o-mini"
        ); // AC-L1a
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
        assert_eq!(
            r.cheapest_usable(&cands, &ModelHealth::default()).unwrap(),
            "openai::gpt-4o-mini"
        ); // AC-L1b
    }

    #[tokio::test]
    async fn route_picks_cheapest_standard_candidate_with_rationale() {
        let c = list_config(
            "standard",
            &["deepseek::deepseek-chat", "openai::gpt-4o-mini"],
        );
        let r = HeuristicRouter::new(c).with_availability(|_| true);
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = r
            .route(
                &prompt,
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Standard);
        assert_eq!(d.model, "openai::gpt-4o-mini");
        assert!(d.rationale.contains("cheapest of 2"), "{}", d.rationale);
    }

    #[tokio::test]
    async fn auto_discovery_routes_to_the_capability_ranked_catalog_model() {
        // Auto-discovery on (default) + a catalog → the mesh ranks by capability (cost folded in),
        // NOT pure cheapest, so a Complex task picks the frontier model over a tiny free one.
        let cat = ModelCatalog::new(vec![
            "groq::llama-3.1-8b-instant".into(),
            "anthropic::claude-opus-4-8".into(),
        ]);
        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(cat);
        let prompt = "design and architect a complex concurrency refactor across modules";
        let d = r
            .route(
                prompt,
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex);
        assert_eq!(d.model, "anthropic::claude-opus-4-8", "{}", d.rationale);
        assert!(d.rationale.contains("auto-selected"), "{}", d.rationale);
    }

    #[tokio::test]
    async fn auto_discovery_trivial_prefers_the_small_fast_model() {
        let cat = ModelCatalog::new(vec![
            "groq::llama-3.1-8b-instant".into(),
            "anthropic::claude-opus-4-8".into(),
        ]);
        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(cat);
        let d = r
            .route(
                "fix typo",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Trivial);
        assert_eq!(d.model, "groq::llama-3.1-8b-instant", "{}", d.rationale);
    }

    #[tokio::test]
    async fn auto_discovery_off_uses_configured_candidates() {
        // With auto off, the catalog is ignored and the configured tier wins (manual override).
        let mut config = Config::default();
        config.mesh.auto_discover = false;
        config.mesh.models.insert(
            "complex".to_string(),
            forge_config::OneOrMany::One("openai::gpt-4o-mini".to_string()),
        );
        let r = HeuristicRouter::new(config)
            .with_availability(|_| true)
            .with_catalog(ModelCatalog::new(vec!["anthropic::claude-opus-4-8".into()]));
        let d = r
            .route(
                "design and architect a complex concurrency refactor across modules",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.model, "openai::gpt-4o-mini", "{}", d.rationale);
    }

    #[tokio::test]
    async fn legacy_single_string_tier_routes_unchanged() {
        // AC-L1c: the single-string form behaves as a one-candidate list. (Built explicitly —
        // the shipped defaults now lead each tier with free multi-candidate lists.)
        let mut c = Config::default();
        c.mesh.models.insert(
            "standard".to_string(),
            forge_config::OneOrMany::One("openai::gpt-4o-mini".to_string()),
        );
        let r = HeuristicRouter::new(c).with_availability(|_| true);
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = r
            .route(
                &prompt,
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
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
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
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
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.model, "claude-cli::");
        assert!(!d.rationale.contains("paid subscription"));
    }

    // --- Model health / failover ---

    fn benched(models: &[&str]) -> ModelHealth {
        ModelHealth::new(models.iter().map(|s| s.to_string()).collect())
    }

    #[tokio::test]
    async fn benched_model_is_skipped_and_next_best_chosen() {
        // Auto-discovery ranks opus #1 for Complex; bench it → the next usable model wins (AC-3).
        let cat = ModelCatalog::new(vec![
            "anthropic::claude-opus-4-8".into(),
            "groq::llama-3.1-8b-instant".into(),
        ]);
        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(cat);
        let prompt = "design and architect a complex concurrency refactor across modules";
        let d = r
            .route(
                prompt,
                BudgetState::default(),
                &benched(&["anthropic::claude-opus-4-8"]),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex);
        assert_ne!(
            d.model, "anthropic::claude-opus-4-8",
            "benched model must not be chosen"
        );
        assert!(
            !d.fallbacks
                .contains(&"anthropic::claude-opus-4-8".to_string()),
            "benched model must not appear as a fallback: {:?}",
            d.fallbacks
        );
    }

    #[tokio::test]
    async fn decision_carries_an_ordered_failover_chain_excluding_the_pick() {
        let cat = ModelCatalog::new(vec![
            "anthropic::claude-opus-4-8".into(),
            "groq::llama-3.1-8b-instant".into(),
        ]);
        let r = HeuristicRouter::new(Config::default())
            .with_availability(|_| true)
            .with_catalog(cat);
        let d = r
            .route(
                "design and architect a complex concurrency refactor across modules",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert!(
            !d.fallbacks.is_empty(),
            "expected a non-empty failover chain"
        );
        assert!(
            !d.fallbacks.contains(&d.model),
            "the pick must not also be a fallback"
        );
    }

    #[tokio::test]
    async fn all_benched_falls_through_to_the_no_fallback_warning() {
        // Every model benched → behaves like nothing usable (AC-6 surfaces downstream).
        let r = HeuristicRouter::new(Config::default()).with_availability(|_| true);
        let everything = HeuristicRouter::new(Config::default()).candidates_for_tier(
            TaskTier::Complex,
            RouteHints::default(),
            &SubscriptionQuota::default(),
        );
        let refs: Vec<&str> = everything.iter().map(String::as_str).collect();
        // Bench the complex candidates AND the cross-tier ones by benching all configured tiers.
        let mut all: Vec<String> = Vec::new();
        for t in [TaskTier::Complex, TaskTier::Standard, TaskTier::Trivial] {
            all.extend(HeuristicRouter::new(Config::default()).candidates_for_tier(
                t,
                RouteHints::default(),
                &SubscriptionQuota::default(),
            ));
        }
        let all_refs: Vec<&str> = all.iter().map(String::as_str).collect();
        let _ = refs; // (kept for clarity; all_refs is the superset used below)
        let d = r
            .route(
                "design the architecture",
                BudgetState::default(),
                &benched(&all_refs),
                &SubscriptionQuota::default(),
            )
            .await;
        assert!(d.fallbacks.is_empty());
        assert!(d.rationale.contains("no usable key"), "{}", d.rationale);
    }

    // --- Classification signal coverage ---

    #[test]
    fn investigation_terms_are_complex() {
        for p in [
            "investigate why the cache warms slowly",
            "audit the permission checks in the auth module",
            "diagnose the memory issue in the worker process",
            "evaluate the design of the new token store API",
            "is there a vulnerability in this authentication code",
            "there is a memory leak in the connection pool",
        ] {
            assert_eq!(
                score_prompt(p).tier,
                TaskTier::Complex,
                "expected Complex for {p:?}"
            );
        }
    }

    #[test]
    fn analysis_terms_alone_lift_to_standard_not_trivial() {
        for p in [
            "review the authentication flow",
            "check the performance of this endpoint",
            "compare these two data structures",
            "help me understand how the scheduler works",
            "is there a security issue here",
            "find the bottleneck in the rendering path",
        ] {
            let tier = score_prompt(p).tier;
            assert_ne!(
                tier,
                TaskTier::Trivial,
                "expected Standard or Complex for {p:?}, got Trivial"
            );
        }
    }

    #[test]
    fn combined_analysis_terms_reach_complex() {
        // Two ANALYSIS_TERMS signals (3+3 = 6 ≥ 5) → Complex.
        assert_eq!(
            score_prompt("there is a performance bottleneck in the hot path").tier,
            TaskTier::Complex,
            "performance + bottleneck → Complex"
        );
        // ANALYSIS_TERM + reasoning term → Complex.
        assert_eq!(
            score_prompt("review and analyze the trade-offs in this design").tier,
            TaskTier::Complex,
            "review + analyze + trade-off → Complex"
        );
        // ANALYSIS_TERM + code → 3+3 = 6 → Complex.
        assert_eq!(
            score_prompt("security review of this ```rust\nfn login() {}\n```").tier,
            TaskTier::Complex,
            "security + code → Complex"
        );
    }

    #[test]
    fn depth_hints_force_complex_regardless_of_prompt_length() {
        for p in [
            "explain this in depth",
            "give an in-depth analysis",
            "deep dive into the scheduler",
            "comprehensive review of the auth module",
            "do a thorough audit of the codebase",
        ] {
            assert_eq!(
                HeuristicRouter::classify(p).0,
                TaskTier::Complex,
                "depth hint must force Complex for {p:?}"
            );
        }
    }

    #[test]
    fn minor_qualifier_cancels_complexity_signal() {
        // "minor" (−5) cancels a REASONING_TERM (+5) → net 0 → Trivial.
        assert_eq!(
            score_prompt("minor refactor of this helper function").tier,
            TaskTier::Trivial,
            "minor + refactor: trivial qualifier must win"
        );
        // "small fix" (−5) cancels a reasoning term.
        assert_eq!(
            score_prompt("small fix for the debug output").tier,
            TaskTier::Trivial,
            "small fix + debug: trivial qualifier must win"
        );
        // "briefly" (−5) cancels "explain" (+5).
        assert_eq!(
            score_prompt("briefly explain this function").tier,
            TaskTier::Trivial,
            "briefly + explain: trivial qualifier must win"
        );
    }

    #[test]
    fn port_and_convert_are_standard_action_verbs() {
        assert_ne!(
            score_prompt("port this Python module to Rust").tier,
            TaskTier::Trivial,
            "porting is non-trivial work"
        );
        assert_ne!(
            score_prompt("convert the callback API to async").tier,
            TaskTier::Trivial,
            "conversion is non-trivial work"
        );
    }

    #[test]
    fn multistep_with_parenthesised_numbers_is_detected() {
        let p = "1) add the migration 2) update the handler 3) write tests";
        assert!(is_multistep(&p.to_lowercase()), "1) 2) format not detected");
    }

    #[test]
    fn new_trivial_patterns_stay_trivial() {
        for p in [
            "update the version to 2.0.0",
            "change the version in Cargo.toml",
            "delete this line from the config",
            "remove this line and nothing else",
        ] {
            assert_eq!(
                score_prompt(p).tier,
                TaskTier::Trivial,
                "expected Trivial for {p:?}"
            );
        }
    }
}
