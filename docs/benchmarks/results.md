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

## 3. SWE-bench resolve rate — Forge-on-bridge vs claude-cli (N=10, fair accounting)

`--agent forge --model claude-cli::sonnet` (Forge harness on the claude bridge, harness mode, forced
model — no mesh) vs `--agent claude-code --model sonnet` (claude's own CLI), same instances, scored by
the official `swebench` Docker evaluator. Runbook: [`swe-bench.md`](./swe-bench.md).

**SWE-bench Lite, 10 instances (requests/flask/pylint/sphinx/pytest ×2), aggressive front-loading on
(`lattice.inject_body_hits = 14`):**

| | resolved | total tokens | avg wall |
|---|---|---|---|
| Forge-on-bridge | **4/10** | **3.72M** | 295s |
| claude-cli direct | **4/10** | 3.97M | **66s** |

**Honest verdict: roughly parity.** Tied on resolve (4/4 — and on *different* instances: Forge
uniquely solves `pylint-5859` with **231k** tokens where claude-cli **fails after 834k**; claude-cli
uniquely solves `requests-2148`). Tokens within ~6% (Forge lower in aggregate, but with large
per-instance variance both ways). Forge is **slower** on average — dominated by two non-converging
instances (547s and 1341s) that made **500+ and 330+ tool calls**; strip those and Forge averages
~130s. The slowness is per-call MCP latency × an unbounded explore/fix loop, not the protocol itself.

**What moved the needle: context front-loading.** Injecting the top task-relevant symbol *bodies* into
the prompt (so the model reads from context instead of `search`/`read_file`-ing) took Forge from
**~1.9× *worse* tokens** (conservative 3-body default) to **parity** here. Caveat learned the hard way:
on a 3-instance light-repo subset this looked like a **44% token win** — it did **not** generalize to
the heavier repos. Small-N benchmarks mislead; trust N≥10.

**Beating the raw CLI on resolve — `mesh.verify_completeness` (opt-in).** Both Forge and claude-cli
failed the same N=10 instances on **under-scoped fixes** (flask: got the blueprint-name dot-check,
missed the *endpoint* one). The raw CLI has no completeness self-check; Forge's harness adds one — an
opt-in preamble clause that makes the model re-read the request and verify its change against EVERY
requirement before finishing. Measured (same 10 instances, same model, only the clause changed):

| | resolved | total tokens |
|---|---|---|
| claude-cli direct | 4/10 | 3.97M |
| Forge front-loaded (no completeness) | 4/10 | 3.72M |
| Forge + completeness (open-ended) | 6/10 | 11.3M |
| **Forge + completeness (bounded, shipped)** | **6/10** | **6.86M** |

So Forge **beats the raw CLI on resolve (6 vs 4)** — gaining `requests-2148` and `pytest-11148` (the
latter claude-cli bailed on instantly). The clause that ships is the **bounded** form (one `git diff`
review pass, no re-exploration): it holds the full 6/10 win at **6.86M tokens — 39% cheaper than the
open-ended version** (11.3M) it replaces. Default-off (a quality-for-cost trade).

**Honest on the remaining cost:** 6.86M is still ~1.85× claude-cli's 3.97M *total*. Per **resolve** the
gap is small — Forge ~1.14M/solve vs claude-cli ~0.99M/solve. So the honest claim is: **Forge resolves
50% more bugs than the raw CLI (6 vs 4) at a ~15% higher per-solve token cost.** A real solve-rate win
at a modest premium, not a free one. (N=10 — a clean fair-accounted signal, not a large-sample proof.)

**Still open** toward winning resolve at *parity total tokens*: the completeness pass still does extra
work; a loop-gated one-shot version (fires once at turn-end vs living in the preamble all turn) is the
next lever. The median-speed gap (per-step MCP cost) is structural. `forge bench swe` now bounds the
in-process Forge turn by `--timeout-secs` (was unbounded → one instance ran 22 minutes).

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
