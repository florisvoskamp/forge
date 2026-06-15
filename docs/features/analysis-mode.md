# Feature: Assay — `forge assay` (AI-slop / quality analysis mode)

> Status: **interactive mode shipped** (2026-06-15). Assay is now a **chat mode**, not a CLI
> subcommand (the `forge assay` command was removed): `/assay` opens an interactive picker —
> **Analysis only** vs **Full cleanup (Refine)** — then runs the parallel, mesh-routed (health-
> aware), adversarially-verified critic crew, renders a ranked report inline, persists it, and on
> **Full cleanup** runs **Refine**: a permission-gated, **undoable** fix turn (reuses the agent
> loop, so edits go through the temper + are shadow-snapshotted; `/undo` reverts them). Covers
> U1, U3–U7, U9 (interactive), U10, U11.
> **Deferred:** git scopes `--diff/--branch/--since` (U2), per-critic live progress rows (U9 full),
> report diff `--since-last` (U12), `--only/--skip` (U13), budget pre-estimate scope-down (U8 —
> the daily cap still applies), fan-out fixing (Refine currently fixes via one seeded turn).
> Original design below.
>
> Status: **DRAFT — design only** (2026-06-15). No Rust written yet.
>
> **Assay** is the critical sibling to plan mode: an *analysis-first* multi-agent
> **critic crew** that does a deep, adversarial investigation of code **and** design
> **and** architecture **and** docs, then reports **prioritized findings**. It produces a
> report; **fixing is opt-in** (hand findings to the normal agent loop, or emit a fix
> plan) — Assay **never auto-applies changes**.
>
> Depends on two planned primitives: **subagent orchestration**
> (`docs/features/subagent-orchestration.md`, *to be written* — referenced here as the
> parallel fan-out/synthesize primitive that maps to roadmap item "Native multi-agent
> orchestration" in `01-requirements.md` §3) and **Model Mesh routing** (FR-4, ADR-0006).
>
> Brand: the `⚒ ASSAY` view. (An *assay* is a metallurgical test of ore quality — fitting
> for a forge.)

---

## 1. Problem (JTBD) — who's affected and why

> **As** a developer who has accumulated a pile of AI-generated and hand-written code,
> **I want** a critical crew of code/design/documentation/architecture reviewers to
> investigate my repo (or a diff/branch) and tell me — ranked — what is bad, dead, unsafe,
> untested, over-engineered, or architecturally wrong, **with the reasoning and a suggested
> fix**, **so that** I can decide what to clean up before it rots, without trusting a single
> model's drive-by opinion.

Today Forge can *write* code (the agent loop, FR-1) and will soon be able to *plan* changes
(plan permission mode, FR-10). What it cannot do is **turn the lens around and judge what
already exists**. The gap this fills:

- **AI slop accumulates silently.** Agentic coding produces plausible-looking code fast:
  dead helpers nobody calls, duplicated logic, over-abstracted layers, `unwrap()` on real
  fallible paths, untested branches, comments that lie. None of this trips a compiler.
- **Linters are mechanical, not architectural.** `clippy` finds idiom nits; it does not say
  "this module violates SRP," "your dependency direction is inverted," or "this doc
  describes behaviour the code no longer has."
- **A single review pass is noisy.** One model asked "find problems" hallucinates issues
  and misses real ones. Good human review crews cut noise by having *different reviewers
  with different lenses* and *a second reviewer who challenges each claim*. Assay encodes
  that: specialized critics + an independent verifier per finding.
- **Cost makes naive deep review impractical.** Sending a whole repo to a frontier model is
  expensive and wasteful — most of the scan (dead code, unused symbols) is mechanical and
  belongs on a cheap/local model. This is exactly what Model Mesh exists to exploit (FR-4,
  FR-5).

**Who's affected:** the solo developer (primary persona, `01-requirements.md` §2) doing a
pre-release cleanup or onboarding onto inherited code; OSS contributors auditing a PR before
merge. **Why it matters:** it operationalizes Forge's cost-control promise on a genuinely
expensive task (deep review) and gives the harness a *judgment* surface, not just an
*execution* one.

---

## 2. Scope (MoSCoW)

### Must have
- **U1 — CLI entry point.** `forge assay [PATH]` runs an analysis over a scope and prints a
  ranked findings report. Default scope = whole repo (cwd).
- **U2 — Scope targeting.** `--diff` (uncommitted working-tree changes), `--branch <b>`
  (this branch vs `main`/default), `--since <ref>` (files changed since a git ref), and a
  positional `PATH` (a subtree or single file). Mutually exclusive scope selectors.
- **U3 — The critic crew.** Run a set of **specialized critics in parallel**, each a
  subagent with its own lens and prompt, via the subagent-orchestration primitive. v0.1
  lenses: dead-weight/unused, correctness/bugs, unsafe-code, test-coverage/untested-paths,
  design (SRP/complexity/coupling), architecture (boundaries/layering/dependency
  direction), documentation rot, over-engineering/AI-slop.
- **U4 — Mesh-routed critics.** Each critic's model is chosen by Model Mesh: mechanical
  scans (dead code, unused, lint-like) route to the **trivial/cheap or local** tier;
  design/architecture judgment routes to the **complex/frontier** tier (FR-4).
- **U5 — Adversarial verification.** Every candidate finding is checked by an **independent
  verifier** (a different prompt, and where it matters a different model) that tries to
  *refute* it. Only findings that survive verification appear in the report; refuted ones
  are dropped (or downgraded in confidence). This is the noise-cut mechanism.
- **U6 — Ranked findings report.** Each finding carries **category, severity
  (critical/high/medium/low), confidence, `file:line`, what's wrong, WHY (critical
  rationale), suggested fix, estimated effort.** The report is sorted by (severity,
  confidence) and grouped with a per-category summary.
