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
    // Small / fast FIRST: a size/speed marker (mini, haiku, -lite, -8b) downgrades even a
    // frontier-family name — `gpt-5.4-mini` and `gpt-4o-mini` are small, not frontier.
    if m.contains("-8b")
        || m.contains("-7b")
        || m.contains("-3b")
        || m.contains("-1b")
        || m.contains("mini")
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
        || m.contains("deepseek-v4")
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

/// Whether a model id reads as frontier-class (top quality prior) — used to count "frontier"
/// models in the `/models` overview. Same family heuristic the router ranks by.
pub fn is_frontier(id: &str) -> bool {
    quality_class(id) == 3
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
}
