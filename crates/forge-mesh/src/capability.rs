//! Transparent capability priors for ranking *discovered* models per task tier (auto-discovery
//! mesh, docs/features/auto-discovery-mesh.md). The "not hardcoded in config" requirement means
//! these priors live here in code (generic model-family heuristics), never as specific ids in a
//! user's config. A model id maps to a coarse (quality, speed) class by family substring; the
//! per-tier score weights those + cost so the router can pick the best *available* model.

use forge_types::TaskTier;

use crate::bench::BenchmarkScores;

/// Divisor that maps an Artificial Analysis index (~0–70, frontier ≈ 60) onto the same 0–3-ish
/// "quality" scale the family heuristic produced, so cost/conservation terms layered on top in the
/// catalog keep working unchanged (a ~60 index ≈ quality 3.0).
const BENCH_INDEX_DIVISOR: f64 = 20.0;

/// Coarse quality class inferred from a model id's family (0 = unknown/small … 3 = frontier).
pub(crate) fn quality_class(id: &str) -> u8 {
    let m = id.to_lowercase();
    // Explicit large-parameter counts override product-family naming conventions. A model that
    // states its size as ≥100 B is frontier-class regardless of whether "small" appears in its
    // product-line name (e.g. mistral-small-4-119b is 119 B despite "small" in the name).
    // Checked BEFORE the small-marker group so the product name does not misclassify it.
    if m.contains("-100b") || m.contains("-119b") || m.contains("-120b") || m.contains("-123b") {
        return 3;
    }
    // Small / fast FIRST: a size/speed marker (mini, haiku, -lite, -8b) downgrades even a
    // frontier-family name — `gpt-5.4-mini` and `gpt-4o-mini` are small, not frontier.
    // Use "-mini" (with dash) not "mini" to avoid matching "minimaxai/minimax-*" (large models).
    // Ollama uses colon-size notation (deepseek-r1:7b, qwen3-coder:8b) — ":Nb" variants are
    // also small-model markers even though the frontier name (deepseek-r1, qwen3-coder) matches
    // the frontier group below. The small check runs first so it wins on both separators.
    if m.contains("-8b")
        || m.contains(":8b")
        || m.contains("-7b")
        || m.contains(":7b")
        || m.contains("-3b")
        || m.contains(":3b")
        || m.contains("-1b")
        || m.contains(":1b")
        || m.contains(":1.")  // catches :1.5b, :1.6b Ollama tags
        || m.contains("-mini")
        || m.contains("nano")
        || m.contains("haiku")
        || m.contains("instant")
        || m.contains("flash-lite")
        || m.contains("-lite")
        || m.contains("small")
    {
        1
    // Frontier / large.
    } else if m.contains("opus")
        || m.contains("gpt-5")
        || m.contains("sonnet")
        || m.contains("-405b")
        || m.contains("-235b")
        || m.contains("-72b")
        || m.contains("-70b")
        || m.contains("deepseek-r1")
        || (m.contains("deepseek-v4") && !m.contains("flash"))
        || m.contains("qwen3-coder")
        || m.contains("grok-4")
    {
        3
    // Strong mid.
    } else if m.contains("gpt-4")
        || m.contains("-32b")
        || m.contains("-34b")
        || m.contains("gemini-3")
        || m.contains("gemini-2.5-pro")
        || m.contains("deepseek")
        || m.contains("large")
        || (m.contains("pro") && !m.contains("flash"))
    {
        2
    } else {
        // Unknown family — assume a capable default (e.g. `flash`, `llama3.2`, codex models).
        2
    }
}

/// Minimum Artificial Analysis intelligence index that qualifies a model as "frontier-class" for
/// the conservation guard (Complex alternative) and overview stats. Calibrated to exclude
/// nominally-large but measurably-weak older models (Llama 3.3 70B = 10.0, Hermes 405B = 9.0)
/// while including capable modern ones (DeepSeek R1 = 20.1, Gemini 2.5 Pro = 27.0).
pub(crate) const FRONTIER_BENCH_THRESHOLD: f64 = 20.0;

/// Minimum intelligence index for a "capable mid" model — used in the Standard-tier conservation
/// guard. Excludes the weakest small models (Llama 3.1 8B = 6.1, GPT-4o-mini = 6.9) while
/// retaining capable ones (Llama 3.3 70B = 10.0, GPT-4o = 12.3).
pub(crate) const CAPABLE_BENCH_THRESHOLD: f64 = 8.0;

/// Ranking demotion for models with unreliable STRUCTURED tool-calling. Forge is a tool-driven
/// harness: a model that emits tool calls as TEXT instead of structured calls is a poor pick even
/// when its raw intelligence/coding bench ranks it top. `forge-provider::tool_recovery` salvages
/// the leaked markup, but only after a wasted round-trip (and weaker models can stall outright), so
/// we'd rather route to an equally-capable peer that calls tools cleanly. The penalty is sized to
/// drop an offender BELOW a comparable tool-reliable model while keeping it in the fallback chain.
const TOOL_UNRELIABLE_PENALTY: f64 = 3.0;

