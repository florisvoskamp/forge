//! A live catalog of usable models, discovered from the providers the user has keys for
//! (auto-discovery mesh, docs/features/auto-discovery-mesh.md). This is a plain data holder +
//! ranking; the async *discovery* (querying each provider's model list) lives in the binary
//! (forge-cli), which has the provider client — forge-mesh stays free of that dependency.

use serde::{Deserialize, Serialize};

use forge_types::{EffortLevel, TaskTier};

use crate::bench::BenchmarkScores;
use crate::capability::{capability_score_b, is_frontier_b, CAPABLE_BENCH_THRESHOLD};
use crate::pricing::Pricing;

/// Discovered `provider::model` ids the user can actually use right now.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCatalog {
    models: Vec<String>,
    /// Measured performance scores (ADR-0011), attached at discovery. When present the router ranks
    /// on real benchmark data; when absent it falls back to the family-name heuristic.
    bench: Option<BenchmarkScores>,
}

/// The provider prefix of a `provider::model` id (`"groq"` from `"groq::llama-3.1-8b"`).
pub fn provider_of(id: &str) -> &str {
    id.split("::").next().unwrap_or(id)
}

/// A $0-marginal subscription bridge (the locally-installed claude/codex CLI), as opposed to a
/// metered or genuinely-free API. Kept separate from "free" in the overview counts.
pub fn is_subscription(id: &str) -> bool {
    id.starts_with("claude-cli::") || id.starts_with("codex-cli::") || id.starts_with("agy-cli::")
}

/// Whether a model is genuinely free to call. "Free" needs *positive* evidence, not just a missing
/// price: OpenRouter is a paid gateway exposing hundreds of metered models (incl. frontier ones
/// like Claude Opus) that we hold no per-model price for — reading "unpriced" as "free" there is
/// the bug. So for OpenRouter, only its `:free`-suffixed variants count; everything else is paid.
/// OpenCode Zen (`opencode_go`) is the same trap: a curated gateway that mixes genuinely-free
/// models with premium ones (glm/kimi/qwen-max) — all billed against ONE shared key balance, none
/// priced in our table. Treating its unpriced premium models as free silently burns that balance
/// (the bug the user hit), so it's paid-by-default too; mark a known-free one via a `:free` suffix
/// or a config price of `0`. Other unpriced providers (local `ollama::`, free-tier
/// `groq`/`cerebras`) are genuinely free.
pub fn is_free(id: &str, cost: f64, subscription: bool) -> bool {
    if subscription || cost > f64::EPSILON {
        return false;
    }
    let provider = provider_of(id);
    // Custom OpenAI-compatible providers (NVIDIA NIM, SambaNova, Mistral, Cerebras, …) carry their
    // own free/paid flag in the registry — a standing free tier counts as genuinely free.
    if let Some(cp) = forge_config::custom_provider(provider) {
        if cp.free {
            return true;
        }
    }
    match provider {
        // Genuinely free: local inference, and free-tier API providers we know charge nothing.
        "ollama" | "groq" => true,
        // Gemini has a standing free tier (Google AI Studio, no card) — but only Flash / Flash-Lite
        // (and the open Gemma models); the Pro models were pulled from the free tier (Apr 2026) and
        // are paid-only. So an unpriced Gemini model is free UNLESS it's a Pro model. (Per Google's
        // Gemini API pricing + rate-limit docs.)
        "gemini" => !id.contains("pro"),
        // Paid gateways: only their explicit `:free`-suffixed variants are free.
        "openrouter" | "opencode_go" => id.contains(":free"),
        // Every other metered API provider (openai, xai, deepseek, anthropic, minimax, mimo, …) has
        // no standing free model tier — only temporary signup/trial credits — so an UNPRICED model
        // is paid-with-unknown-cost, NOT free. Reading "no price in our bundled table" as "free" was
        // the bug — it billed the user by routing to e.g. gpt-5-pro thinking it cost $0. A model
        // counts free only with positive evidence (a config price of 0, or a `:free` variant).
        _ => false,
    }
}