- **U7 — Persisted, resumable, diffable reports.** Each assay run is persisted in
  forge-store with its scope and findings, so a later run can answer **"did we fix the
  findings?"** by diffing against the prior report for the same scope.
- **U8 — Budget respected.** Assay obeys the daily/monthly budget cap (FR-5). If a run would
  exceed the remaining budget it **scopes down** (sample / drop the most expensive critics /
  refuse) rather than silently overspending. Live cost meter as it runs.
- **U9 — TUI rendering.** A `⚒ ASSAY` summary view + expandable per-finding detail, with
  **live progress** as critics run (reuse the spinner/statusline + inline-scrollback model
  of `tui-inline-scrollback.md`). A plain/headless mode for pipes and CI.

### Should have
- **U10 — Interactive `/assay`.** Inside `forge chat`, typing `/assay [scope]` runs the crew
  against the current session's cwd and renders the report inline, then offers to **hand the
  findings to the agent loop** as a fix task or **emit a fix plan** (plan mode, FR-10).
- **U11 — Fix hand-off (opt-in).** From a finished report the user can select findings (all
  / by severity / by id) and have Forge open a normal agent turn pre-seeded with those
  findings as the task — fixing reuses the existing loop and permission broker, it is not a
  new execution path inside Assay.
- **U12 — Report diff view.** `forge assay --since-last` (or a `--compare <run-id>` flag)
  renders **new / fixed / still-open / regressed** findings against the previous run for the
  same scope.
- **U13 — Critic selection.** `--only <lens,…>` / `--skip <lens,…>` to run a subset (e.g.
  fast `--only dead-weight,unsafe` pass).

### Could have
- **C1 — Severity gating exit code** (`--fail-on <severity>`) so CI can fail a build — the
  first step toward CI integration (explicit later-phase non-goal below).
- **C2 — Custom lenses** via config (user-defined critic prompt + tier).
- **C3 — Sampling strategy knobs** for huge repos (`--sample <pct>`, hot-path weighting).
- **C4 — Machine-readable output** (`--format json`) for tooling.

### Won't have (this iteration)
- **Not a linter replacement.** Assay does not reimplement `clippy`/`rustfmt`; it *may shell
  out to them* as cheap signal sources for the mechanical critics, but its value is the
  design/architecture/doc judgment a linter can't give.
- **Not auto-fix by default.** No finding is applied to the working tree automatically.
  Fixing is always an explicit, separate, permission-gated agent turn (U11).
- **Not CI-gating in v0.1.** `--fail-on` (C1) is the seed; a full CI mode (annotations,
  baseline files, PR comments) is a deliberately deferred later phase.
- **Not cross-repo / monorepo-wide intelligence beyond scoping.** Monorepos are handled by
  *scoping* (PATH/diff), not by a global cross-package model.

### Non-goals (explicit)
- Replacing static analysis tools, formatters, or type checkers.
- Mutating code without an explicit, separately-permissioned user action.
- Acting as a quality gate in CI in this iteration.
- Guaranteeing zero false positives — the goal is **noise reduction via verification**, not
  perfection; every finding shows a confidence the user can weigh.

