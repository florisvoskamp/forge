# Harness Competitive Analysis — Tier 2 (v0.3.0)

> **See also:** [competitor-gap-analysis.md](competitor-gap-analysis.md) — the v1.0.0 (P0.2) refresh
> covering opencode / openclaude / pi with a ranked, no-defer adoption backlog (tool-failure loop
> guard, `LoopOutcome` enum, compaction prune, direct-path goal verification, two-phase context
> pipeline, headless/RPC embed mode).

Goal: make Forge's agentic harness the best available. This captures a recon of two competing
open-source coding-agent harnesses, what Forge already does better, and a prioritized backlog of
techniques worth porting. Sources were read directly from their repos (2026-06-23):

- **`anomalyco/opencode`** — TypeScript/Bun; two harness generations (v1 AI-SDK, v2 Effect-native).
- **`Gitlawb/openclaude`** — TypeScript Claude-Code clone; `src/query.ts` main loop.

## What Forge already leads on

Confirmed against both repos — do **not** re-port these:

- **Cross-provider / cross-model failover.** opencode retries the *same* model on error (only
  credential `orElse`); it has no cross-provider failover or task-aware routing. Forge's mesh
  routing + capability-failover + subscription conservation is genuinely ahead.
- **Structural depth-1 subagent guard.** Both use the same structural (not numeric) recursion guard
  Forge already has.
- **Post-edit auto-fix loop.** Forge has `run_autofix_stage` / `edits_this_turn`; openclaude's
  `autoFixRunner` is the same idea.
- **Header-aware rate-limit waits, capability-error handling, per-model health bench.** Already in
  Forge (`model-health-failover`, `capability-failover`).

## Verified findings from the SWE-bench bring-up

Running `forge bench swe` against SWE-bench_Lite with a local API model (`ollama::qwen3-coder:30b`,
which exercises **Forge's own loop**, unlike the claude-code/codex CLI bridges) surfaced three real
issues — two now fixed in this PR:

1. **FIXED — `forge bench swe` was broken for a relative `--workdir`.** `prepare_repo` cloned with
   `current_dir(root)` while passing the full relative target path, so the clone double-nested to
   `root/root/<id>` and the subsequent checkout failed with `os error 2`. The default workdir
   (`.forge/swe-bench`) is relative, so the command failed out of the box. Now absolutizes the
   workdir first. (`crates/forge-cli/src/bench.rs`)

2. **FIXED — Forge couldn't drive models that emit Hermes/Qwen-style `<tool_call>` XML.** genai's
   native Ollama adapter leaves those tool calls unparsed; they leaked into message text and the
   turn dead-ended with "empty response" (0 tool calls). Ollama's own `/v1` OpenAI-compatible
   endpoint parses them into structured `tool_calls`, so we now retarget `ollama::` through `/v1`
   with the OpenAI adapter (same resolver pattern as Cerebras). Verified: tool calls are now
   captured and executed. Unlocks the entire Qwen family + any ollama tool-calling model.
   (`crates/forge-provider/src/genai_provider.rs`)

3. **ENVIRONMENTAL — ollama's default `num_ctx` is 4096.** Forge's system prompt + lattice context
   fills it; after a tool result the context overflows and ollama truncates, dropping the plan, so
   the model emits empty on the *second* step. Not a Forge bug — raise `OLLAMA_CONTEXT_LENGTH` (or
   load the model with a larger `num_ctx`) when benchmarking local models. A real benchmark number
   should use a model with a genuine context window.

> Takeaway for the headline benchmark: pin an **API model with native tool-calling and a real
> context window**; local `qwen3-coder:30b` at default ctx is not a viable bench subject.

## Prioritized backlog (highest leverage first)

### Tier A — edit & loop reliability (biggest success-rate levers)

1. **DONE (this PR) — anchored-block fuzzy edit tier + disproportionate-match guard.** Match a
   block's first/last lines as anchors, replace the unique span between them even if the interior
   was paraphrased; reject if the span balloons past `old` (≥ max(old+3, old*2) lines / max(+500,
   *4) chars). Forge had exact + whitespace tiers only; this adds the single biggest edit-success
   lever both competitors rely on (opencode's BlockAnchor replacer, openclaude's indentation-flex
   matcher). (`crates/forge-tools/src/core_tools.rs`)
2. **Continuation / empty-response nudge.** When a model returns no text and no tool call, Forge
   stops the turn. Both competitors instead inject a "continue with the task" nudge (bounded count)
   and retry — recovering the narrate-then-stall and transient-empty failure modes. Small change at
   the `wants_tools()` dead-end in `forge-core`'s loop.
3. **Tool-failure loop guard (doom-loop).** Halt or re-route after N identical failing tool calls
   (keyed on tool + error category + path); any success resets the counter. opencode
   `DOOM_LOOP_THRESHOLD=3`, openclaude `toolFailureLoopGuard`. Kills the classic agent death-spiral.
4. **Tool-call repair.** On a malformed/invalid call, return a correctable error to the model
   (rename near-miss tool names, coerce known arg mistakes) instead of failing the turn.

### Tier B — context efficiency

5. **Zero-LLM context prune / microcompaction.** Reclaim context by blanking *older* completed
   tool results in place (protect the most recent N turns + a recent-output window) without a
   summarization round-trip. opencode `prune`, openclaude `microCompact`. Pairs with Forge's
   existing `/compact`; defers expensive full compaction.
6. **Anchored incremental compaction summary.** A fixed-section template (Goal/Constraints/Progress/
   Decisions/NextSteps/Files) updated in place across compactions, with verbatim user-message quotes
   — the load-bearing anti-drift mechanism in both repos.
7. **Spillover truncation with recovery hint.** Cap any tool result, write the full output to disk,
   return a preview + how to recover (Grep/Read with offset, or delegate to a subagent).

### Tier C — verification & orchestration

8. **Baseline-diffed LSP diagnostics.** Surface only edit-*introduced* errors (snapshot before the
   edit, diff after) so the model isn't blamed for pre-existing noise; cap per-file/total. Forge has
   an LSP/auto-lint loop; the baseline-diff refinement is the missing piece.
9. **Bounded parallel sub-agent pool.** Both competitors spawn subagents **sequentially**. A tokio +
   semaphore bounded fan-out pool is a place Forge can beat them outright.
10. **Speculative streaming tool execution.** Start a tool the moment its call-block finishes
    parsing, while the model is still streaming later tokens. Biggest wall-clock win; hardest to
    retrofit — design for it deliberately.

## A/B methodology

Every change above is measurable: re-run `forge bench swe` (steps 2–3 of `docs/benchmarks/swe-bench.md`)
before and after, same model + instances, and compare the resolved rate. That is the ground truth for
"does this make the harness better."