/// Whether a model id is a chat/text-generation model the mesh can route a turn to. Provider
/// model lists mix in non-conversational endpoints — image (`imagen`, `veo`, `lyria`, `*-image`,
/// `nano-banana`), audio/TTS (`*-tts`, `whisper`, `*-audio`), embeddings, async deep-research,
/// `computer-use`/`robotics`, and moderation/guard models. Routing to one breaks the turn (or, for
/// `deep-research`, silently picks a slow research endpoint for a trivial edit — the bug). They
/// stay visible in `forge models` but are excluded from the routing ranking.
pub fn is_routable(id: &str) -> bool {
    let m = id.to_lowercase();
    const BLOCK: &[&str] = &[
        "imagen",
        "veo",
        "lyria",
        "nano-banana",
        "image",
        "-tts",
        "tts-",
        "whisper",
        "embedding",
        "deep-research",
        "computer-use",
        "robotics",
        "guard",
        "safeguard",
        "content-safety",
        "moderation",
        "-audio",
        "audio-",
        "-ocr",
        "sora",       // video generation
        "realtime",   // realtime voice/audio sessions, not a chat-completions model
        "transcribe", // speech-to-text
        "babbage",    // legacy base-completion models (not chat)
        "davinci",
    ];
    !BLOCK.iter().any(|b| m.contains(b))
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

/// The full routing score for one model: capability fit + cost-class preference + the mild prior,
/// minus a quota penalty so a near-limit subscription drops below its alternatives (L3). The
/// penalty is applied in the SCORE (not just a post-sort) so non-subscription alternatives make it
/// into the truncated shortlist — otherwise the top picks are all the (pressured) subscription.
fn route_score(
    id: &str,
    tier: TaskTier,
    cost: f64,
    code_heavy: bool,
    quota: &forge_types::SubscriptionQuota,
    bench: Option<&BenchmarkScores>,
) -> f64 {
    let base = capability_score_b(id, tier, code_heavy, bench)
        + cost_pref(tier, cost_class(id, cost))
        + code_prior(provider_of(id), code_heavy, tier)
        - crate::capability::tool_reliability_penalty(id);
    if is_subscription(id) {
        match quota.status_for(provider_of(id)) {
            forge_types::QuotaStatus::Exhausted => return base - 100.0, // effectively last
            forge_types::QuotaStatus::Warning => return base - 5.0,     // below any plausible alt
            forge_types::QuotaStatus::Ok => {}
        }
    }
    base
}

/// Soft demotion applied to subscription models when this prompt is chosen for conservation.
/// Large enough to drop an `Ok` subscription below the best free-frontier alternative, small
/// enough that the subscription stays in the shortlist as a fallback if every alternative fails.
const CONSERVE_PENALTY: f64 = 4.0;

/// How freely a plan may be spent: a bigger plan has more headroom, so it is conserved *less*
/// (lower factor → lower spread probability). Unknown/unset plans stay neutral (1.0) — we don't
/// over-conserve a plan the user never told us about.
fn plan_factor(slug: &str) -> f64 {
    let s = slug.to_lowercase();
    if s.contains("20x") {
        0.8
    } else if s.contains("max") || s.contains("pro") {
        0.85
    } else {
        1.0 // plus / team / unknown
    }
}

/// Probability that this prompt routes OFF the subscriptions onto a free-frontier model, given the
/// tier, how full the strictest window is (`fraction`), the plan headroom, and code-heaviness.
/// Trivial always spreads (subs are never worth spending on it); Standard mostly spreads; Complex
/// spreads a minority while fresh and ramps to ~1.0 as the window approaches the 80% Warning line.
fn conserve_probability(tier: TaskTier, fraction: f64, plan: &str, code_heavy: bool) -> f64 {
    let base = match tier {
        TaskTier::Trivial => 1.0,
        TaskTier::Standard => 0.65,
        TaskTier::Complex if code_heavy => 0.15, // code-heavy complex: subscriptions earn their keep
        TaskTier::Complex => 0.30,
    };
    let ramp = (fraction / 0.80).clamp(0.0, 1.0) * (1.0 - base);
    ((base + ramp) * plan_factor(plan)).clamp(0.0, 1.0)
}

/// Whether a model qualifies as a capable alternative for `tier`, bench-aware. For Complex the
/// bar is frontier (bench ≥ `FRONTIER_BENCH_THRESHOLD`, else name-heuristic class 3); for
/// Standard it's capable mid (bench ≥ `CAPABLE_BENCH_THRESHOLD`, else class 2). This prevents
/// conservation from firing based on a nominally-large but measurably-weak old model (e.g. a
/// Hermes 405B at score 9.0 would pass the old name check but fails the bench threshold).
fn is_capable_alternative(id: &str, tier: TaskTier, bench: Option<&BenchmarkScores>) -> bool {
    match tier {
        TaskTier::Complex => is_frontier_b(id, bench),
        TaskTier::Standard => match bench.and_then(|b| b.score_for(id)) {
            Some(s) => s.intelligence >= CAPABLE_BENCH_THRESHOLD,
            None => crate::capability::quality_class(id) >= 2,
        },
        TaskTier::Trivial => true,
    }
}

/// Whether a genuine non-subscription alternative of the right calibre exists for `tier` — a
/// guard so conservation never drops a hard task onto a weak model when the only capable option
/// IS the subscription. Complex needs a frontier alternative; Standard a capable (mid+) one.
fn has_nonsub_alternative(
    models: &[String],
    tier: TaskTier,
    bench: Option<&BenchmarkScores>,
) -> bool {
    models
        .iter()
        .any(|m| !is_subscription(m) && is_routable(m) && is_capable_alternative(m, tier, bench))
}

/// One model's scored row for the routing inspector: the score broken out so a human can see WHY
/// it ranked where it did. `rotation`/`fine` are the tiebreak keys (kept for a stable, explainable
/// sort), not shown directly.
#[derive(Debug, Clone)]
pub struct ScoreRow {
    pub model: String,
    pub provider: String,
    /// Pure capability fit for the tier (speed/quality blend).
    pub capability: f64,
    /// 0 = free, 1 = subscription, 2 = paid.
    pub cost_class: u8,
    /// Conservation demotion applied to this model for this prompt (0.0 if none).
    pub conserve_penalty: f64,
    /// Final ranking score (capability + cost/code priors − quota − conservation).
    pub final_score: f64,
    pub subscription: bool,
    pub frontier: bool,
    rotation: u64,
    weight: u8,
    fine: f64,
    pub bench_score: Option<f64>,
    pub cost: f64,
    pub speed: u8,
}

/// The full, inspectable conservation decision for a prompt (the data the `/mesh` inspector and
/// `forge mesh explain` surface). `fired` is what routing acts on.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConserveDecision {
    /// Conservation enabled in config.
    pub enabled: bool,
    /// A subscription is present AND a capable non-subscription alternative exists for the tier.
    pub eligible: bool,
    /// The spread probability used (max conservation pull across the present subscriptions).
    pub probability: f64,
    /// The deterministic per-prompt draw in [0,1).
    pub roll: f64,
    /// `roll < probability` — this prompt spreads off the subscriptions.
    pub fired: bool,
}

