# Feature: Subagent Orchestration (the crew primitive)

> A first-class multi-agent primitive for Forge: spawn child agents with their own task,
> tool subset, context window, and Model-Mesh-routed model; fan them out in parallel under
> bounded concurrency; collect and synthesize results. Implemented across `forge-core`
> (a subagent runner), `forge-tools` (a `task` tool), `forge-mesh` (per-subagent routing),
> `forge-store` (subagent transcripts + cost rollup) and `forge-tui` (a live agent tree).
>
> Status: design. This is the substrate the **Assay** analysis mode is built on (§7) — Assay
> is a curated crew preset, not a separate engine.

## 1. Problem (JTBD)

> When a task naturally decomposes into independent or specialized subtasks — "audit these
> 8 files", "research these 4 approaches", "investigate X while you refactor Y" — I want the
> agent to **fan the work out to parallel child agents**, each scoped to its own subtask with
> its own context and the cheapest model that can do it, then collect and synthesize the
> results — so I get breadth and speed without one giant context window, one model tier, or one
> serial loop, and without it costing a fortune or running away.

Today a Forge `Session` runs a single linear agent loop (`Session::run_turn`,
`crates/forge-core/src/lib.rs:170`): one transcript, one routed model per turn, up to
`MAX_STEPS` (`:18`) model↔tool round trips, all in one context window. There is no way to:
- run subtasks **concurrently** (parallel file audits, multi-file edits, N-way research);
- give a subtask its **own** isolated context (so a 12-file sweep doesn't blow the parent's window);
- route **each** subtask independently (a frontier model for the hard one, a trivial-tier model
  for the eight narrow ones) — the Mesh today routes one prompt per turn.

**Why this is the headline differentiator.** Delegation is the owner's dominant workflow: the
`/orchestrate` command is his #1 slash command (62 uses) and his history shows 233 subagent
(Agent/Task) spawns. The Helm note frames it directly: *"fan out tasks to parallel agents,
collect results, synthesize — not a plugin, a first-class primitive."* Forge's existing Model
Mesh (per-subagent cheap-vs-frontier routing) and budget caps make orchestration *affordable*,
which is the part competitors get wrong — fan-out is where token spend explodes.

**Who's affected:** power users running broad/parallel tasks; the agent itself (it gains a
`task` tool to delegate autonomously); and the Assay mode, which is a crew preset on top of this.

## 2. Scope (MoSCoW)

**Must have**
- A **subagent primitive** (`SubagentRunner` in `forge-core`): given a `SubagentSpec` (task prompt,
  tool subset, optional system prompt, tier hint, budget cap, depth), run an isolated agent loop
  (its own transcript + context window) and return a `SubagentResult` (text + cost + status).
- Each subagent's model is **Model-Mesh-routed independently** via the existing `Router` trait
  (`crates/forge-mesh/src/lib.rs:52`), honouring a `tier_hint` and the live budget.
- A **`task` tool** (`forge-tools`) the parent model can call to spawn one subagent autonomously
  — exposed to the model like any other tool (it goes through the same permission broker).
- **Parallel fan-out**: spawn N subagents as Tokio tasks under a **bounded worker pool**
  (`max_concurrency`), collect all results, hand the aggregate back to the parent for synthesis.
- **Depth, concurrency and budget caps**: `max_depth` (no infinite nesting), `max_concurrency`
  (no subagent storms), per-subagent `budget_usd` and an orchestration-total cap — all tied to
  the Mesh's existing `BudgetState`/`BudgetStatus` machinery (`crates/forge-mesh/src/lib.rs:13`).
- **Error isolation**: a subagent that fails (provider error, budget exceeded, panic) yields a
  `Failed`/`Skipped` result; siblings and the parent continue (a failure never crashes the parent).
- **Persistence**: each subagent's transcript is stored as a child session linked to the parent;
  subagent cost rolls up into the parent session total.
- **Presenter lifecycle events**: `SubagentSpawned` / `SubagentProgress` / `SubagentDone` so any
  presenter (headless or TUI) can render orchestration without `forge-core` knowing the UI.

**Should have**
- An explicit **orchestration mode/command** for the user: `forge orchestrate "<goal>"` and a
  `/orchestrate` command inside `forge chat`, where a planner turn decomposes the goal into specs.
- **Schema-validated results**: a `SubagentSpec.result_schema` (JSON Schema); the runner validates
  the subagent's final answer against it and reports a `SchemaMismatch` on failure.