---

## 3. Acceptance criteria (Given / When / Then)

### Happy paths
```
Given a git repo with code, docs, and some untested/dead code
When I run `forge assay`
Then the crew runs its critics in parallel with a live ⚒ ASSAY progress view
And each candidate finding is verified by an independent verifier
And surviving findings are shown ranked by severity then confidence
And a per-category summary (count by severity) is printed
And the run is persisted with a run-id and the cost is shown
```
```
Given I want to review only my unmerged work
When I run `forge assay --branch feature/x`
Then the scope is the diff of feature/x vs the default branch
And critics only consider changed files/hunks (plus minimal context)
And the report headers state the scope so it is unambiguous
```
```
Given I previously ran assay on this repo and fixed some issues
When I run `forge assay --since-last`
Then the report marks each finding as new / fixed / still-open / regressed
And the summary reports "N of M previous findings fixed"
```
```
Given a finished report in `forge chat` via `/assay`
When I choose "fix high+ findings"
Then Forge opens a normal agent turn seeded with those findings as the task
And every file write still goes through the permission broker (FR-10)
And Assay itself applied nothing
```

### Negative / edge paths
```
Given the estimated cost of a full assay exceeds the remaining daily budget
When I run `forge assay`
Then Forge does NOT silently overspend
And it either scopes down (sampling / dropping the costliest critics) with a clear notice,
    or refuses with a message telling me the estimate and the remaining cap
And the budget cap is never exceeded (NFR: cost-correctness)
```
```
Given a candidate finding that the verifier refutes (false positive)
When the report is assembled
Then that finding is dropped (or shown only under a "low-confidence / unverified" fold)
And it does not inflate the ranked list
```
```
Given a repo with no tests at all
When the test-coverage critic runs
Then it reports the absence as a baseline finding (not one finding per untested function)
And does not flood the report with thousands of low-value items
```
```
Given two critics disagree (e.g. design says "extract", over-engineering says "inline")
When findings are merged
Then the conflict is surfaced as a single reconciled finding noting both views
And confidence is lowered rather than emitting two contradictory findings
```
```
Given a non-tty / piped invocation (`forge assay --format json | …`)
When the run completes
Then no interactive TUI is drawn and machine-readable output is emitted
```
```
Given the target is not a git repo and `--diff/--branch/--since` is passed
When the scope is resolved
Then Forge errors clearly ("--diff requires a git repository") and exits non-zero
```
```
Given a single critic subagent fails (provider error / timeout)
When the crew finishes
Then the run degrades gracefully: other critics' findings are still reported
And the failed lens is listed as "skipped (error)" in the summary, never corrupting the run
    (NFR: reliability)
```

---

## 4. Impact analysis — crates touched + insertion points

| Crate | Change | Specific insertion point |
|-------|--------|--------------------------|
| **forge-cli** | New `assay` subcommand + `/assay` chat handler | `Command` enum in `crates/forge-cli/src/main.rs:29` — add `Assay { … }` variant; dispatch arm at `main.rs:98` match; new `async fn assay(...)`. `/assay` parsed in the chat input loop (`run_chat_tui`, `main.rs:297`). |
| **forge-core** | Orchestrates the crew; owns the Assay run loop | New `assay` module beside the `Session` loop in `crates/forge-core/src/lib.rs`. Reuses the permission broker (`permission` mod) only for the **opt-in fix** turn, which is just `Session::run_turn` (`lib.rs:170`). Critics run **read-only** (no permission broker, no writes). |
| **forge-mesh** | Route per-critic models | Reuse `Router::route` (`crates/forge-mesh/src/lib.rs:94`) + `RoutingDecision`. Add a small `AssayTier`→tier mapping (mechanical→`TaskTier::Trivial`, judgment→`TaskTier::Complex`) so each critic carries an intended tier. Budget enforcement via existing `BudgetState`/`BudgetStatus` (`lib.rs:13`,`:20`). No change to the `Router` trait. |
| **forge-provider** | None (reused) | Critics are model calls through the existing `Provider` trait; subagent orchestration sits above it. |
| **forge-store** | Persist reports | New tables `assay_run` + `finding` in `crates/forge-store/src/schema.rs` (idempotent `CREATE TABLE` batch, matching existing style). New methods in `crates/forge-store/src/lib.rs`: `create_assay_run`, `add_finding`, `list_assay_runs`, `load_findings`, `latest_run_for_scope`. |
| **forge-tui** | Render `⚒ ASSAY` view + live progress + new events | New `PresenterEvent` variants (below) added to the enum at `crates/forge-tui/src/lib.rs:19`; the inline-scrollback renderer learns the assay summary/detail blocks. Reuses spinner/statusline. |
| **forge-types** | Shared finding types | `Finding`, `Severity`, `Confidence`, `FindingCategory`, `AssayScope`, `AssayReport` live here (already home to `TaskTier`, `Message`, `PermissionMode`) so core/store/tui/cli share them. |
| **forge-config** | Critic/lens config + assay budget knob | Optional `[assay]` section: which lenses are enabled, lens→tier overrides, sampling defaults, `assay_max_usd` per run (separate from the daily cap). Reuses Figment layering (ADR-0007). |
| **docs** | This spec; depends on `subagent-orchestration.md` (to be written) | — |