/// Decide — deterministically for this prompt — whether to spread off the subscriptions. Takes the
/// strongest conservation pull across the present subscription providers (protect whichever is most
/// pressured / smallest-plan), then draws a stable per-prompt value against it. Does not fire when
/// disabled, when there are no subscriptions, or when no capable alternative exists.
pub(crate) fn conserve_decision(
    models: &[String],
    tier: TaskTier,
    code_heavy: bool,
    seed: u64,
    quota: &forge_types::SubscriptionQuota,
    bench: Option<&BenchmarkScores>,
) -> ConserveDecision {
    let mut d = ConserveDecision {
        enabled: quota.conserve_enabled(),
        ..Default::default()
    };
    if !d.enabled {
        return d;
    }
    let mut sub_providers: Vec<&str> = models
        .iter()
        .filter(|m| is_subscription(m))
        .map(|m| provider_of(m))
        .collect();
    sub_providers.sort_unstable();
    sub_providers.dedup();
    d.eligible = !sub_providers.is_empty() && has_nonsub_alternative(models, tier, bench);
    if !d.eligible {
        return d;
    }
    d.probability = sub_providers
        .iter()
        .map(|prov| {
            conserve_probability(
                tier,
                quota.fraction_for(prov),
                quota.plan_for(prov),
                code_heavy,
            )
        })
        .fold(0.0_f64, f64::max);
    d.roll = (stable_hash(&format!("{seed}:conserve")) % 10_000) as f64 / 10_000.0;
    d.fired = d.roll < d.probability;
    d
}

/// A per-prompt provider ordering key: hashing `seed:provider` means different prompts rotate
/// which provider wins a genuine score tie, so a workload spreads across equally-good providers
/// (claude ↔ codex) instead of always picking the alphabetically-first one — while staying fully
/// deterministic for a given prompt.
fn provider_rotation(provider: &str, seed: u64) -> u64 {
    stable_hash(&format!("{seed}:{provider}"))
}

