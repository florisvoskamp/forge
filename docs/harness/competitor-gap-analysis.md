# Competitor gap analysis — techniques to adopt (Road to v1.0.0, P0.2)

Source: structured read of three open coding-agent harnesses against Forge's current feature set, to
find concrete techniques worth porting. The goal is the **best harness that exists**, so this is a
ranked *adoption backlog*, not a survey. Each item must be implemented completely (no deferral) and,
where it affects task success or efficiency, verified with the benchmark harness
(`docs/benchmarks/swe-bench.md`).

Projects read:
- **opencode** (`sst/opencode`, the canonical repo `anomalyco/opencode` aliases) — TS/Bun server +
  Go TUI; headless client/server with an SSE event bus, fine-grained permission globs, plugins.
- **openclaude** (`Gitlawb/openclaude`) — a reconstructed/de-minified Claude Code fork with a
  multi-provider layer. Caveat: much of its polished loop is *Claude Code's own* engineering surfaced
  in readable form, not novel — we steal the ideas regardless of authorship.
- **pi** (`earendil-works/pi`) — TS, provider-agnostic headless runtime; session branching, a
  two-phase context pipeline, an RPC/SDK embedding mode.

What Forge already has (so excluded from gaps): multi-provider mesh w/ cost/capability/quota ranking
+ rank-faithful failover; CLI-bridge harness; MCP client; skills + hooks (MVP); subagent
orchestration (depth-1); lattice code-index; shell tool + error interceptor; web tools; session
replay; context compaction; token gauge; temper permission modes; auto model discovery; LSP; autofix
loop; plan mode; tool-call recovery.

---

## Ranked adoption backlog

Ranked by (leverage × certainty) ÷ effort. S/M/L = effort. Each maps to a v1.0.0 workstream.

### Tier 1 — cheap, high-leverage robustness/efficiency (do first, P0.2)

1. **Tool-failure loop guard** (S) — *openclaude `toolFailureLoopGuard.ts`, opencode `doom_loop`.*
   Track tool failures on a few signatures (per (tool, error-category), per-turn, per-path); trip at
   a threshold (default 3) into a terminal `ToolFailureLoop` outcome that surfaces "stuck on X — check
   perms/schema" instead of letting a model burn turns/quota re-failing the same edit. Forge has a
   shell-error interceptor but no cross-turn failure dedup/abort. **Direct quota + reliability win.**
   Sketch: a `ToolFailureTracker` on `Session` in forge-core's tool-invoke path; normalize errors to a
   small `ErrorCategory` enum; reset (tool,cat) on success.

2. **Repeated-identical-call (doom-loop) detection** (S) — *opencode.* Subset/cousin of #1: hash
   `(tool_name, normalized_args)` into a small ring buffer; N identical consecutive calls → Ask or a
   hard nudge. Catches a model stuck re-issuing the exact same call. Fold into the same tracker as #1.

3. **Compaction `prune` pass** (S/M) — *opencode `compaction.prune`/`reserved`.* Before the LLM
   summarize step, drop/truncate the oldest large tool-result message parts (keep last K), governed by
   a `reserved` token budget from the token gauge. Reclaiming context by pruning bulky tool output is
   **free** (no model call) vs Forge's summarize-only compaction. Pairs with the token gauge.

4. **`.env`-denied-by-default + `external_directory` gate** (S) — *opencode permissions.* Deny reads/
   writes of `*.env` and paths outside the project worktree by default (allowlist to override). Closes
   a real secret-leak / exfiltration footgun. Routes through Forge's temper gate, not around it.

