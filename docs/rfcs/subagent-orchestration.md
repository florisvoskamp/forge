# RFC: Subagent orchestration — parallel, mesh-routed child agents

| Field | Value |
|-------|-------|
| Status | FULLY SHIPPED — Phases 1–3c (parallel, agent types, live TUI w/ nested streaming, CLI-bridge exposure + visibility, bounded recursion) |
| Author | Floris (with Claude) |
| Created | 2026-06-15 |
| Last updated | 2026-06-15 |
| Reviewers | — |
| Decision due | — |
| Implements | "subagent orchestration" (next feature) |

---

## Summary

Give the main Forge agent a `spawn_agents` tool that launches one or more **child
agents** to work on subtasks, each in its own isolated context, **routed independently
through the Model Mesh** (so a Complex parent can fan out cheap Trivial subagents).
Subagents can be **ad-hoc** (a task string) or a named **agent type** loaded from
`.forge/agents/<name>.md`. Multiple subagents run **concurrently** (bounded), and their
combined results return to the parent as a single tool result. This is the foundation
for the harness's headline workflows — parallel review, multi-file search, design panels.

---

## Problem statement

A single agent loop has one context window and one model. Two costs follow:

1. **Context dilution.** Reading 20 files to answer one question burns the parent's
   window with material it won't need again; the signal (the answer) is buried in noise
   (the file dumps). Today `run_turn` accumulates *everything* into one `transcript`
   (`forge-core/src/lib.rs:48`).
2. **One model for the whole job.** The mesh routes the *turn* to one tier
   (`router.route(prompt, budget)` once per turn, `lib.rs:237`). A Complex task that
   contains ten Trivial lookups pays Complex prices for all of them — the mesh's
   cost win is left on the table within a turn.

Subagents fix both: each child gets a **fresh, scoped transcript** (isolation), is
**routed on its own subtask** (a file-search subagent runs on a cheap tier even when
the parent is on an expensive one), and runs **in parallel** so wall-clock for N
independent subtasks is ~max, not ~sum.

### Non-goals

- **Recursive subagents.** v1 depth is exactly 1 — subagents do **not** get the
  `spawn_agents` tool (prevents fork-bombs / runaway cost). Deeper trees are a future RFC.
- **Cross-subagent communication.** Subagents are independent; they don't see each
  other's transcripts. A subagent that needs another's output is the parent's job to
  sequence across turns.
- **Streaming each subagent's inner tokens into the TUI.** v1 surfaces coarse
  lifecycle events (start / done + summary + cost). Live nested streaming of N
  concurrent agents is a follow-up (interleaving/rendering is its own problem).
- **Interactive permission prompts inside a subagent.** A parallel subagent can't own
  the single TUI prompt; `Ask` resolves deterministically (see Security).
- **A general actor/job system.** This is task-scoped fan-out/gather, not a durable
  queue.

---

## Background and context

Verified against the current code (not assumed):

- **Agent loop:** `Session::run_turn` (`forge-core/src/lib.rs:187`) routes once, then
  runs a `MAX_STEPS = 8` model↔tool loop, dispatching tool calls via `invoke_tool`
  (`lib.rs:350`) which runs `permission::decide` then `tool.run(&args).await`.
- **Tool seam:** `Tool` trait — `async fn run(&self, args: &Value) -> Result<String,
  ToolError>` (`forge-tools/src/lib.rs`). **Tools have no access to Session, Store,
  Provider, or Router.** This is the central design constraint: a subagent needs all of
  them, so `spawn_agents` cannot be an ordinary registry tool.
- **Provider:** `Provider::complete(model, messages, tools, on_event)` is stateless and
  `Send + Sync` (`forge-provider/src/lib.rs:71`); `DispatchProvider` routes by model
  prefix. The mesh (`Router::route`, `forge-mesh/src/lib.rs:78`) is `async` + `Send +
  Sync`.
- **Ownership:** `Session` owns `provider: Box<dyn Provider>`, `router: Box<dyn
  Router>`, and `store: Store` **by value** (`lib.rs:38-40`). `Store` is `{ conn:
  Mutex<Connection> }` — a single WAL connection behind a mutex, **not `Clone`**
  (`forge-store/src/lib.rs:62`). Concurrent children therefore require `Arc` sharing;
  the mutex already serializes writes safely.
