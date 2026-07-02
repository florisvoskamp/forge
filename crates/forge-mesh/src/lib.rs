//! The Model Mesh (ADR-0006): classify a task, then route it to the cheapest configured
//! model that can handle it — adjusting for the remaining budget. Routing is deterministic
//! and adds no model calls. The [`Router`] trait keeps a smarter (e.g. LLM-based)
//! classifier pluggable later without changing callers.

use async_trait::async_trait;
use forge_config::Config;
use forge_types::{EffortLevel, ModelHealth, ProjectContext, SubscriptionQuota, TaskTier};

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
    /// Minimum context window (in tokens) required for the selected model. When set, models whose
    /// known window is smaller than this value are skipped during routing.
    pub min_context_tokens: Option<u32>,
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
            min_context_tokens: None,
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
    #[allow(clippy::too_many_arguments)]
    async fn route(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
        effort: Option<EffortLevel>,
        project: &ProjectContext,
    ) -> RoutingDecision;

    /// Route with an optional tier hint from an invoked command/skill (`tier:` frontmatter).
    /// The default ignores the hint and delegates to [`Router::route`]; classifying routers
    /// override this to pin the tier (an explicit user `--model` pin still wins, handled in
    /// `decide`). A `None` hint is exactly today's behaviour.
    #[allow(clippy::too_many_arguments)]
    async fn route_hinted(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
        _tier_override: Option<TaskTier>,
        effort: Option<EffortLevel>,
        project: &ProjectContext,
    ) -> RoutingDecision {
        self.route(prompt, budget, health, quota, effort, project)
            .await
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
    // Hyphenated form — a plain-string list can't normalize "step-by-step" to "step by step"
    // without extra machinery, so both spellings are listed explicitly (matches the existing
    // "in depth"/"in-depth" pair below).
    "step-by-step",
    "in depth",
    "in-depth",
    "deep dive",
    "comprehensive",
    "thorough",
    "think it through",
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
    // Planning/proposal work is inherently a reasoning task — producing a good plan REQUIRES
    // weighing approaches, not mechanically executing one. Catches the reported failure
    // ("produce a step-by-step plan...") even without any other strong signal present.
    "plan",
    "propose",
    "restructure",
];
/// Core-infrastructure vocabulary — only a complexity signal when [`ProjectContext::is_self_hosting`]
/// is true. In any OTHER project these are just ordinary words with no special stakes (a project
/// with its own unrelated "router" module shouldn't get an unearned complexity bump); but when the
/// agent is genuinely working on its own source, a task touching its own routing/classification
/// logic carries real, wide-blast-radius stakes the raw prompt text alone can't convey.
const SELF_HOSTING_INFRA_TERMS: &[&str] = &[
    "mesh",
    "router",
    "routing",
    "classifier",
    "classification",
    "task tier",
    "model selection",
    "provider adapter",
    "harness",
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
    "improve",
    "valida",
    "wire ",
    "port ",
    "convert ",
    "add a ",
    "write a ",
    "create a ",
    "build a ",
];
/// Trivial-edit patterns — a strong pull toward Trivial (-8 pts) regardless of length.
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
    /// Known context-window sizes (model id → token count). Used to filter out models that
    /// cannot fit the current transcript during routing.
    context_windows: std::collections::HashMap<String, u32>,
}

fn default_model_available(model: &str) -> bool {
    forge_config::has_api_key(forge_config::provider_of(model))
}

/// A model billed to an already-paid subscription (the CLI bridges) — $0 marginal cost.
fn is_subscription(model: &str) -> bool {
    matches!(
        forge_config::provider_of(model),
        "claude-cli" | "codex-cli" | "agy-cli"
    )
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

/// Scale the minimum required context window by the active effort level. HIGH effort inflates it
/// by 1.5×, XHIGH by 2×. No adjustment for Low/Medium or when no minimum is set.
fn effective_min_context(min_tokens: Option<u32>, effort: Option<EffortLevel>) -> Option<u32> {
    min_tokens.map(|t| match effort {
        Some(EffortLevel::High) => t.saturating_mul(3) / 2,
        Some(EffortLevel::XHigh) | Some(EffortLevel::WhiteHot) => t.saturating_mul(2),
        _ => t,
    })
}

/// Whether `needle` occurs in `haystack` starting at a word boundary — i.e. not immediately
/// preceded by an alphanumeric character. Plain `str::contains` lets short verbs like "port "
/// match inside unrelated words that happen to end the same way (e.g. "port " inside "report ",
/// "export "), so ACTION_VERBS and other short-verb checks must use this instead.
fn contains_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut search_start = 0usize;
    while let Some(rel_idx) = haystack[search_start..].find(needle) {
        let abs_idx = search_start + rel_idx;
        let preceded_by_alnum = haystack[..abs_idx]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric());
        if !preceded_by_alnum {
            return true;
        }
        search_start = abs_idx + needle.len();
    }
    false
}

