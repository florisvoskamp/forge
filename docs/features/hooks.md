# Feature: pre/post tool-use shell hooks

> **Status: extended.** `[[hooks]]` config entries run a shell command around tool calls and
> session lifecycle events. `PreToolUse` blocks a call; `PostToolUse` observes; `UserPromptSubmit`
> can rewrite or block a user prompt; `SessionStart`/`SessionEnd` fire at session boundaries.
> Wired into both the direct tool path and the plain/TUI chat loops.

## 1. Problem (JTBD)

> When I run Forge, I want my own shell commands to run automatically around tool calls — to
> enforce a policy, rewrite/observe a command, or trigger side work (re-index, notify) — so my
> environment behaves like my heavily-hooked Claude Code setup instead of a bare agent.

The owner's whole environment is hook-driven (a token-proxy on every command, a graph injector
after edits, auto-title). Without hooks, none of that carries over. This MVP gives the two
load-bearing events.

## 2. Scope (MoSCoW)

**Must have (shipped)**
- `[[hooks]]` config: `event` (`pre_tool_use` | `post_tool_use`), optional `matcher`
  (tool-name filter), `command` (POSIX `sh -c`), `timeout_secs` (default 30).
- The tool call is passed to the hook as JSON on **stdin** (`{tool, args}` for pre,
  `{tool, args, result, ok}` for post).
- **PreToolUse blocks on non-zero exit**: the tool does not run; the hook's stderr (or stdout)
  becomes the result the model sees (`blocked by hook: <reason>`).
- **PostToolUse observes**: stdout is surfaced as a note; exit code is advisory.
- Time-bounded: a hook that exceeds `timeout_secs` is killed (`kill_on_drop`) and noted, never
  hangs the turn. Inert (zero overhead) when no hooks are configured.

**Shipped (follow-up)**
- `UserPromptSubmit` — fires before each agent turn; hook stdout replaces the prompt on exit 0;
  non-zero blocks the turn with stderr as the reason. Enables RTK-style prompt rewriting.
- `SessionStart` / `SessionEnd` — observe-only lifecycle events; fire in both TUI and plain chat
  loops. Payload: `{"session_id": "<id>", "event": "session_start|session_end"}`.

**Shipped (arg rewriting)**
- `PreToolUse` exit 0 + JSON object on stdout → rewrites tool args before the tool runs.
  Exit 0 + plain text → note only (unchanged args). Exit non-zero → block.

**Shipped (structured directive protocol)**
- A hook can emit a JSON object with an explicit `"action"` field on stdout (exit 0) to do more
  than rewrite — the same protocol works for `PreToolUse` **and** `PostToolUse`:
  - `{"action":"rewrite","args":{…}}` — replace the tool's args (PreToolUse).
  - `{"action":"inject","context":"…"}` — inject model-visible context: queued as a system hint and
    shown to the model right after the tool result (e.g. lint output, "this file is generated", a
    policy reminder). No block, no rewrite. The first capability that lets a hook *teach* the model,
    not just gate it.
  - `{"action":"block","reason":"…"}` — block the call (PreToolUse). On `PostToolUse` (the call has
    already run, nothing to unwind) it degrades to a note.
  - `{"action":"allow"}` — explicit no-op (approve without changing anything).
  - An unrecognised `action`, or a malformed directive (missing `args`/`context`), degrades to a note
    so the author sees their output instead of it vanishing.
- **Back-compatible:** a bare JSON object with no `"action"` field keeps the legacy meaning
  (rewrite args, PreToolUse only); plain text is still a note; exit non-zero is still a hard block.

**Shipped (MCP tool hooks)**
- `PreToolUse` and `PostToolUse` now fire for MCP tool calls too (e.g. `helm__get_today`,
  `test__echo`). Block, observe, and arg-rewrite all work identically to native tools.
  MCP tool names use the `server__tool` namespace; the existing `matcher` comma-list
  already handles them (e.g. `matcher = "helm__create_task"`).

**Deferred**
- Per-hook environment templating beyond the stdin JSON.
- Other events: `notification`, `PostSessionCompact`.

## Non-goals
- Changing the agent loop, permission model, or tool contract. Hooks wrap `invoke_tool`; a
  block is reported exactly like a denied/errored tool call.

## 3. Acceptance criteria
```
Given a [[hooks]] entry event=pre_tool_use matcher="shell" command exits non-zero
When the model calls the shell tool
Then the tool does NOT run and the model receives "blocked by hook: <stderr>"

Given a pre_tool_use hook exits zero
When the matching tool is called
Then the tool runs normally and the hook's stdout is shown as a note

Given a post_tool_use hook
When a matching tool completes
Then the hook runs with {tool,args,result,ok} on stdin and its stdout is a note

Given a hook that runs longer than timeout_secs
When it fires
Then it is killed and noted; the turn proceeds (pre: not blocked)

Given no [[hooks]] configured
Then invoke_tool behaves byte-for-byte as before (no overhead)
```

## 4. Config example
```toml
[[hooks]]
event = "pre_tool_use"
matcher = "shell"            # absent or "*" = all tools; comma-list = several
command = "my-policy-check"  # reads {"tool","args"} on stdin; exit!=0 blocks

[[hooks]]
event = "post_tool_use"
matcher = "edit_file,write_file"
command = "graphify update . >/dev/null 2>&1 || true"
timeout_secs = 60
```

## 5. Design
`forge-config`: `HookConfig { event, matcher, command, timeout_secs }` + `HookEvent`; a
`Vec<HookConfig>` under `config.hooks`. `HookConfig::matches(tool)` does the name filter.

`forge-core::hooks::run_hooks(hooks, event, tool, payload)` filters to matching hooks, runs each
via `tokio::process::Command("sh","-c",cmd)` with the payload on stdin, `kill_on_drop(true)` +
`tokio::time::timeout`. Returns `HookOutcome { blocked: Option<String>, notes: Vec<String> }`.
`Session::invoke_tool` calls it before the tool (PreToolUse — short-circuits to a blocked
result) and after recording the result (PostToolUse — emits notes as warnings).

**Cross-platform:** hooks are POSIX `sh -c` only, like the shell tool (see known-issues.md).

## 6. Definition of done
- [x] `[[hooks]]` parses; `matches()` filter; default timeout.
- [x] PreToolUse non-zero blocks with the hook output as the reason; zero passes through.
- [x] PostToolUse runs with the result payload; stdout surfaced.
- [x] Timeout kills a wedged hook without hanging the turn.
- [x] Inert when unconfigured (existing tool tests unchanged).
- [x] Unit tests (runner) + an end-to-end test (a hook blocks a real `list_dir` call in a turn).
- [x] `cargo fmt` + `clippy -D warnings` clean.