- **Presenter:** flat `PresenterEvent` enum, no nesting concept today
  (`forge-tui/src/lib.rs:18`).
- **Persistence:** `session`/`message`/`tool_call`/`usage` tables; **no parent/child
  link** (`forge-store/src/schema.rs`). Budget is `store.spend_today_usd()` /
  `spend_this_month_usd()` aggregated over all sessions.

---

## Proposed solution

### High-level design

```
parent run_turn (Complex tier)
  │  model emits tool_call: spawn_agents([{agent:"reviewer",task:"…"}, {task:"find all call sites of X"}])
  ▼
core intercepts spawn_agents BEFORE registry lookup (it needs provider/router/store)
  ├─ resolve each entry → AgentDef (file or inline default)
  ├─ budget pre-check (refuse if over cap)
  ├─ spawn N child runners CONCURRENTLY (bounded, default cap 4)
  │     each child:
  │        fresh transcript = [system(agent prompt), user(task)]
  │        own ToolRegistry (agent's allowed tools; NEVER spawn_agents)
  │        router.route(task) → its OWN tier  ← mesh win
  │        run the same MAX_STEPS loop → final_text + usage
  │        persist as child session (parent_session_id = parent)
  └─ gather → one combined tool result string returned to the parent transcript
  ▼
parent continues with the gathered results as a tool result
```

### Detailed design

**1. Shareable machinery (`Box` → `Arc`).** Change `Session.provider` and
`Session.router` to `Arc<dyn Provider>` / `Arc<dyn Router>`, and hold `store:
Arc<Store>`. `Session::start`/`resume` signatures take `Arc<…>`. `DispatchProvider`,
`HeuristicRouter`, `LlmRouter` are already `Send + Sync`, so this is mechanical. This
lets the orchestrator hand each concurrent child cheap clones of the same backends. The
shared `Store` mutex serializes the (small, infrequent) child writes; WAL keeps reads
concurrent.

**2. `spawn_agents` is a *virtual tool* owned by core, not the registry.** Because it
needs provider/router/store, it is **not** a `Box<dyn Tool>`. Instead:
- Its `ToolSpec` (name/description/schema) is appended to the `specs` list sent to the
  provider, so the model can call it — *only when subagents are enabled and depth == 0*.
- `invoke_tool` checks `if call.name == "spawn_agents"` **before** the registry lookup
  and routes to the orchestrator, which has `&self` access to the shared backends.

This keeps the `Tool` trait untouched (rejected alternative: a `ContextualTool` trait
threading `Arc<everything>` into every tool — invasive for one caller).

Schema:
```json
{
  "type": "object",
  "properties": {
    "agents": {
      "type": "array", "minItems": 1, "maxItems": 8,
      "items": {
        "type": "object",
        "properties": {
          "agent": { "type": "string",
            "description": "named agent type from .forge/agents; omit for a general read-only agent" },
          "task": { "type": "string", "description": "the subtask, self-contained" }
        },
        "required": ["task"]
      }
    }
  },
  "required": ["agents"]
}
```

**3. The subagent runner.** Extract the body of `run_turn`'s model↔tool loop into a
free function `run_agent_loop(ctx: &AgentCtx, transcript, tools, mode, rules, presenter,
emit_prefix) -> AgentOutcome { final_text, usage }`, where `AgentCtx` bundles the shared
`Arc<dyn Provider>`, `Arc<dyn Router>`, `Arc<Store>`, `Config`, `Pricing`. `run_turn`
becomes a thin caller of it for the parent. A subagent call builds:
- `transcript = [Message::system(agent.system_prompt), Message::user(task)]`
- `tools = registry filtered to agent.allowed_tools` (default: read-only set —
  `read_file`, `list_dir`, `search`), **never** including `spawn_agents` (depth guard).
- routing: `router.route(task, budget)` unless the agent file pins `tier`/`model`.

**4. Parallel fan-out.** The orchestrator maps entries to futures and drives them with a
bounded concurrency limit (`mesh.subagents.max_concurrency`, default 4) via
`futures::stream::buffer_unordered` (or a `Semaphore`). Each future is independent;
results collected in input order. A child that errors yields a labeled error string
rather than aborting siblings. The combined result returned to the parent:
```
[agent 1: reviewer] <final_text or error>
[agent 2: (inline)]  <final_text or error>
```

