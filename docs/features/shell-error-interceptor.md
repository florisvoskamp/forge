# Shell error interceptor — fail → explain + fix, no prompt

> Status: **MVP done** — a failed `shell` command is auto-diagnosed by one cheap model call,
> surfaced alongside the result. On by default; `[shell] explain_errors = false` disables it.

## Why

The Helm-note vision lists "command fails → AI auto-explains + offers a fix, no prompt needed"
as a Wave-4 differentiator. Bash is the owner's #1 tool (5,909 uses); failed commands are a
constant, and today every explanation costs a manual round-trip. The interceptor removes that
round-trip: the moment a shell command fails, Forge attaches a terse likely-cause + concrete
fix.

## What shipped

- After the `shell` tool returns a **failure** — non-zero exit, signal, timeout, or spawn
  error — Forge makes **one trivial-tier model call** (the cheapest mesh route) asking for the
  likely cause and a concrete fix in ≤3 sentences, and emits `PresenterEvent::ShellDiagnosis`
  (rendered as `⚠ shell failed <cmd>` + the dimmed explanation in both the TUI and headless
  output).
- **Best-effort, never intrusive:** it does not alter the tool result the model sees, it is
  skipped when the budget is exhausted, and any model error is swallowed silently — it can
  never derail or fail the turn.
- Gated by `[shell] explain_errors` (default `true`).

## Design

- **Failure detection:** `shell_command_failed(result)` in `forge-core` is pure (unit-tested) —
  it reads the tool's first line (`shell: exit N …` / `timed out` / `error:` / `failed to
  start`); only `exit 0` is success.
- **The call:** `Session::diagnose_shell_error` reuses the compaction pattern —
  `Router::route_hinted(Trivial)` + a single `provider.complete` with a fixed system prompt
  (`SHELL_DIAGNOSE_SYSTEM`) and the command + result as the user message.
- **Wiring:** in `invoke_tool`, after the PostToolUse hooks and before returning, when
  `side_effect == Shell && config.shell.explain_errors && shell_command_failed(&result)`.

## Deferred

- Feed the diagnosis back into the transcript as a `system` hint (currently UI-only, so the
  model isn't re-prompted with it — the loop already sees the raw failure).
- A one-keystroke "apply the suggested fix" action, and recording the diagnosis call's usage
  against the budget (parity with compaction, which also doesn't record its own call).
- Pattern cache for common failures (missing binary, wrong cwd) to skip the model call.
