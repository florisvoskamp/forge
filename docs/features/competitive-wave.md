# Feature wave: competitive parity + differentiators

> Status: **SHIPPED** (2026-06-19). One branch, one PR.
>
> Seven features closing the most-cited gaps vs Codex / Claude Code / aider, plus differentiators
> that play to Forge's mesh + Lattice + Assay strengths. Every feature is **opt-in and inert by
> default** — existing behaviour is byte-for-byte unchanged unless you turn one on.

Decided from an orchestrated competitive analysis. Three "gaps" the analysis flagged turned out to
be **already built** and were verified in source, not rebuilt: turn-level failover (`run_turn`),
subagent depth > 1 (`max_depth`), and quota-aware routing (`SubscriptionQuota` on `route()`).
Deferred to a later wave: OS sandbox (Landlock) and Lattice 130-language polyglot expansion.

---

## 1. LSP live-diagnostic feedback loop  `[lsp]`

New `forge-lsp` crate: a minimal JSON-RPC/Content-Length LSP client that lazily spawns a language
server per (language, repo-root) over stdio, sends `initialize` + `didOpen`, and collects
`publishDiagnostics` with a timeout. After a successful write in `invoke_tool`, Forge fetches
diagnostics for the edited file and pushes a `[lsp diagnostics]` hint onto the same `pending_hints`
channel the shell-error interceptor uses — so the model sees the type errors / unresolved imports on
its next step and self-corrects. Built-in server defaults for rust / typescript / javascript /
python / go; degrades silently when the binary isn't on `PATH`.

```toml
[lsp]
enabled = true
timeout_ms = 3000
[lsp.servers.rust]
command = "rust-analyzer"
[lsp.servers.typescript]
command = "typescript-language-server"
args = ["--stdio"]
```

## 2. Auto-lint / auto-test self-healing loop  `[autofix]`

After a turn makes edits, Forge runs the configured lint and/or test command; on a non-zero exit it
feeds the failing output back as a user message and re-enters the model↔tool loop, up to
`max_iterations`, before finishing. No edits or autofix off → zero overhead.

```toml
[autofix]
auto_lint = true
auto_test = true
lint_cmd = "cargo clippy --all-targets"
test_cmd = "cargo test"
max_iterations = 3
```

## 3. Git-worktree isolation for parallel write agents  `[mesh.subagents] worktree_isolation`

Each write-capable child spawned by `spawn_agents` gets its own `git worktree` (branch
`forge/subagent/<id>`); its write/shell tool args are rebased into the worktree, so concurrent
children can't corrupt the shared tree. On finish, changes merge back via `git apply --3way`
(serialized across children); conflicts surface as the child's result text. A RAII guard tears the
worktree + branch down on drop (including panics). Read-only children and non-git repos skip it.

## 4. MCP tool hooks

PreToolUse / PostToolUse hooks now fire for MCP tool calls on the CLI-bridge path (they previously
covered only built-in tools — a large fraction of real tool traffic bypassed the hook-based
permission/logging story). Pre hooks can block or rewrite args; post hooks' notes are prefixed onto
the result. No config — uses the existing `[[hooks]]` entries.

## 5. Architect dual-model pipeline  `[mesh] architect_mode`

Each turn runs a **plan** phase (a strong planner model, no tools, prose plan) then an **edit** phase
(a cheaper editor model runs the normal tool loop, seeing the plan). Plays to the mesh: planner and
editor are independently routed/pinned. Both phases record usage. Off → byte-for-byte unchanged.

```toml
[mesh]
architect_mode = true
architect_model = "claude-cli::opus"      # optional; else Complex-tier route
editor_model    = "openrouter::..."        # optional; else Standard-tier route
```

## 6. Assay-gated auto-review  `[assay] auto_review`

Before a write turn finishes (after autofix), Forge builds a unified diff of the files changed this
turn and runs the existing Assay critic crew over it (`AssayScope::Diff`). Findings at/above
`gate_severity` are surfaced; in `gate_mode = "block"` the turn fails. Skipped when the diff is below
`min_diff_bytes`. Turns Forge's strongest review engine into a pre-commit safety gate.

```toml
[assay]
auto_review = true
gate_severity = "high"     # low | medium | high
gate_mode = "warn"         # warn | block
min_diff_bytes = 200
```

## 7. Quick wins: `/model` picker + `/effort`  `[mesh] default_effort`

- bare `/model` opens the model picker and pins the chosen model; `/model <id>` still pins directly.
- `/effort low|medium|high|xhigh` pins per-session reasoning effort, threaded to API providers via a
  new `Provider::complete_with` + `CompletionOptions` (no change to existing `complete()` call
  sites); CLI bridges ignore it. Seedable via `[mesh] default_effort`.

---

## Known follow-ups

- **Autofix inner loop is duplicated** verbatim in `run_turn` rather than extracted into a shared
  helper (threading `active_model`/`specs`/`max_steps`/etc. is a larger refactor). Bracketed and
  flagged; extract in a cleanup pass.
- **MCP-hook bridge test**: no in-process MCP echo harness exists yet, so the bridge-path hook change
  rides on the built-in-branch parity + the `hooks.rs` unit tests until a harness lands.
- Deferred features: OS sandbox (Landlock/seccomp), Lattice 130-language polyglot + PageRank repo-map.