**5. Agent type files — `.forge/agents/<name>.md`.** Front-matter + system prompt:
```markdown
---
name: reviewer
description: Reviews a code change for bugs and risk.
tools: [read_file, list_dir, search]   # optional; default = read-only set
tier: standard                          # optional; omit → mesh-routed per task
---
You are a meticulous code reviewer. Given a task, inspect the relevant files and
report concrete findings as `file:line — issue`. No praise, no scope creep.
```
Loaded by a new `forge-config` function `load_agents(dir) -> HashMap<String, AgentDef>`.
Unknown `agent` names → the orchestrator returns a tool error the model can recover from
(it can retry inline). Inline entries (no `agent`) use a built-in default read-only
agent with a generic system prompt.

**6. Event surfacing (coarse, v1).** New `PresenterEvent` variants:
```rust
SubagentStart  { id: String, agent: String, task: String },
SubagentResult { id: String, agent: String, ok: bool, summary: String, cost_usd: f64 },
```
Rendered as a labeled, indented block (`⤷ spawn reviewer: …` / `  ✓ reviewer ($0.001)`).
Inner tokens are not streamed in v1.

**7. Persistence + budget.** Add nullable `parent_session_id TEXT REFERENCES session(id)`
to the `session` table (best-effort `ALTER TABLE`, matching the existing migration
pattern at `forge-store/src/lib.rs:84`). Each subagent is a real child session row, so
its messages/usage persist and **roll into the same daily/monthly budget automatically**
(budget aggregates across sessions). The orchestrator does a **pre-spawn budget check**:
if already over a hard cap, it refuses to spawn and returns that as the tool result.
Parent's final `Cost` event reflects children (shared store aggregation).

### Data model changes

```sql
ALTER TABLE session ADD COLUMN parent_session_id TEXT;  -- nullable; child→parent link
```
No other schema changes. (Index deferred — child counts are tiny.)

### Config changes

```toml
[mesh.subagents]
enabled = true          # default true; gates whether spawn_agents is advertised
max_concurrency = 4     # bounded fan-out
max_agents = 8          # hard cap per spawn_agents call (matches schema maxItems)
agents_dir = ".forge/agents"
```

---

## Alternatives considered

### Alternative 1: `ContextualTool` trait (subagent as a real registry tool)
**Description:** Add a second trait whose `run_with_context(args, &SessionContext)` gets
`Arc`s to store/provider/router, register `SpawnAgentTool` normally.
**Why rejected:** Invasive — every tool dispatch path and the registry must learn about
two trait shapes, for the benefit of exactly one tool. Core already holds all the
machinery; intercepting one well-known name in `invoke_tool` is far less surface area.

### Alternative 2: Subagents over MCP (reuse `forge mcp-serve`)
**Description:** Model the subagent as an MCP tool served by a child `forge` process,
like the CLI bridge.
**Why rejected:** A subagent needs the mesh, budget rollup, and shared store — all
in-process state. Spawning a process per subagent loses the shared budget/store and adds
IPC + cold-start cost for an in-process concern. (The CLI bridge uses MCP because the
*model* is external; here everything is ours.)

### Alternative 3: Inline-only, sequential (do nothing fancy)
**Description:** One subagent at a time, no agent files.
**Why rejected (partially):** It's the safe core and is literally Phase 1 below — but
shipping *only* this misses the headline win (parallel fan-out) the feature is for. We
phase to it, we don't stop at it.

### Alternative: Do nothing
The mesh's per-subtask cost win and context isolation stay unrealized; "review these 8
files" remains one bloated, single-model turn. Rejected.

---

## Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Cost blow-up from fan-out (N children × loop) | Med | High | Pre-spawn budget check; `max_agents`/`max_concurrency` caps; children mesh-route (cheap by default); depth-1 only (no recursion) |
| SQLite write contention under concurrency | Med | Med | Single `Arc<Store>` mutex serializes writes (already the model); WAL keeps reads concurrent; child writes are small/infrequent |
| `Box`→`Arc` refactor ripples through callers | High | Low | Mechanical; providers/routers already `Send+Sync`; compiler-guided; covered by existing tests |
| Permission `Ask` undecidable in a parallel child | High | Med | Deterministic resolution: in a subagent, `Ask` → **Deny** unless mode is `acceptEdits`/`bypass`; denylist always applies; default child toolset is read-only (no writes/shell unless the agent file opts in) |
| Runaway/hung child | Med | Med | Per-subagent timeout (reuse provider timeouts); a child error is isolated, siblings continue |
| Model over-uses fan-out for trivial things | Med | Low | Tool description steers usage; `max_agents`; cost visible per child |

---

## Security considerations

- **Permissions:** every subagent tool call runs the **same `permission::decide`** (mode
  + rules + unoverridable safety denylist). Because a parallel child can't drive the
  single interactive prompt, `Ask` resolves **deterministically**: `Deny` unless the
  session mode already implies auto-approve (`acceptEdits`/`bypass`). Default child
  toolset is **read-only**, so a subagent can't write/shell unless its agent file
  explicitly lists those tools *and* the mode permits it. This is strictly no-more-
  privileged than the parent.
- **Recursion / resource exhaustion:** depth-1 guard (no `spawn_agents` in child
  registries) + concurrency/agent caps + budget pre-check bound the blast radius.
- **Agent files** are local, user-authored config (like `.forge/config.toml`); no
  network fetch, no code execution beyond the tools the model already has.

## Operational considerations

New coarse events to render; child sessions appear in the store (filterable by
`parent_session_id`). No new external dependencies beyond a futures-concurrency helper
(likely already present via tokio). `forge session list` should learn to fold/hide child
sessions (follow-up, not blocking).

## Performance considerations

Wall-clock for N independent subtasks drops from ~Σ to ~max (bounded by
`max_concurrency`). Added latency per spawn is child cold transcript + routing (one cheap
classify each). Store mutex is not a bottleneck at these write volumes.

---

## Rollout plan

- **Phase 1 — the runner (sequential, inline, read-only). ✅ SHIPPED.** `Box`→`Arc`
  refactor of `Session`'s provider/router/store; `spawn_agents` as a core-owned virtual tool
  (advertised only at depth 0, intercepted in `invoke_tool`); `run_subagent` runs each child
  in a fresh transcript with a read-only registry (no `spawn_agents` → structural depth-1
  guard), routed independently through the mesh, persisted as a child session with
  `parent_session_id`; `Ask`→`Deny` in children; coarse `SubagentStart`/`SubagentResult`
  events; usage rolls into the shared budget. (Implemented sequentially; the schema already
  accepts N agents — parallelism is the Phase-2 change.)
- **Phase 2 — parallel fan-out + agent files + live TUI. ✅ SHIPPED.** Children run
  concurrently on tokio tasks bounded by a `max_concurrency` `Semaphore`, sharing the parent's
  `Arc` backends; since the presenter is single-threaded, each child reports completion over an
  `mpsc` channel that `spawn_agents` drains on the main task, emitting `SubagentResult` **live**
  as each finishes. `.forge/agents/*.md` agent types load via `forge_config::load_agents`
  (name/description/tools/tier front matter, dependency-free parser); a named type sets the
  child's system prompt, tool subset, and optional pinned tier (else mesh-routed); unknown/inline
  → default read-only investigator. **TUI:** running children animate with a spinner in the live
  preview region; as each completes it flows into a grouped scrollback box
  (`╭─ subagents` · `├─ ✓ [agent] $cost summary` · `╰─ N agents · $total`).
- **Phase 3a — expose `spawn_agents` to CLI-bridge (claude/codex) turns. ✅ SHIPPED.**
  `forge mcp-serve` now builds its own provider/router/store and advertises `spawn_agents`,
  running the children in-process via the shared [`orchestrate`] driver. **Recursion guard
  across the process boundary:** before running children, mcp-serve sets
  `FORGE_NO_SPAWN_AGENTS=1`; any nested CLI-bridge child inherits the env var and does **not**
  re-advertise `spawn_agents`, so depth stays 1 even though the guard now spans processes
  (verified live: tool present with the var unset, absent with it set; a `tools/call
  spawn_agents` ran a mesh-routed ollama child end-to-end at $0). The presenter-agnostic
  `orchestrate(ctx, parent_id, requests, agents, budget, max_concurrency, on_event)` is shared
  by `Session` (TUI events) and mcp-serve (headless logging).