- **TUI agent tree**: a live tree of running subagents in the pinned live region with per-agent
  status / tier+model / running cost, reusing the existing spinner + statusline + scrollback.

**Could have**
- **Git worktree isolation** for parallel *file-editing* subagents (each edits an isolated checkout;
  results are surfaced as diffs to merge), avoiding write conflicts. Mentioned as advanced (§6, §8).
- **Result streaming** from subagent to parent (partial synthesis) rather than collect-then-synthesize.
- A subagent **retry policy** (1 retry on transient provider error before marking `Failed`).

**Won't have (this iteration)**
- Cross-subagent messaging / shared scratchpad (subagents are isolated; they communicate only via
  their returned result through the parent).
- Subagents spawning a live interactive UI of their own (only the root session reads user input).
- Distributed / multi-machine execution. Automatic worktree *merge-conflict resolution*.
- A learned/LLM planner for decomposition (v1 decomposition is the parent model emitting `task` calls).

### Non-goals
- This does **not** change single-turn behaviour: a session with no `task` calls runs exactly as today.
- It does **not** introduce a new provider or routing algorithm — it reuses `Provider` and `Router`.
- It is **not** a plugin system; the primitive is native to the core.

## 3. Acceptance criteria (Given / When / Then)

```
# Single delegated subagent (the task tool)
Given an agent session and a parent model that emits a `task` tool call
      { task: "summarize crates/forge-core/src/lib.rs", tools: ["read_file"], tier_hint: "trivial" }
When the parent turn executes that tool call
Then a child SubagentRunner starts with its OWN empty transcript (not the parent's)
And the Mesh routes the child independently (trivial tier → cheapest configured model)
And the child runs its own loop limited to the {read_file} tool subset only
And a SubagentResult{status: Ok, text, cost_usd} is returned to the parent as the tool result
And the child's cost is added to the parent session total

# Parallel fan-out + synthesis
Given a goal that decomposes into 6 independent audit subtasks
When the parent emits 6 `task` calls (or `orchestrate` produces 6 specs)
Then up to `max_concurrency` (default 4) subagents run concurrently as Tokio tasks
And the remaining specs queue and start as workers free up (backpressure, not all-at-once)
And once all complete, the aggregated results are fed back to the parent for a synthesis turn
And the synthesized answer is the turn's final text

# Tool-subset isolation (negative)
Given a subagent spec'd with tools: ["read_file", "search"]
When the subagent's model attempts to call `shell` or `write_file`
Then the call is rejected ("tool not available to this subagent") without prompting
And the rejection is recorded in the subagent transcript; the subagent continues

# Depth cap (negative — no infinite nesting)
Given orchestration config max_depth = 2 and a subagent already at depth 2
When that subagent's model emits a `task` call
Then the `task` tool returns an error result "max subagent depth (2) reached; not spawning"
And no grandchild is created; the subagent continues with that error as the tool result

# Concurrency cap (negative — no subagent storm)
Given max_concurrency = 4 and 50 `task` calls requested in one turn
When the fan-out runs
Then at most 4 subagents are in-flight at any moment (semaphore-bounded)
And the other 46 queue; total spawned in one turn is capped by `max_fanout` (default 16),
    beyond which extra specs are rejected with a "fan-out cap reached" result

# Per-subagent budget cap (negative — runaway cost)
Given a subagent with budget_usd = 0.05 that has spent 0.05
When its loop is about to make another model call
Then the subagent stops, returns status: BudgetExceeded with its partial work, and never exceeds
    its cap; the parent's orchestration-total cap likewise halts new spawns when reached

# Partial failure isolation
Given 5 parallel subagents where subagent #3 hits a provider error
When results are collected
Then subagent #3's result is { status: Failed, error } and #1,#2,#4,#5 still return Ok
And the parent receives all 5 results (4 Ok + 1 Failed) and synthesizes from the survivors
And the parent process does NOT crash or abort the turn

# Schema-validated result mismatch (negative)
Given a subagent spec with result_schema requiring { "verdict": string, "score": number }
When the subagent's final answer does not satisfy the schema
Then the runner marks it status: SchemaMismatch, includes the raw text + validation error,
    and (Should-have) retries once with a "return JSON matching this schema" reminder

# Persistence + resume
Given an orchestrated turn that spawned 3 subagents
When the session is later resumed (`forge ... resume <id>`)
Then the parent transcript rehydrates as today, and each subagent transcript is retrievable
    as a child session linked by parent_session_id (inspectable, not silently discarded)

# Live TUI tree
Given `forge chat` (interactive) running a parallel fan-out
When subagents are spawning/running/finishing
Then the live region shows a tree: root → children with per-child status, tier+model, $cost
And finished children scroll into native scrollback with their final one-line summary
And the statusline running cost includes subagent spend
```

