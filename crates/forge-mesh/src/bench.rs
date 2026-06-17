//! Measured model performance scores (ADR-0011) used to rank models on REAL data rather than the
//! family-name heuristic in [`capability`]. Scores come from the Artificial Analysis Data API
//! (a 0–100 composite `intelligence` index + a `coding` index, covering closed + open models);
//! the binary fetches + caches them and attaches a [`BenchmarkScores`] to the catalog. This module
//! is pure data + the id↔model-name matching; the async fetch lives in the binary.
//!
//! Matching is the hard part: Artificial Analysis names a model "Claude 4.5 Sonnet" while Forge's
//! id is `anthropic::claude-sonnet-4-5` (and the bridges are bare, `claude-cli::opus`). We reduce
//! both to a token *set* (split on separators and letter↔digit boundaries, lowercased) so word
//! order doesn't matter, try an exact set match, then fall back to best token-overlap that shares a
//! family word. Unmatched models just fall back to the heuristic — no wrong guess is forced.

use std::collections::HashMap;

/// One model's measured performance — Artificial Analysis indices, each roughly 0–70 today.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BenchScore {
    /// Composite general-intelligence index (reasoning/knowledge/science/agentic/coding blend).
    pub intelligence: f64,
    /// Coding-specific index (LiveCodeBench/SciCode/terminal-style benches).
    pub coding: f64,
}

/// Measured performance for the models a data source knew about, matchable to Forge ids.
#[derive(Debug, Clone, Default)]
pub struct BenchmarkScores {
    /// Exact lookup by sorted-token canonical key (fast path).
    by_canon: HashMap<String, BenchScore>,
    /// All rows as (token set, score) for the overlap fallback.
    entries: Vec<(Vec<String>, BenchScore)>,
}

impl BenchmarkScores {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Record one source row under `name` (the source's model name or slug, e.g. "Claude 4.5
    /// Sonnet" or "gpt-5-2"). Later rows with the same canonical key overwrite earlier ones.
    pub fn insert(&mut self, name: &str, intelligence: f64, coding: f64) {
        let score = BenchScore {
            intelligence,
            coding,
        };
        let toks = tokens(name);
        if toks.is_empty() {
            return;
        }
        self.by_canon.insert(canon(&toks), score);
        self.entries.push((toks, score));
    }

    /// The score for a Forge `provider::model` id, or `None` if no confident match exists.
    pub fn score_for(&self, id: &str) -> Option<BenchScore> {
        if self.entries.is_empty() {
            return None;
        }
        let want = id_tokens(id);
        if want.is_empty() {
            return None;
        }
        // Fast path: identical token set.
        if let Some(s) = self.by_canon.get(&canon(&want)) {
            return Some(*s);
        }
        // Fallback: the row sharing the most tokens, requiring a shared *family* word (an
        // alphabetic token ≥3 chars) so we never match purely on a stray version number.
        let mut best: Option<(usize, f64, BenchScore)> = None; // (overlap, intelligence, score)
        for (toks, score) in &self.entries {
            let shared = overlap(&want, toks);
            let family = want
                .iter()
                .any(|t| t.len() >= 3 && t.chars().all(|c| c.is_alphabetic()) && toks.contains(t));
            if !family || shared < 2 {
                continue;
            }
            // Prefer more shared tokens; break ties toward the higher-intelligence row (a bare
            // bridge alias like `claude-cli::opus` should map to the latest/best Claude-Opus).
            let better = match best {
                None => true,
                Some((bo, bi, _)) => shared > bo || (shared == bo && score.intelligence > bi),
            };
            if better {
                best = Some((shared, score.intelligence, *score));
            }
        }
        best.map(|(_, _, s)| s)
    }
}

/// Tokens for a Forge id: provider-derived family words (so the bare CLI bridges match) plus the
/// model part's own tokens.
fn id_tokens(id: &str) -> Vec<String> {
    let (provider, model) = id.split_once("::").unwrap_or(("", id));
    let mut toks = match provider {
        "claude-cli" | "anthropic" => vec!["claude".to_string()],
        "codex-cli" => vec!["gpt".to_string()],
        _ => Vec::new(),
    };
    toks.extend(tokens(model));
    toks
}

/// Lowercased alphanumeric tokens, split on separators AND letter↔digit boundaries, so
/// "claude-opus-4-8", "Claude 4.8 Opus" and "llama3.2" all tokenise comparably. A leading
/// gateway path (`anthropic/claude-...`) is dropped to its last segment first.
fn tokens(s: &str) -> Vec<String> {
    let s = s.rsplit('/').next().unwrap_or(s).to_lowercase();
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_digit = false;
    for c in s.chars() {
        if c.is_alphanumeric() {
            let d = c.is_ascii_digit();
            if !cur.is_empty() && d != cur_digit {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(c);
            cur_digit = d;
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // Drop noise tokens that don't help identify a model.
    out.retain(|t| !matches!(t.as_str(), "latest" | "preview" | "exp" | "instruct" | "it"));
    out
}

/// A stable key for a token set (order-independent): sorted, deduped, joined.
fn canon(toks: &[String]) -> String {
    let mut v: Vec<&str> = toks.iter().map(String::as_str).collect();
    v.sort_unstable();
    v.dedup();
    v.join("-")
}

/// Count of distinct `want` tokens also present in `have`.
fn overlap(want: &[String], have: &[String]) -> usize {
    let mut seen = std::collections::HashSet::new();
    want.iter()
        .filter(|t| have.contains(t) && seen.insert(t.as_str()))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scores() -> BenchmarkScores {
        let mut b = BenchmarkScores::new();
        b.insert("GPT-5.2", 58.0, 55.0);
        b.insert("Claude 4.5 Sonnet", 60.0, 62.0);
        b.insert("Claude 4.5 Opus", 64.0, 66.0);
        b.insert("Llama 3.3 70B", 41.0, 38.0);
        b.insert("Gemini 3 Pro", 62.0, 57.0);
        b
    }

    #[test]
    fn exact_token_set_matches_regardless_of_word_order() {
        let b = scores();
        // Forge id has the version after the family; the source put it before — same token set.
        let s = b.score_for("anthropic::claude-sonnet-4-5").unwrap();
        assert_eq!(s.intelligence, 60.0);
        assert_eq!(s.coding, 62.0);
    }

    #[test]
    fn version_dotted_id_matches_dashed_source_slug() {
        let b = scores();
        let s = b.score_for("openai::gpt-5.2").unwrap();
        assert_eq!(s.intelligence, 58.0);
    }

    #[test]
    fn bare_bridge_alias_maps_via_injected_family_token() {
        let b = scores();
        // `claude-cli::opus` has no version — must map to a Claude-Opus row (the higher one on tie).
        let s = b.score_for("claude-cli::opus").unwrap();
        assert_eq!(s.intelligence, 64.0, "bare opus → best Claude-Opus");
    }

    #[test]
    fn unknown_model_has_no_score() {
        let b = scores();
        assert!(b.score_for("groq::some-unlisted-9000").is_none());
    }

    #[test]
    fn does_not_match_on_a_stray_number_alone() {
        let b = scores();
        // Shares "3" with Llama 3.3 / Gemini 3 but no family word → no match.
        assert!(b.score_for("foo::random-3").is_none());
    }
}