/// Whole-word match: `needle` must not be immediately preceded OR followed by an alphanumeric
/// character. Stricter than `contains_word_boundary` (which only checks the leading side) —
/// needed for single ambiguous words like "rename" that legitimately appear as a substring of an
/// unrelated word ("a script that renames files" describes what the script DOES, not an
/// instruction to rename something — `contains_word_boundary` alone still matches it since
/// nothing precedes "rename" inside "renames" at that position other than a non-alnum boundary).
fn contains_whole_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut search_start = 0usize;
    while let Some(rel_idx) = haystack[search_start..].find(needle) {
        let abs_idx = search_start + rel_idx;
        let before_ok = haystack[..abs_idx]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_idx = abs_idx + needle.len();
        let after_ok = haystack[after_idx..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        search_start = abs_idx + needle.len();
    }
    false
}

/// Whether a prompt reads as a coding task (code fences, code tokens, or a dev-action verb) — the
/// signal behind the mild coding-provider prior.
fn is_code_heavy(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    prompt.contains("```")
        || CODE_TOKENS.iter().any(|t| lower.contains(t))
        || ACTION_VERBS
            .iter()
            .any(|v| contains_word_boundary(&lower, v))
}

/// Score a prompt's difficulty from weighted local signals (deterministic, no I/O of its own —
/// `project` is computed once per session by the caller). Capability signals (reasoning terms,
/// code, errors) can lift a *short* prompt to Complex; trivial-edit patterns and "quick" hints
/// pull it down. Length is one capped signal, never the decider — this is the fix for the old
/// length-bucket classifier.
fn score_prompt(prompt: &str, project: &ProjectContext) -> Classification {
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
    if ACTION_VERBS
        .iter()
        .any(|v| contains_word_boundary(&lower, v))
    {
        pts += 2;
        reasons.push("dev-action verb");
    }
    if is_multistep(&lower) {
        pts += 2;
        reasons.push("multi-step scope");
    }
    if contains_word_boundary(&lower, "test")
        || lower.contains("benchmark")
        || lower.contains("edge case")
    {
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
    if project.is_self_hosting && SELF_HOSTING_INFRA_TERMS.iter().any(|t| lower.contains(t)) {
        pts += 5;
        reasons.push("self-hosting: touches this agent's own core routing/infra");
    }

    // Trivial pulls (strong, regardless of length).
    if TRIVIAL_HINTS.iter().any(|h| lower.contains(h)) {
        pts -= 5;
        reasons.push("explicit 'quick' hint");
    }
    if TRIVIAL_PATTERNS
        .iter()
        .any(|p| contains_whole_word(&lower, p))
    {
        // -8, not -4: an explicit trivial-edit pattern is a strong, deliberate signal (the user
        // is describing a mechanical single-file edit) and should reliably win over ONE weak
        // REASONING_TERMS hit from a word that's ambiguous outside its own context — e.g. "add a
        // comment EXPLAINING this function" trips "explain" (+5, normally a strong Complex
        // signal) despite the task itself being exactly what TRIVIAL_PATTERNS's "add a comment"
        // describes. -4 left that case net-positive (Standard); -8 does not.
        pts -= 8;
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
            context_windows: std::collections::HashMap::new(),
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

    /// Attach known context-window sizes so the router can skip models that can't fit the
    /// current transcript.
    pub fn with_context_windows(mut self, windows: std::collections::HashMap<String, u32>) -> Self {
        self.context_windows = windows;
        self
    }

    /// Returns `true` when `model`'s known context window comfortably exceeds `min_tokens`.
    /// Models with no recorded window are assumed to fit (fail-open).
    fn context_fits(&self, model: &str, min_tokens: Option<u32>) -> bool {
        let Some(min) = min_tokens else {
            return true;
        };
        self.context_windows.get(model).is_none_or(|&w| w > min)
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
        effort: Option<EffortLevel>,
    ) -> Vec<String> {
        if self.auto_active() {
            // Rank EVERY routable discovered model (not a top-N): the result feeds the failover
            // chain, and the mesh must keep trying down the full list rather than give up after a
            // handful when tens of usable free models remain. The primary pick is still the
            // first usable entry, so a longer tail never changes selection — it only deepens
            // failover. (The bug: a top-5 cap meant ~6 unique models across tiers, so a few dead
            // providers exhausted the chain while most of the catalog went untried.)
            let catalog = self.catalog.as_ref().unwrap();
            let ranked = catalog.ranked_seeded(
                tier,
                &self.pricing,
                catalog.models().len(),
                hints.code_heavy,
                hints.seed,
                quota,
                effort,
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

    fn classify(prompt: &str, project: &ProjectContext) -> (TaskTier, String) {
        let c = score_prompt(prompt, project);
        (c.tier, c.reasons.join(", "))
    }

    /// Like [`classify`] but also reports whether the heuristic is confident enough that an
    /// LLM second-opinion would add little value. High confidence means the score is far from
    /// both tier boundaries (≤−4 for Trivial, ≥8 for Complex) OR a COMPLEX_HINTS hard-override
    /// fired. A near-boundary score (−3…7) is "uncertain" — hybrid classifiers should call an
    /// LLM to decide. This is the hook that makes [`ClassifierKind::Hybrid`] cheap: obvious
    /// Trivial / strongly-signalled Complex skip the extra model call entirely.
    pub fn classify_confident(prompt: &str, project: &ProjectContext) -> (TaskTier, bool, String) {
        let c = score_prompt(prompt, project);
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

    /// Whether `m` may be auto-routed / failed-over to under the active credit mode. `Strict` means
    /// "free + subscription only" (the doc contract): a paid, metered model is dropped from the
    /// candidate set so neither the primary pick nor the failover chain can ever spend API credit
    /// without the user asking. Normal/Frugal impose no model restriction (Frugal is a token cap).
    /// This gates AUTO routing only — an explicit `--model` pin bypasses it (the pin path checks
    /// [`is_usable`] directly), so a deliberate paid pin still works.
    fn allowed_under_credit_mode(&self, m: &str) -> bool {
        if self.config.mesh.credit_mode != forge_types::CreditMode::Strict {
            return true;
        }
        is_subscription(m) || catalog::is_free(m, self.pricing.estimated_cost(m), false)
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

    /// Usable candidates for one tier, in preference order: the auto-discovered capability
    /// ranking (cost folded in) when auto is active, else cheapest-first over the configured
    /// candidates.
    fn ordered_usable_for_tier(
        &self,
        tier: TaskTier,
        health: &ModelHealth,
        hints: RouteHints,
        quota: &SubscriptionQuota,
        effort: Option<EffortLevel>,
        min_context: Option<u32>,
    ) -> Vec<String> {
        let candidates = self.candidates_for_tier(tier, hints, quota, effort);
        let min = effective_min_context(min_context, effort);
        let mut usable: Vec<String> = candidates
            .iter()
            .filter(|m| self.is_usable(m, health, quota))
            .filter(|m| self.allowed_under_credit_mode(m))
            .filter(|m| self.context_fits(m, min))
            .cloned()
            .collect();
        if !self.auto_active() {
            // Configured path: cost-aware order (auto path keeps the ranked order verbatim).
            usable.sort_by_key(|m| self.cost_rank(m));
        }
        // Demote a near-limit subscription (Warning, L3) to the back — still a fallback, but the
        // mesh tries everything else first. Stable, so it preserves the order within each group.
        usable.sort_by_key(|m| quota.is_pressured(forge_config::provider_of(m)));
        // Failover follows the mesh ranking verbatim: the Nth model Forge tries is the Nth-best
        // ranked model, not the top model of the Nth provider. (A previous round-robin interleave
        // destroyed cross-provider rank order — e.g. it sent release work to a low-ranked free
        // model after a higher-ranked provider's first model failed.) Rate-limit storms are
        // handled lazily downstream instead: forge-core skips a provider's *remaining* chain
        // entries only after one of its models actually returns a rate-limit error, so rank order
        // is preserved for every other failure mode.
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
        effort: Option<EffortLevel>,
        min_context: Option<u32>,
    ) -> Vec<String> {
        let mut chain =
            self.ordered_usable_for_tier(routed, health, hints, quota, effort, min_context);
        for tier in [TaskTier::Complex, TaskTier::Standard, TaskTier::Trivial] {
            if tier == routed {
                continue;
            }
            for m in self.ordered_usable_for_tier(tier, health, hints, quota, effort, min_context) {
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
    #[allow(clippy::too_many_arguments)]
    pub fn decide(
        &self,
        classified_tier: TaskTier,
        classify_reason: String,
        budget: BudgetState,
        health: &ModelHealth,
        hints: RouteHints,
        quota: &SubscriptionQuota,
        effort: Option<EffortLevel>,
    ) -> RoutingDecision {
        let exhausted = budget.status() == BudgetStatus::Exhausted;
        let bg_override_pin = self.config.mesh.budget.cap_overrides_pin;
        let min_context = budget.min_context_tokens;

        // A pin bypasses classification unless an exhausted budget may override it.
        if let Some(pin) = self
            .pin
            .as_ref()
            .filter(|_| !(exhausted && bg_override_pin))
        {
            let mut why = "pinned via --model".to_string();
            // Fallbacks even for a pin: if the pinned model is rate-limited/down mid-turn we
            // still want to keep working.
            let mut chain =
                self.build_chain(classified_tier, health, hints, quota, effort, min_context);
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
        let routed_usable =
            self.ordered_usable_for_tier(tier, health, hints, quota, effort, min_context);
        let mut chain = self.build_chain(tier, health, hints, quota, effort, min_context);
        match chain.first().cloned() {
            Some(model) => {
                if routed_usable.contains(&model) {
                    // `routed_usable` (computed above) already applies the FULL routing filter
                    // (usable + credit-mode + context-fit) — reuse its count rather than
                    // `usable_count()`, which only checks `is_usable` and so overstates how many
                    // candidates `decide()` actually considered (e.g. it counts paid models even
                    // under `credit_mode = Strict`, where they're never actually routable).
                    let n = routed_usable.len();
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
                        .candidates_for_tier(tier, hints, quota, effort)
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "unknown".into());
                    // Report WHY the primary was skipped — `is_usable` has three failure modes and
                    // only one is a missing key; for a benched or quota-exhausted model the key IS
                    // present, so "no usable key" was misleading. A model can also be `is_usable`
                    // but still dropped by the separate strict-credit-mode filter (a paid/metered
                    // model policy exclusion, not a quota problem) — check that before defaulting
                    // to "quota exhausted".
                    let reason = if !(self.model_available)(&original) {
                        "no usable key"
                    } else if health.is_benched(&original) {
                        "model benched"
                    } else if !self.allowed_under_credit_mode(&original) {
                        "excluded by strict credit mode"
                    } else {
                        "quota exhausted"
                    };
                    why.push_str(&format!(
                        " — fell back to {model} ({reason} for {original})"
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
                    .candidates_for_tier(tier, hints, quota, effort)
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
        effort: Option<EffortLevel>,
        project: &ProjectContext,
    ) -> RoutingDecision {
        let (tier, reason) = Self::classify(prompt, project);
        self.decide(
            tier,
            reason,
            budget,
            health,
            RouteHints::from_prompt(prompt),
            quota,
            effort,
        )
    }

    async fn route_hinted(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
        tier_override: Option<TaskTier>,
        effort: Option<EffortLevel>,
        project: &ProjectContext,
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
                effort,
            ),
            None => {
                self.route(prompt, budget, health, quota, effort, project)
                    .await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground-truth-labeled corpus for classifier accuracy — the "prove it" mechanism for a real,
    /// live-reported failure: `score_prompt("produce a step-by-step plan to improve forge's mesh
    /// task-classification system")` scored 0 (Trivial) despite being an obviously Complex,
    /// self-referential planning task. Every entry is a realistic prompt shape actually seen or
    /// plausible in normal usage — not synthetic keyword-stuffing — labeled by what the task
    /// genuinely REQUIRES (per the classifier's own stated design principle), not by length or
    /// surface phrasing. `classifier_accuracy_meets_bar` asserts against this corpus directly, so
    /// it's both the regression guard and the numeric proof of any future change here.
    const LABELED_CORPUS: &[(&str, TaskTier)] = &[
        // --- Trivial: mechanical, single-file, no real decision-making ---
        ("fix this typo", TaskTier::Trivial),
        ("rename this variable to snake_case", TaskTier::Trivial),
        ("bump the version to 1.2.3", TaskTier::Trivial),
        ("add a comment explaining this function", TaskTier::Trivial),
        ("remove this unused import", TaskTier::Trivial),
        (
            "what does this error mean: undefined variable x",
            TaskTier::Trivial,
        ),
        ("say hi", TaskTier::Trivial),
        ("what's 2+2", TaskTier::Trivial),
        ("format this file with prettier", TaskTier::Trivial),
        ("delete this commented-out line", TaskTier::Trivial),
        ("reformat this JSON file", TaskTier::Trivial),
        ("what does HTTP 429 mean", TaskTier::Trivial),
        // --- Standard: real but bounded, single-concern changes ---
        (
            "add a retry-with-backoff wrapper around the HTTP client",
            TaskTier::Standard,
        ),
        (
            "write a unit test for the parse_config function",
            TaskTier::Standard,
        ),
        (
            "add input validation to the signup form",
            TaskTier::Standard,
        ),
        (
            "implement pagination for the /users endpoint",
            TaskTier::Standard,
        ),
        (
            "review the authentication flow for obvious issues",
            TaskTier::Standard,
        ),
        ("compare these two sorting approaches", TaskTier::Standard),
        (
            "check the performance of this endpoint under load",
            TaskTier::Standard,
        ),
        (
            "add a CLI flag to skip the confirmation prompt",
            TaskTier::Standard,
        ),
        (
            "write a script that renames all .jpeg files to .jpg",
            TaskTier::Standard,
        ),
        (
            "port this Python script to a bash script",
            TaskTier::Standard,
        ),
        // --- Complex: real design/reasoning/architectural stakes ---
        ("investigate why the cache warms slowly", TaskTier::Complex),
        (
            "audit the permission checks in the auth module",
            TaskTier::Complex,
        ),
        (
            "debug the race condition in the scheduler",
            TaskTier::Complex,
        ),
        (
            "design a plan to migrate the database to Postgres",
            TaskTier::Complex,
        ),
        ("architect a plugin system for the CLI", TaskTier::Complex),
        (
            "there is a memory leak in the connection pool, find it",
            TaskTier::Complex,
        ),
        // The exact reported failure — hyphenated "step-by-step", no other strong keyword.
        (
            "produce a step-by-step plan to improve forge's mesh task-classification system",
            TaskTier::Complex,
        ),
        (
            "produce a step-by-step plan to improve the auth module",
            TaskTier::Complex,
        ),
        (
            "come up with a plan for refactoring the billing service",
            TaskTier::Complex,
        ),
        (
            "propose an approach for making the API idempotent",
            TaskTier::Complex,
        ),
        (
            "what's the best way to restructure this module — think it through",
            TaskTier::Complex,
        ),
        (
            "evaluate whether we should switch to a different ORM",
            TaskTier::Complex,
        ),
        (
            "think hard about the tradeoffs here before answering",
            TaskTier::Complex,
        ),
        (
            "give me an in-depth review of this design",
            TaskTier::Complex,
        ),
        (
            "investigate then fix the flaky test, explaining the root cause",
            TaskTier::Complex,
        ),
        (
            "re-evaluate our current difficulty tiers and check if this is the best setup",
            TaskTier::Complex,
        ),
        (
            "dig into why the mesh keeps under-routing tasks and fix it, proven with real testing",
            TaskTier::Complex,
        ),
    ];

    #[test]
    fn classifier_accuracy_meets_bar() {
        let mut failures = Vec::new();
        for (prompt, expected) in LABELED_CORPUS {
            let got = score_prompt(prompt, &ProjectContext::default()).tier;
            if got != *expected {
                failures.push(format!("{prompt:?}: expected {expected:?}, got {got:?}"));
            }
        }
        let accuracy = 1.0 - (failures.len() as f64 / LABELED_CORPUS.len() as f64);
        assert!(
            failures.is_empty(),
            "classifier accuracy {:.1}% ({}/{} correct) — failures:\n{}",
            accuracy * 100.0,
            LABELED_CORPUS.len() - failures.len(),
            LABELED_CORPUS.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn self_hosting_escalates_infra_talk_that_would_otherwise_be_trivial() {
        // No REASONING_TERMS/ACTION_VERBS/ANALYSIS_TERMS hit here — outside a self-hosting
        // session this scores 0 (Trivial). The self-hosting signal is the ONLY thing that
        // should change the verdict, proving it actually does something rather than being
        // decorative — the same words in an unrelated project must NOT get the bump.
        let p = "look at the mesh routing code";
        assert_eq!(
            score_prompt(p, &ProjectContext::default()).tier,
            TaskTier::Trivial,
            "outside self-hosting, infra vocabulary alone must not escalate"
        );
        let self_hosting = ProjectContext {
            project_name: Some("forge-agent".to_string()),
            is_self_hosting: true,
        };
        assert_eq!(
            score_prompt(p, &self_hosting).tier,
            TaskTier::Complex,
            "self-hosting must escalate the SAME prompt"
        );
    }

    #[test]
    fn self_hosting_does_not_escalate_unrelated_infra_talk_in_a_different_project() {
        // A project that happens to use the words "mesh"/"router" for its OWN unrelated purpose
        // (is_self_hosting: false) must not get the bump just because those words appear.
        let unrelated = ProjectContext {
            project_name: Some("some-other-app".to_string()),
            is_self_hosting: false,
        };
        assert_eq!(
            score_prompt("look at the mesh routing code", &unrelated).tier,
            TaskTier::Trivial
        );
    }

    #[test]
    fn failover_chain_follows_mesh_rank_order_not_provider_interleave() {
        // The failover chain must walk models in the SAME order the mesh ranks them — the Nth
        // model tried is the Nth-best ranked model, NOT the top model of the Nth provider. (A
        // prior round-robin interleave broke this, which is how release work landed on a
        // low-ranked free model after a higher-ranked provider's first model failed over.)
        let r = mixed_router();
        let health = ModelHealth::default();
        let quota = SubscriptionQuota::default();
        let hints = RouteHints::default();
        let tier = TaskTier::Complex;

        let ranked_usable: Vec<String> = r
            .candidates_for_tier(tier, hints, &quota, None)
            .into_iter()
            .filter(|m| r.is_usable(m, &health, &quota))
            .collect();
        let chain = r.ordered_usable_for_tier(tier, &health, hints, &quota, None, None);
        assert_eq!(
            chain, ranked_usable,
            "failover order must equal mesh rank order with no provider interleaving"
        );
    }

    fn router() -> HeuristicRouter {
        // Treat every provider as available so tier-classification tests are deterministic
        // (no dependence on ambient env/keyring) and exercise no fallback.
        HeuristicRouter::new(Config::default()).with_availability(|_| true)
    }

    #[test]
    fn strict_credit_mode_excludes_paid_models_from_routing_and_failover() {
        // Regression: `credit_mode = "strict"` promises "free + subscription only", but it was wired
        // only to the token cap — paid models stayed in the failover chain, so a free pick that
        // failed over could land on a PAID model (e.g. openrouter/gemini-pro) without consent.
        let strict = {
            let mut c = Config::default();
            c.mesh.credit_mode = forge_types::CreditMode::Strict;
            HeuristicRouter::new(c)
                .with_availability(|_| true)
                .with_catalog(mixed_catalog())
        };
        let (health, quota, hints) = (
            ModelHealth::default(),
            SubscriptionQuota::default(),
            RouteHints::default(),
        );
        let chain = strict.build_chain(TaskTier::Standard, &health, hints, &quota, None, None);
        // Paid, metered model is gone from the WHOLE chain (primary + every failover step).
        assert!(
            !chain.iter().any(|m| m == "gemini::gemini-2.5-pro"),
            "strict must drop paid gemini-pro; chain = {chain:?}"
        );
        // Subscription ($0 marginal) and unpriced-free local models remain routable.
        assert!(
            chain.iter().any(|m| m == "claude-cli::sonnet"),
            "subscription stays under strict; chain = {chain:?}"
        );
        assert!(
            chain.iter().any(|m| m == "ollama::llama3.2"),
            "free local stays under strict; chain = {chain:?}"
        );

        // Control: under Normal (default) the paid model stays in the chain.
        let normal = mixed_router();
        let normal_chain =
            normal.build_chain(TaskTier::Standard, &health, hints, &quota, None, None);
        assert!(
            normal_chain.iter().any(|m| m == "gemini::gemini-2.5-pro"),
            "normal mode keeps paid models routable"
        );
    }

    #[tokio::test]
    async fn strict_credit_mode_exclusion_reports_correct_fallback_reason() {
        // Regression: a candidate that IS `is_usable` (key present, not benched, not exhausted)
        // but gets dropped only by the separate strict-credit-mode filter must not be mislabeled
        // "quota exhausted" in the fallback rationale — that's misleading since the provider
        // quota is fine; it was simply disallowed by policy.
        let mut c = Config::default();
        c.mesh.models.insert(
            TaskTier::Standard.as_str().into(),
            forge_config::OneOrMany::Many(vec!["gemini::gemini-2.5-pro".to_string()]),
        );
        c.mesh.credit_mode = forge_types::CreditMode::Strict;
        let r = HeuristicRouter::new(c).with_availability(|_| true);
        let prompt = "add a new endpoint that returns the list of users as json".repeat(2);
        let d = r
            .route(
                &prompt,
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
                None,
                &ProjectContext::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Standard);
        assert_ne!(
            d.model, "gemini::gemini-2.5-pro",
            "strict mode must not pick the paid model"
        );
        assert!(
            d.rationale.contains("excluded by strict credit mode"),
            "{}",
            d.rationale
        );
        assert!(
            !d.rationale.contains("quota exhausted"),
            "must not mislabel a credit-mode policy exclusion as quota exhaustion: {}",
            d.rationale
        );
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
            None,
            &ProjectContext::default(),
        )
        .await
        .model
    }

    async fn route_model_q(r: &HeuristicRouter, prompt: &str, q: &SubscriptionQuota) -> String {
        r.route(
            prompt,
            BudgetState::default(),
            &ModelHealth::default(),
            q,
            None,
            &ProjectContext::default(),
        )
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                    None,
                    &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                .route(
                    p,
                    BudgetState::default(),
                    &ModelHealth::default(),
                    &quota,
                    None,
                    &ProjectContext::default(),
                )
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
                    None,
                    &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Trivial);
    }

    // --- Scoring classifier: capability over length (the headline fix) ---

    #[test]
    fn hard_short_prompt_is_complex_despite_length() {
        // "design a lock-free queue" is 24 chars — the old <80 rule called this Trivial.
        assert_eq!(
            score_prompt("design a lock-free queue", &ProjectContext::default()).tier,
            TaskTier::Complex
        );
        assert_eq!(
            score_prompt("prove this sort is stable", &ProjectContext::default()).tier,
            TaskTier::Complex
        );
        assert_eq!(
            score_prompt("debug this deadlock", &ProjectContext::default()).tier,
            TaskTier::Complex
        );
    }

    #[test]
    fn trivial_edit_stays_trivial_even_with_a_path() {
        assert_eq!(
            score_prompt("rename foo to bar in utils.rs", &ProjectContext::default()).tier,
            TaskTier::Trivial
        );
        assert_eq!(
            score_prompt("fix typo", &ProjectContext::default()).tier,
            TaskTier::Trivial
        );
        assert_eq!(
            score_prompt("bump version to 1.2.0", &ProjectContext::default()).tier,
            TaskTier::Trivial
        );
    }

    #[test]
    fn action_and_multistep_is_standard_not_complex() {
        let p = "write a function that validates email addresses against the RFC rules and \
                 returns which inputs were rejected, then wire it into the signup handler";
        assert_eq!(
            score_prompt(p, &ProjectContext::default()).tier,
            TaskTier::Standard
        ); // AC-A3
    }

    #[test]
    fn long_prose_without_signals_is_not_auto_complex() {
        // Length alone is a capped nudge — 200 plain words must not force Complex.
        let p = "word ".repeat(200);
        assert_ne!(
            score_prompt(&p, &ProjectContext::default()).tier,
            TaskTier::Complex
        ); // AC-A7
    }

    #[test]
    fn every_decision_names_a_signal() {
        for p in [
            "fix typo",
            "design a lock-free queue",
            "add a logging helper module",
        ] {
            assert!(
                !score_prompt(p, &ProjectContext::default())
                    .reasons
                    .is_empty(),
                "no reason for {p:?}"
            );
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
            min_context_tokens: None,
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
                None,
                &ProjectContext::default(),
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
            None,
        );
        let refs: Vec<&str> = everything.iter().map(String::as_str).collect();
        // Bench the complex candidates AND the cross-tier ones by benching all configured tiers.
        let mut all: Vec<String> = Vec::new();
        for t in [TaskTier::Complex, TaskTier::Standard, TaskTier::Trivial] {
            all.extend(HeuristicRouter::new(Config::default()).candidates_for_tier(
                t,
                RouteHints::default(),
                &SubscriptionQuota::default(),
                None,
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
                None,
                &ProjectContext::default(),
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
                score_prompt(p, &ProjectContext::default()).tier,
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
            let tier = score_prompt(p, &ProjectContext::default()).tier;
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
            score_prompt(
                "there is a performance bottleneck in the hot path",
                &ProjectContext::default()
            )
            .tier,
            TaskTier::Complex,
            "performance + bottleneck → Complex"
        );
        // ANALYSIS_TERM + reasoning term → Complex.
        assert_eq!(
            score_prompt(
                "review and analyze the trade-offs in this design",
                &ProjectContext::default()
            )
            .tier,
            TaskTier::Complex,
            "review + analyze + trade-off → Complex"
        );
        // ANALYSIS_TERM + code → 3+3 = 6 → Complex.
        assert_eq!(
            score_prompt(
                "security review of this ```rust\nfn login() {}\n```",
                &ProjectContext::default()
            )
            .tier,
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
                HeuristicRouter::classify(p, &ProjectContext::default()).0,
                TaskTier::Complex,
                "depth hint must force Complex for {p:?}"
            );
        }
    }

    #[test]
    fn minor_qualifier_cancels_complexity_signal() {
        // "minor" (−5) cancels a REASONING_TERM (+5) → net 0 → Trivial.
        assert_eq!(
            score_prompt(
                "minor refactor of this helper function",
                &ProjectContext::default()
            )
            .tier,
            TaskTier::Trivial,
            "minor + refactor: trivial qualifier must win"
        );
        // "small fix" (−5) cancels a reasoning term.
        assert_eq!(
            score_prompt("small fix for the debug output", &ProjectContext::default()).tier,
            TaskTier::Trivial,
            "small fix + debug: trivial qualifier must win"
        );
        // "briefly" (−5) cancels "explain" (+5).
        assert_eq!(
            score_prompt("briefly explain this function", &ProjectContext::default()).tier,
            TaskTier::Trivial,
            "briefly + explain: trivial qualifier must win"
        );
    }

    #[test]
    fn port_and_convert_are_standard_action_verbs() {
        assert_ne!(
            score_prompt(
                "port this Python module to Rust",
                &ProjectContext::default()
            )
            .tier,
            TaskTier::Trivial,
            "porting is non-trivial work"
        );
        assert_ne!(
            score_prompt(
                "convert the callback API to async",
                &ProjectContext::default()
            )
            .tier,
            TaskTier::Trivial,
            "conversion is non-trivial work"
        );
    }

    #[test]
    fn report_and_export_do_not_falsely_match_the_port_action_verb() {
        // Regression: "port " (an ACTION_VERBS entry, to catch "port this module to Rust") is a
        // substring of "report " and "export ", so naive `str::contains` gave these common words
        // a spurious "dev-action verb" point and marked them code_heavy — unrelated to porting.
        assert!(
            !is_code_heavy("please generate a report for the crash"),
            "\"report\" must not match the \"port \" action verb"
        );
        assert!(
            !is_code_heavy("export the data to csv"),
            "\"export\" must not match the \"port \" action verb"
        );
        for p in [
            "please generate a report for the crash",
            "export the data to csv",
        ] {
            assert!(
                !score_prompt(p, &ProjectContext::default())
                    .reasons
                    .contains(&"dev-action verb"),
                "{p:?} must not score a dev-action verb point: {:?}",
                score_prompt(p, &ProjectContext::default()).reasons
            );
        }
    }

    #[test]
    fn latest_fastest_and_contest_do_not_falsely_match_test() {
        // Regression: `lower.contains("test")` also matched inside "latest"/"fastest"/"contest",
        // spuriously adding a "tests/edge-cases" point unrelated to actual testing.
        for p in [
            "what is the latest version of this crate",
            "pick the fastest algorithm here",
            "there was a contest about this last year",
        ] {
            assert!(
                !score_prompt(p, &ProjectContext::default())
                    .reasons
                    .contains(&"tests/edge-cases"),
                "{p:?} must not score a tests/edge-cases point: {:?}",
                score_prompt(p, &ProjectContext::default()).reasons
            );
        }
        // Control: a real "test" mention still scores the point.
        assert!(
            score_prompt("please add a test for this", &ProjectContext::default())
                .reasons
                .contains(&"tests/edge-cases")
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
                score_prompt(p, &ProjectContext::default()).tier,
                TaskTier::Trivial,
                "expected Trivial for {p:?}"
            );
        }
    }
}
