# Feature: Lattice token savings — body injection

> Status: **SHIPPED** (2026-06-19).
>
> Make Lattice context injection actually *save* tokens by injecting the **source bodies** of the
> top-ranked symbols, not just their signatures — so the model reads the relevant code from
> context instead of spending whole-file `read_file` calls. Measured **~60% reduction** in total
> agent-loop tokens vs the previous (signature-only) default on a repo-question benchmark.

---

## 1. Problem

Lattice already retrieved relevant symbols and injected them before each turn, but **signature-only**:
a flat list of `kind signature (path:line)` lines. That tells the model *where* code lives, but to
actually read it the model still issues `read_file` — which dumps the **entire file** (often
thousands of tokens) into the transcript, where it is then re-sent on every subsequent step. The
injection cost tokens without removing the expensive exploration it was meant to replace.

A baseline benchmark confirmed it: signature-only injection ("Current") was **+22% tokens vs no
Lattice at all** on a single-rep run, and roughly break-even (−8.7%) across three reps — i.e. the
feature was not earning its cost.

## 2. Change

`[lattice] inject_bodies = true` (default). During retrieval, the top **3** ranked symbols whose
body fits `body_max_tokens` (default 800) are injected as a fenced source block:

```
crates/forge-core/src/lib.rs:2497 — function inject_budget
```rs
fn inject_budget(base: usize, status: BudgetStatus) -> usize { … }
```
```

instead of a bare signature line. The remaining hits stay signature lines. A precise ~40-line body
(~300 tokens) replaces a whole-file read (2–6k tokens), and because the answer is already in
context the model typically completes in **one step** instead of a multi-step read/grep loop.

Supporting changes:
- `NodeHit` now carries `span_start`/`span_end` (already in `LatticeNodeRow`; previously dropped in
  `rows_to_hits`) so a body can be sliced from the file with no re-parse and no schema migration.
- `Lattice::repo_root()` accessor resolves the file to read.
- **Prompt-adaptive budget:** the effective injection budget scales with the number of identifiers
  named in the prompt (each "earns" ~one body's worth), clamped to `inject_token_budget` (raised
  3000). A one-symbol prompt never gets the full budget padded with low-value context.
- `body_max_tokens` caps a single injected body — a symbol whose body exceeds it falls back to a
  signature line (injecting a huge body would cost more than the read it saves). Stale/oversize
  spans and read failures degrade gracefully to the signature line; never panics.

Config:

```toml
[lattice]
inject_bodies = true      # default
body_max_tokens = 800     # per-symbol body ceiling
inject_token_budget = 3000
```

## 3. Benchmark

`cargo run -p xtasks -- bench-lattice` (crate `crates/xtasks`). It drives a **real** model
(`FORGE_BENCH_MODEL`, default `openrouter::google/gemini-2.5-flash`) through the full agent loop on
five repo-specific questions whose answers live in this codebase, under three conditions:

| Condition | Injection |
|-----------|-----------|
| Off       | none (model must grep/read to answer) |
| Current   | signature-only (the old default) |
| Improved  | signature + body (the new default) |

Each run is isolated (fresh in-memory session store, pinned model, no failover, no budget cutoff,
embeddings off, watcher off, shared pre-built index). The metric is **total input+output tokens
summed across every provider call** of the turn, read from the `usage` table via
`Store::session_tokens` / `Store::session_step_count`. A real model is required — the mock returns
fixed token counts and would defeat the measurement.

### Results (reps=3)

| | Off | Current | Improved |
|--|-----|---------|----------|
| mean total tokens / task | 9345 | 8536 | **3412** |
| mean steps | ~3.6 | ~2.3 | **~1.4** |

- **Improved vs Current (old default): −60.0%**
- **Improved vs Off (no Lattice): −63.5%**

Improved is also far **lower-variance**: because the answer is injected, the model answers in one
step deterministically, where Off/Current sometimes spiral into 4–6-step explorations (a single
59k-token outlier in Off). The one task where injection slightly *adds* tokens is a tiny struct the
model could already answer cheaply — dominated by the large wins elsewhere.

> The benchmark is single-turn and uses a live model, so absolute numbers are noisy run-to-run; the
> ≥30% target is met with wide margin and is robust to the outliers.

## 4. Future levers (not needed to hit the target, documented for later)

- **Cross-turn dedup** — don't re-inject a symbol already injected earlier in the session (helps
  long multi-turn sessions; the single-turn benchmark wouldn't show it).
- **Repo/symbol map** — a compact file list to kill `glob`/`ls` orientation calls.
- **1-hop call-graph names** — callers/callees of the top symbols.
- **Ranking** — centrality penalty for noise names (`new`, `run`) + file-locality boost.
