# Forge — measured results

Forge's single most important job is the **harness**: be the best coding-agent harness, proven with
metrics, on BOTH API and bridge/subscription models. This file records what has been **measured and
reproduced** so far — honestly, including where the measurement turned out to contradict the goal.

## Re-confirmation on the build current at the time (2026-06-28, v0.4.65 — since superseded by v1.0.0–v2.0.0)

The headline comparison below (§ "Firming run — N=20") was measured on **v0.4.39** — ~26 releases
old at that point. Re-ran the same setup on the then-current build to check it still held after all
the session-5/6 changes: 10 SWE-bench Lite instances (`sub10b`), `claude sonnet`, **same model both
arms**, Forge's loop-gated-completeness config (`cfg-aggr`), scored by the official `swebench`
Docker evaluator.

| (build current at the time, N=10, same `sonnet`) | resolved | total tokens | **tokens / resolve** |
|---|---|---|---|
| claude-cli direct | 4/10 | 14.28M | 3.57M |
| **Forge (loop-gated)** | **6/10** | 16.96M | **2.83M** |

**The win holds — and is cleaner here than at N=20.** Forge **strictly dominates**: every instance
the raw CLI solved, Forge also solved (`django-10914`, `pytest-5103`, `scikit-learn-10297`,
`sphinx-11445`), **plus two the CLI did not** (`seaborn-2848`, `pylint-7080`) — **0 CLI-only solves**.
Forge is **~21% cheaper per resolve** (2.83M vs 3.57M); total tokens are 1.19× because it does more
total work (6 solves vs 4).

**Honest caveats:** N=10 is small (the ~21% per-resolve gap is noisier than the N=20 ~11%; the
*direction* is consistent across both). `psf__requests-2317` hung in its test container on **both**
arms and was excluded as an error on each side (so the comparison stays fair). One raw-CLI token
metric was incomplete (the errored instance). This is a signal that the build current at the time
did **not regress** the headline, not a fresh large-sample proof.

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
| Completion-gate decision table | the pure completion authority returns the right outcome for all 4 input cases (Reverify / AcceptClean / AcceptNoArtifacts / AcceptUnverified) | `completion_gate_covers_its_four_outcomes` |
| Doom-loop halt | identical tool call repeated → nudge then HALT, not run to the cap | `doom_loop_halts_a_model_repeating_the_same_call` |
| Oscillation halt | a non-consecutive `A,B,A,B` tool-call ping-pong (evades the consecutive doom-loop AND the failure-loop) → halt | `doom_loop_halts_a_model_oscillating_between_two_calls` |
| Failure-loop halt (serial **and** concurrent) | same error KIND across differing args → halt; now also when the calls run as a concurrent read-only batch | `failure_loop_halts_a_model_failing_the_same_way`, `concurrent_batch_failure_loop_is_caught` |
| Step cap | a runaway turn that always wants another tool call stops at `max_steps`, never spins | `step_cap_halts_a_runaway_turn` |
| Autofix self-heal cap | the lint/test self-heal loop stops at `max_iterations` when checks never pass, never spins | `autofix_iteration_cap_halts_the_self_heal_loop` |
| Empty-response | bounded nudges then stop — never spin forever | `empty_response_is_nudged_then_stops_not_loops` |
| Tool-call-as-text (direct) | narrated `<invoke>` that didn't execute → nudge, then end loudly (no phantom success) | `tool_call_written_as_text_never_silently_succeeds` |
| Tool-call-as-text (**bridge**) | a bridge model that writes a tool call as prose → recovered + executed (no 553× spiral), and NOT double-executed if the CLI already ran it natively | `recovers_prose_tool_call_the_bridge_did_not_execute`, `prose_recovery_skipped_when_cli_ran_a_native_tool` |
| Untrusted-input fuzz / no panics | adversarial model output (incl. malformed `<parameter>` tags), bridge stdout, prompt-cap never panic + hold their contracts | `recovery_never_panics_*`, `malformed_parameter_tag_does_not_panic`, `bridge_line_parsers_never_panic_*`, `clamp_to_chars_never_*` |

**Reproduce:** `cargo test -p forge-core -p forge-provider` (all run in CI on every PR).

**Why this is the differentiator.** Each row is a failure mode a real coding model produces on a real
task (we hit the 553× prose-spiral and the malformed-`<parameter>` panic in actual SWE-bench runs).
A raw `claude`/`codex`/`aider` loop has no equivalent guard for most of these — it either spins to a
token-budget death, crashes the turn, or "succeeds" having done nothing. Forge's harness turns each
into a bounded, observable, test-pinned outcome. That is the concrete, reproducible sense in which
Forge is a better *harness* than the CLI it wraps — not a claim, a suite you can run.

---

## 3. SWE-bench resolve rate — Forge-on-bridge vs claude-cli (N=10→20, fair accounting)

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

