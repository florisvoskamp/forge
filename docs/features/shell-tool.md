# Feature Spec: Shell / Bash Execution Tool

Status: Draft
Author: design pass (feature-design)
Depends on: `docs/features/fix-permission-rules.md` (fine-grained per-command allow/ask/deny rules — referenced, not yet written)
Touches: `forge-tools`, `forge-types`, `forge-core`, `forge-tui`

---

## 1. Problem

Forge ships five tools (`read_file`, `write_file`, `edit_file`, `list_dir`, `search`) and a
`ShellTool` stub in `crates/forge-tools/src/core_tools.rs:69` that is registered but is barely a
tool: it runs `sh -c <command>` with no working directory, no environment control, no timeout,
no streaming, no truncation, no background support, and only the most superficial permission
wiring (`SideEffect::Shell` → broker `Ask`). It cannot run a build, watch a test, or kill a hung
process. For real coding work that is disqualifying.

### Evidence

In the owner's Claude Code usage, **Bash is the #1 tool at 5,909 uses** — more than 2x the next
tool (Read, 2,650). The owner's loop is `cargo build` / `cargo test` / `git` / `gh` / running the
binary. Every one of those is a shell invocation. A coding harness that cannot run shell commands
cannot close its own loop: it can write code but never compile, test, or run it.

### Jobs to be Done

> When I'm in an agent turn and need to **build, test, run, or inspect the project with a CLI
> tool**, I want Forge to **execute the command, stream its output live, and hand the model the
> exit code and output**, so I can **iterate on code without leaving the harness** — while being
> **confident a wrong or malicious command can't wipe my machine or leak my secrets.**

The job is twofold: *capability* (run the command, see it live, feed the result back) and *safety*
(every command is reviewed against my intent before it touches the system). Safety is the core of
this design — a shell tool that isn't safe is worse than no shell tool.

### Why this matters

- It unblocks the dominant developer loop (build/test/run/git).
- It is the single highest-impact missing capability in Forge today.
- It is also the single highest-risk tool: `shell` can do anything `read_file`/`write_file`
  collectively can, plus delete, exfiltrate, or brick the host. The permission layer must be
  correspondingly stronger than the binary `Ask` it gets today.

---

## 2. Scope

### User stories (MoSCoW)

**Must have**
- M1. As the agent, I run a command with a chosen working directory and a timeout, and receive
  stdout, stderr, and exit code as the tool result.
- M2. As the user, I see the running command and its output stream **live** in the TUI as it runs,
  ending in an exit-status line.
- M3. As the user, every shell command routes through the permission broker; in `default` mode I am
  prompted with the exact command and cwd before it runs, and can allow/deny.
- M4. As the user, a denylist of catastrophic patterns (`rm -rf /`, `rm -rf ~`, fork bombs,
  disk-overwrite, reading `.env`/secret files) is **always blocked**, even in `bypass` mode.
- M5. As the agent, a command that exceeds its timeout is killed (process group) and reported as a
  timeout, not left hanging.
- M6. As the agent, huge output is truncated to a token budget before being returned to the model,
  with a clear truncation marker; the user still sees full live output in the TUI scrollback.
- M7. As the user, the child gets **no TTY/stdin** by default; commands that block on input fail
  fast rather than hang forever.

**Should have**
- S1. As the agent, I can start a **background job** (e.g. `cargo watch`, a dev server), get a job
  id immediately, and later poll its output / collect its exit status / kill it.
- S2. As the user, a per-project policy file maps commands to allow / ask / deny (e.g.
  `cargo *` → allow, `git push *` → ask, `curl * | sh` → deny), evaluated before the mode default
  (depends on `fix-permission-rules.md`).
- S3. As the agent, ANSI escape codes are stripped from the text returned to the model (token
  noise) but preserved for the TUI rendering.
