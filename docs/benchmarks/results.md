# Forge — measured results

Forge's single most important job is the **harness**: be the best coding-agent harness, proven with
metrics, on BOTH API and bridge/subscription models. This file records what has been **measured and
reproduced** so far. Two kinds of evidence:

1. **Deterministic, in-repo proofs** (run in CI, no external setup) — locked down by tests.
2. **SWE-bench resolve-rate** — the gold-standard external comparison; methodology in
   [`swe-bench.md`](./swe-bench.md). Requires Docker + the `swebench` evaluator, so it is run
   deliberately (see that doc) rather than in CI.

---

## 1. Bridge subscription superiority — token efficiency ✓ MEASURED

**Claim:** running a model *through Forge's bridge* sends far fewer tokens than the naive bridge
would, by reusing the CLI's own session instead of re-streaming the whole transcript every turn.

**Result:** for a realistic multi-step harness turn (a 4000-char system preamble + accumulating
assistant/tool turns over **6 re-drives**), bridge session-resume sends:

| | prompt bytes streamed to the CLI |
|---|---|
| resume **off** (re-render full transcript each call) | **≈ 59,706** |
| resume **on** (send only the new delta) | **≈ 4,221** |
| **saving** | **≈ 92% fewer** |

The gap **widens** as the transcript grows: resume-off is ~quadratic in turns (it re-sends
everything each time), resume-on is ~linear in new content.

**Why measured this way:** claude's own token accounting *hides* the saving — it prompt-caches the
repeated transcript, so its reported `input_tokens` barely moves. The real, Forge-controlled cost is
the bytes serialized + streamed to the CLI's stdin, which is what this measures.

**Reproduce (deterministic, no network):**
```bash
cargo test -p forge-provider resume_sends_dramatically_fewer_prompt_bytes_over_a_turn -- --nocapture
```

**Correctness, live-verified** (real CLIs, both bridges):
```bash
cargo test -p forge-provider e2e_claude_resume_preserves_context_across_calls -- --ignored
cargo test -p forge-provider e2e_codex_resume_preserves_context_across_calls  -- --ignored
```
Each drives two real turns; the *resumed* turn recalls a fact established in turn 1 while only the
delta was sent — proving context is preserved end-to-end. (Claude `--resume <id>`, Codex
`exec resume <id>`; both ship in v0.4.9/v0.4.10.)

---

## 2. Harness correctness — conformance suite ✓ PROVEN

The harness's reliability guards are proven by **deterministic end-to-end tests** that drive the
real run-loop with scripted mock providers (no model, no network) and assert the loop's behavior.
This is the "best harness" claim's correctness half — and writing these tests already caught a real
bug (the direct-path verification gate silently failing, fixed in v0.4.6).

| Guarantee | What's asserted | Test |
|---|---|---|
| No phantom "done" (direct + bridge) | a model claiming every task done must PROVE it with a real tool-grounded check, else re-drive / flag UNVERIFIED | `direct_gate_*`, `bridge_completion_*`, `verification_reopens_*` |
| Doom-loop halt | identical tool call repeated → nudge then HALT, not run to the cap | `doom_loop_halts_a_model_repeating_the_same_call` |
| Failure-loop halt | same error KIND across differing args → halt where the doom-loop can't see it | `failure_loop_halts_a_model_failing_the_same_way` |
| Empty-response | bounded nudges then stop — never spin forever | `empty_response_is_nudged_then_stops_not_loops` |
| Tool-call-as-text | narrated `<invoke>` that didn't execute → nudge, then end loudly (no phantom success) | `tool_call_written_as_text_never_silently_succeeds` |
| Stall / no-progress | a stalled stream times out + fails over; a no-progress bridge halts loudly | `stalled_stream_*`, `cli_bridge_no_progress_stall_*` |

**Reproduce:** `cargo test -p forge-core` (all run in CI on every PR).

The report tooling that produces the SWE-bench efficiency comparison is itself tested for arithmetic
+ honesty (a partial token capture can never understate tok/success): `cargo test -p forge-cli
summarize_agent tok_per_success`.

---

## 3. SWE-bench resolve rate — methodology ready, run deliberately

The external gold standard: same instances through `--agent forge` vs `--agent claude-code` /
`--agent codex`, scored by the official evaluator, comparing **resolve rate AND tokens-per-success**.
Full runbook in [`swe-bench.md`](./swe-bench.md) (`forge bench swe` → `swebench` evaluator → `forge
bench report`). It needs Docker + multi-GB per-instance images + real model calls, so it is run as a
deliberate, supervised session rather than in CI.

> Record the resulting numbers here (replace this note) once a run completes — the `forge bench
> report` table for `--agent forge --model <bridge-id>` vs `--agent claude-code`, on the same model
> family, is the headline bridge-superiority result.

---

## Appendix — prediction-pipeline verified + a single-task token smoke

The `forge bench swe` **prediction** pipeline (clone → real agent turn → capture patch) is verified
end-to-end for **both** agent paths on a trivial real instance (`octocat/Hello-World`, "add a
GREETING.txt"). Running it for the first time also **caught a real bug** — the patch was captured
with a plain `git diff`, which drops untracked files, so any *new-file* solution scored as an empty
patch (fixed in v0.4.19; now `git add -A` + `git diff --cached`).

Observed token cost for that one task, **same underlying model (haiku)**:

| agent | total tokens | patch |
|---|---|---|
| `--agent forge --model claude-cli::haiku` (Forge bridge) | **793** | ✓ 8 lines |
| `--agent claude-code --model haiku` (Claude Code's own CLI) | **46,124** | ✓ 8 lines |

**Read this honestly:** this is **one trivial task**, so the gap is dominated by *fixed
per-invocation overhead* — Forge's harness disables claude's built-in tools (`--tools ""`) and
serves a focused `mcp__forge` toolset, whereas `claude -p` ships its full system prompt + every
built-in tool schema every run. On harder, multi-step tasks that overhead is amortized over more
work, so the ratio **shrinks**. It is **not** a resolve-rate result and must not be quoted as a
headline efficiency number. It *does* confirm (a) both comparison paths work end-to-end and (b) the
direction of the bridge-efficiency thesis. The representative figure is the multi-instance
`forge bench report` table from a full, Docker-scored run (see §3 / `swe-bench.md`).
