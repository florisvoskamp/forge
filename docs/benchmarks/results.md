# Forge — measured results

Forge's single most important job is the **harness**: be the best coding-agent harness, proven with
metrics, on BOTH API and bridge/subscription models. This file records what has been **measured and
reproduced** so far — honestly, including where the measurement turned out to contradict the goal.

> ## ⚠️ Correction (2026-06-26): the earlier "bridge token-efficiency superiority" claim was wrong
> An initial measurement appeared to show Forge-through-the-bridge using **far fewer tokens** than the
> raw `claude`/`codex` CLI. **That was an accounting bug**, not a real win: the bridge recorded only
> claude's *uncached* `input_tokens` and dropped `cache_read`/`cache_creation` tokens, while the
> raw-CLI metric counted them — so Forge looked ~10–150× cheaper purely from counting less.
> Once **both** sides count input + cache-reads + cache-writes (fixed in this version), a fair
> re-measure on a real SWE-bench instance (`psf__requests-1963`, same `sonnet` model) shows the
> **opposite**:
>
> | | Forge-on-bridge (harness) | claude-cli direct |
> |---|---|---|
> | total tokens | **303,823** | **158,837** |
> | wall time | 163s | **40s** |
>
> Forge's MCP-per-turn harness currently costs **~1.9× the tokens and ~4× the wall time** of the
> native CLI loop (each MCP tool round-trip re-processes the growing context). On resolve rate it is
> **tied** (1/3 vs 1/3 on a 3-instance Lite subset — too small to be conclusive either way).
> **Conclusion: Forge does not yet beat the raw CLIs on efficiency or resolve rate.** Closing that gap
> (fewer/batched MCP round-trips, aggressive context pruning, real per-turn resume during the loop) is
> open work, not a settled result. Nothing below should be read as "bridge superiority is proven."

---

## 1. Bridge session-resume reduces *bytes streamed to the CLI* (an internal optimization)

**What this is — and is NOT.** This measures the bytes Forge serializes to the CLI's stdin with
session-resume on vs off. It is a **Forge-vs-Forge** optimization (don't re-stream the whole
transcript each `complete()` call); it is **not** a token or subscription-usage comparison against
the raw CLI, and it does **not** by itself mean Forge uses fewer billed tokens (claude prompt-caches
the repeated transcript, so the byte saving largely does not translate to token savings — see the
correction above).

| (4000-char preamble + accumulating turns over 6 re-drives) | prompt bytes streamed to the CLI |
|---|---|
| resume **off** (re-render full transcript each call) | ≈ 59,706 |
| resume **on** (send only the new delta) | ≈ 4,221 |

**Reproduce (deterministic, no network):**
```bash
cargo test -p forge-provider resume_sends_dramatically_fewer_prompt_bytes_over_a_turn -- --nocapture
```

**Correctness, live-verified** (real CLIs, both bridges) — resume preserves context end-to-end:
```bash
cargo test -p forge-provider e2e_claude_resume_preserves_context_across_calls -- --ignored
cargo test -p forge-provider e2e_codex_resume_preserves_context_across_calls  -- --ignored
```
Each drives two real turns; the *resumed* turn recalls a fact from turn 1 while only the delta was
sent. (Claude `--resume <id>`, Codex `exec resume <id>`.) This is a real, working feature — it just
isn't, on its own, evidence that Forge beats the raw CLI on tokens.

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
| Untrusted-input fuzz | adversarial model output / bridge stdout / prompt-cap never panic + hold their contracts | `recovery_never_panics_*`, `bridge_line_parsers_never_panic_*`, `clamp_to_chars_never_*` |

**Reproduce:** `cargo test -p forge-core -p forge-provider` (all run in CI on every PR).

---

## 3. SWE-bench resolve rate — first real run (N=3, directional only)

Same instances through `--agent forge --model claude-cli::sonnet` (Forge harness on the claude bridge,
harness mode, forced model — no mesh) vs `--agent claude-code --model sonnet` (claude's own CLI),
scored by the official `swebench` Docker evaluator. Runbook: [`swe-bench.md`](./swe-bench.md).

**First run — SWE-bench Lite, 3 instances (requests, flask, pylint):**

| | resolved | tokens on the shared solve (pylint) | wall (pylint) |
|---|---|---|---|
| Forge-on-bridge | **1/3** | 11,718 out | 301s |
| claude-cli direct | **1/3** | **2,405 out** | **49s** |

Both solved `pylint-5859`; both failed `requests-1963` and `flask-4045` (incomplete fixes — the model
under-scoped the issue; e.g. flask: got the blueprint-name dot check, missed the *endpoint* dot
check). **Tied on resolve, and Forge was less efficient** (see the correction banner for the
fair full-token numbers). N=3 is far too small to conclude — it is a directional baseline, not a
verdict. A larger run (10–20+ instances) is needed before any resolve-rate claim.

---

## Appendix — the single-task token smoke was the source of the artifact (kept as a caution)

An earlier note here reported, for one trivial task (`octocat/Hello-World`), Forge-bridge **793**
tokens vs claude-code **46,124** — and read it as evidence of the efficiency thesis. **That number is
invalid** for the reason in the correction banner (the 793 dropped claude's cache-read tokens; the
46,124 counted them). It is retained only as a documented caution: do not compare token totals across
the bridge and the raw CLI unless both count cache reads + writes identically (now enforced in
`cli_provider::usage_from` and `bench::parse_external_usage`). The smoke *did* legitimately catch a
real bug — the patch was captured with a plain `git diff`, dropping new files, so any new-file
solution scored empty (fixed in v0.4.19; now `git add -A` + `git diff --cached`).