/// Tool-call-reliability penalty for `id` (0.0 = clean). Evidence-based, not a capability judgement:
/// the **Gemini *flash* family** leaks function-call markup as text (`<function=…>` / `<invoke>`)
/// observed both via genai's native adapter and through OpenRouter, despite a top intelligence
/// score. Matched by name so it spans providers (`gemini::…`, `openrouter::google/gemini-…-flash`).
/// Reversible: drop the entry once the upstream tool-call parsing is fixed.
pub(crate) fn tool_reliability_penalty(id: &str) -> f64 {
    let l = id.to_lowercase();
    if l.contains("gemini") && l.contains("flash") {
        TOOL_UNRELIABLE_PENALTY
    } else {
        0.0
    }
}

/// Whether a model id reads as frontier-class — benchmark-aware when scores are available. A
/// measured intelligence index ≥ `FRONTIER_BENCH_THRESHOLD` supersedes the name heuristic, so
/// nominally-large but measurably-weak old models (Hermes 405B = 9.0) are correctly excluded while
/// unnamed but high-scoring models are correctly included. Falls back to the name heuristic when
/// no score exists for the model.
pub fn is_frontier_b(id: &str, bench: Option<&BenchmarkScores>) -> bool {
    match bench.and_then(|b| b.score_for(id)) {
        Some(s) => s.intelligence >= FRONTIER_BENCH_THRESHOLD,
        None => quality_class(id) == 3,
    }
}

/// Whether a model id reads as frontier-class (top quality prior) — used to count "frontier"
/// models in the `/models` overview. Heuristic-only; the bench-aware version is [`is_frontier_b`].
pub fn is_frontier(id: &str) -> bool {
    is_frontier_b(id, None)
}

/// Coarse speed class — roughly the inverse of size (3 = fastest small model).
pub(crate) fn speed_class(id: &str) -> u8 {
    match quality_class(id) {
        3 => 1,
        2 => 2,
        _ => 3,
    }
}

/// The pure *capability* fit of a model for a tier (higher = better), with no cost/provider terms
/// — those are layered on in the catalog's routing score so cost-tiering + spread stay in one
/// place. Trivial favours speed, Complex favours quality, Standard balances. Heuristic-only
/// (no benchmark data); thin wrapper over [`capability_score_b`]. Test-only — production paths call
/// `capability_score_b` directly so they can pass benchmark data + code-heaviness.
#[cfg(test)]
pub(crate) fn capability_score(id: &str, tier: TaskTier) -> f64 {
    capability_score_b(id, tier, false, None)
}