| | resolved | total tokens | **tokens / resolve** |
|---|---|---|---|
| claude-cli direct | 4/10 | 3.97M | 993k |
| Forge front-loaded (no completeness) | 4/10 | 3.72M | 930k |
| Forge + completeness, open-ended | 6/10 | 11.3M | 1.88M |
| Forge + completeness, bounded preamble | 6/10 | 6.86M | 1.14M |
| **Forge + completeness, loop-gated (shipped)** | **6/10** | **5.53M** | **922k** |

So Forge **beats the raw CLI on resolve (6 vs 4)** — gaining `requests-2148` and `pytest-11148` (the
latter claude-cli bailed on instantly). Three forms of the completeness check were measured; the one
that ships is **loop-gated** — fired ONCE at turn-end by the core run-loop (the model works the turn
normally, then does a single bounded `git diff` review), not an always-on preamble clause. It holds
the full 6/10 win at **5.53M tokens** — the cost premium fell **3× → 1.85× → 1.39×** (open-ended →
bounded-preamble → loop-gated). Default-off (`mesh.verify_completeness`).

**The headline, fairly stated:** per **resolve**, Forge is now **922k vs claude-cli's 993k — ~7%
cheaper — while solving 50% more bugs (6 vs 4).** So Forge-on-bridge is genuinely better than the raw
CLI on *both* axes that matter: more solved, *and* lower cost per solve. Total tokens are 1.39× because
it does more total work (solving 6 vs 4). (N=10 — a clean fair-accounted signal, not a large-sample
proof; the per-resolve edge is small and would firm up or soften with more instances.)

**Firming run — N=20 (the N=10 above + 10 fresh instances).** Same arms, 10 new Lite instances
(requests/flask/pylint/sphinx/pytest/scikit-learn/sympy/django/seaborn/xarray), scored by the same
official evaluator:

| (N=20, same model, loop-gated completeness) | resolved | total tokens | **tokens / resolve** |
|---|---|---|---|
| claude-cli direct | 9/20 | 12.16M | 1.35M |
| **Forge loop-gated** | **11/20** | 13.18M | **1.20M** |

The win **holds and gets cleaner at N=20**: Forge resolves **11 vs 9** (3 Forge-only solves —
`pylint-5859`, `pytest-11148`, `pytest-5103`; 1 claude-cli-only — `requests-2317`), at **~11% lower
cost per resolve** (1.20M vs 1.35M), and **total tokens fall to 1.08×** — near parity, down from the
N=10 1.39× (the second batch's per-instance token use was more comparable). **Honest caveat:** on the
*new* 10 instances alone, the two arms **tied 5/5** — the +2 net edge comes from the first batch, so
the resolve advantage is real but **modest** (+2 of 20). The direction is consistent across both
batches (Forge never behind on either), but a still-larger N would tighten the resolve margin.

`forge bench swe` also bounds the in-process Forge turn by `--timeout-secs` (was unbounded → one
instance ran 22 minutes).

**Structural gap — per-step MCP latency — and the batch-tools experiment that backfired (v0.4.40–44).**
The remaining gap is median per-step MCP latency (Forge ~3× claude-code's in-process tool steps),
because every bridge tool call is a round-trip that re-processes the growing context. Two affordances
were added to attack it — batch `read_file` (a `paths` array) and `search context:N` (grep -C lines) —
plus a harness-preamble nudge steering the bridge model to use them. A 2-instance A/B suggested the
nudge cut tool round-trips ~21% (23 → 18) but left **total tokens flat** (617k → 619k): already only a
latency, not a token, signal.

**Then a 10-instance run exposed the real cost, and the experiment was reverted.** Steering the model
toward `paths`/`context` calls measurably increased how often it emitted them as *prose*
(`<function_calls><invoke>` text) instead of native `tool_use`. On the bridge those never execute, and
the model — seeing no result — repeated them: **553 unexecuted `<function_calls>` on a single
instance**, contaminating the run. Root cause: the bridge path never recovered prose tool calls (the
direct path always had). Fix (v0.4.44): the bridge now runs `recover_text_tool_calls` so a prose call
executes and the loop re-drives, and the nudge was reverted (unproven benefit, real spiral cost). The
batch capabilities remain for native use; Forge no longer steers toward them. **Honest takeaway: the
nudge was a net-negative experiment — reverted. The durable wins are the prose-fallback recovery and
the v0.4.42 oscillation guard, which make Forge's bridge strictly more robust to malformed model
output than a naive one-shot bridge** (both proven by deterministic tests:
`recovers_prose_tool_call_the_bridge_did_not_execute`, `doom_loop_halts_a_model_oscillating_between_two_calls`).
The per-step-latency gap itself remains open for the P1 persistent stream-json transport.

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