## 4. Impact analysis & insertion points

The design is deliberately additive and **trait-respecting**: it reuses `Router`, `Provider`,
`Tool`/`ToolRegistry`, `Store`, and `Presenter` exactly as they exist. The agent loop is
refactored once so the parent and every subagent share it.

| Layer | Insertion point | Change |
|-------|-----------------|--------|
| `forge-core` | `crates/forge-core/src/lib.rs:170` `run_turn` | Extract the model↔tool loop body into a reusable `run_agent_loop(ctx)` used by both the root `Session` and `SubagentRunner` (the loop is identical; only the owning context differs). |
| `forge-core` | new `crates/forge-core/src/subagent.rs` | `SubagentRunner`, `SubagentSpec`, `SubagentResult`, `OrchestrationConfig`; the bounded worker pool (`tokio::task` + `Semaphore`); depth/budget enforcement. |
| `forge-core` | `Session` struct (`:33`) | Carry a `depth: usize` and `OrchestrationConfig`; root session is depth 0. A subagent is a `Session`-shaped context at depth+1 reusing the same store/provider/router. |
| `forge-tools` | new `crates/forge-tools/src/task_tool.rs`; register in `with_core_tools` (`:49`) | A `TaskTool` implementing `Tool`. Its `side_effect()` is a new `SideEffect::Spawn` class (permission-gated like Shell). Its `run` is wired to the `SubagentRunner` via a handle (see §5 on the wiring seam). |
| `forge-types` | `crates/forge-types/src/lib.rs:152` `SideEffect` | Add `Spawn` variant (subagent spawning is a side effect the broker can allow/ask/deny). Add `SubagentStatus` enum. |
| `forge-mesh` | `Router::route` (`crates/forge-mesh/src/lib.rs:52`) | **No signature change.** The runner passes the subagent's task prompt + a `tier_hint`; reuse `route`. (Optional: `route_with_hint` later; v1 prepends/uses the hint via the existing classifier path.) Per-subagent `BudgetState` derived from the orchestration cap. |
| `forge-store` | `crates/forge-store/src/lib.rs` + `schema` | `session.parent_session_id` (nullable) + `session.depth`; `create_child_session(parent_id, depth, ...)`. Subagent transcripts use the existing `message` table (a child session id). Cost rolls up: a `roll_up_cost(child→parent)` or recording subagent `record_usage` against the parent total. |
| `forge-tui` | `crates/forge-tui/src/lib.rs:19` `PresenterEvent` | Add `SubagentSpawned{id,parent,task,tier,model}`, `SubagentProgress{id,...}`, `SubagentDone{id,status,cost}`. Headless renders them as indented lines; `ChannelPresenter` forwards them unchanged (`driver.rs:42`). |
| `forge-tui` | `crates/forge-tui/src/app.rs` (render) | An `AgentTree` view in the live region: nodes keyed by subagent id, per-node status/model/cost; reuses existing spinner/tick + statusline cost rollup. |
| `forge-cli` | `crates/forge-cli/src/main.rs` (`Command`, `Mode`) | New `Command::Orchestrate { goal, .. }`; `/orchestrate` handled in `run_chat_tui`. Orchestration config surfaced as flags (`--max-concurrency`, `--max-depth`, `--orchestration-budget`). |
| `forge-config` | `MeshConfig`/`Config` (`crates/forge-config/src/lib.rs:39`) | New `[orchestration]` block: `max_depth`, `max_concurrency`, `max_fanout`, `subagent_budget_usd`, `orchestration_budget_usd`. Sensible safe defaults. |

**Risk note (the one structural change):** `Session` today *owns* its `provider`/`router`/`store`
behind `Box<dyn …>`. Subagents need shared, concurrent access to provider + router + store across
parallel Tokio tasks. Resolution (§5): move the shared dependencies behind `Arc<dyn Provider>` /
`Arc<dyn Router>` / `Arc<Store>` (Store is already `Sync` via its internal `Mutex`,
`crates/forge-store/src/lib.rs:24`). This is the single most invasive edit and should land first
as a no-behaviour-change refactor (`Box`→`Arc`), proven green by the existing core tests.