- **Phase 3b — nested live token streaming (native turns). ✅ SHIPPED.** Each child's streamed
  text/reasoning is forwarded from its task over the orchestrator channel
  (`ChildMsg::Progress` → `Lifecycle::Progress` → `PresenterEvent::SubagentProgress`); the TUI
  shows the trailing edge of that activity in the child's live panel row (falling back to the
  task before it streams). Bounded to the last ~80 chars per row; no interleaving since each
  child writes only its own row.
- **Phase 3c — bridge-subagent TUI visibility + bounded recursion. ✅ SHIPPED.**
  - *Visibility:* the process-topology block (events in forge → claude → `mcp-serve`, claude's
    stream-json can't carry Forge events) is solved with an **out-of-band JSONL sink**.
    `CliProvider` creates a temp file, passes its path to the bridge via the `FORGE_SUBAGENT_SINK`
    env var (inherited forge → claude → mcp-serve), and **tails it concurrently** with claude's
    stdout (`tokio::select!`, so events drain live while claude is silent awaiting the tool
    result). mcp-serve writes `start`/`progress`/`done` JSONL there; `CliProvider` parses each
    into a new `StreamEvent::Subagent*`, which core maps to the same `PresenterEvent::Subagent*`
    as native turns — so bridge-spawned subagents render in the **native TUI panel** (verified
    live: mcp-serve emitted start + live progress deltas + done to the sink).
  - *Recursion:* `mesh.subagents.max_depth` (default 2). `AgentCtx` carries `depth`/`max_depth`;
    a native child advertises + intercepts `spawn_agents` while `depth < max_depth` and recurses
    via a boxed (`Pin<Box<dyn Future + Send>>`) `run_nested_spawn` (breaks the async opaque-type
    cycle). Across processes the depth rides the `FORGE_SUBAGENT_DEPTH` env var (replaces the
    old boolean guard): each `mcp-serve` advertises `spawn_agents` only while `depth < max_depth`
    and bumps it for anything it spawns. Bounded + terminating (verified: a self-recursing
    provider stops at exactly `max_depth` generations). Per-call `max_agents`/`max_concurrency`
    caps still apply at every level.

### Spike checklist (before Phase 2)
1. Confirm `Box`→`Arc` compiles cleanly across `forge-cli` construction sites and tests.
2. Run two trivial children concurrently against the MockProvider; assert both persist
   with `parent_session_id` and usage rolls into the day's spend.
3. Assert a child cannot call `spawn_agents` (not in its specs) and that `Ask` in a
   child resolves to `Deny` under default mode.

---

## Definition of done

- [ ] `spawn_agents` advertised only at depth 0 and only when `mesh.subagents.enabled`.
- [ ] Children route independently through the mesh; a Trivial child on a Complex parent
      uses the cheap tier (test asserts the routed model differs).
- [ ] N children run concurrently, bounded by `max_concurrency`; results returned in
      input order; a single child error doesn't abort siblings.
- [ ] `.forge/agents/<name>.md` loads (name, description, tools, optional tier); unknown
      name → recoverable tool error.
- [ ] Default child toolset is read-only; writes/shell require an opt-in agent file +
      permitting mode; denylist always applies; `Ask`→`Deny` in a child by default.
- [ ] Child sessions persist with `parent_session_id`; usage rolls into budget;
      pre-spawn budget check refuses when over cap.
- [ ] Depth-1 guard verified (no recursive spawning).
- [ ] Coarse `SubagentStart`/`SubagentResult` events render in TUI + headless.
- [ ] fmt + clippy `-D warnings` + full workspace tests green.

---

## References

- `docs/rfcs/cli-bridge-full-harness.md` — the in-process-MCP pattern (contrast: external model)
- `docs/adr/` ADR-0006 (Model Mesh), ADR-0005 (SQLite store), ADR-0003 (Provider trait)
- `forge-core/src/lib.rs` (run_turn/invoke_tool), `forge-mesh/src/lib.rs` (Router)