**Dependency note:** Assay is **blocked on** the subagent-orchestration primitive
(`docs/features/subagent-orchestration.md`). That primitive must provide: spawn N subagents
with distinct system prompts + tool scopes, run them concurrently with bounded fan-out,
collect their structured outputs, and a synthesize/reduce step — all under one budget. Assay
is the first consumer of it; if the primitive lands first, Assay is mostly wiring + types +
rendering.

---

## 5. Technical design

### 5.1 Vertical slice trace (one `forge assay` run)

```
forge-cli   `forge assay --branch feature/x`
            → parse Command::Assay { scope }, build Session deps (store, provider, router,
              presenter, config) exactly like `run`/`chat`
forge-core  AssayRun::start(scope):
            1. Resolve scope → file/hunk set (git plumbing for --diff/--branch/--since;
               walk for PATH/repo). Apply budget-aware sampling if over `assay_max_usd`.
            2. Build the crew: one CriticSpec per enabled lens, each with
               { lens, system_prompt, intended_tier }.
            3. Estimate cost = Σ over critics of (tier model price × est. tokens for scope).
               If estimate > remaining budget → scope down or refuse (emit AssayBudget event).
forge-mesh  For each CriticSpec: Router::route(critic_probe, budget) → model for its tier.
            (Mechanical lenses land on Trivial/local; judgment lenses on Complex/frontier.)
[subagent-orchestration]  Fan out the critics CONCURRENTLY (read-only tools: read_file,
            list_dir, search — never write/edit). Each returns Vec<CandidateFinding>.
forge-core  Verification pass: for each candidate, spawn an independent VERIFIER subagent
            (refute-this prompt; different model where the lens is judgment-heavy).
            Keep survivors; drop/downgrade refuted; reconcile conflicting candidates into a
            single finding with both views + lowered confidence.
forge-core  Synthesize: dedupe by (file, line, category), rank by (severity, confidence),
            compute per-category summary, assemble AssayReport.
forge-store create_assay_run(scope, cost) → run_id; add_finding(run_id, …) per finding.
            If --since-last: diff against latest_run_for_scope → tag new/fixed/open/regressed.
forge-tui   Throughout: PresenterEvent::Assay* events drive the live ⚒ ASSAY progress and the
            final summary + expandable detail. Cost event reuses existing meter.
[opt-in]    User picks "fix" → Session::run_turn seeded with selected findings (FR-10 gated),
            OR "plan" → a plan-mode turn. Assay never writes on its own.
```

Mechanical critics may **shell out** (e.g. `cargo clippy --message-format json`,
`cargo +nightly udeps`, dead-code/unused via the existing `search` tool) to gather cheap
signal *before* spending model tokens — keeping the trivial-tier critics genuinely cheap.

### 5.2 Key types (proposed — in `forge-types`)