## 5. Technical design

### 5.1 Data model

```rust
// forge-types
pub enum SideEffect { ReadOnly, Write, Shell, Spawn }   // + Spawn

pub enum SubagentStatus {
    Ok,
    Failed,          // provider/tool/internal error
    BudgetExceeded,  // hit per-subagent or orchestration cap
    DepthExceeded,   // refused: would exceed max_depth
    SchemaMismatch,  // final answer failed result_schema validation
    Skipped,         // fan-out cap reached / not started
}

// forge-core::subagent
pub struct SubagentSpec {
    pub task: String,                 // the child's user prompt
    pub system: Option<String>,       // optional role/instructions for the child
    pub tools: Vec<String>,           // tool-name allowlist; empty = inherit a safe read-only set
    pub tier_hint: Option<TaskTier>,  // bias the Mesh (frontier for hard, trivial for narrow)
    pub budget_usd: Option<f64>,      // per-subagent cap; default = config.subagent_budget_usd
    pub result_schema: Option<serde_json::Value>, // JSON Schema for a structured result
    pub label: Option<String>,        // short name for the TUI tree
}

pub struct SubagentResult {
    pub id: String,                   // child session id
    pub label: String,
    pub status: SubagentStatus,
    pub text: String,                 // final answer (or partial on failure/budget)
    pub structured: Option<serde_json::Value>, // present iff result_schema matched
    pub model: String,
    pub tier: TaskTier,
    pub cost_usd: f64,
    pub error: Option<String>,
}

pub struct OrchestrationConfig {
    pub max_depth: usize,             // default 2  (root=0, child=1, grandchild=2)
    pub max_concurrency: usize,       // default 4  (in-flight subagents)
    pub max_fanout: usize,            // default 16 (spawns per orchestrating turn)
    pub subagent_budget_usd: f64,     // default 0.50 per subagent
    pub orchestration_budget_usd: f64,// default 2.00 across one orchestrated turn (≤ daily cap)
}
```

### 5.2 The runner (bounded fan-out)

`run_turn` is refactored to call a shared `run_agent_loop`. Spawning is a thin layer over it:

```text
SubagentRunner::fan_out(specs, parent_ctx):
  if parent_ctx.depth + 1 > cfg.max_depth          -> each spec -> DepthExceeded result
  specs = specs.take(cfg.max_fanout)                 (rest -> Skipped)
  sem   = Arc::new(Semaphore::new(cfg.max_concurrency))
  spent = Arc<AtomicCost>                            (orchestration-total accumulator)
  futures = for spec in specs:
      tokio::spawn(async move {
          let _permit = sem.acquire().await;         // bounded concurrency / backpressure
          if spent.load() >= cfg.orchestration_budget_usd { return Skipped }
          let child = ChildContext {                 // OWN transcript, OWN context window
              transcript: vec![ system?, user(spec.task) ],
              tools: registry.subset(&spec.tools),
              depth: parent.depth + 1,
              budget: min(spec.budget_usd, remaining_orchestration_budget),
              router, provider, store: child_session_id,
          };
          let r = catch(run_agent_loop(child)).await; // panic -> Failed, never propagates
          spent.fetch_add(r.cost_usd);
          validate_schema(&r, spec.result_schema)     // -> SchemaMismatch if bad
      })
  results = join_all(futures)                          // collect ALL, Ok and not-Ok
  return results
```

Key properties:
- **Bounded concurrency**: a `tokio::sync::Semaphore` caps in-flight subagents; extra specs await a
  permit (backpressure) rather than all spawning at once.
- **Isolation**: each child gets a fresh transcript seeded only with its `system` + `task`. It never
  sees the parent's transcript, and the parent never sees the child's — only the `SubagentResult`.
- **Budget**: per-subagent cap enforced inside the child loop (checked before each model call, reusing
  `BudgetState::status`); orchestration-total cap enforced by the shared `spent` accumulator before
  acquiring work. A subagent at its cap stops cleanly with its partial work and `BudgetExceeded`.
- **Failure isolation**: each child future is wrapped so a provider error or panic becomes a `Failed`
  result; `join_all` collects everything. One bad subagent never aborts siblings or the parent turn.