- S4. As the user, an allow decision can be remembered for the session ("allow `cargo *` for the
  rest of this session") so I'm not re-prompted for every build.

**Could have**
- C1. OS-level sandboxing of the child (Linux: `unshare`/`bwrap`/seccomp; macOS: `sandbox-exec`) to
  contain even allowed commands — filesystem/network confinement.
- C2. Per-command resource limits (max output bytes, max wall-clock independent of timeout, memory).
- C3. Structured environment redaction: scrub known secret-shaped env vars from the child unless
  explicitly passed.
- C4. A `--dry-run` that classifies a command (allow/ask/deny + why) without executing it.

**Won't have (this iteration)**
- Interactive TTY passthrough / PTY allocation so the model can drive `vim`, `top`, REPLs, or
  answer prompts mid-command. Non-interactive only.
- A full shell parser. Guards operate on the raw command string + a lightweight tokenizer, not a
  semantic AST. (Documented limitation, see §5.7.)
- Remote / SSH execution.

> **Shipped since this was written:** Windows support. The tool runs `cmd /C` on Windows (`sh -c`
> on Unix) and kills the process tree via `taskkill /F /T` (vs. process-group SIGTERM/SIGKILL on
> Unix); see `crates/forge-tools/src/shell.rs` and its `exec_windows` test module. This was a
> non-goal at the time of this iteration and has since been added — not a "won't have" anymore.

### Non-goals

- This feature does **not** give the model an interactive terminal.
- This feature does **not** attempt to be a security sandbox by itself (denylist + permission ≠
  sandbox). True isolation is C1, explicitly a could-have.
- This feature does **not** replace `read_file`/`search`; the model should prefer those over
  `cat`/`grep` (encouraged via tool description, not enforced).
- This feature does **not** persist shell state (cwd, env, shell variables) between separate
  `shell` calls. Each invocation is a fresh process unless it is a background job being polled.

---

## 3. Acceptance criteria (Given/When/Then)

### Happy paths

```
M1 — run and capture
Given permission mode is bypass (or the command is allowed by policy)
When the agent calls shell{ command: "cargo build", cwd: ".", timeout_secs: 120 }
Then the child runs in the project root
And the tool result contains exit_code, truncated stdout, truncated stderr, and duration_ms
```

```
M2 — live streaming
Given a command that prints output over several seconds
When it runs
Then the TUI shows a "running" line with the command and elapsed time
And output lines appear in the scrollback as they are produced (not only at the end)
And on completion a status line shows exit code, duration, and bytes captured
```

```
M3 — default-mode prompt
Given permission mode is default
When the agent calls shell{ command: "git push origin main" }
Then the user is shown a permission prompt containing the exact command and cwd
And choosing "deny" returns a denied result to the model and runs nothing
And choosing "allow" runs the command once
```

```
S1 — background job
Given the agent calls shell{ command: "cargo watch -x test", background: true }
When the call returns
Then it returns immediately with a job_id and status "running"
And a later shell_poll{ job_id } returns new output since the last poll and current status
And shell_kill{ job_id } terminates the process group and reports the final status
```

```
S4 — remember-for-session
Given mode is default and the user is prompted for "cargo test"
When the user chooses "allow (cargo * for this session)"
Then this and subsequent commands matching cargo * run without re-prompting until the session ends
```

### Negative paths

```
M4 — catastrophic denylist beats bypass
Given permission mode is bypass
When the agent calls shell{ command: "rm -rf /" }
Then the command is denied before execution
And the result explains it matched a hard-denied dangerous pattern
And nothing runs — even though bypass would normally allow all side effects
```

```
M4b — secret read blocked
Given any mode
When the agent calls shell{ command: "cat .env" } or "cat ~/.aws/credentials"
Then it is denied as a secret-exposure pattern
And the denial reason names the matched rule (not the file contents)
```

```
M5 — timeout / hang
Given a command that never exits (e.g. "sleep 999" with timeout_secs 5)
When 5 seconds elapse
Then the child's process group is killed (SIGTERM, then SIGKILL after a grace period)
And the result is status "timed_out" with whatever output was captured so far and a note
```

```
M7 — interactive prompt with no TTY
Given a command that blocks reading stdin (e.g. "ssh-keygen" awaiting a passphrase)
When it runs with stdin closed (/dev/null) and no TTY
Then the child sees EOF on stdin and either errors or proceeds non-interactively
And it does not hang indefinitely (timeout is the backstop)
```

```
command not found
Given the agent calls shell{ command: "definitelynotacommand" }
When sh runs it
Then the tool result reports exit_code 127 and the shell's "not found" stderr
And this is a normal (non-error) tool result the model can react to — not a ToolError
```

```
non-zero exit
Given "cargo test" fails
When it exits with code 101
Then the result includes exit_code 101 and the captured output
And ok=false is signalled to the TUI (✗) but the model still receives full output to fix the failure
```

```
cwd outside project
Given cwd resolves outside the project root (e.g. "/etc" or "../../..")
When the command is evaluated
Then in default/accept-edits modes this requires an explicit ask (or is denied by policy)
And the resolved absolute cwd is shown in the permission prompt so the user isn't fooled by ..
```

```
Plan mode
Given permission mode is plan (read-only)
When any shell command is requested
Then it is denied (SideEffect::Shell is never read-only), matching permission.rs:20
```

---

## 4. Impact analysis & insertion points

Layers touched:

- [x] Tool layer (`forge-tools`) — replace the stub, add guards, streaming, background registry
- [x] Domain types (`forge-types`) — streaming/permission context carried to the broker & TUI
- [x] Core agent loop + broker (`forge-core`) — richer permission decision, command context, policy
- [x] Presenter / TUI (`forge-tui`) — live output rendering, running/exit status, risky-command prompt
- [ ] No DB schema change (background jobs live in-process for this iteration; see §5.6)

### Insertion points (specific files / symbols)

> **Not what shipped:** the plan below was to split shell support across a new
> `crates/forge-tools/src/shell/` directory (`mod.rs`/`guards.rs`/`exec.rs`/`jobs.rs`/
> `output.rs`). The shipped code instead keeps everything — background jobs, timeouts, ANSI
> handling — in a single `crates/forge-tools/src/shell.rs` file, and the denylist /
> dangerous-pattern matcher this doc assigns to `guards.rs` actually lives in
> `crates/forge-core/src/permission.rs` (not in `forge-tools` at all). This file:symbol map is
> no longer a usable guide to the real code.

```
forge-tools:
  crates/forge-tools/src/core_tools.rs:69  ShellTool — replace the stub implementation
  crates/forge-tools/src/shell/             NEW module:
    mod.rs        ShellTool + ShellPollTool + ShellKillTool, schema, run()
    guards.rs     denylist / dangerous-pattern matcher (always-on, mode-independent)
    exec.rs       spawn (process group), stream stdout/stderr, timeout+kill, capture+truncate
    jobs.rs       background JobRegistry (job_id → handle, ring buffer of output, status)
    output.rs     ANSI strip (for model) + preserve (for TUI), truncation to token budget
  crates/forge-tools/src/lib.rs:13          export new tools
  crates/forge-tools/src/lib.rs:49          with_core_tools(): register shell_poll, shell_kill
  crates/forge-tools/Cargo.toml             deps: tokio (process, io, time), nix or libc (setsid/killpg)

forge-types:
  crates/forge-types/src/lib.rs:152  SideEffect — Shell stays; optionally carry a CommandContext
  crates/forge-types/src/lib.rs:163  PermissionDecision — extend to carry a reason string
                                     (Deny{reason}, Ask{reason}) OR add a sibling DecisionReason.
  NEW: CommandPolicy / PolicyRule types (command glob → allow|ask|deny), shared by core + tools.
       (Final shape defined by docs/features/fix-permission-rules.md.)

forge-core:
  crates/forge-core/src/permission.rs:10  decide() — accept a CommandContext for Shell so the
                                     denylist + per-project policy are consulted BEFORE the mode
                                     default. New precedence (see §5.3):
                                     hard-deny > policy-deny > plan-mode-deny > policy-allow >
                                     bypass-allow > policy-ask > mode default.
  crates/forge-core/src/lib.rs:312   ToolStart emission — for shell, stream deltas as they arrive
  crates/forge-core/src/lib.rs:301/333 ToolResult emission — carry exit code / status

forge-tui:
  crates/forge-tui/src/lib.rs:34     PresenterEvent — add ToolOutputDelta + ToolEnd (see §5.4)
  crates/forge-tui/src/lib.rs:55     Presenter::confirm — extend to pass the command+cwd+reason
                                     so the prompt can show them (today it gets only tool + SideEffect)
  crates/forge-tui/src/app.rs:168    apply(): render running command, append streamed output,
                                     finalize with exit-status line
  crates/forge-tui/src/app.rs:214    tool_start_line / tool_result_line — shell-specific variants
```

Existing pattern to follow: every tool implements the `Tool` trait (`lib.rs:28`) and declares its
`SideEffect`; the broker (`permission.rs:10`) is the single chokepoint (ADR-0008). This feature
keeps that contract — the new behaviour is (a) a richer decision input for `Shell`, and (b) two new
streaming `PresenterEvent`s. No tool gets to bypass the broker.

### Regression risk

- `permission.rs::decide` signature change ripples to `forge-core/src/lib.rs` and the broker tests
  (`permission.rs:34`). Keep `ReadOnly`/`Write` behaviour byte-identical; only `Shell` gains context.
- `Presenter::confirm` signature change ripples to `HeadlessPresenter` and the TUI presenter and
  `app.rs` tests (`app.rs:482`). Provide the command context as an optional struct so non-shell
  confirmations are unaffected.
- The existing `side_effect_classes_are_correct` test (`lib.rs:100`) must still pass — `shell` stays
  `SideEffect::Shell`.

---

## 5. Technical design

### 5.1 Tool schema (data model)

`shell` advertised schema (foreground + background):

```json
{
  "type": "object",
  "properties": {
    "command":      { "type": "string", "description": "POSIX sh -c command line." },
    "cwd":          { "type": "string", "description": "Working dir; defaults to project root. Resolved & validated." },
    "timeout_secs": { "type": "integer", "minimum": 1, "description": "Default 120; hard max 600." },
    "background":   { "type": "boolean", "description": "If true, start detached and return a job_id immediately." }
  },
  "required": ["command"]
}
```

Companion tools for background lifecycle (S1):

```json
// shell_poll  — read new output and status for a running/finished job
{ "type":"object", "properties": { "job_id": {"type":"string"} }, "required":["job_id"] }

// shell_kill  — terminate a background job's process group
{ "type":"object", "properties": { "job_id": {"type":"string"} }, "required":["job_id"] }
```

Internal result struct (serialized to the model as the tool-result string, and to the TUI via events):

```
ShellOutcome {
  status: Completed | TimedOut | Killed | Denied { reason } | SpawnFailed { reason },
  exit_code: Option<i32>,        // None for timed_out/killed/denied
  stdout: String,                // ANSI-stripped, truncated to budget
  stderr: String,                // ANSI-stripped, truncated to budget
  truncated: bool,
  bytes_captured: u64,           // full size before truncation
  duration_ms: u64,
  job_id: Option<String>,        // Some when background
}
```

Defaults & limits: `timeout_secs` default **120**, hard max **600** (clamped, not rejected, with a
note). Output budget: cap returned-to-model text at **~8 KB / ~2,000 tokens per stream** (config in
`Config`), keeping head + tail with a `… <N> bytes truncated …` marker in the middle. The TUI keeps
full output in scrollback independent of the model budget.

### 5.2 Execution (exec.rs)

- Spawn `sh -c <command>` via `tokio::process::Command`, `cwd` set, `stdin = /dev/null` (no TTY),
  `stdout`/`stderr = piped`.
- Put the child in its **own process group** (`setsid` / `pre_exec`) so a timeout kill takes down
  the whole tree (a command like `a | b | c` or one that forks children).
- Read stdout/stderr concurrently; emit `ToolOutputDelta` per chunk/line to the Presenter AND append
  to a capture buffer.
- Timeout via `tokio::time::timeout`. On expiry: `killpg(SIGTERM)`, wait a short grace (e.g. 2s),
  then `killpg(SIGKILL)`. Report `TimedOut` with output captured so far.
- Background (`background:true`): spawn, register in `JobRegistry`, return `job_id` + status
  `running` immediately. A reader task drains output into a bounded ring buffer keyed by `job_id`.
  `shell_poll` returns output since the last poll + status; `shell_kill` does the killpg dance.

### 5.3 Safety — guards, broker, policy

Three layers, evaluated in this precedence (strongest first):

1. **Hard denylist (always-on, mode-independent — beats even bypass).** In `guards.rs`. Patterns:
   - `rm -rf /`, `rm -rf ~`, `rm -rf /*`, `rm` targeting `/` or `$HOME` roots
   - disk overwrite: `dd of=/dev/...`, `mkfs`, `> /dev/sd*`, `:(){ :|:& };:` (fork bomb)
   - history/credential exfil & secret reads: `cat`/`less`/`cp`/`curl --data @` on `.env`,
     `*.pem`, `id_rsa`, `~/.aws/credentials`, `~/.ssh/*`, `.git-credentials`, etc.
   - piping remote content to a shell: `curl ... | sh`, `wget ... | bash`
   Match on a normalized command string + lightweight tokenization (collapse whitespace, split on
   `|`/`&&`/`;`). This is a *hard* deny: returns `Denied{reason}` regardless of mode.
2. **Per-project policy (`fix-permission-rules.md`).** `allow | ask | deny` rules by command glob,
   loaded per project. A `deny` here also beats bypass. `allow` here beats the mode default. `ask`
   forces a prompt even under accept-edits.
3. **Mode default (existing `permission.rs`).** `plan`→Deny, `bypass`→Allow, `accept-edits`→Ask for
   shell, `default`→Ask.

So `decide()` becomes (for Shell): hard-deny → policy-deny → plan-deny → policy-allow →
bypass-allow → policy-ask → mode-default. ReadOnly/Write paths are unchanged.

`PermissionDecision` gains a reason so the TUI can explain *why* (e.g. "matched dangerous pattern
`rm -rf ~`" or "policy: cargo * allowed"). Session-remembered allows (S4) live in the session as an
in-memory set of approved globs consulted at the policy layer.

The cwd is canonicalized; `..`-escapes and absolute paths outside the project root downgrade the
decision to at-least-Ask (default/accept-edits) and the **resolved absolute path** is what the
prompt shows, so a deceptive `cwd: "../../.."` can't slip past visual inspection.

### 5.4 Streaming & TUI wiring

Add two `PresenterEvent` variants (`crates/forge-tui/src/lib.rs:34`):

```
ToolOutputDelta { name: String, stream: Stdout|Stderr, chunk: String },  // live, ANSI preserved
ToolEnd { name: String, status: String, exit_code: Option<i32>, duration_ms: u64, truncated: bool },
```

`Presenter::confirm` is extended to receive an optional `CommandContext { command, cwd, reason }`
so the risky-command prompt can show the full command, the resolved cwd, and why it's being asked.
`HeadlessPresenter` keeps its current deny-when-non-interactive behaviour (`lib.rs:non-interactive`).

ANSI handling: deltas to the **TUI** preserve ANSI (so colored cargo/test output renders);
the **capture buffer returned to the model** is ANSI-stripped (output.rs) to save tokens and avoid
control-character noise. Binary output (non-UTF-8 / NUL bytes detected) is not streamed verbatim —
it is summarized as `<binary output: N bytes, not shown>` to the model and rendered as a single
notice line in the TUI.

### 5.5 TUI mockups

Running command with live streaming, then exit status:

```
  ↳ shell  cargo test            cwd: ~/dev/forge          ⠹ running  3.4s
    Compiling forge-tools v0.1.0
    Compiling forge-core v0.1.0
       Running unittests src/lib.rs
    test permission::tests::bypass_allows_everything ... ok
    test core_tools::tests::edit_file_replaces_a_unique_occurrence ... ok
  ──────────────────────────────────────────────────────────────────────
  ✓ shell  exit 0   ·   2.1 KB out   ·   8.7s
```

Failing command (non-zero exit, output still fed to the model):

```
  ↳ shell  cargo test            cwd: ~/dev/forge          ⠹ running  6.0s
    test core::run_turn::full_turn_routes_calls_tool_and_persists ... FAILED
    failures:
        core::run_turn::full_turn_routes_calls_tool_and_persists
  ──────────────────────────────────────────────────────────────────────
  ✗ shell  exit 101   ·   1.4 KB out   ·   6.2s
```

Permission prompt for a risky command (default mode):

```
  ┌─ permission required ───────────────────────────────────────────────┐
  │ shell wants to run:                                                  │
  │                                                                      │
  │     git push origin main                                            │
  │                                                                      │
  │   cwd:    ~/dev/forge                                                │
  │   reason: policy → `git push *` is set to ASK                        │
  │                                                                      │
  │   [a] allow once   [s] allow `git push *` this session   [d] deny    │
  └──────────────────────────────────────────────────────────────────────┘
  >
```

Hard-denied command (cannot be allowed, shown even in bypass):

```
  ↳ shell  rm -rf ~
  ✗ shell  DENIED — matched dangerous pattern `rm -rf ~` (hard deny, not overridable)
```

### 5.6 Background job lifecycle

- `JobRegistry` is an in-process `HashMap<job_id, Job>` owned by the tool layer (lives as long as the
  process). `Job` holds: child handle (process group leader), bounded output ring buffer, status,
  start time. No persistence across Forge restarts this iteration (documented limitation).
- States: `running → (completed | killed | timed_out | failed)`. `shell_poll` is the only way the
  model learns a background job finished; the TUI shows a compact "bg job <id> running/done" line.
- On Forge shutdown, all background process groups are killed (no orphans).
- Backpressure: ring buffer drops oldest output when full and sets a `dropped: true` flag surfaced in
  `shell_poll`.

### 5.7 Documented limitations / sandbox note

- The denylist is **pattern-based**, not a semantic shell analysis, so it is best-effort against
  obfuscation (`r''m -rf /`, base64-piped payloads). It is a guardrail against accidents and obvious
  hostility, **not** a security boundary. True containment is OS sandboxing (C1) — recommended as the
  next safety milestone: run the child inside `bwrap`/seccomp (Linux) or `sandbox-exec` (macOS) with
  filesystem scoped to the project and network optionally denied.
- Non-interactive only: any command that truly requires a human at a TTY will fail/timeout by design.

### 5.8 Edge-case table

| Edge case | Behaviour |
|-----------|-----------|
| Command hangs forever | Timeout fires → killpg SIGTERM, grace, SIGKILL → `TimedOut` with partial output |
| Huge output (GBs) | Live-streamed to TUI scrollback; model gets head+tail truncated to token budget + marker; `bytes_captured` reports true size |
| Binary / non-UTF-8 output | Not streamed verbatim; `<binary output: N bytes, not shown>` to model, single notice in TUI |
| ANSI-heavy output (colored cargo) | TUI keeps ANSI; model text is ANSI-stripped |
| Interactive prompt (passphrase, `y/N`) | stdin = /dev/null, no TTY → EOF; command errors or proceeds; timeout is backstop |
| Non-zero exit | Normal tool result: `exit_code` + output returned, `ok=false`/✗ in TUI; NOT a ToolError |
| Command not found | exit 127 + shell stderr; normal result, model can correct |
| cwd outside project (`..`, `/etc`) | Canonicalize; at-least-Ask in default/accept-edits; resolved abs path shown in prompt |
| cwd does not exist | `SpawnFailed{reason}` returned to model; nothing runs |
| `rm -rf /`, `rm -rf ~`, fork bomb | Hard deny — beats bypass; not overridable; reason shown |
| `cat .env`, `~/.ssh/id_rsa`, `~/.aws/credentials` | Hard deny (secret exposure); reason names rule, never the contents |
| `curl … | sh` | Hard deny (remote-to-shell pipe) |
| Secret in command args (e.g. `export TOKEN=...`) | Args shown in prompt are scanned; secret-shaped values masked in TUI/transcript display |
| Plan mode | Denied (Shell is never ReadOnly) — matches `permission.rs:20` |
| Bypass mode + safe command | Allowed without prompt (unless hard-deny / policy-deny) |
| accept-edits mode | Shell still asks (matches `permission.rs:26`), unless policy says allow |
| Background job, Forge exits | Process group killed on shutdown; no orphans |
| Background ring buffer overflows | Oldest output dropped; `dropped:true` in `shell_poll` |
| Timeout > hard max (600s) | Clamped to 600 with a note in the result; not rejected |
| Non-interactive (piped) session asks for confirm | `HeadlessPresenter` denies by default (safe), matching current behaviour |

---

## 6. Definition of done

- [ ] All Must-Have acceptance criteria (M1–M7) pass with tests.
- [ ] `shell` runs with cwd + timeout, captures stdout/stderr/exit code, returns `ShellOutcome`.
- [ ] Output streams live to the TUI via `ToolOutputDelta`/`ToolEnd`; exit-status line renders.
- [ ] Hard denylist blocks `rm -rf /`, `rm -rf ~`, fork bombs, disk overwrite, secret reads, and
      `curl|sh` — verified to beat `bypass` mode in tests.
- [ ] Timeout kills the whole process group (SIGTERM→SIGKILL) and reports `TimedOut`; no orphans.
- [ ] stdin is `/dev/null`; an input-blocking command does not hang past timeout.
- [ ] Model-facing output is ANSI-stripped and truncated to the token budget with a marker; TUI keeps
      full ANSI output in scrollback.
- [ ] `decide()` precedence implemented (hard-deny > policy-deny > plan > policy-allow > bypass >
      policy-ask > mode default); ReadOnly/Write behaviour unchanged; existing broker tests pass.
- [ ] `Presenter::confirm` shows the exact command, resolved cwd, and reason; `HeadlessPresenter`
      still denies non-interactively.
- [ ] cwd is canonicalized; out-of-project cwd forces at-least-Ask and shows the resolved abs path.
- [ ] Background jobs (S1): `shell{background:true}` → job_id; `shell_poll`/`shell_kill` work; jobs
      killed on shutdown. (If S1 deferred, tracked as a follow-up — M-set is shippable without it.)
- [ ] Every edge case in §5.8 has a defined, tested behaviour.
- [ ] `with_core_tools()` registers `shell` (and `shell_poll`/`shell_kill` if S1 lands);
      `registry_has_core_tools` and `side_effect_classes_are_correct` still pass.
- [ ] Secret-shaped args masked in TUI/transcript display.
- [ ] Sandbox (C1) documented as the next safety milestone (not required to ship).
```