```text
enum Severity   { Critical, High, Medium, Low }
enum Confidence { High, Medium, Low }          // post-verification confidence
enum FindingCategory {
    DeadWeight,        // unused / unreachable / dead code
    Correctness,       // bugs, wrong logic, panics on real paths
    Unsafe,            // unsafe blocks, unchecked unwrap/expect on fallible paths, data races
    TestCoverage,      // untested branches / missing tests / baseline "no tests"
    Design,            // SRP, complexity, coupling, naming, leaky abstraction
    Architecture,      // layering, module boundaries, dependency direction
    DocumentationRot,  // docs/comments that disagree with code, stale README
    OverEngineering,   // needless abstraction, AI-slop patterns, premature generality
}

enum AssayScope {
    Repo,
    Path(String),
    Diff,                 // working-tree changes
    Branch(String),       // vs default branch
    Since(String),        // git ref
}

struct Finding {
    id: String,
    category: FindingCategory,
    severity: Severity,
    confidence: Confidence,
    file: String,
    line: Option<u32>,        // None for file/module/architecture-level findings
    title: String,            // one-line "what's wrong"
    rationale: String,        // WHY it's a problem (the critic's reasoning)
    suggested_fix: String,
    effort: Effort,           // Trivial | Small | Medium | Large
    lens: String,             // which critic raised it
    verified: bool,           // survived adversarial verification
    status: FindingStatus,    // New | StillOpen | Fixed | Regressed (only when diffing)
}

struct AssayReport {
    run_id: String,
    scope: AssayScope,
    findings: Vec<Finding>,             // pre-sorted (severity, confidence)
    summary: BTreeMap<FindingCategory, SeverityCounts>,
    cost_usd: f64,
    skipped_lenses: Vec<(String, String)>,   // (lens, reason) — graceful degradation
}
```

### 5.3 New `PresenterEvent` variants (`forge-tui` enum at `lib.rs:19`)

```text
AssayStarted   { scope: String, critic_count: usize }
CriticStarted  { lens: String, model: String, tier: String }
CriticProgress { lens: String, candidates: usize }
CriticDone     { lens: String, candidates: usize }
CriticSkipped  { lens: String, reason: String }      // graceful degradation
Verifying      { remaining: usize }                  // adversarial pass progress
AssayBudget    { estimate_usd: f64, remaining_usd: f64, action: String }  // scoped-down/refused
AssayReportReady { /* carries AssayReport summary for rendering */ }
```
The existing `Routing`, `Cost`, and `Warning` variants are reused (per-critic routing,
running cost, budget advisories). Persistence reuses the `Cost` accounting path.

### 5.4 TUI mockups (monospace)

**Live progress (pinned live region, inline-scrollback model):**
```
⚒ ASSAY  scope: branch feature/x vs main   (14 files, 3 hunks)
─────────────────────────────────────────────────────────────
 ✓ dead-weight        gpt-4o-mini   ·trivial·   6 candidates
 ✓ unsafe-code        gpt-4o-mini   ·trivial·   2 candidates
 ⠋ correctness        sonnet        ·complex·   running…
 ⠋ architecture       opus          ·complex·   running…
 ⏳ design             sonnet        ·complex·   queued
 ⏭ documentation      —             skipped (--skip)
─────────────────────────────────────────────────────────────
 verifying 8 candidates…        spent $0.041 / cap $2.000   ⠋
```

**Summary view (`⚒ ASSAY` report header):**
```
⚒ ASSAY REPORT   run a1b9c2   scope: branch feature/x vs main
═════════════════════════════════════════════════════════════
 SEVERITY     crit  high  med  low      CATEGORY          n
 ─────────    ────  ────  ───  ───      ───────────────  ──
 totals          1     3    5    4      architecture      2
                                        correctness       3
 verified: 13/17 candidates survived    dead-weight       4
 4 dropped as false positives           over-engineering  2
                                        test-coverage      2
 cost: $0.118   est. effort: ~3.5h      design            2
─────────────────────────────────────────────────────────────
 #  SEV   CONF  CATEGORY        WHERE                 TITLE
 1  CRIT  high  correctness     core/lib.rs:204       unwrap() on provider result panics turn
 2  HIGH  high  architecture    tui/lib.rs:19         core depends on tui types (inverted)
 3  HIGH  med   dead-weight     tools/search.rs:88    fn never called; 40 LOC unreachable
 4  HIGH  med   over-eng        mesh/pricing.rs:12    trait with one impl; needless indirection
 …  press ↑/↓ to move · ↵ expand · f=fix · p=plan · /=filter
```