/// How heavy a model is on its subscription (1 = light, 3 = the heavy flagship). When two models
/// tie on score — e.g. `claude-cli::opus` and `claude-cli::sonnet` both rank q3 for a complex task
/// — the mesh should spend the LIGHTER one to conserve the flagship's quota. This distinguishes
/// siblings the capability prior treats as equal (opus and sonnet are both "frontier"). It only
/// matters as a tiebreak: a genuinely weaker model (mini/haiku) already scores lower and never
/// enters the tie. Family-agnostic via name markers, so new bridges order sensibly too.
fn model_weight(id: &str) -> u8 {
    let m = id.to_lowercase();
    if m.contains("opus") || m.contains("-pro") || m.contains("-max") || m.contains("ultra") {
        3
    } else if m.contains("haiku")
        || m.contains("-mini")
        || m.contains("nano")
        || m.contains("flash")
        || m.contains("-lite")
        || m.contains("instant")
    {
        1
    } else {
        2 // sonnet, gpt-5.x, and other mid-tier flagships
    }
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
    // Cap digits actually accumulated (not just `i`'s advance) so a long digit run in a model id
    // (embedded hash, snowflake id, timestamp, ...) can't overflow the u32 accumulator; 9 digits
    // safely fits (max 999,999,999 < u32::MAX) and is far beyond any real version number.
    const MAX_DIGITS: u32 = 9;
    let mut major: u32 = 0;
    let mut major_digits = 0u32;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        if major_digits < MAX_DIGITS {
            major = major * 10 + (bytes[i] - b'0') as u32;
            major_digits += 1;
        }
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
            if (digits as u32) < MAX_DIGITS {
                minor = minor * 10 + (bytes[i] - b'0') as u32;
                digits += 1;
            }
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
    fn classify(id: &str, pricing: &Pricing, bench: Option<&BenchmarkScores>) -> Self {
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
            frontier: is_frontier_b(id, bench),
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
        Self {
            models,
            bench: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn models(&self) -> &[String] {
        &self.models
    }

    /// Attach measured benchmark scores (ADR-0011) so ranking uses real performance data. A `None`
    /// or empty set is a no-op — ranking stays on the family heuristic.
    pub fn with_benchmarks(mut self, bench: Option<BenchmarkScores>) -> Self {
        self.bench = bench.filter(|b| !b.is_empty());
        self
    }

    /// How many of the catalog's models have a benchmark score (for `forge benchmarks` coverage).
    pub fn benchmark_coverage(&self) -> (usize, usize) {
        match &self.bench {
            Some(b) => (
                self.models
                    .iter()
                    .filter(|m| b.score_for(m).is_some())
                    .count(),
                self.models.len(),
            ),
            None => (0, self.models.len()),
        }
    }

    /// The discovered models ranked best-first for `tier` (display / non-prompt callers): the
    /// cost-tiered routing score with a neutral context (not code-heavy, fixed seed). The live
    /// router uses [`ranked_seeded`](Self::ranked_seeded) so genuine ties spread across providers
    /// per prompt instead of always picking the alphabetically-first one.
    pub fn ranked_for(&self, tier: TaskTier, pricing: &Pricing, top: usize) -> Vec<String> {
        self.ranked_seeded(
            tier,
            pricing,
            top,
            false,
            0,
            &forge_types::SubscriptionQuota::default(),
            None,
        )
    }

    /// Prompt-aware ranking: cost-tiered capability score, with genuine ties broken by a
    /// per-prompt `seed` rotation across providers (fair spread) then id (stable). `code_heavy`
    /// applies the mild coding-provider prior. The single place the routing policy lives.
    #[allow(clippy::too_many_arguments)]
    pub fn ranked_seeded(
        &self,
        tier: TaskTier,
        pricing: &Pricing,
        top: usize,
        code_heavy: bool,
        seed: u64,
        quota: &forge_types::SubscriptionQuota,
        effort: Option<EffortLevel>,
    ) -> Vec<String> {
        // Proactive subscription conservation: for this prompt, decide whether to spread off the
        // subscription bridges onto a free-frontier model (so a complex/standard-heavy workload
        // doesn't exhaust the plan). When it fires, subscriptions take a soft penalty so the best
        // alternative leads while the subscription stays available as a fallback.
        let conserve = conserve_decision(
            &self.models,
            tier,
            code_heavy,
            seed,
            quota,
            self.bench.as_ref(),
        )
        .fired;

        struct ScoredModel<'a> {
            id: &'a String,
            route_score: f64,
            cost_class: u8,
            provider_rotation: u64,
            model_weight: u8,
            fine_capability: f64,
            bench_score: Option<f64>,
            cost: f64,
            speed: u8,
        }

        let mut scored: Vec<ScoredModel> = self
            .models
            .iter()
            .filter(|m| is_routable(m))
            .map(|m| {
                let cost = pricing.estimated_cost(m);
                let mut score = route_score(m, tier, cost, code_heavy, quota, self.bench.as_ref());
                if conserve && is_subscription(m) {
                    score -= CONSERVE_PENALTY;
                }
                let bench_score = self.bench.as_ref().and_then(|b| b.score_for(m)).map(|s| {
                    if code_heavy {
                        s.coding
                    } else {
                        s.intelligence
                    }
                });
                ScoredModel {
                    id: m,
                    route_score: score,
                    cost_class: cost_class(m, cost),
                    provider_rotation: provider_rotation(provider_of(m), seed),
                    model_weight: model_weight(m),
                    fine_capability: fine_capability(m),
                    bench_score,
                    cost,
                    speed: crate::capability::speed_class(m),
                }
            })
            .collect();

        let active_effort = effort.unwrap_or(EffortLevel::Medium);
        scored.sort_by(|a, b| match active_effort {
            EffortLevel::High | EffortLevel::XHigh => {
                if let (Some(sa), Some(sb)) = (a.bench_score, b.bench_score) {
                    if (sa - sb).abs() >= 1.0 {
                        return sb.total_cmp(&sa);
                    }
                }
                b.route_score
                    .total_cmp(&a.route_score)
                    .then_with(|| a.cost_class.cmp(&b.cost_class))
                    .then_with(|| a.provider_rotation.cmp(&b.provider_rotation))
                    .then_with(|| a.model_weight.cmp(&b.model_weight))
                    .then_with(|| b.fine_capability.total_cmp(&a.fine_capability))
                    .then_with(|| a.id.cmp(b.id))
            }
            EffortLevel::Low => {
                if let (Some(sa), Some(sb)) = (a.bench_score, b.bench_score) {
                    if (sa - sb).abs() >= 1.0 {
                        return sb.total_cmp(&sa);
                    }
                }
                a.cost_class
                    .cmp(&b.cost_class)
                    .then_with(|| a.cost.total_cmp(&b.cost))
                    .then_with(|| b.speed.cmp(&a.speed))
                    .then_with(|| {
                        b.route_score
                            .total_cmp(&a.route_score)
                            .then_with(|| a.provider_rotation.cmp(&b.provider_rotation))
                            .then_with(|| a.model_weight.cmp(&b.model_weight))
                            .then_with(|| b.fine_capability.total_cmp(&a.fine_capability))
                            .then_with(|| a.id.cmp(b.id))
                    })
            }
            EffortLevel::Medium => b
                .route_score
                .total_cmp(&a.route_score)
                .then_with(|| a.cost_class.cmp(&b.cost_class))
                .then_with(|| a.provider_rotation.cmp(&b.provider_rotation))
                .then_with(|| a.model_weight.cmp(&b.model_weight))
                .then_with(|| b.fine_capability.total_cmp(&a.fine_capability))
                .then_with(|| a.id.cmp(b.id)),
        });

        scored.into_iter().take(top).map(|s| s.id.clone()).collect()
    }

    /// The full ranked candidate table for a tier with each model's score broken out — the data
    /// behind `/mesh` and `forge mesh explain`. Same ordering as [`ranked_seeded`](Self::ranked_seeded),
    /// but every routable model is returned (not truncated) with its capability, cost class, the
    /// conservation penalty applied (if any), and the final score. Pure (no health/usability — the
    /// router overlays that).
    pub fn ranked_rows(
        &self,
        tier: TaskTier,
        pricing: &Pricing,
        code_heavy: bool,
        seed: u64,
        quota: &forge_types::SubscriptionQuota,
        effort: Option<EffortLevel>,
    ) -> (ConserveDecision, Vec<ScoreRow>) {
        let decision = conserve_decision(
            &self.models,
            tier,
            code_heavy,
            seed,
            quota,
            self.bench.as_ref(),
        );
        let mut rows: Vec<ScoreRow> = self
            .models
            .iter()
            .filter(|m| is_routable(m))
            .map(|m| {
                let cost = pricing.estimated_cost(m);
                let base = route_score(m, tier, cost, code_heavy, quota, self.bench.as_ref());
                let sub = is_subscription(m);
                let penalty = if decision.fired && sub {
                    CONSERVE_PENALTY
                } else {
                    0.0
                };
                let bench_score = self.bench.as_ref().and_then(|b| b.score_for(m)).map(|s| {
                    if code_heavy {
                        s.coding
                    } else {
                        s.intelligence
                    }
                });
                ScoreRow {
                    model: m.clone(),
                    provider: provider_of(m).to_string(),
                    capability: capability_score_b(m, tier, code_heavy, self.bench.as_ref()),
                    cost_class: cost_class(m, cost),
                    conserve_penalty: penalty,
                    final_score: base - penalty,
                    subscription: sub,
                    frontier: is_frontier_b(m, self.bench.as_ref()),
                    rotation: provider_rotation(provider_of(m), seed),
                    weight: model_weight(m),
                    fine: fine_capability(m),
                    bench_score,
                    cost,
                    speed: crate::capability::speed_class(m),
                }
            })
            .collect();

        let active_effort = effort.unwrap_or(EffortLevel::Medium);
        rows.sort_by(|a, b| match active_effort {
            EffortLevel::High | EffortLevel::XHigh => {
                if let (Some(sa), Some(sb)) = (a.bench_score, b.bench_score) {
                    if (sa - sb).abs() >= 1.0 {
                        return sb.total_cmp(&sa);
                    }
                }
                b.final_score
                    .total_cmp(&a.final_score)
                    .then_with(|| a.cost_class.cmp(&b.cost_class))
                    .then_with(|| a.rotation.cmp(&b.rotation))
                    .then_with(|| a.weight.cmp(&b.weight))
                    .then_with(|| b.fine.total_cmp(&a.fine))
                    .then_with(|| a.model.cmp(&b.model))
            }
            EffortLevel::Low => {
                if let (Some(sa), Some(sb)) = (a.bench_score, b.bench_score) {
                    if (sa - sb).abs() >= 1.0 {
                        return sb.total_cmp(&sa);
                    }
                }
                a.cost_class
                    .cmp(&b.cost_class)
                    .then_with(|| a.cost.total_cmp(&b.cost))
                    .then_with(|| b.speed.cmp(&a.speed))
                    .then_with(|| {
                        b.final_score
                            .total_cmp(&a.final_score)
                            .then_with(|| a.rotation.cmp(&b.rotation))
                            .then_with(|| a.weight.cmp(&b.weight))
                            .then_with(|| b.fine.total_cmp(&a.fine))
                            .then_with(|| a.model.cmp(&b.model))
                    })
            }
            EffortLevel::Medium => b
                .final_score
                .total_cmp(&a.final_score)
                .then_with(|| a.cost_class.cmp(&b.cost_class))
                .then_with(|| a.rotation.cmp(&b.rotation))
                .then_with(|| a.weight.cmp(&b.weight))
                .then_with(|| b.fine.total_cmp(&a.fine))
                .then_with(|| a.model.cmp(&b.model)),
        });
        (decision, rows)
    }

    /// The per-provider spread probability for a tier (the `/mesh` quota view) — how likely a task
    /// of this tier routes off that subscription given its window fraction + plan.
    pub fn spread_probability(tier: TaskTier, fraction: f64, plan: &str, code_heavy: bool) -> f64 {
        conserve_probability(tier, fraction, plan, code_heavy)
    }

    /// Every discovered model classified for display (id order preserved).
    pub fn infos(&self, pricing: &Pricing) -> Vec<ModelInfo> {
        self.models
            .iter()
            .map(|m| ModelInfo::classify(m, pricing, self.bench.as_ref()))
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
        // Skip bare bridge ids (`claude-cli::`, `codex-cli::`) — they are valid routing
        // aliases for the CLI's own default model but show up as confusingly empty rows.
        for info in self
            .infos(pricing)
            .into_iter()
            .filter(|m| !m.name.is_empty())
        {
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
    fn benchmark_scores_override_the_name_heuristic() {
        use crate::bench::BenchmarkScores;
        // By name heuristic, gpt-5.2 is frontier (q3) and "mystery-x" is unknown (q2) → gpt wins.
        let cat = ModelCatalog::new(vec![
            "openai::gpt-5.2".into(),
            "openrouter::acme/mystery-x".into(),
        ]);
        let plain = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 2);
        assert_eq!(
            plain[0], "openai::gpt-5.2",
            "heuristic: named frontier leads"
        );

        // Now attach REAL scores where mystery-x measures far higher than gpt-5.2 → it must lead.
        let mut b = BenchmarkScores::new();
        b.insert("gpt-5.2", 35.0, 30.0);
        b.insert("acme mystery-x", 68.0, 66.0);
        let cat = cat.with_benchmarks(Some(b));
        let ranked = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 2);
        assert_eq!(
            ranked[0], "openrouter::acme/mystery-x",
            "benchmark data must override the name heuristic: {ranked:?}"
        );
        let (covered, total) = cat.benchmark_coverage();
        assert_eq!((covered, total), (2, 2));
    }

    #[test]
    fn tool_unreliable_gemini_flash_ranks_below_a_comparable_tool_reliable_model() {
        use crate::bench::BenchmarkScores;
        // Equal top benchmark scores: without the tool-reliability penalty these would tie. The
        // Gemini *flash* model leaks tool calls as text, so it must rank BELOW the tool-reliable
        // peer for a (tool-driven) Complex task — while staying in the chain as a fallback.
        let cat = ModelCatalog::new(vec![
            "openrouter::google/gemini-3.5-flash".into(),
            "openrouter::deepseek/deepseek-v4".into(),
        ]);
        let mut b = BenchmarkScores::new();
        b.insert("google gemini-3.5-flash", 60.0, 58.0);
        b.insert("deepseek deepseek-v4", 60.0, 58.0);
        let cat = cat.with_benchmarks(Some(b));
        let r = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 2);
        assert_eq!(
            r[0], "openrouter::deepseek/deepseek-v4",
            "tool-reliable peer outranks tool-leaky gemini-flash at equal bench: {r:?}"
        );
        assert!(
            r.contains(&"openrouter::google/gemini-3.5-flash".to_string()),
            "gemini-flash stays in the chain as a fallback: {r:?}"
        );
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
    fn non_chat_models_are_excluded_from_routing() {
        // Provider lists mix in image/video/tts/embedding/deep-research endpoints. The mesh must
        // never route a turn to one — for trivial that was picking a slow deep-research model.
        assert!(!is_routable("gemini::deep-research-pro-preview-12-2025"));
        assert!(!is_routable("gemini::imagen-4.0-generate-001"));
        assert!(!is_routable("gemini::veo-3.0-generate-001"));
        assert!(!is_routable("gemini::gemini-2.5-flash-image"));
        assert!(!is_routable("gemini::gemini-embedding-001"));
        assert!(!is_routable("groq::whisper-large-v3"));
        assert!(!is_routable("groq::meta-llama/llama-prompt-guard-2-86m"));
        // OpenAI's list mixes in video / realtime-voice / speech-to-text / legacy base models too.
        assert!(!is_routable("openai::sora-2"));
        assert!(!is_routable("openai::sora-2-pro"));
        assert!(!is_routable("openai::gpt-realtime"));
        assert!(!is_routable("openai::gpt-realtime-mini"));
        assert!(!is_routable("openai::gpt-4o-transcribe"));
        assert!(!is_routable("openai::davinci-002"));
        assert!(!is_routable("openai::babbage-002"));
        assert!(is_routable("gemini::gemini-flash-lite-latest"));
        assert!(is_routable("codex-cli::gpt-5.5"));
        assert!(is_routable("groq::llama-3.1-8b-instant"));
        assert!(
            is_routable("openai::gpt-5.5"),
            "real chat model stays routable"
        );
        assert!(
            is_routable("openai::gpt-4o-search-preview"),
            "search-augmented chat stays routable"
        );

        // A trivial pick from a gemini-like set must be a fast chat model, not deep-research.
        let cat = ModelCatalog::new(vec![
            "gemini::deep-research-pro-preview-12-2025".into(),
            "gemini::gemini-flash-lite-latest".into(),
            "gemini::imagen-4.0-generate-001".into(),
        ]);
        let r = cat.ranked_for(TaskTier::Trivial, &Pricing::default(), 5);
        assert_eq!(
            r.first().unwrap(),
            "gemini::gemini-flash-lite-latest",
            "{r:?}"
        );
        assert!(!r
            .iter()
            .any(|m| m.contains("deep-research") || m.contains("imagen")));
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
            "opencode_go::glm-5.2".into(),                // PAID gateway model billing key balance
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
    fn opencode_zen_unpriced_models_are_paid_not_free() {
        // OpenCode Zen bills a shared key balance for premium models (glm/kimi/qwen-max). Reading
        // its unpriced models as free silently drains that balance — they must read as paid.
        let infos = overview_catalog().infos(&Pricing::default());
        let glm = infos
            .iter()
            .find(|m| m.id == "opencode_go::glm-5.2")
            .unwrap();
        assert!(glm.paid && !glm.free, "{glm:?}");
    }

    #[test]
    fn unpriced_metered_api_models_are_paid_not_free() {
        // The live billing bug: gpt-5.5 / gpt-5-pro / gemini-3-pro have no entry in the bundled
        // price table, so the old `_ => true` fallback read them as FREE and cost-routing would
        // bill the user. An UNPRICED model from a metered API provider must read as paid; only
        // genuinely-free providers (local/free-tier) are free without a price.
        let cat = ModelCatalog::new(vec![
            "openai::gpt-5.5".into(),
            "openai::gpt-5-pro".into(),
            "gemini::gemini-3-pro-preview".into(),
            "xai::grok-4".into(),
            "deepseek::deepseek-v4-pro".into(),
            "ollama::qwen2.5-coder:3b".into(),
        ]);
        let infos = cat.infos(&Pricing::default());
        for id in [
            "openai::gpt-5.5",
            "openai::gpt-5-pro",
            "gemini::gemini-3-pro-preview",
            "xai::grok-4",
            "deepseek::deepseek-v4-pro",
        ] {
            let m = infos.iter().find(|m| m.id == id).unwrap();
            assert!(
                m.paid && !m.free,
                "unpriced metered API model must be paid, not free: {m:?}"
            );
        }
        let local = infos.iter().find(|m| m.provider == "ollama").unwrap();
        assert!(local.free, "local ollama is genuinely free");
    }

    #[test]
    fn gemini_flash_is_free_but_pro_is_paid() {
        // Gemini keeps a standing free tier for Flash / Flash-Lite (and Gemma), but Pro is paid-only
        // since Apr 2026. Unpriced Flash → free; unpriced Pro → paid.
        let cat = ModelCatalog::new(vec![
            "gemini::gemini-3-flash-preview".into(),
            "gemini::gemini-2.5-flash-lite".into(),
            "gemini::gemini-flash-latest".into(),
            "gemini::gemini-3-pro-preview".into(),
            "gemini::gemini-pro-latest".into(),
        ]);
        let infos = cat.infos(&Pricing::default());
        for id in [
            "gemini::gemini-3-flash-preview",
            "gemini::gemini-2.5-flash-lite",
            "gemini::gemini-flash-latest",
        ] {
            let m = infos.iter().find(|m| m.id == id).unwrap();
            assert!(
                m.free && !m.paid,
                "unpriced Gemini Flash is free-tier: {m:?}"
            );
        }
        for id in ["gemini::gemini-3-pro-preview", "gemini::gemini-pro-latest"] {
            let m = infos.iter().find(|m| m.id == id).unwrap();
            assert!(m.paid && !m.free, "Gemini Pro is paid-only: {m:?}");
        }
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
        assert_eq!(s.total, 9);
        assert_eq!(s.providers, 7); // anthropic, openai, groq, ollama, claude-cli, openrouter, opencode_go
        assert_eq!(s.frontier, 4); // anthropic-opus, groq-70b, or-opus, or-deepseek-r1
        assert_eq!(s.subscription, 1); // claude-cli
        assert_eq!(s.free, 4); // groq-8b, groq-70b, ollama, or-deepseek-r1:free
        assert_eq!(s.paid, 4); // anthropic-opus, gpt-4o-mini, or-opus, opencode-glm
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
    fn on_a_score_tie_the_lighter_sibling_wins() {
        // opus and sonnet both rank q3 (frontier) for complex → identical score. The mesh should
        // spend the lighter sonnet, conserving opus' quota. (User rule: lightest-on-tie.)
        let cat = ModelCatalog::new(vec!["claude-cli::opus".into(), "claude-cli::sonnet".into()]);
        let r = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 2);
        assert_eq!(
            r[0], "claude-cli::sonnet",
            "lighter sibling leads on a tie: {r:?}"
        );

        // But a genuinely weaker sibling (haiku, lower score) must NOT jump ahead for complex.
        let cat2 = ModelCatalog::new(vec![
            "claude-cli::opus".into(),
            "claude-cli::sonnet".into(),
            "claude-cli::haiku".into(),
        ]);
        let r2 = cat2.ranked_for(TaskTier::Complex, &Pricing::default(), 3);
        assert_eq!(r2[0], "claude-cli::sonnet");
        assert_eq!(
            r2.last().unwrap(),
            "claude-cli::haiku",
            "weak sibling stays last: {r2:?}"
        );
    }

    #[test]
    fn bench_aware_conservation_guard_rejects_weak_large_models() {
        use crate::bench::BenchmarkScores;
        // Hermes 405B name-heuristic is q3 (via "-405b"), so the old guard would say "yes, capable
        // frontier alternative" and enable conservation. Bench score 9.0 is below
        // FRONTIER_BENCH_THRESHOLD (20.0), so with bench data the guard must refuse.
        let mut b = BenchmarkScores::new();
        b.insert("hermes 405b", 9.0, 8.0);
        let models = vec![
            "claude-cli::sonnet".to_string(),
            "openrouter::nousresearch/hermes-3-llama-3.1-405b".to_string(),
        ];
        let quota = forge_types::SubscriptionQuota::default()
            .with_fractions(std::collections::HashMap::from([(
                "claude-cli".to_string(),
                0.85,
            )]))
            .with_plans(std::collections::HashMap::from([(
                "claude-cli".to_string(),
                "plus".to_string(),
            )]))
            .with_conserve(true);
        let d = conserve_decision(
            &models,
            forge_types::TaskTier::Complex,
            false,
            42,
            &quota,
            Some(&b),
        );
        assert!(
            !d.eligible,
            "hermes 405B bench score 9.0 < FRONTIER_BENCH_THRESHOLD — not a frontier alternative: {d:?}"
        );
    }

    #[test]
    fn bench_aware_frontier_classification() {
        use crate::bench::BenchmarkScores;
        // A model the name heuristic misses (unknown family) but bench-scoring above
        // FRONTIER_BENCH_THRESHOLD must be classified as frontier.
        let mut b = BenchmarkScores::new();
        b.insert("acme mystery x", 55.0, 48.0);
        let cat =
            ModelCatalog::new(vec!["openrouter::acme/mystery-x".into()]).with_benchmarks(Some(b));
        let infos = cat.infos(&Pricing::default());
        assert!(
            infos[0].frontier,
            "bench 55.0 > FRONTIER_BENCH_THRESHOLD → frontier: {:?}",
            infos[0]
        );
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
    fn fine_capability_does_not_overflow_on_long_digit_runs() {
        // Regression: model ids are sourced from external provider/gateway catalogs and could
        // contain a long digit run (embedded hash, snowflake id, timestamp, ...). The accumulator
        // must not overflow `u32` (dev/test builds have `overflow-checks = true` and would panic).
        let _ = fine_capability("openrouter::model-99999999999999999999");
        let _ = fine_capability("openrouter::model-1.99999999999999999999");
        let _ = fine_capability("openrouter::model-18446744073709551616");
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

    fn effort_test_catalog(a_score: f64, b_score: f64) -> (ModelCatalog, Pricing) {
        use crate::bench::BenchmarkScores;
        use crate::pricing::ModelRate;
        use std::collections::HashMap;

        let mut rates = HashMap::new();
        rates.insert(
            "openai::model-a".to_string(),
            ModelRate {
                input_per_1k: 0.1,
                output_per_1k: 0.1,
                cache_read_per_1k: None,
            },
        );
        rates.insert(
            "openai::model-b".to_string(),
            ModelRate {
                input_per_1k: 0.001,
                output_per_1k: 0.001,
                cache_read_per_1k: None,
            },
        );

        let mut bench = BenchmarkScores::new();
        bench.insert("openai model-a", a_score, a_score);
        bench.insert("openai model-b", b_score, b_score);

        (
            ModelCatalog::new(vec!["openai::model-a".into(), "openai::model-b".into()])
                .with_benchmarks(Some(bench)),
            Pricing::from_rates(rates),
        )
    }

    #[test]
    fn none_and_medium_effort_keep_existing_routing_order() {
        use forge_types::{EffortLevel, SubscriptionQuota};

        let (cat, pricing) = effort_test_catalog(25.0, 20.0);
        let quota = SubscriptionQuota::default();
        let none = cat.ranked_seeded(TaskTier::Complex, &pricing, 2, false, 0, &quota, None);
        let medium = cat.ranked_seeded(
            TaskTier::Complex,
            &pricing,
            2,
            false,
            0,
            &quota,
            Some(EffortLevel::Medium),
        );

        assert_eq!(none, medium);
    }

    #[test]
    fn high_effort_prefers_higher_benchmark_over_lower_cost() {
        use forge_types::{EffortLevel, SubscriptionQuota};

        let (cat, pricing) = effort_test_catalog(25.0, 20.0);
        let r = cat.ranked_seeded(
            TaskTier::Complex,
            &pricing,
            2,
            false,
            0,
            &SubscriptionQuota::default(),
            Some(EffortLevel::High),
        );

        assert_eq!(r[0], "openai::model-a");
    }

    #[test]
    fn low_effort_prefers_lower_cost_when_benchmark_gap_is_small() {
        use forge_types::{EffortLevel, SubscriptionQuota};

        let (cat, pricing) = effort_test_catalog(20.5, 20.0);
        let r = cat.ranked_seeded(
            TaskTier::Complex,
            &pricing,
            2,
            false,
            0,
            &SubscriptionQuota::default(),
            Some(EffortLevel::Low),
        );

        assert_eq!(r[0], "openai::model-b");
    }

    // ── Routing scenario tests: no model should monopolise all tiers ──────────────────

    fn minimax_catalog() -> ModelCatalog {
        // A realistic NVIDIA NIM catalog: minimax-m3 (large free) vs a genuinely fast small
        // model (llama-8b on groq, also free). minimax-m3's name formerly matched the "mini"
        // small-model check, giving it speed_class=3 and making it win every tier.
        ModelCatalog::new(vec![
            "nvidia::minimaxai/minimax-m3".into(),
            "groq::llama-3.1-8b-instant".into(),
            "groq::llama-3.3-70b-versatile".into(),
        ])
    }

    #[test]
    fn minimax_m3_does_not_win_trivial_over_fast_small_model() {
        // Trivial tier heavily weights speed (s*2 + q*0.5). After the -mini fix, minimax-m3
        // is quality_class=2 (speed_class=2), while llama-8b is quality_class=1 (speed_class=3).
        // llama-8b must lead on trivial; minimax-m3 must NOT be first.
        let r = minimax_catalog().ranked_for(TaskTier::Trivial, &Pricing::default(), 3);
        assert_ne!(
            r[0], "nvidia::minimaxai/minimax-m3",
            "minimax-m3 must not win trivial over a genuinely fast small model: {r:?}"
        );
        assert_eq!(
            r[0], "groq::llama-3.1-8b-instant",
            "the fast 8b model must lead trivial: {r:?}"
        );
    }

    #[test]
    fn minimax_m3_does_not_monopolise_all_tiers_without_bench() {
        // Without benchmark data the heuristic alone must not funnel every tier to minimax.
        let cat = minimax_catalog();
        let trivial = cat.ranked_for(TaskTier::Trivial, &Pricing::default(), 1);
        let complex = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 1);
        assert_ne!(
            trivial[0], "nvidia::minimaxai/minimax-m3",
            "trivial must not go to minimax: {trivial:?}"
        );
        // Complex is fine to go to 70b or minimax — just asserting trivial spreads.
        let _ = complex;
    }

    #[test]
    fn minimax_m3_does_not_monopolise_all_tiers_with_high_bench_score() {
        use crate::bench::BenchmarkScores;
        // Even with a high AA intelligence score (35, a real value for MiniMax M3), the
        // trivial tier must still prefer the genuinely fast small model.
        let mut b = BenchmarkScores::new();
        b.insert("minimax m3", 35.0, 33.0);
        b.insert("llama 3.1 8b instant", 6.1, 5.0);
        b.insert("llama 3.3 70b versatile", 10.0, 9.0);
        let cat = minimax_catalog().with_benchmarks(Some(b));

        let trivial = cat.ranked_for(TaskTier::Trivial, &Pricing::default(), 3);
        assert_ne!(
            trivial[0], "nvidia::minimaxai/minimax-m3",
            "even with high bench score minimax must not dominate trivial: {trivial:?}"
        );

        // Complex: with bench 35 vs 10, minimax is the strongest available — that IS correct.
        let complex = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 3);
        assert_ne!(
            complex[0], "groq::llama-3.1-8b-instant",
            "tiny 8b must not win complex: {complex:?}"
        );
    }

    #[test]
    fn fast_small_model_leads_trivial_across_provider_mix() {
        // Realistic multi-provider set: ensure a tiny fast free model beats large frontier
        // on trivial regardless of how many large models are present.
        let cat = ModelCatalog::new(vec![
            "nvidia::minimaxai/minimax-m3".into(),
            "nvidia::meta/llama-3.1-70b-instruct".into(),
            "groq::llama-3.1-8b-instant".into(),
            "claude-cli::opus".into(),
            "openrouter::deepseek/deepseek-r1:free".into(),
        ]);
        let r = cat.ranked_for(TaskTier::Trivial, &Pricing::default(), 5);
        assert_eq!(
            r[0], "groq::llama-3.1-8b-instant",
            "fast free 8b must win trivial in a mixed catalog: {r:?}"
        );
    }

    #[test]
    fn no_provider_monopolises_all_three_tiers_in_realistic_catalog() {
        // With a balanced catalog, the routing should spread across tiers: no single model
        // wins trivial + standard + complex simultaneously (healthy tier differentiation).
        use crate::bench::BenchmarkScores;
        let cat = ModelCatalog::new(vec![
            "nvidia::minimaxai/minimax-m3".into(),
            "groq::llama-3.1-8b-instant".into(),
            "groq::llama-3.3-70b-versatile".into(),
            "claude-cli::sonnet".into(),
        ]);
        let mut b = BenchmarkScores::new();
        b.insert("minimax m3", 35.0, 33.0);
        b.insert("llama 3.1 8b instant", 6.1, 5.0);
        b.insert("llama 3.3 70b versatile", 10.0, 9.0);
        let cat = cat.with_benchmarks(Some(b));

        let trivial = cat.ranked_for(TaskTier::Trivial, &Pricing::default(), 1);
        let complex = cat.ranked_for(TaskTier::Complex, &Pricing::default(), 1);

        assert_ne!(
            trivial[0], complex[0],
            "trivial and complex must not route to the same model — healthy tier spread expected: trivial={:?} complex={:?}",
            trivial, complex
        );
        assert_eq!(
            trivial[0], "groq::llama-3.1-8b-instant",
            "trivial must pick the fast 8b: {trivial:?}"
        );
    }
}