5. ~~**Provider-aware subagent fan-out cap**~~ **DONE (#238).** `[mesh.subagents] max_per_provider`
   (default 2): each child acquires a per-provider semaphore (keyed by `provider_of(routed_model)`)
   in addition to the global `max_concurrency`, so a burst of children routed to one subscription/key
   is throttled while different providers still run in parallel. Acquired provider-first to avoid
   head-of-line blocking; `0` disables. Unit-tested (serializes same-provider; parallel when off).

### Tier 2 — medium, structural quality (P0.2 → P1)

6. **Unified `LoopOutcome` enum** (M) — *openclaude `transitions.ts`.* One enum resolved every turn
   iteration (`Completed | MaxTurns | PromptTooLong | ToolFailureLoop | ProviderFallback(Model) |
   CompactRetry | MaxTokensEscalate | BudgetContinue | Nudge | NextTurn`); a single `match` decides
   terminate-vs-loop. Folds Forge's scattered failover/compaction/cap-retry recovery into one
   auditable, unit-testable place (synthetic outcomes, no live model). Enables #1, #7 cleanly.

7. ~~**Direct-path goal-verification gate**~~ **DONE (#237).** Extracted to one shared completion
   authority (`completion_gate` pure fn + `Session::run_completion_gate`) used by both the CLI-bridge
   and direct-API arms of `run_model_loop`; a direct model marking every task Done without inspecting
   real state is now gated identically to a bridge. Unit-tested; the bridge arm calls the same helper.

8. **Token-budget continuation w/ diminishing-returns stop** (M) — *openclaude `tokenBudget.ts`.*
   When a turn used < ~90% of budget and emitted no tool calls but the goal isn't verified, nudge to
   continue; stop when `continuation_count ≥ 3 && Δtokens < 500`. Catches premature "I'm done" *and*
   stalls. Pairs with #7 and compaction (compact first, then nudge).

9. **Two-phase context pipeline + UI-only message class** (M) — *pi `transformContext`/`convertToLlm`.*
   A `ContextTransform` seam run before every provider call: `prune_and_inject(&mut Vec<Message>)` then
   `to_llm(&[Message])`, with a `visibility: {Llm, UiOnly}` tag so token-gauge notes, plan cards, and
   tool-detail blocks stop polluting the prompt. Turns Forge's growing pile of injected context
   (lattice, MCP, gauge) into a disciplined, testable injection point.

10. ~~**Parallel tool execution**~~ **ALREADY SHIPPED.** `run_model_loop` detects a batch of ≥2
    independent `SideEffect::ReadOnly` calls (and no hooks configured) and runs them via
    `run_readonly_batch`: serial preflight (announce + permission resolve, no prompt for ReadOnly),
    then `join_all` the executes concurrently, then append results in original order. Side-effect /
    hook-bearing batches stay on the serial `invoke_tool` path. Exactly the proposed design.

11. ~~**Finish hooks: rewrite/inject phases**~~ **DONE (#239).** The `[[hooks]]` runner now parses a
    structured directive on a hook's exit-0 stdout: `{action:"rewrite",args}` / `{action:"inject",
    context}` / `{action:"block",reason}` / `{action:"allow"}`, for both `PreToolUse` and
    `PostToolUse`. `inject` queues model-visible context (via `pending_hints`) — the first way a hook
    can *teach* the model, not just gate it. Back-compatible: a bare object still rewrites args,
    plain text is a note, non-zero still blocks. Unit-tested; the P3 "complete hooks" workstream.

### Tier 3 — larger, differentiating (P1 → P4, stage later)

12. **Persistent re-addressable subagents** (M) — *pi + openclaude coordinator.* Workers stay alive;
    a coordinator `SendMessage`s follow-ups instead of re-spawning (keep the depth-1 guard). Unlocks
    iterative coordinator→worker refinement without losing worker context. Extends `spawn_agents` to
    return an `agent_id` + retain the `Session`.

13. **Session branching / fork-and-continue (`/tree`)** (M) — *pi.* Add a `parent_id` to the message
    table; a `forge tree` / TUI overlay to pick any past node, continue from there, switch branches.
    Forge replay is read-only; branching adds A/B exploration + bad-turn recovery. Reuses
    `Store::load_replay` for the read side. Fits the P4 TUI workstream.

14. **Snapshot + `/undo` of file edits** (M) — *opencode `snapshot`.* Per-step file checkpoint (git
    stash/worktree, dovetails with existing worktree isolation) so an agent's edits can be reverted.
    Forge has read-only replay, not file-state rollback. Expose `/undo` in the TUI.

15. **Fine-grained per-command permission globs** (M) — *opencode `bash:{"git *":"allow","rm *":
    "deny"}` last-match-wins.* A rule table evaluated *after* the temper mode resolves: parse the bash
    command, match ordered `pattern → allow|ask|deny` globs. Temper sets the baseline; globs are the
    precise override → fewer prompts at equal safety. (Complements #4.)

16. **Headless server + SSE event bus / RPC embed mode** (L) — *opencode `serve`, pi `--mode rpc`.*
    Extract the run-turn engine behind an axum HTTP+SSE service (or an LF-delimited JSON stdio RPC
    loop), make the TUI one client, gate with token/basic auth. Forge *consumes* other agents
    (CLI-bridge) but isn't cleanly *embeddable* — this inverts it and unlocks IDE/editor/mobile
    integration. Biggest unlock, heaviest; reuse the `mcp_serve` plumbing + existing event enum. Stage
    after Tiers 1–2 land.

---

## Explicitly do NOT copy
- **Hosted gateways / model proxies as the default path** (opencode Zen, openclaude Opengateway) —
  antithetical to Forge's self-hosted mesh; adds infra cost + a privacy/vendor surface.
- **Uploading real dev sessions as shareable/training data** (opencode `share`, pi `pi-share-hf`) —
  privacy/IP liability; only ever strictly opt-in + redacted.
- **A JS/WASM in-process plugin runtime** (pi `jiti` extensions, opencode/openclaude JS plugins) — in
  Rust it means a sandbox-escape surface Forge doesn't need; take the *capabilities* (replace-a-tool,
  loop hooks, richer event points) via the existing config/MCP/hook seams instead.
- **openclaude `remoteBridgeCore.ts`** — drives Anthropic's private cloud `/v1/code/sessions` via
  reverse-engineered worker-JWT/SSE; fragile + ToS-gray. Forge's CLI-bridge already gets subscription
  access legitimately.
- **Reconstructed Claude-Code internals** (`CLAUDE_CODE_*` env, `isAnt`, `feature()` idioms) — steal
  the idea (the loop-outcome enum, the failure guard), never the minified-reconstruction code.

---

## Net
The fastest wins (Tier 1) are robustness/efficiency plumbing that pays off immediately in fewer wasted
turns and protected quota — **tool-failure loop guard** first. The structural Tier-2 items
(`LoopOutcome` enum, direct-path goal verification, two-phase context pipeline) make the harness
auditable and testable offline. The one genuinely transformational gap is the **headless server / RPC
embed mode** (Tier 3) — staged last, after the cheap robustness wins are banked and benchmarked.