/// Capability fit, preferring REAL benchmark scores (ADR-0011) when available. The quality term is
/// the measured index (coding index for `code_heavy` tasks, else the general intelligence index)
/// scaled onto the heuristic's 0–3 range; speed stays a size-derived heuristic (benchmarks don't
/// rank "fast for a trivial edit"). Falls back to the family `quality_class` when the model has no
/// score, so a missing/disabled benchmark layer changes nothing.
pub(crate) fn capability_score_b(
    id: &str,
    tier: TaskTier,
    code_heavy: bool,
    bench: Option<&BenchmarkScores>,
) -> f64 {
    let (q, s) = match bench.and_then(|b| b.score_for(id)) {
        Some(score) => {
            let index = if code_heavy {
                score.coding
            } else {
                score.intelligence
            };
            (
                (index / BENCH_INDEX_DIVISOR).clamp(0.0, 4.0),
                speed_class(id) as f64,
            )
        }
        None => (quality_class(id) as f64, speed_class(id) as f64),
    };
    match tier {
        TaskTier::Trivial => s * 2.0 + q * 0.5,
        TaskTier::Standard => q + s,
        TaskTier::Complex => q * 2.0 + s * 0.25,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_reliability_penalty_flags_gemini_flash_across_providers_only() {
        // The Gemini flash family leaks tool calls as text — penalized regardless of provider prefix.
        for id in [
            "gemini::gemini-3.5-flash",
            "openrouter::google/gemini-3.5-flash",
            "gemini::gemini-2.5-flash-lite",
            "gemini::gemini-flash-latest",
        ] {
            assert!(
                tool_reliability_penalty(id) > 0.0,
                "gemini flash must be penalized: {id}"
            );
        }
        // Tool-reliable models (incl. non-flash Gemini) are not penalized.
        for id in [
            "claude-cli::sonnet",
            "openai::gpt-5.5",
            "gemini::gemini-3-pro-preview",
            "openrouter::deepseek/deepseek-v4",
            "groq::llama-3.3-70b-versatile",
        ] {
            assert_eq!(
                tool_reliability_penalty(id),
                0.0,
                "must not be penalized: {id}"
            );
        }
    }

    #[test]
    fn trivial_prefers_a_fast_small_model_over_a_frontier_one() {
        let small = capability_score("groq::llama-3.1-8b-instant", TaskTier::Trivial);
        let big = capability_score("anthropic::claude-opus-4-8", TaskTier::Trivial);
        assert!(
            small > big,
            "trivial should favour the fast/small model: {small} vs {big}"
        );
    }

    #[test]
    fn complex_prefers_a_frontier_model_over_a_tiny_one() {
        let big = capability_score("anthropic::claude-opus-4-8", TaskTier::Complex);
        let small = capability_score("groq::llama-3.1-8b-instant", TaskTier::Complex);
        assert!(
            big > small,
            "complex should favour the strong model: {big} vs {small}"
        );
    }

    #[test]
    fn mini_and_haiku_are_small_not_frontier() {
        // The reorder fix: a size marker downgrades even a frontier-family name.
        assert!(!is_frontier("codex-cli::gpt-5.4-mini"));
        assert!(!is_frontier("openai::gpt-4o-mini"));
        assert!(!is_frontier("claude-cli::haiku"));
        assert!(is_frontier("codex-cli::gpt-5.4"));
        assert!(is_frontier("claude-cli::opus"));
    }

    #[test]
    fn ollama_colon_size_tags_are_classified_as_small() {
        // Ollama uses colon separators: deepseek-r1:7b, qwen3-coder:8b, deepseek-r1:1.5b.
        // Without the ":Nb" checks these pass the small-group (-7b etc.) and hit the frontier
        // check (deepseek-r1, qwen3-coder) → quality_class=3 for a 7B distilled model.
        assert_eq!(
            quality_class("ollama::deepseek-r1:7b"),
            1,
            "distilled 7b must be small"
        );
        assert_eq!(quality_class("ollama::deepseek-r1:8b"), 1);
        assert_eq!(quality_class("ollama::deepseek-r1:1.5b"), 1);
        assert_eq!(quality_class("ollama::qwen3-coder:7b"), 1);
        assert_eq!(quality_class("ollama::qwen3-coder:8b"), 1);
        // 30B+ Ollama tags are not in the small list — they should be default/frontier.
        assert!(
            quality_class("ollama::deepseek-r1:70b") >= 2,
            "70b is not small"
        );
        assert!(
            quality_class("ollama::qwen3-coder:30b") >= 2,
            "30b is not small"
        );
    }

    #[test]
    fn deepseek_v4_flash_is_not_frontier() {
        // deepseek-v4 → quality_class=3 (frontier), but a flash variant is lighter.
        // The pro/flash guard already exists for other families; apply the same to deepseek-v4.
        assert!(
            quality_class("openrouter::deepseek/deepseek-v4") >= 3,
            "full deepseek-v4 is frontier"
        );
        assert!(
            quality_class("opencode_go::deepseek-v4-flash") < 3,
            "deepseek-v4-flash must not be frontier: {}",
            quality_class("opencode_go::deepseek-v4-flash")
        );
    }

    #[test]
    fn large_param_count_overrides_small_product_name() {
        // "mistral-small-4-119b" is 119 B — product-family "small" must NOT give it speed_class=3.
        // Same false-speed-boost bug as minimax-m3 (which matched "mini"). The -119b guard fires
        // first so these get quality_class=3 (frontier), speed_class=1.
        assert_eq!(
            quality_class("nvidia::mistralai/mistral-small-4-119b-2603"),
            3,
            "119 B model must be frontier despite 'small' in product name"
        );
        assert_eq!(quality_class("something::model-123b-instruct"), 3);
        assert_eq!(quality_class("something::model-120b"), 3);
        // Normal "small" models still downgrade.
        assert_eq!(quality_class("mistral::mistral-small-2506"), 1);
        assert_eq!(quality_class("openai::gpt-4o-mini"), 1);
    }

    #[test]
    fn minimax_is_not_classified_as_small() {
        // "minimax" contains "mini" as a substring — guard against that false match.
        // MiniMax M3 is a large frontier model; it must NOT get quality_class=1 (tiny/fast).
        assert!(quality_class("nvidia::minimaxai/minimax-m3") > 1);
        assert!(quality_class("nvidia::minimaxai/minimax-m2.7") > 1);
        // Real -mini models must still be downgraded.
        assert_eq!(quality_class("openai::gpt-4o-mini"), 1);
        assert_eq!(quality_class("codex-cli::gpt-5.4-mini"), 1);
        // trivial tier: minimax must NOT outscore a real fast model due to speed_class inflation.
        let minimax = capability_score("nvidia::minimaxai/minimax-m3", TaskTier::Trivial);
        let fast = capability_score("groq::llama-3.1-8b-instant", TaskTier::Trivial);
        assert!(
            fast >= minimax,
            "trivial: a genuinely fast small model ({fast}) should beat minimax-m3 ({minimax})"
        );
    }
}
