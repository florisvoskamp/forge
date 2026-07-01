# RFC: Workflow scripts — mesh-routed, script-based multi-agent orchestration

| Field | Value |
|-------|-------|
| Status | FULLY SHIPPED — script engine, `agent`/`log`/`phase`/`workflow` host functions, `run_workflow` tool, TUI phase grouping, `/workflow` command |
| Author | Floris (with Forge) |
| Created | 2026-07-01 |
| Last updated | 2026-07-01 |
| Reviewers | — |
| Decision due | — |
| Implements | a Forge-native equivalent of Claude Code's Workflow tool |

---

## Summary

Let the top-level model author a real JavaScript script — using `agent()`, `parallel()`,
`pipeline()`, `phase()`, `log()`, and `workflow()` — that a deterministic, sandboxed runtime
(embedded rquickjs) executes, fanning mesh-routed child agents out with genuine concurrency.
This is the same efficiency property a compiled script gives over re-invoking the model for
every control-flow decision: loops, conditionals, and accumulation across rounds run for free
inside the script, instead of costing one LLM call per iteration.

---

## Problem statement

`spawn_agents` (docs/rfcs/subagent-orchestration.md) already lets the model fan a **fixed,
upfront batch** of subtasks out to mesh-routed children. What it can't express: a workflow whose
shape depends on what earlier steps found — "keep retrying until the tests pass," "run stage 2
only on the items stage 1 flagged," "decompose into an unknown number of phases." Doing that with
`spawn_agents` alone means the model re-issuing tool calls turn after turn, re-reading its own
prior output each time to decide what to do next — real LLM-call overhead and a probabilistic
text-comprehension act standing in for what should be a deterministic check.

A real script fixes this the same way it does for Claude Code's own Workflow tool: the
orchestrating model pays for **one** authoring call, then a plain interpreter runs the resulting
control flow for free.

### Non-goals

- **A general cross-phase DAG/dependency executor.** `pipeline()`'s shape (N items × M ordered
  stages, no fan-in/fan-out between items) is deliberately the only multi-stage primitive.
  Cross-turn sequencing beyond that remains the parent's job, exactly as
  `docs/rfcs/subagent-orchestration.md`'s own non-goals already state for `spawn_agents`.
- **Structured (`schema`-validated) `agent()` output.** `agent()` returns plain text, matching
  what `run_subagent` already produces. A JSON-schema-constrained variant is a real, separate
  feature, not built here.
- **Direct script access to Forge's own file/shell tools.** A script's only capabilities are the
  host functions below — this is the entire sandboxing story (see Security).
- **Real throughput-measured adaptive concurrency.** The health/quota signals available today
  (`Store::current_benched()`, `SubscriptionQuota`) are binary/coarse, not a real per-provider
  request-rate measurement — building genuine adaptive concurrency on top of them would be a
  disproportionate, separate feature.

---

## Background and context

Verified against the current code (not assumed), via two independent research passes before any
implementation began:

- **Reusable as-is:** `subagent::route_child()` (mesh-routes a child independently from its own
  task text) and `subagent::run_subagent()` (runs one child's full turn) are the exact primitives
  `agent()` needs — `crates/forge-core/src/subagent.rs`. Repeated `run_subagent` calls against the
  same `child_id` are safe: `Store::add_message_full` self-heals `seq` collisions
  (`crates/forge-store/src/lib.rs`), so a pipeline item's stages can share one continuous,
  legible transcript.
- **The activity panel had a real, load-bearing bug** (`crates/forge-tui/src/app.rs`):
  `SubagentStart`'s handler cleared every row the instant a new batch's first child arrived, if
  every existing row was already done. Invisible for a single `spawn_agents` call (only one batch
  per turn), but it would have silently dropped every earlier workflow phase's rows the moment a
  second phase began. Fixed as its own PR before anything depended on it.
- **No generic multi-phase primitive existed.** The closest precedent, Assay
  (`crates/forge-core/src/assay.rs`), hard-codes exactly two phases (critics → verifiers) for one
  domain (code-quality findings) — not reusable as a general N-phase executor.

### Choosing the script engine

A second research pass compared five candidate Rust-embeddable engines against three hard
requirements: (1) the scripted language should be JS, to match Claude Code's own Workflow-tool
syntax the model already writes fluently; (2) an async Rust host function must be genuinely
awaitable from script code, not a blocking/polling hack; (3) the engine must not complicate the
existing 5-target release cross-compile matrix (`x86_64`/`aarch64`-linux, `x86_64`/`aarch64`-
darwin, `x86_64`-windows).

| Candidate | Verdict |
|---|---|
| **rquickjs** (chosen) | Real JS, a clean `Function::new(ctx, Async(fn))` → awaitable Promise bridge, zero known RUSTSEC advisories, its own CI proven to build all 5 target triples. Needs a C compiler (vendors QuickJS's C source) — a real but solved cost for this matrix. |
| `boa_engine` | Pure Rust, but its tokio-driving pattern only became documented/battle-tested in Jan 2025 and needs ~100 lines of hand-written `JobExecutor` glue — a thinner, newer foundation. |
| `deno_core` (V8) | The most turnkey async bridge (`#[op2(async)]`, Deno's own production mechanism) and prebuilt V8 libs cover all 5 targets — the documented fallback if rquickjs's C build ever proves flaky, at the cost of real binary-size growth and one open, unaddressed `rusty_v8` checksum-verification gap invisible to `cargo audit`/`cargo deny`. |
| `rhai` | Ruled out — confirmed (maintainer statement + current feature docs) to have no async support at all, a hard mismatch for `await agent()`. |
| `mlua` (Lua) | Ruled out — real async via coroutines, but forces the scripted language to be Lua, breaking the "mirror Claude Code's JS API" goal. |

`rquickjs`'s `PR0` shipped as a standalone spike proving the bridge end to end (including real
concurrency via `Promise.all`) before any feature code was built on top of it.

---

## Proposed solution

### High-level design

```
top-level model (Complex tier)
  │  authors a script using agent()/parallel()/pipeline()/phase()/log()/workflow()
  ▼
Session::run_workflow intercepts the run_workflow tool call (core-owned virtual tool,
same pattern as spawn_agents — it needs provider/router/store, not a registry Tool)
  ├─ builds ONE shared concurrency budget (global Semaphore + per-provider
  │  HashMap<String, Arc<Semaphore>>) for the WHOLE script's execution
  ├─ registers 4 host functions against a sandboxed rquickjs context:
  │     agent(prompt, opts?)   → one mesh-routed child (route_child + run_subagent)
  │     log(message)           → a transcript note
  │     phase(title)           → labels subsequent agent() calls for TUI grouping
  │     workflow(name, args?)  → loads + runs a saved .forge/workflows/<name>.js
  ├─ prepends a small JS prelude implementing parallel()/pipeline() PURELY in terms
  │  of agent() + native Promise.all/per-item async closures — NOT separate Rust
  │  primitives (see below)
  ├─ runs the (prelude + model's) script; drains its WorkflowEvents concurrently
  │  with script execution via tokio::select!, converting each into the SAME
  │  PresenterEvent::SubagentStart/Progress/Result the activity panel already renders
  ▼
tool result = the script's own return value; TUI shows phase-grouped live rows
```

### Detailed design

**1. Only ONE real execution primitive is implemented in Rust.** `agent(prompt, opts)` is a
single mesh-routed child call — everything `parallel()`/`pipeline()` need. They are **not**
separate Rust orchestration functions; they are a small JS prelude
(`crates/forge-core/src/workflow.rs::PRELUDE`) prepended to every script:

```js
function parallel(thunks) { return Promise.all(thunks.map((fn) => fn())); }
async function pipeline(items, ...stages) {
    return Promise.all(items.map(async (item, index) => {
        let prev = null;
        for (const stage of stages) { prev = await stage(prev, item, index); }
        return prev;
    }));
}
```

This exactly mirrors how those primitives work conceptually in the reference design: `parallel`
is `Promise.all`; `pipeline` is "each item's own async closure runs stage-by-stage, `Promise.all`
just waits for all of them" — JS's own event loop already gives the "no barrier between items"
property for free. Reimplementing that concurrency shape in Rust would have been strictly more
code for no behavioral difference, and it's invisible to the authoring model either way (it calls
`pipeline(items, stage1, stage2)` exactly the same regardless of which side implements it).

**2. Shared concurrency budget for the whole script.** The global `Semaphore` and per-provider
`HashMap<String, Arc<Semaphore>>` are built **once** per `run_workflow` call
(`WorkflowState::new`) and closed over by every host-function registration — a `parallel()` in
one phase and a `pipeline()` in a later phase draw from the same real budget, not two independent
ones. Acquisition order mirrors `spawn_agents`' `orchestrate()`: the per-provider permit first
(without holding the global one), then the global permit — a saturated provider can't
head-of-line-block `agent()` calls bound for other providers.

**3. `phase()` grouping is a real field, not a text hack.** An earlier iteration prefixed the
task string with `"[phase] task"`; this was replaced with a genuine `phase: Option<String>` field
threaded through `WorkflowEvent::AgentStart` → `PresenterEvent::SubagentStart` → `SubRow` →
`ActivitySummary`, so the TUI groups by real data instead of parsing a rendered string.
`opts.phase` on an individual `agent()` call overrides the ambient `phase()` label for that one
call only.

**4. `workflow(name, args)` — saved scripts.** `.forge/workflows/<name>.js` are plain files,
checked into the project's own git repo — a concrete advantage over session-scoped script
persistence: a team can review, version, and share a workflow script exactly like any other
source file. Sandboxed strictly against path traversal (rejects any name containing `/`, `\`, or
`..`) and bounded by the same `max_depth` structural recursion guard `spawn_agents` already uses
(a nested `workflow()` call shares every `Arc`'d resource with its parent but runs one level
deeper). `args` is exposed to the saved script as a global `const args = <json>;`.

**5. `/workflow run <name> [args]` — no model in the loop.** Running a saved script directly
(skipping the authoring turn entirely, a genuine efficiency win for repeated workflows) needed
real new plumbing, not just a `RunTurn` outcome: `Session::run_saved_workflow` is spawned as its
own background task (`spawn_saved_workflow`, mirroring `spawn_compact`'s busy/spinner/interrupt
semantics) via a new `DispatchOutcome::RunSavedWorkflow`. Running it synchronously inside
`dispatch_command` would have held the session lock for the workflow's whole duration and
reintroduced the exact render-loop-blocking deadlock class fixed earlier in the same development
cycle (a `session.lock().await` from the render loop can be held for a turn's entire duration,
not just during a permission prompt).

### Real bugs found during implementation (worth recording — not obvious from the design alone)

1. **Invoking a JS function from inside an async host function's spawned future corrupts
   QuickJS's GC state.** The first `js_to_json`/`json_to_js` implementation used the engine's own
   `JSON.stringify`/`JSON.parse` for the Rust↔JS value boundary — the obvious choice. This
   reliably hit a `JS_FreeRuntime` assertion failure (`list_empty(&rt->gc_obj_list)`). Reading or
   constructing values *natively* (`Object`/`Array`/`String` accessors, no `Function::call`) from
   the same spot is fine; only invoking a JS-level function call from there isn't. Fixed by
   hand-rolling the conversion with native accessors only.
2. **Capturing an extra `Ctx` clone into a registered host function's closure corrupts the same
   GC state — but only once a SECOND function is registered.** A single closure holding a
   `ctx.clone()` (e.g. to construct a thrown exception on an error path) didn't visibly break;
   registering two such closures together did, even on the success path where the error branch
   never ran. Root-caused by bisecting a dozen minimal scratch reproductions down to the exact
   trigger. `Ctx<'js>` implements `FromParam`, so the correct pattern is to receive it as a
   genuine call parameter (rquickjs supplies a fresh one per invocation) — never capture it.
3. **A test asserting concurrency via wall-clock timing was too tight for CI.** A 30ms sleep with
   a 45ms upper bound passed locally but failed on a slower/shared macOS CI runner (observed
   53ms). Not a functional regression — widened the sleep (50ms) and bound (90ms) so scheduling
   overhead can't cross the serialized-vs-concurrent line.

### Config changes

```toml
[mesh.workflows]
max_total_agents = 200   # hard safety backstop on total agent() calls per script run
                          # (including nested workflow() calls) — mirrors the reference
                          # Workflow tool's own total-agent cap
```

Concurrency (`max_concurrency`, `max_per_provider`) is deliberately **not** forked into a separate
config section — it's shared with `mesh.subagents.{max_concurrency,max_per_provider}`, one real
budget governing both `spawn_agents` and workflow scripts.

---

## Alternatives considered

### Alternative 1: iterative tool-calling instead of a real script (rejected)

**Description:** Have the orchestrating model drive the workflow by calling `spawn_agents`
repeatedly across multiple turns, inspecting each call's result before deciding the next one —
no script engine, no new dependency.

**Why rejected:** This was the first design proposed and explicitly rejected by the user. A
real DSL/script gives genuine control flow (loops, conditionals, accumulation across rounds) that
a deterministic interpreter runs for free; iterative tool-calling re-pays an LLM call for every
control-flow decision and substitutes probabilistic text comprehension for what should be a
deterministic check (e.g. "are there still failing tests?"). This is exactly the efficiency
property that makes the reference Workflow tool cheap and fast, and it's the whole reason to
build this rather than just iterating `spawn_agents` calls.

### Alternative 2: `deno_core` (V8) as the primary script engine (rejected as primary, kept as fallback)

**Description:** Use V8 via `deno_core` instead of QuickJS via `rquickjs` — more turnkey async
bridging, prebuilt binaries already cover all 5 release targets.

**Why rejected (as primary):** A real binary-size/build-graph cost, and one currently-open,
unaddressed supply-chain caveat (`rusty_v8#545`, a missing checksum verification on the prebuilt
static-lib download) that `cargo audit`/`cargo deny` cannot see at all, since the download happens
outside Cargo's lockfile-resolved dependency graph. `rquickjs`'s C-toolchain need is a real but
solved cost (verified via its own CI matrix); if that ever proves flaky on a target Forge cares
about, `deno_core` is the documented next thing to try.

### Alternative 3: generalize `spawn_agents`' `orchestrate()` to also drive `pipeline()` (rejected)

**Description:** Add a `pipeline`-shaped parameter to the existing `orchestrate()` function
instead of writing new script host functions.

**Why rejected:** `orchestrate()`'s concurrency unit is "one child, one routing decision, one
provider permit held for the child's whole life" — a pipeline item needs "N stages, each
independently routed, each acquiring/releasing its own provider permit," a genuinely different
shape. Once the design moved to `agent()` as the one Rust primitive with `parallel()`/`pipeline()`
as pure JS compositions over it, this alternative became moot — no Rust-level pipeline executor
was needed at all.

---

## Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| A pathological/runaway script spawns unbounded agents | Med | High | `mesh.workflows.max_total_agents` hard cap (default 200), checked on every `agent()` call |
| Script recursion via `workflow()` (a saved script calling itself, directly or via a cycle) | Low | Med | Same `max_depth` structural guard `spawn_agents` uses — a script only gets access to `workflow()` while `depth < max_depth` |
| Script escapes its sandbox (filesystem/network/process access) | Low | High | No ambient access exists in rquickjs to begin with; only the 4 explicitly-registered host functions are callable, and `workflow()` is hard-sandboxed to `.forge/workflows/` (path-separator/`..` rejection) |
| The activity panel silently truncates rows once phase headers consume render budget | Med (caught before shipping) | Med | Both the height-reservation and row-fitting logic share one `needs_phase_header` helper so they can't disagree about how much space is needed — caught by a test before merge |
| `/workflow run` holds the session lock for a whole script's duration if dispatched synchronously | Med (caught before shipping) | High (repeats a previously-fixed deadlock class) | Spawned as its own background task with the same busy/interrupt semantics as `/compact`, never awaited directly inside `dispatch_command` |

---

## Security considerations

- **Sandboxing is a property of the host-function allowlist, not the engine.** Neither rquickjs
  nor any of the compared alternatives expose ambient filesystem/network/process access — a
  script can only call what Forge explicitly registers as a global. The entire security story is
  "only 4 functions are registered, and one of them (`workflow`) is itself sandboxed to one
  directory with path-traversal rejection."
- **No new privilege beyond `spawn_agents`.** `agent()` ultimately calls the same
  `route_child`/`run_subagent` machinery, subject to the same read-only-by-default toolset,
  `Ask`→`Deny` resolution in children, and safety denylist as `spawn_agents`.
- **Recursion/resource exhaustion:** the same `max_depth` guard plus the new
  `max_total_agents` cap bound the blast radius of both a pathological script and a
  pathological chain of nested `workflow()` calls.

## Operational considerations

No new external dependencies beyond `rquickjs` (pure addition to `forge-agent-workflow`, a new,
small, domain-agnostic crate — kept separate from `forge-core` so the JS engine isn't a dependency
of plain `spawn_agents` usage). `.forge/workflows/*.js` is a new, user-visible directory
convention; `/workflow list` surfaces its contents.

## Performance considerations

Every spawned agent is mesh-routed by its own task complexity, exactly like `spawn_agents` — cheap
subtasks land on free/fast models automatically, not one fixed tier for the whole workflow. Real
concurrency (proven via a wall-clock test: 3 concurrent 50ms calls complete in ~50-90ms, not
150ms+) comes from JS's own event loop plus the shared semaphore budget, not from any bespoke
Rust scheduling.

---

## Rollout plan

- **PR0 — spike. ✅ SHIPPED.** Standalone proof that an `Async`-wrapped Rust host function is
  genuinely awaitable from a script via `rt.drive()` run in the background, including real
  concurrency via `Promise.all`. No user-facing behavior.
- **PR1 — activity-panel batch-clear fix. ✅ SHIPPED.** Prerequisite: rows must survive a second
  batch starting in the same turn, or phase-grouping would be invisible in practice.
- **PR2 — generic engine + domain host functions + `run_workflow` tool. ✅ SHIPPED (two PRs).**
  `forge-agent-workflow`'s `HostFunction`/`run_script` (domain-agnostic), then
  `crates/forge-core/src/workflow.rs`'s `agent`/`log`/`phase`/`workflow` + the JS prelude +
  `Session::run_workflow` intercepted in `invoke_tool` exactly like `spawn_agents`.
- **PR3 — TUI phase grouping + `/workflow` command. ✅ SHIPPED (two PRs).** Real `phase` field
  threaded end to end + grouped activity-panel rendering; `/workflow <goal>` (authoring turn),
  `/workflow run <name> [args]` (saved script, background task), `/workflow list`.

---

## Definition of done

- [x] `agent(prompt, opts)` mesh-routes a real child and returns its answer as a string.
- [x] `parallel()`/`pipeline()` run concurrently (proven via timing + peak-concurrency tests), not
      serially — no Rust-side duplication of `spawn_agents`' orchestration logic.
- [x] `phase()`/`opts.phase` label agents with a real field, grouped visibly in the activity panel
      without breaking `activity_idx`/Ctrl+O/Enter-to-zoom.
- [x] `workflow(name, args)` and `/workflow run <name> [args]` both load and run a saved script,
      sandboxed against path traversal, with `args` exposed as a script global.
- [x] Recursion bounded by the existing `max_depth` guard; total-agent runaway bounded by a new
      `max_total_agents` cap.
- [x] `/workflow run` never blocks the render loop — spawned as its own background task.
- [x] fmt + clippy `-D warnings` + full workspace tests green (rquickjs bridge tests repeated 5×
      to rule out GC-corruption flakiness specifically, given its history in this feature).

---

## References

- `docs/rfcs/subagent-orchestration.md` — the `spawn_agents`/`route_child`/`run_subagent`
  machinery this feature reuses verbatim for its one real execution primitive.
- `crates/forge-workflow/src/lib.rs` — the domain-agnostic script engine (`HostFunction`,
  `run_script`), including the two GC-corruption bugs documented inline where they were found.
- `crates/forge-core/src/workflow.rs` — the domain host functions, `run_workflow`, `run_saved`,
  `list_saved`.
- `crates/forge-tui/src/app.rs` — `needs_phase_header`, `activity_panel_height`,
  `render_activity_panel`.
