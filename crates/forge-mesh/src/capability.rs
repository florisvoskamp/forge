//! Transparent capability priors for ranking *discovered* models per task tier (auto-discovery
//! mesh, docs/features/auto-discovery-mesh.md). The "not hardcoded in config" requirement means
//! these priors live here in code (generic model-family heuristics), never as specific ids in a
//! user's config. A model id maps to a coarse (quality, speed) class by family substring; the
//! per-tier score weights those + cost so the router can pick the best *available* model.

use forge_types::TaskTier;

/// Coarse quality class inferred from a model id's family (0 = unknown/small … 3 = frontier).
fn quality_class(id: &str) -> u8 {
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
fn speed_class(id: &str) -> u8 {
    match quality_class(id) {
        3 => 1,
        2 => 2,
        _ => 3,
    }
}

/// Score a model for a tier (higher = better fit). `cost` is the estimated USD/turn (0 = free).
/// Trivial favours speed + cheapness; Complex favours quality; Standard balances. A free model
/// gets a small bonus so $0 options edge out paid ones of equal class.
pub fn tier_score(id: &str, tier: TaskTier, cost: f64) -> f64 {
    let q = quality_class(id) as f64;
    let s = speed_class(id) as f64;
    let free_bonus = if cost <= f64::EPSILON { 0.5 } else { 0.0 };
    let cost_penalty = cost * 4.0; // cents-scale; keeps cheap models ahead without dominating
    let base = match tier {
        TaskTier::Trivial => s * 2.0 + q * 0.5,
        TaskTier::Standard => q + s,
        TaskTier::Complex => q * 2.0 + s * 0.25,
    };
    base + free_bonus - cost_penalty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_prefers_a_fast_small_model_over_a_frontier_one() {
        let small = tier_score("groq::llama-3.1-8b-instant", TaskTier::Trivial, 0.0);
        let big = tier_score("anthropic::claude-opus-4-8", TaskTier::Trivial, 0.06);
        assert!(
            small > big,
            "trivial should pick the fast/cheap model: {small} vs {big}"
        );
    }

    #[test]
    fn complex_prefers_a_frontier_model_over_a_tiny_one() {
        let big = tier_score("anthropic::claude-opus-4-8", TaskTier::Complex, 0.06);
        let small = tier_score("groq::llama-3.1-8b-instant", TaskTier::Complex, 0.0);
        assert!(
            big > small,
            "complex should pick the strong model: {big} vs {small}"
        );
    }

    #[test]
    fn free_breaks_ties_within_a_class() {
        let free = tier_score("groq::llama-3.3-70b-versatile", TaskTier::Complex, 0.0);
        let paid = tier_score("openrouter::meta/llama-3.3-70b", TaskTier::Complex, 0.02);
        assert!(free > paid, "equal-class free model edges out the paid one");
    }
}
