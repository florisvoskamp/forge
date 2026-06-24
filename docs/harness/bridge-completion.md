# Bridge completion: guarantees & how we test them

How Forge guarantees a **CLI-bridge** turn (claude-cli / codex-cli) actually finishes the work — and,
more importantly for the future, **how we test that end-to-end**. Read the testing method (§3) before
touching this code; the guarantees are easy to regress in ways unit tests alone won't catch.

## 1. The problem

A CLI bridge is a **one-shot subprocess**: Forge flattens the transcript, spawns `claude --print` /
`codex exec`, the CLI runs *its own* internal agent loop, prints its final text, and **exits**. By the
time Forge sees the response, the process is gone — Forge cannot reach in and keep it going.

Two real failures came out of this (both seen while Forge cut its own releases):

- **Stops mid-plan.** The bridge did a few steps (merged a PR, pushed a tag), exited right after
  launching the async release build, and the dependent steps (fill brew sha, verify) never ran. Forge
  accepted the subprocess exit as "done."
- **Phantom success.** A model narrated a tool call as *text* (never executed) and Forge recorded a
  clean finish — "PR merged, tag pushed" with nothing done.

## 2. The guarantee: completion is defined by the task list and VERIFIED — not by the subprocess exiting

All in `forge-core` `run_model_loop` (the `is_cli_bridge` arm) + the `cli_provider` preamble:

1. **Task-driven re-drive.** While tracked tasks are unfinished, Forge re-invokes the bridge with a
   continue instruction (a clean new process — exactly what a user typing `continue` does). Tasks are
   reloaded from the store each time (the bridge's `update_tasks` runs in the separate `mcp-serve`
   process). Bounded by `MAX_BRIDGE_CONTINUE_NUDGES`.
2. **Progress gate (anti-spiral).** A re-run must make *progress* — start at least one tool
   (counted in the stream sink via `tools_ran`) **or** close a task — or the turn **HALTS loudly**
   instead of re-driving. A bridge that just re-narrates without acting therefore cannot loop. This is
   the guard the old "nudge the bridge" approach lacked (it spiralled).
3. **Objective verification gate.** When the bridge reports *every* task Done, Forge forces a
   verification turn: "PROVE it — run a real inspection tool (`shell git/gh/ls/cat`, `read_file`) and
   look at the actual output; reopen anything not truly done." Self-reported "done" is never trusted.
4. **Inspection requirement.** A verification turn that just re-marks `update_tasks` without inspecting
   does **not** count (`inspect_ran` counts tools other than `update_tasks`/`present_plan`). Forge
   re-prompts up to `MAX_VERIFY_ATTEMPTS`; if the model still never inspects, the turn ends **flagged
   UNVERIFIED** — never a silent success. **Scoped to turns that did real work:** the hard inspection
   requirement only applies when the turn ran inspectable tools at all (`did_real_work`). A pure
   reasoning/analysis plan (the deliverable is the answer text — no external state to check) would
   over-fire, so it's accepted after one verification pass with a calm "not tool-verified (no external
   artifacts)" note instead of UNVERIFIED.
5. **Preamble mandate** (`HARNESS_TOOL_PREAMBLE`): complete the whole task and **WAIT** for any async
   job you launch (release build, CI) — "launched" ≠ "done".

### The invariant

> **Forge never reports a phantom success.** When tracked work is incomplete, Forge completes+verifies
> it, re-drives it, halts loudly, or flags it UNVERIFIED — it never silently "succeeds." This holds for
> bridge *and* direct models, regardless of which guard catches it.

Everything below tests *this invariant*, not any one code path.

## 3. How we test it (the important part)

Bridge behaviour can't be fully tested with a mock — the bug *is* the real one-shot-subprocess
lifecycle. So testing is two layers: deterministic unit tests for the internals, and **real-bridge
e2e** for the invariant.

### 3a. Unit layer — deterministic provider doubles (`crates/forge-core/src/lib.rs` tests)

Each guard has a `Provider` double that scripts exactly the bad behaviour, so the harness logic is
pinned without a network or a real model:

| Test double | Scripts | Proves |
| --- | --- | --- |
| `BridgeProvider {runs_tool}` | bridge turn: text only, optional `ToolStarted` | re-drive bounds, no-progress halt, verification accept-on-inspection, UNVERIFIED-without-inspection |
| `EchoProvider` | returns its own model id; fails a `bad` set | failover order / lazy 429-skip |
| `ReopenOnVerifyProvider` | claims done, then on verification reopens + refinishes | verification reopening a false 'done' |
| `NarrateThenAnswerProvider` | emits a tool call as TEXT, then answers | honest-failure guard (no phantom on direct path) |

Key trick: the **progress signal is observable** — `ToolStarted` events flow through the same stream
sink for bridges, so tests assert "made progress / inspected" by emitting (or not) a `ToolStarted`.
Disable the end-of-turn recap (`session.config.recap.enabled = false`) so invocation counts are exact.

These give deterministic coverage of paths that are **hard or impossible to force with a real model** —
notably the "model actively lies and re-asserts done without checking" case, because real capable
models *refuse* to do that (see §3c).

### 3b. Live layer — `scripts/bridge-e2e.sh`

Drives **real bridge turns** on the **cheapest subscription model** (`claude-cli::haiku`,
`codex-cli::gpt-5.4-mini`) — **no direct-API credits** — and asserts on the **real filesystem + the run
log**. Each scenario forces a different bad path:

| Case | Forced condition | What it proves |
| --- | --- | --- |
| **V** verify-confirms | normal multi-file plan | verification gate fires + confirms with a real check |
| **A** async-not-done | launch a job that writes a file after ~7s | Forge doesn't finish before the async result exists (the original bug) |
| **D** planted-defect | pre-seed a **WRONG** file | verification reads it, catches it, fixes it |
| **R** re-drive | "create ONE file per turn, then stop" | Forge re-drives (`continuing the plan`) to completion |
| **P** no-phantom | model told to mark done without doing the work | Forge never silently succeeds (file made / flagged / model refuses) |

Run it:
```bash
cargo build --release -p forge-cli
scripts/bridge-e2e.sh                 # claude-cli::haiku (cheapest)
scripts/bridge-e2e.sh --both          # also codex-cli::gpt-5.4-mini
BRIDGE_MODEL=codex-cli::gpt-5.4-mini scripts/bridge-e2e.sh
```

### 3c. Principles that make the live tests trustworthy

- **Assert on REAL state, not narration.** Check the file exists with the right bytes (`fis`), and the
  run log for the loud-failure markers — never trust the model's "done."
- **Assert the INVARIANT, not a specific internal path.** Model behaviour varies run to run: the same
  "fake done" prompt makes one model comply, another refuse, another get caught by verification. All
  three are *non-phantom* outcomes. So case **P** passes if the file was really made **OR** Forge
  flagged it (UNVERIFIED / no-progress halt / unfinished) **OR** the model honestly refused. Asserting
  a single path would be flaky; asserting "no phantom" is the real contract.
- **The bad paths that won't reproduce live are unit-tested.** A capable model **refuses** to fake
  completion ("marking incomplete work as done would be dishonest"), so you cannot force a live phantom.
  That refusal is itself a safe outcome; the lying-model path (verification → UNVERIFIED) is pinned in
  `bridge_completion_flagged_unverified_when_model_never_inspects`.
- **`--mode bypass`, isolated workdir, real subscription bridge.** No mock can substitute — the failure
  is in the subprocess lifecycle, so the test must spawn the real CLI.
- **Cheapest model only.** haiku / `*-mini`. The scenarios are tiny; cost is subscription, not API.

### What "done" looks like

Green run = every case's filesystem assertion holds, the relevant guard fired in the log, no `panic` /
`No usable model` / timeout, and case P shows no phantom. Sample (claude-cli::haiku + codex-cli mini):
all of V/A/D/R/P pass on both bridges; verification gate fires on every completion; the async job is
waited for; the planted WRONG file is fixed; re-drive drives the one-per-turn plan to all 5 files.

## 4. Adding a new completion guard

1. Add the logic in the `is_cli_bridge` arm of `run_model_loop` (or the direct-model `else` for both).
2. Add a deterministic `Provider` double that scripts the bad behaviour + a unit test asserting the
   guard fires and is bounded (no infinite loop).
3. Add a live case to `scripts/bridge-e2e.sh` that *forces* the condition and asserts the **invariant**
   on the real filesystem + log.
4. Never assert a single internal path in the live test — assert "no phantom success."
