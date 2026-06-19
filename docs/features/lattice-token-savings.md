# Feature: Lattice token savings — body injection

> Status: **SHIPPED** (2026-06-19).
>
> Make Lattice context injection actually *save* tokens by injecting the **source bodies** of the
> top-ranked symbols, not just their signatures — so the model reads the relevant code from
> context instead of spending whole-file `read_file` calls, **and** by querying only high-signal
> symbol-shaped identifiers so the injection is relevant rather than a wall of noise. Measured
> **~50% median per-task reduction** in agent-loop tokens vs the previous (signature-only) default,
> **~37% vs no Lattice**, on a repo-question benchmark — and the model answers in 1–2 steps instead
> of 3–4.

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
context the model typically completes in **1–2 steps** instead of a multi-step read/grep loop.

### 2.1 Retrieval relevance (the fix that made the bodies pay off)

The first cut of body injection regressed on some prompts: it *added* tokens. Root cause was
retrieval noise, not the bodies themselves. The prompt was tokenized into all words ≥3 chars, so
prose ("value", "return", "variant", "forge", "core") became query terms that each matched a
handful of unrelated symbols — especially **test functions**. A single prompt injected ~30 snippets
(~720 tokens) of mostly-junk signatures, plus a fat body for a fuzzy match (e.g. `ForgeMcp` for the
word "forge"). All of it was re-sent on **every agent step**, and the wall of noise made the model
distrust the one relevant body and explore anyway.

Fixes (`retrieve.rs`):
- **Symbol-shaped query terms only.** Prefer tokens that look like code identifiers — snake_case
  (`inject_budget`) or multi-word CamelCase (`BudgetStatus`) — and, when the prompt names any, query
  *only* those. All-caps acronyms ("SQL", "INSERT") and Titlecase prose ("Answer", "Store") are
  excluded; a backtick-quoted token (`` `Usage` ``) is always treated as a symbol. Only when the
  prompt contains no symbol-shaped token at all do we fall back to plain prose words.
- **Bodies only on a confident (symbol-shaped) query.** A prose fallback injects signatures only —
  a fat body for a fuzzy prose match (e.g. "insert" exact-matching an unrelated helper) is the
  worst case, costing hundreds of tokens of noise per step.
- **Bodies only for exact-name hits**, and **signature lines for fuzzy hits dropped on the prose
  path** (so a low-confidence query injects a few exact matches or nothing — degrading to the
  no-injection baseline rather than misleading the model).
- **Signature cap** (`MAX_SIG_SNIPPETS = 8`) bounds the long tail of fuzzy matches.

This took the example prompt's injection from ~720 tokens / 30 snippets to ~160 / 3 (the two
relevant bodies + one signature).

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
embeddings off, watcher off, shared pre-built index). The per-task metric is **total input+output
tokens summed across every provider call** of the turn, read from the `usage` table via
`Store::session_tokens` / `Store::session_step_count`. A real model is required — the mock returns
fixed token counts and would defeat the measurement.

### Aggregation: median, equal-weight per task

A live single-turn run is **noisy** — the model occasionally spirals into a 4–6-step exploration, a
5–30× token outlier. An earlier version of this benchmark reported the *arithmetic mean of summed
tokens*, which is dominated by whichever condition happened to hit that outlier on one task. That
produced a "−60%" headline that did not reproduce: a later run on the same code measured only −20%.
The mean was measuring luck, not the change.

The report now uses, per task, the **median total across reps**, and aggregates conditions by the
**mean and median of the per-task percentage reductions** (each task weighted equally — so one
giant-token task can't swing the headline). The median-of-tasks is the figure to trust; the mean is
shown alongside and is positive whenever a single task blows up.

### Results (median of per-task reductions, 3 runs at reps=3–4)

| metric | typical |
|--------|---------|
| Improved vs Current (old signature-only default) | **−48% to −54%** |
| Improved vs Off (no Lattice) | **−37% to −38%** |
| steps, Off → Improved | 3–4 → 1–2 |

The big wins are on **symbol-named questions** (T1 `Usage`, T2 `inject_budget`/`BudgetStatus`, T5
`PermissionMode`): the relevant body is injected, the model answers in one step. The strongest
single case was T2, the prompt that previously *regressed*: the noisy injection that sent the model
exploring is gone, and Improved now lands ~2.5k tokens vs Current's 5–150k (run-dependent).

**Honest caveat — prose questions.** T4 names no symbol ("the retrieval code extracts candidate
identifiers… minimum length… stopword"). On the prose-fallback path Lattice can only inject a few
exact-name guesses or nothing, so it is **neutral-to-slightly-negative vs no injection** there — the
model reads the file regardless. Lattice helps when the prompt names what it wants; it does not hurt
much when it can't. Run-to-run, absolute numbers still swing widely (T2-Current ranged 5.7k–151k
across runs); the **median per-task reduction is stable** and clears the ≥30% target with margin.

A diagnostic, `cargo run -p xtasks -- probe-retrieve`, prints exactly which symbols (body vs
signature, token cost) each task's prompt would inject — used to root-cause regressions without
spending model calls.

## 4. Future levers (not needed to hit the target, documented for later)

- **Cross-turn dedup** — don't re-inject a symbol already injected earlier in the session (helps
  long multi-turn sessions; the single-turn benchmark wouldn't show it).
- **Repo/symbol map** — a compact file list to kill `glob`/`ls` orientation calls.
- **1-hop call-graph names** — callers/callees of the top symbols.
- **Ranking** — centrality penalty for noise names (`new`, `run`) + file-locality boost.