**Expanded finding detail:**
```
⚒ finding #1   CRITICAL · correctness · confidence: high · verified ✓
─────────────────────────────────────────────────────────────
 where : crates/forge-core/src/lib.rs:204
 lens  : correctness     effort: small (~15m)

 WHAT  unwrap() on the provider call result panics the whole agent turn
       on any transient provider error, corrupting session state.

 WHY   FR / NFR-reliability requires a provider failure to degrade
       gracefully (retry/fallback tier), never crash. This is on the
       hot path of every turn, so a single 5xx aborts the session.

 FIX   propagate via `?` into CoreError::Provider (already exists) and
       let run_turn surface a Warning + fallback tier instead of panicking.

 [f] queue this for fix    [p] add to fix plan    [d] dismiss    [↵] back
```

### 5.5 Edge-case handling

| Edge case | Handling |
|-----------|----------|
| **Huge repo** | Scope resolution estimates token cost; if over `assay_max_usd`/budget, sample (config `--sample`, hot-path weighting) or drop costliest (judgment) critics with a clear `AssayBudget` notice. Mechanical critics shell out first to shrink what the model sees. |
| **Monorepo** | No global model — handled by **scoping**: run per-package via `PATH`, or `--diff/--since` to bound to changed packages. Documented as the intended monorepo workflow. |
| **No tests** | Test-coverage critic emits **one baseline finding** ("no test suite present"), not one-per-function flooding. |
| **False positives** | Adversarial verifier refutes; refuted findings dropped or folded under low-confidence. Confidence shown on every finding so users can weigh. |
| **Conflicting critics** | Synthesis reconciles overlapping/contradictory candidates into a single finding noting both views, with lowered confidence — never two contradictory entries. |
| **Cost control** | Per-critic Mesh routing + per-run `assay_max_usd` + the daily `BudgetState` cap. Estimate-before-run; scope-down or refuse rather than overspend (NFR cost-correctness). Live cost meter. |
| **Language-agnostic vs Rust-first** | Lens prompts are language-agnostic (read-only tools work on any text). Mechanical signal sources (clippy/udeps/cargo) are **Rust-first** in v0.1; on non-Rust repos those critics fall back to model-only scanning and note reduced precision. |
| **Critic failure** | One critic erroring → `CriticSkipped` event + listed in `skipped_lenses`; the run still produces a report from the rest (graceful degradation). |
| **Non-tty / pipe** | No TUI; `--format json` or plain text. `--fail-on <sev>` (C1) sets exit code for the CI seed. |
| **Not a git repo** | `--diff/--branch/--since` error clearly and exit non-zero; `PATH`/repo scope still works. |

---

## 6. Definition of done

- [ ] `forge assay [PATH|--diff|--branch <b>|--since <ref>]` runs and prints a ranked report;
      scope selectors are mutually exclusive and validated.
- [ ] `/assay [scope]` works inside `forge chat` and renders the report inline.
- [ ] The crew runs the v0.1 lenses **in parallel** via the subagent-orchestration primitive,
      read-only (no writes, no permission broker during analysis).
- [ ] Each critic's model is chosen by **Model Mesh**; mechanical lenses land on the cheap/
      local tier, judgment lenses on the frontier tier; recorded per critic.
- [ ] **Adversarial verification** runs on every candidate; refuted findings are excluded
      from (or folded under low-confidence in) the ranked list; verification stats shown.
- [ ] Every finding has category, severity, confidence, `file:line`, what/why, suggested fix,
      and estimated effort; the report is sorted by (severity, confidence) with a per-category
      summary.
- [ ] Runs are **persisted** (`assay_run` + `finding` tables); `--since-last`/`--compare`
      diffs new/fixed/open/regressed against the prior run for the same scope.
- [ ] **Budget is never exceeded**: a run estimated over the cap scopes down or refuses with a
      clear message; live cost meter shown; daily `BudgetState` honoured (FR-5).
- [ ] TUI shows the `⚒ ASSAY` live progress, summary view, and expandable finding detail using
      the existing spinner/statusline + inline-scrollback model; a plain/pipe mode exists.
- [ ] **Fixing is opt-in**: from a report the user can hand selected findings to a normal
      permission-gated agent turn or emit a fix plan; Assay applies nothing on its own.
- [ ] Graceful degradation: a single critic failing yields `CriticSkipped` and a partial
      report, never a corrupted run.
- [ ] New shared types (`Finding`, `AssayReport`, `Severity`, `Confidence`,
      `FindingCategory`, `AssayScope`) live in `forge-types`; new `PresenterEvent` variants
      added; store methods covered by tests at ≥ the project's baseline.
- [ ] Spec's dependency on `docs/features/subagent-orchestration.md` is satisfied (that
      primitive exists) before implementation starts.
