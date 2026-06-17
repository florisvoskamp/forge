# ADR-0011: Benchmark-driven model ranking

**Date:** 2026-06-18
**Status:** Accepted

## Context

The mesh ranks models for a task tier with `capability_score`, whose "quality" term comes
from `capability::quality_class` — a family-name heuristic (substring match: `opus`/`gpt-5`/
`sonnet`/`-70b` → quality 3, `mini`/`haiku`/`flash` → 1, etc.). This is coarse and brittle:

- It can't tell two "frontier" models apart (opus vs sonnet vs gpt-5.5 all collapse to quality 3;
  only the lightest-on-tie + version tiebreaks separate them).
- It has no notion of *actual* performance — a model is "good" because its name matches a pattern,
  not because it scores well. A new family name is unknown until the substring list is edited.
- It can't distinguish general reasoning from coding ability.

The mesh should rank on **real, trustworthy, dynamic performance data** so it makes an informed
choice, not a name-pattern guess.

## Decision

Add an optional **benchmark score layer** sourced from the **Artificial Analysis Data API**
(`https://artificialanalysis.ai/api/v2/data/llms/models`, `x-api-key`). Per model it exposes
`artificial_analysis_intelligence_index` (composite of MMLU-Pro, GPQA, LiveCodeBench, SciCode,
HLE, … — a widely-cited 0–100 index) and `artificial_analysis_coding_index`. It covers both
closed frontier and open models and is refreshed continuously.

- **Fetch + cache:** `forge-cli` fetches once and caches to `.forge/benchmarks.json` with a
  `fetched_at` stamp; refreshed when older than 7 days (scores move slowly) or via
  `forge benchmarks --refresh`. Best-effort + background at startup — never blocks a turn.
  Auth: free key from `ARTIFICIALANALYSIS_API_KEY` env or the `artificialanalysis` keyring entry
  (`forge auth artificialanalysis`).
- **Mapping:** Artificial Analysis slugs (e.g. `gpt-5-2`, `claude-4-5-sonnet`) → Forge
  `provider::model` ids via a normalizer (lowercase, drop provider prefix, collapse separators,
  extract family + version) plus a small alias map for the CLI bridges
  (`claude-cli::opus` → the AA Claude-Opus row). Match is by canonical key; unmatched models simply
  fall back.
- **Scoring:** `capability_score` gains an optional `&BenchmarkScores`. When a model has a score,
  the quality term is `index / 20` (so a ~60 index ≈ quality 3.0, on the same scale the old
  heuristic produced, so cost/conserve terms are unchanged). Complex uses the intelligence index;
  **code-heavy** complex/standard use the **coding index**; trivial keeps the speed-favouring blend.
  When a model has no score (or the layer is disabled / no key), it falls back to `quality_class` —
  so behaviour degrades gracefully and existing tests (no benchmark data) are unchanged.
- **Carrier:** `ModelCatalog` holds an optional `BenchmarkScores` (attached at discovery like the
  model list), so `route_score`/`ranked_rows` read it with no new threading through call sites.
- **Config:** `mesh.benchmark_ranking` (default `true`). With no key/data it's a no-op (heuristic).

## Rationale

- Artificial Analysis is the one source that is **dynamic, programmatic (real API, not scraping),
  trustworthy (composite of standard benchmarks), and covers closed + open** in a single feed with
  a free tier — and it already separates a **coding index** from general intelligence, which the
  mesh needs (it routes code-heavy tasks differently).
- Keeping it an *optional layer over* the heuristic means zero regression risk: the mesh works
  exactly as before without a key, and gets sharper rankings with one.

## Alternatives considered

- **LMArena / Chatbot Arena Elo** (HF dataset): trustworthy human-preference signal, but coding is
  split across separate arenas, model-name mapping is messier, and it's a dataset to track rather
  than a clean per-model API. Good *secondary* signal; not the primary.
- **OpenRouter models API:** has pricing/context metadata but **no benchmark/performance scores** —
  doesn't answer "which is more capable".
- **Aider polyglot leaderboard:** excellent coding signal but coding-only and a smaller model set;
  subsumed by the AA coding index.
- **HF Open LLM Leaderboard:** open models only — misses the closed frontier the mesh routes to.
- **Keep the family heuristic:** rejected — it's the problem being solved.

## Consequences

**Positive:** rankings reflect measured performance; new models are ranked the moment AA lists them
(no code edit); coding tasks rank on coding ability.
**Negative:** best results need a (free) API key; an external dependency (mitigated by caching +
heuristic fallback); slug→id mapping needs maintenance for odd names (mitigated by `forge benchmarks`
to inspect coverage).
**Neutral:** the heuristic `quality_class` stays as the fallback, not removed.
</content>