- **Routing per subagent**: each child calls `router.route(task, child_budget)`; the existing Mesh
  heuristics (+ `tier_hint`) pick the tier/model, so cheap subtasks get cheap models and the hard one
  can be pinned to a frontier tier — all under the cost caps.

### 5.3 Two surfaces

**(a) As a tool (autonomous delegation).** `TaskTool` implements `Tool`; the parent model calls it
like any tool. Wiring seam: the `SubagentRunner` cannot live *inside* the tool (tools are
`dyn Tool` with no core access), so the runner is invoked at the core boundary — `invoke_tool`
(`crates/forge-core/src/lib.rs:292`) special-cases the `task` tool (or the tool holds an
`Arc<SubagentRunner>` injected at registry build). A `task` call goes through the permission broker
as a `SideEffect::Spawn`. A single `task` call = a fan-out of one; multiple `task` calls in one
assistant turn are fanned out together (collected before the next parent step).

**(b) As a mode/command (explicit orchestration).** `forge orchestrate "<goal>"` (and `/orchestrate`
in chat) runs a **planner turn**: the parent model is asked to emit a set of `task` calls decomposing
the goal, which fan out, then a **synthesis turn** feeds the collected results back for a final answer.
This is just (a) with a decomposition-biased system prompt — no separate engine.

### 5.4 Persistence

- `create_child_session(parent_id, depth, cwd, mode)` inserts a `session` row with `parent_session_id`
  and `depth` set. Subagent messages use the existing `message` table under the child id, so resume,
  `list_sessions`, routing/usage/tool-call recording all work unchanged.
- Cost rollup: subagent `record_usage` updates the child total as today; on subagent completion the
  runner adds the child's `cost_usd` to the **parent** session total (so `session_cost(parent)` and the
  statusline include subagent spend). Child sessions are filtered out of the top-level `list_sessions`
  view (or shown nested) by their non-null `parent_session_id`.

### 5.5 Presenter events & TUI mockup

New events flow through the same `Presenter` seam (`crates/forge-tui/src/lib.rs:54`); `ChannelPresenter`
forwards them to the render loop unchanged. The TUI keeps an `AgentTree` keyed by subagent id.

Live fan-out (interactive `forge chat`, pinned live region):

```
 ⚒ FORGE   orchestrate · audit the auth module

 ▸ root  [complex] anthropic::claude-opus-4-8   $0.0412   synthesizing…
   ├─ ⠹ a1 login.rs        [trivial]  ollama::llama3.2        $0.0006   running  (2 tools)
   ├─ ✓ a2 session.rs      [standard] openai::gpt-4o-mini     $0.0031   done
   ├─ ⠴ a3 tokens.rs       [standard] openai::gpt-4o-mini     $0.0024   running  (4 tools)
   ├─ ✓ a4 middleware.rs   [trivial]  ollama::llama3.2        $0.0005   done
   ├─ ✗ a5 oauth.rs        [standard] openai::gpt-4o-mini     $0.0019   failed: provider 429
   └─ ◌ a6 csrf.rs         [trivial]  —                       —         queued
                                                          3/6 done · 2 running · 1 queued

 ⠹ orchestrating   [complex] anthropic::claude-opus-4-8   $0.0497   default     ↵ send · esc quit
```

After completion, each child collapses into one scrollback line, e.g.
`  ✓ a2 session.rs → "no issues; uses constant-time compare" [standard gpt-4o-mini] $0.0031`,
and the synthesized parent answer streams below as a normal assistant reply.
Headless renders the same tree as indented `↳`/`✓`/`✗` lines (no animation), reusing the
existing event-to-line mapping in `HeadlessPresenter::emit`.

### 5.6 Git worktree isolation (advanced, Could-have)

For parallel **file-editing** subagents (not read-only audits), concurrent writes to the same
working tree race. Advanced option: a subagent flagged `isolate: worktree` runs in a
`git worktree add` checkout (or a temp copy) so each edits independently; on completion the runner
surfaces the child's diff. v1 leaves merging to the user/parent (no auto-resolution). For the common
read-only fan-out (audits, research) no isolation is needed — those subagents only read + report.

### 5.7 Edge cases

