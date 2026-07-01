# Why Forge is a better *harness* — the honest, test-backed case

A coding agent has two parts: the **model** (Claude, GPT, …) and the **harness** (the loop that feeds
it tools, runs them, and decides when it's done). Forge can run *the exact same model* you'd run with
`claude`/`codex` directly — through their own CLIs, as a bridge. So the only thing that differs is the
harness. This document is the honest argument, with a runnable test for every claim, that Forge's
harness is better than the loop inside the raw CLI it wraps.

> Honesty first. Forge's benchmark history includes a claim that turned out to be a token-**accounting
> artifact** (see [`../benchmarks/results.md`](../benchmarks/results.md) §0). Everything below is either
> proven by a deterministic test you can run, or labelled as a measured-but-modest / still-open result.
> Where Forge does **not** win (raw token efficiency), this says so.

## 1. The claim that matters: reliability under real model failure

Real models fail in characteristic ways on real tasks — they repeat a call, they oscillate between
two, they write a tool call as prose that never executes, they emit a malformed tag, they claim "done"
without checking. A raw `claude`/`codex`/`aider` loop handles **almost none** of these: it spins to a
token-budget death, crashes the turn, or "succeeds" having done nothing. Forge's harness turns each
into a **bounded, observable, test-pinned** outcome.

These aren't hypotheticals — we hit them in real SWE-bench runs:

| Failure mode (seen in real runs) | Raw CLI loop | Forge | Proven by |
|---|---|---|---|
| Model writes a tool call as **prose** (`<function_calls><invoke>`) instead of a native call | not executed → model loops (we observed **553×** on one instance) | recovered + executed, loop re-driven; not double-run if the CLI already ran it natively | `recovers_prose_tool_call_the_bridge_did_not_execute`, `prose_recovery_skipped_when_cli_ran_a_native_tool` |
| Malformed `<parameter>` tag in model output | — | **doesn't panic** the turn (was a real crash) | `malformed_parameter_tag_does_not_panic` + a 5000-case fuzz corpus |
| Same tool call repeated step after step | spins to the cap | nudge → halt | `doom_loop_halts_a_model_repeating_the_same_call` |
| `A,B,A,B` oscillation (evades the repeat check) | spins to the cap | halt | `doom_loop_halts_a_model_oscillating_between_two_calls` |
| Same error KIND across changing args (serial **and** concurrent) | spins to the cap | nudge → halt | `failure_loop_halts_a_model_failing_the_same_way`, `concurrent_batch_failure_loop_is_caught` |
| Runaway turn that always wants one more tool | depends on the CLI | stops at `max_steps` | `step_cap_halts_a_runaway_turn` |
| Model claims every task "done" without checking | accepted | forced to PROVE it with a real tool-grounded check, else re-driven / flagged UNVERIFIED | `direct_gate_*`, `bridge_completion_*`, `completion_gate_covers_its_four_outcomes` |
| Corrupt/truncated line mid-stream | can abort | skipped, turn survives | `truncated_stream_line_is_skipped_not_fatal` |

**Verify the whole thing in one command:**

```bash
cargo test -p forge-core -p forge-provider
```

That's **300+ deterministic tests** (no model, no network — scripted mock providers drive the real
run-loop and assert its behaviour). A raw CLI ships no equivalent guarantees because it isn't built to
be a *general* harness — it's built to drive its own model interactively. That difference is the
concrete, reproducible sense in which Forge is a better harness. See
[`../benchmarks/results.md`](../benchmarks/results.md) §2 for the full table.

## 2. Resolve rate — measured, fair-accounted, honest

On SWE-bench Lite, same model (`sonnet`), Forge-on-the-claude-bridge vs the raw `claude` CLI, scored by
the official `swebench` evaluator (N=20):

| | resolved | tokens / resolve |
|---|---|---|
| claude-cli direct | 9/20 | 1.35M |
| **Forge (loop-gated completeness)** | **11/20** | **1.20M** |

Forge resolves **more** (the completeness re-drive catches under-scoped fixes the raw CLI ships) at a
**lower cost per resolve**. Honest caveat: a second batch of 10 instances tied 5/5, so the +2 edge is
modest — this is a real but not blowout result. Full method + caveats in `results.md` §3.

**Re-confirmed on the build current at the time (2026-06-28, v0.4.65 — since superseded by
v1.0.0–v2.0.0)** — the N=20 above is from v0.4.39, ~26 releases old at that point, so the same
setup was re-run on a fresh 10-instance batch: **Forge 6/10 vs raw CLI 4/10**, Forge **strictly
dominating** (every CLI solve is also Forge's, +2 Forge-only, 0 CLI-only) at **~21% lower
tokens/resolve**. N=10 is small, but it confirms the headline did not regress at that point. See
`results.md` → "Re-confirmation on the build current at the time".

## 3. Where Forge does NOT (yet) win — stated plainly

- **Raw total-token efficiency.** Each bridge tool call is an MCP round-trip that re-processes the
  growing context, so Forge's harness takes more, smaller steps than the model's in-process loop. On
  total tokens it's roughly **parity**, not a win, and that has not changed. A batch-tools experiment
  aimed at this **backfired** (it triggered the prose spiral) and was reverted — documented honestly
  in `results.md` §3.
- **Per-turn latency — partially addressed.** The persistent claude `--input-format stream-json`
  transport **shipped (v0.4.63)**: one long-lived process across turns/re-drives removes the per-turn
  process-spawn + session-reload cost (measured **~0.88s/turn**, saved on every re-drive). Honest
  scope: it's a real per-re-drive saving that compounds, **not** a token change (both paths already
  send deltas) and **not** a headline multiplier (model inference dominates). The deeper variant —
  Forge driving the model's *inner* tool loop turn-by-turn instead of letting the CLI run it — is
  still open.

## 4. Features the raw CLIs lack (real, not aspirational)

Confirmed against competitor repos (see [`competitive-analysis.md`](competitive-analysis.md) and
[`competitor-gap-analysis.md`](competitor-gap-analysis.md)):

- **Cross-provider / capability-aware failover + mesh routing** — a raw CLI retries the same model;
  Forge fails over across providers and routes by task/capability.
- **Fine-grained shell permissions** — ordered glob `allow`/`ask`/`deny` rules that *unwrap* the
  command (`bash -c`, `;`/`&&`/`|` chains, wrapper binaries) before matching, over an unoverridable
  built-in deny floor (`crates/forge-core/src/permission.rs`).
- **Run any model through a subscription CLI as a bridge** AND apply all of the above on top of it.

---

**Bottom line.** Forge's harness is *demonstrably* more reliable than the loop inside the CLI it
wraps — proven by a test suite you can run in seconds — resolves modestly more SWE-bench instances at
lower cost per resolve, and adds routing/failover/permission features the raw CLIs don't have. It is
**not** a raw-token-efficiency win yet, and this document says so. "Better harness" here is a claim with
a `cargo test` behind it, not a slogan.