| Edge case | Behaviour |
|-----------|-----------|
| Subagent storm (model emits 50 `task` calls) | `max_fanout` caps spawns/turn (default 16); `max_concurrency` caps in-flight (default 4); excess → `Skipped`. |
| Runaway cost | Per-subagent `budget_usd` + orchestration-total cap; both checked before each model call / spawn; halts cleanly with partial work, never overshoots. Ties into Mesh `BudgetStatus` (warn/exhausted). |
| Infinite nesting | `max_depth` (default 2); a deeper `task` call returns `DepthExceeded` instead of spawning. |
| Subagent panic / provider error | Caught per-future → `Failed` result; siblings + parent unaffected. |
| All subagents fail | Parent receives N `Failed` results and synthesizes "all subtasks failed: <reasons>" rather than emitting a hollow success. |
| Parallel edits to the same file | Read-only fan-out: safe. Editing fan-out: serialize edits, or use worktree isolation (§5.6); document the conflict risk; default-deny editing tools in subagents unless explicitly granted. |
| Result schema mismatch | `SchemaMismatch` status + raw text + validation error; Should-have: one retry with a schema reminder. |
| Subagent tries a tool outside its subset | Rejected without prompting ("tool not available to this subagent"); recorded; child continues. |
| Subagent tries to spawn under `Plan` mode | `Spawn` side effect denied by the broker in `Plan`; `task` returns "spawning denied by policy". |
| Budget already exhausted before fan-out | No subagents spawned; all specs → `Skipped`; parent told the budget is exhausted. |
| Deadlock from over-subscription | Semaphore-bounded + no child waits on a sibling's result (no inter-subagent dependencies in v1), so the pool always drains. |
| Resume mid-orchestration | In-flight subagents are not resumable in v1; on resume the parent transcript is intact and completed-subagent results are persisted as tool results; incomplete ones are absent (not silently faked). |
| Cost double-counting | Subagent usage is recorded against the child session; rolled into the parent total exactly once on completion — never recorded twice against the parent. |

## 6. Relationship to Assay (dependency)

The **Assay analysis mode is built on this crew primitive** — it is *not* a separate subsystem.
Assay is a curated orchestration preset: a fixed decomposition (e.g. spawn one read-only subagent
per file / per analysis lens — security, performance, style — under a tight per-subagent budget and a
read-only tool subset), then a synthesis turn that assembles a report. Assay therefore **depends on**
`SubagentRunner`, `SubagentSpec`/`SubagentResult`, the bounded worker pool, and the subagent
persistence + TUI tree shipped here. **This feature must land first.** Assay then reduces to: a preset
that builds the specs, a synthesis system prompt, and a report renderer — no new orchestration engine.

## 7. Definition of done

- [ ] `Box`→`Arc` refactor of `Session`'s shared deps lands first, no behaviour change, existing core tests green.
- [ ] `run_turn` refactored to a reusable `run_agent_loop`; existing `forge-core` tests still pass.
- [ ] `SubagentSpec` / `SubagentResult` / `OrchestrationConfig` / `SubagentStatus` defined; `SideEffect::Spawn` added.
- [ ] `SubagentRunner::fan_out` with semaphore-bounded concurrency, depth cap, per-subagent + orchestration budget caps, panic/error isolation.
- [ ] `TaskTool` registered; goes through the permission broker as `Spawn`; tool-subset allowlist enforced.
- [ ] Mesh routes each subagent independently (tier_hint honoured); cheap subtasks get cheap models under caps.
- [ ] `forge-store`: `parent_session_id` + `depth` columns, `create_child_session`, subagent cost rolls into parent total exactly once; child sessions distinguishable in `list_sessions`.
- [ ] `PresenterEvent::SubagentSpawned/Progress/Done` added; `HeadlessPresenter` renders indented lines; `ChannelPresenter` forwards them.
- [ ] TUI `AgentTree` renders the live tree with per-agent status/tier+model/cost; finished children collapse to scrollback; statusline cost includes subagent spend.
- [ ] `forge orchestrate` command + `/orchestrate` in chat; config `[orchestration]` block with safe defaults and CLI overrides.
- [ ] Tests: single-delegation, parallel fan-out + synthesis, depth cap, concurrency cap, per-subagent budget, orchestration budget, partial failure isolation, tool-subset rejection, schema mismatch, persistence/resume of child sessions.
- [ ] Edge-case table behaviours covered by tests where feasible (storm/cap, runaway cost, all-fail).
- [ ] `cargo fmt` + `clippy -D warnings` clean; verified live in the TUI against a real provider with a 3+-way fan-out.
- [ ] Assay-readiness note: the primitive exposes everything Assay needs (specs in, results out, tree rendered) — confirmed by a throwaway 3-file read-only fan-out preset.
