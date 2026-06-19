<div align="center">

# ⚒ Forge

**A fast, model-agnostic AI coding agent for the terminal — built in Rust.**

*You don't pick a model. Forge routes every task to the optimal model for cost × capability.*

[![CI](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml/badge.svg)](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

</div>

---

Forge is a self-hosted AI coding agent for the terminal — like Claude Code, but not locked to one provider and not locked to one model. Its **Model Mesh** classifies every task by complexity and routes it to the cheapest model that can handle it well: trivial edits go to a local or free model, hard reasoning goes frontier. You set a budget; Forge stays under it, and falls back automatically when a model is rate-limited or down.

```
$ forge chat
$ forge run "add pagination to the user list endpoint"
$ forge models --probe
$ forge lattice query "UserRepository"
```

---

## Feature Overview

| Category | Features |
|----------|----------|
| **Model Mesh** | Auto-discovery, cost-tiered routing, benchmark ranking, health-aware failover, subscription bridges, daily/weekly/monthly budget caps, credit-conservation modes |
| **Providers** | Anthropic, OpenAI, Ollama, Claude Code CLI, Codex CLI, Groq, Gemini, DeepSeek, OpenRouter, xAI, Cerebras, and more |
| **Planning mode** | `/plan` investigates read-only and proposes a plan; `/execute` approves it and carries it out |
| **Code Intelligence** | Lattice: tree-sitter symbol graph (9 languages), semantic embeddings, hybrid retrieval, blast-radius, call-chain, git provenance |
| **LSP feedback** | Live diagnostics from a language server fed back after edits so the model self-corrects (`[lsp]`, opt-in; rust/ts/js/python/go) |
| **Autofix loop** | Run lint/test after edits and self-heal on failure, up to N iterations (`[autofix]`, opt-in) |
| **Architect mode** | Dual-model turns — strong planner drafts a plan, cheaper editor applies it (`[mesh] architect_mode`, opt-in) |
| **Context** | `@file` mentions inject file contents; project memory auto-loaded from `.forge/AGENTS.md` (scaffold with `/init`); Lattice auto-injection |
| **Vision** | Attach images by `/image <path>` or paste them straight into the input bar as inline blocks |
| **Assay** | Parallel critic crew, adversarial verification, ranked findings, git scopes (diff/branch/since), lens selection, auto-diff vs prior run; opt-in auto-review gate over a turn's diff (`[assay] auto_review`, warn/block) |
| **MCP** | Client for external MCP servers (stdio + HTTP/SSE), OAuth 2.0 + PKCE, deferred loading, allowlist gating |
| **TUI** | ratatui live progress, cost meter, context-window token gauge, fuzzy command palette, session/checkpoint pickers, `/usage` + `/mesh` overlays, `/model` picker, `/effort` reasoning knob, focus-aware blinking cursor |
| **Skills & Commands** | Markdown prompt templates + skill methodology injection; Claude Code format compatible |
| **Subagents** | Parallel fan-out (`spawn_agents`), mesh-routed children, live TUI tree, depth-limited, opt-in git-worktree isolation for write-capable children |
| **Session Management** | Checkpoints, `/undo` with file restore, session replay + JSON export, transcript diff, assay run history |
| **Remote control** | Drive a session from a phone/desktop browser (`/remote`) — LAN, loopback, or public tunnel |
| **Hooks** | Pre/post tool-use shell hooks — block (pre) or observe (post) any tool call; fires on both direct and CLI-bridge paths, including MCP tool calls |
| **Cost** | Prompt caching, per-model pricing fetched from OpenRouter (cache-read aware), persistent cross-restart usage store |
| **Git** | Optional model-aware co-author attribution on commits + PR bodies |
| **Safety** | Permission broker, per-tool rules, diff preview before write, shadow file snapshots, unoverridable denylist; opt-in OS shell sandbox (Linux Landlock, `[shell] sandbox`) |

---

## Install

### One-line install (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/florisvoskamp/forge/main/install.sh | sh
```

Detects your OS/arch, downloads the matching release binary (verifying its
SHA-256), and installs `forge` to `~/.local/bin`. Override with `FORGE_VERSION`
(a tag) or `FORGE_INSTALL_DIR`. Linux x86-64 and macOS (Apple Silicon + Intel)
are supported; on other arches it falls back to building from source.

### Homebrew

```bash
brew tap florisvoskamp/forge https://github.com/florisvoskamp/forge
brew install forge
```

### Prebuilt binaries

Grab the latest release for your OS from the [**Releases**](https://github.com/florisvoskamp/forge/releases) page:

| OS | Asset |
|----|-------|
| Linux (x86-64) | `forge-x86_64-unknown-linux-gnu.tar.gz` |
| macOS (Apple Silicon) | `forge-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `forge-x86_64-apple-darwin.tar.gz` |
| Windows (x86-64) | `forge-x86_64-pc-windows-msvc.zip` |

Unpack and put `forge` on your `PATH`.

### From source

```bash
cargo build --release          # produces target/release/forge
cp target/release/forge ~/.local/bin/   # or anywhere on PATH
```

Requires a recent stable Rust toolchain.

---

## Quick Start

```bash
# First-run wizard (API keys + subscription plans)
forge init

# Generate project memory the agent auto-loads next session
forge chat          # then type: /init

# Interactive chat
forge chat

# One-shot task
forge run "refactor the auth middleware to use tower layers"

# See discovered models + auto-pick per tier
forge models

# Index your codebase
forge lattice update .
forge lattice query "authenticate"
```

No API key required to test — `--mock` runs an offline deterministic provider:

```bash
forge run --mock "hello"
forge chat --mock
```

---

## Model Mesh

Forge's routing engine classifies every task into a tier, then picks the cheapest model that meets the bar:

| Tier | Examples | Goes to |
|------|----------|---------|
| **Trivial** | Single-line edits, simple lookups | Local / cheapest free model |
| **Standard** | Multi-file refactors, code review | Mid-tier cloud model |
| **Complex** | Architecture, deep debugging, new features | Frontier model |

The mesh is **health-aware**: rate-limited or unavailable models are benched with a cooldown and the next fallback is tried automatically (down the full ranked catalog, not a fixed top-5). It is **benchmark-ranked** against real Artificial Analysis intelligence + coding scores, and **conservation-aware** — under budget pressure or to spare a metered subscription, it spreads work onto free frontier models.

Inspect any routing decision live with `/mesh [task]` or `forge mesh "<task>"`.

### Supported Providers

| Provider | Mode | Notes |
|----------|------|-------|
| Anthropic | Direct (API key) | Claude family |
| OpenAI | Direct (API key) | GPT-5 family |
| Ollama | Local (no key) | Any local model |
| Claude Code CLI | Subscription bridge | Uses your Claude subscription |
| Codex CLI | Subscription bridge | Uses your OpenAI subscription |
| Groq | Direct (API key) | Free tier available |
| Gemini | Direct (API key) | Free tier available |
| DeepSeek | Direct (API key) | |
| OpenRouter | Direct (API key) | Routes to many providers |
| xAI, MiniMax, MiMo, Cerebras | Direct (API key) | |

Store keys in your OS keyring — never in plaintext config:

```bash
forge auth anthropic    # reads from stdin
forge auth openai
forge auth --remove openai
```

---

## CLI Reference

### `forge chat`

Interactive multi-turn session with the full TUI.

```bash
forge chat
forge chat --resume abc123                      # resume a previous session
forge chat --model anthropic::claude-opus-4-8   # pin a model
forge chat --mode accept-edits                  # auto-allow file writes
forge chat --plain                              # headless / CI mode
```

**In-session slash commands:**

| Command | Description |
|---------|-------------|
| `/plan <task>` | Planning mode — investigate read-only and propose a plan (no edits) |
| `/execute` | Approve the proposed plan and carry it out (switches to Auto-edit); aliases `/approve`, `/go` |
| `/init` | Scan the repo and write `.forge/AGENTS.md` project memory |
| `/new` | Start a fresh session |
| `/resume [id]` | Resume a previous session |
| `/sessions` | Browse + pick a past session |
| `/undo` | Revert last turn (restores edited files) |
| `/checkpoint [label]` | Save a named checkpoint here |
| `/checkpoints` | Browse + rewind to any checkpoint |
| `/compact` | Summarize older context to free the window (also auto-triggers at 80% gauge) |
| `/mode` | Switch permission mode (temper) interactively |
| `/model [<id>]` | Pin a model for this session; no arg clears the pin |
| `/models` | Browse all discovered models |
| `/usage` | API spend + token usage across providers (incl. subscription %) |
| `/mesh [task]` | Inspect mesh routing — classification, scores, quota, conservation |
| `/mcp [server]` | Show MCP server status (or one server's tools) |
| `/assay [--diff\|--branch <b>\|--since <ref>\|<path>] [--only <lens,…>] [--skip <lens,…>]` | Run code-quality analysis crew |
| `/goal <objective>` | Set a persistent goal the agent tracks |
| `/loop <task>` | Run autonomously until complete (≤25 turns) |
| `/replay <id> [<id2>]` | Show a session transcript inline, or diff two sessions |
| `/lattice <symbol>` | Query code intelligence inline |
| `/image <path>` | Attach an image to the next message (vision) |
| `/thinking` | Toggle display of model reasoning/thinking blocks |
| `/remote [--lan\|--local\|--anywhere]` | Toggle browser remote control |
| `/config` | Open configuration wizard |
| `/clear` | Clear the screen (keep the session) |
| `/` | Open command palette (fuzzy-find skills + commands) |

Type `@` to fuzzy-pick a file; the `@path` token's **contents** are injected into the turn on submit.

**Keyboard shortcuts:**

| Key | Action |
|-----|--------|
| `SHIFT+TAB` | Cycle temper (Read-only → Ask → Auto-edit); the chosen temper is remembered as your default |
| `Ctrl+O` | Open subagent viewer (↑↓ select agent, Enter open transcript, Esc close) |
| `Ctrl+J` | Insert a newline without submitting |
| `Esc` | Close palette / cancel / stop a running turn |
| `↑ / ↓` | Navigate palettes and pickers (or prompt history in the input) |
| `y / n / a` | Allow / deny / always-allow a permission prompt (`a` persists to config) |

### `forge run`

Single agent turn, non-interactive.

```bash
forge run "add tests for the payment service"
forge run --tui "debug the startup crash"      # with live TUI
forge run --mode bypass "apply all the diffs"  # no prompts
```

### `forge models`

```bash
forge models            # catalog overview + auto-pick per tier
forge models --probe    # ping every model, persist health results
forge models --clear    # forget all benched/rate-limited marks
```

### `forge lattice`

Code intelligence — tree-sitter symbol graph over your repo.

```bash
forge lattice update .               # (re)index, incremental by content hash
forge lattice status                 # files, symbols, edges, embeddings
forge lattice query "UserRepository" # find symbol by name
forge lattice impact "UserService"   # blast radius — what depends on it
forge lattice path "main" "persist"  # shortest call chain A → B
forge lattice why "authenticate"     # git provenance — who last changed it
forge lattice embed                  # compute semantic embeddings for nodes
```

### `forge git`

```bash
forge git setup          # install the model-aware co-author commit hook
```

### Audit + migrate

```bash
forge sessions               # list sessions, newest first
forge replay abc123          # reconstruct turn-by-turn transcript
forge replay abc123 def456   # diff two session summaries
forge replay abc123 --json   # emit full transcript as JSON
forge assay list             # list past assay runs
forge assay compare a1 b2    # diff findings: fixed / new / still-open
forge commands               # list discovered commands + skills
forge import claude          # migrate ~/.claude commands/skills/agents
forge import codex           # migrate ~/.codex/prompts as commands
forge mcp                    # MCP server status
forge mcp import             # wizard: scan installed AI CLIs
forge auth anthropic         # store an API key in the OS keyring
```

---

## Planning Mode

For non-trivial work, plan before you act:

```
/plan migrate the store layer from rusqlite to sqlx
```

Forge switches to **read-only (Plan)** temper, investigates the codebase using its tools, and presents an ordered, step-by-step plan — making **no edits**. Review it, then:

```
/execute        (or /approve, /go)
```

Forge switches to **Auto-edit** and carries out the plan it proposed, step by step.

---

## `@file` Context & Project Memory

- **`@file` mentions** — type `@` to fuzzy-pick a file; on submit the file's contents are read and injected into the turn as context (size- and binary-capped). The `@path` stays in your prompt; the contents ride along behind the scenes.
- **Project memory** — `/init` scans the repo and writes `.forge/AGENTS.md` (overview, build/test/run, layout, conventions). On every future session Forge auto-loads `.forge/AGENTS.md` (or a top-level `AGENTS.md`) as a standing system prompt.

---

## Vision Input

Forge accepts images as vision input:

```
/image screenshots/bug.png        # attach a file
```

You can also **paste an image directly** into the input bar — it appears as an inline `[image …]` block (deletable as a unit), and is sent as vision input on submit. Costs are priced and tracked like any other usage.

---

## Lattice — Code Intelligence

Lattice is Forge's built-in code graph. It parses your repo with **tree-sitter** across **9 languages** — Rust, Python, JavaScript, TypeScript (+TSX), Go, Java, C, C++, Ruby — stores the symbol graph in SQLite, optionally computes **semantic embeddings**, and injects relevant context before each agent turn automatically.

```
forge lattice update .
✓  Indexed 312 files — 4 217 symbols, 18 923 edges (2.1s)

forge lattice impact "UserRepository"
→  UserService  (calls, 3 refs)
→  AuthMiddleware  (uses, 1 ref)

forge lattice path "main" "persist"
→  main → run_session → agent_loop → tool_registry → persist
```

Context injection is hybrid (embedding-based semantic neighbors + structural references), scaled down under budget pressure.

---

## Assay — Code Quality Analysis

`/assay` runs a parallel **critic crew** that scans your codebase (or a scoped diff) and produces a ranked, adversarially-verified findings report. Every candidate is independently challenged by a refuter; false positives are dropped before the report is assembled.

```
/assay                         # full repo
/assay src/lib.rs              # single file or subtree
/assay --diff                  # uncommitted working-tree changes only
/assay --branch feature/x      # files changed vs main
/assay --since HEAD~10         # files changed since a git ref
/assay --only dead-weight,unsafe   # run only these critics
/assay --skip documentation        # run all except these
```

**Lenses:** `dead-weight`, `correctness`, `unsafe`, `test-coverage`, `design`, `architecture`, `documentation`, `over-engineering`.

**Modes:** *Analysis only* (read-only ranked report) or *Full cleanup (Refine)* — hands findings to a permission-gated, undoable fix turn. Each run is persisted; subsequent runs for the same scope show *N fixed / N new / N still-open* vs the prior run.

---

## Subagent Orchestration

The agent can spawn concurrent child agents via `spawn_agents` for decomposed parallel work. Each child gets its own mesh-routed session, persisted to the store, with live progress visible in the TUI as an expandable tree. Depth is bounded (`max_depth = 2` by default). Subscription CLI bridges (claude, codex) can also fan out — they receive Forge's tools through MCP.

---

## Skills & Commands

Forge supports reusable prompt templates (commands) and methodology guides (skills) as markdown files. Claude Code's `~/.claude/commands` and `~/.claude/skills` format is compatible — import them directly with `forge import claude`.

**Command** — expands a template with your args:
```markdown
---
name: refactor
description: Clean up code structure
args: [scope]
tier: standard
---

Refactor $1 for readability and maintainability.
```

**Skill** — injects a methodology into the agent's context:
```markdown
---
name: tdd
description: Red-green-refactor workflow
---

Write a failing test first. Then make it pass with the simplest code possible. Then refactor.
```

Type `/` to open the fuzzy palette. Use `/skill <name>` to load a skill explicitly, or `/command args` to expand a command. **Scope precedence:** Project (`./.forge`) shadows User (`~/.config/forge`) shadows Builtin. Project-scope commands are confirmed on first use.

---

## MCP Integration

Connect Forge to any MCP server (stdio or HTTP/SSE) by declaring it in `.forge/mcp.toml`:

```toml
[[servers]]
name = "github"
transport = "stdio"
command = "npx -y @modelcontextprotocol/server-github"

[servers.auth]
token_env = "GITHUB_TOKEN"

[servers.allowlist]
tools = ["create_issue", "list_prs"]
```

Server tools are exposed to the agent as namespaced `ToolSpec`s (e.g. `github__create_issue`). An optional allowlist keeps the tool space small. All MCP calls pass through Forge's permission broker; secrets come from env vars or the OS keyring. OAuth 2.0 + PKCE is supported for protected HTTP servers (`forge mcp login <server>`). Import existing configs from Claude Code, Cursor, Windsurf, VS Code, or Codex with `forge mcp import`.

---

## Hooks

Run shell commands around tool calls:

```toml
# .forge/config.toml
[[hooks]]
event = "pre_tool_use"
tool_pattern = "shell"
command = "bash -c 'jq .args <<< $FORGE_TOOL_INPUT >> audit.log'"
timeout_secs = 5

[[hooks]]
event = "post_tool_use"
tool_pattern = "*"
command = "bash -c 'echo done >> hooks.log'"
```

`pre_tool_use` hooks can **block** a call by exiting non-zero (stderr becomes the reason shown to the model). `post_tool_use` hooks observe only. Both receive the tool call as JSON on stdin, time-bounded, and run on **both** the direct path and the CLI-bridge path. On Windows hooks use `cmd /C`; on Unix, `sh -c`.

---

## Git Attribution

Enable model-aware co-author attribution on commits and PRs:

```toml
# .forge/config.toml
[git]
coauthor = true
```

When enabled, Forge auto-installs a `prepare-commit-msg` hook that strips any Claude/Codex/Anthropic co-author lines and adds `Co-Authored-By: Forge (<model>) <noreply@forge.dev>` — where `<model>` is whichever model actually ran the turn. The agent is also primed to credit Forge in PR bodies it opens. Run `forge git setup` to (re)install the hook manually.

---

## Session Safety

- **Permission broker** — every side-effecting tool call requires confirmation (or is auto-allowed based on temper + per-tool rules)
- **Diff preview** — file writes show a unified diff *before* the permission prompt
- **Shadow snapshots** — pre-edit bytes are captured before each permitted write; `/undo` restores them
- **Checkpoints** — every turn creates a checkpoint; rewind to any point with `/checkpoints`
- **Audit trail** — all tool calls, routing decisions, and permission outcomes are persisted

**Tempers** (cycle with `SHIFT+TAB`, or pick with `/mode`; the choice is remembered as your default):

| Temper | File writes | Shell | External |
|--------|-------------|-------|----------|
| **Read-only** | Denied | Denied | Denied |
| **Ask** | Prompt | Prompt | Prompt |
| **Auto-edit** | Allowed | Prompt | Prompt |
| **Full** | Allowed | Allowed | Allowed |

`Full` is opt-in only (`--mode full` / config) — never reachable by cycling. An unoverridable denylist always applies.

---

## Cost & Budget Control

Forge prices every turn from per-model rates fetched at discovery (OpenRouter, cache-read aware) and tracks spend in a persistent store that survives restarts. Prompt caching is used where the provider supports it. See live spend and token usage with `/usage`.

```toml
[mesh]
daily_budget_usd = 5.0
monthly_cap_usd = 50.0
```

At 80% Forge warns; at the cap it stops (override with `FORGE_BUDGET_OVERRIDE=1`). Under budget pressure the mesh downshifts to cheaper models and reduces Lattice context. **Context auto-compaction:** when the token gauge reaches 80% of the model's window at turn-end, Forge runs `/compact` automatically.

---

## Continuous Integration

Run the Assay critic crew headlessly in any pipeline:

```bash
forge assay run --scope diff --format markdown --fail-on high
```

- `--format human|markdown|json|sarif` — `sarif` uploads to GitHub code-scanning; `markdown` is PR-comment shaped.
- `--fail-on low|medium|high|critical` — exit code `2` when a finding meets the threshold (CI fails); omit to never fail.
- `--scope diff|repo` (+ `--branch`/`--since`/`--path`). Reads `ANTHROPIC_API_KEY` / `OPENROUTER_API_KEY` from the environment (no keyring needed in CI).

**GitHub Action** — post findings as a sticky PR comment:

```yaml
- uses: florisvoskamp/forge/.github/actions/forge-assay@main
  with:
    version: v0.1.0          # a release whose binary has `forge assay run`
    scope: diff
    fail-on: high
    anthropic-api-key: ${{ secrets.ANTHROPIC_API_KEY }}
```

See `docs/ci/forge-review.yml` for a full example workflow to copy into your repo's `.github/workflows/`.

---

## Configuration

Layered config — defaults → user → project → env vars:

| Layer | Path |
|-------|------|
| User | `~/.config/forge/config.toml` |
| Project | `./.forge/config.toml` |
| MCP servers | `./.forge/mcp.toml` |
| Agent types | `./.forge/agents/<name>.toml` |
| Project memory | `./.forge/AGENTS.md` |
| Env override | `FORGE_*` prefix |
| API keys | OS keyring (via `forge auth`) |

Key sections: `[mesh]` (routing, budget, conservation, failover), `[permissions]` (per-tool rules), `[lattice]` (indexing + embeddings), `[shell]` (error interceptor), `[[hooks]]`, `[mcp]`, `[git]`, `[commands]`.

---

## Documentation

| Doc | What |
|-----|------|
| [`docs/architecture/01-requirements.md`](./docs/architecture/01-requirements.md) | Confirmed requirements |
| [`docs/architecture/02-architecture.md`](./docs/architecture/02-architecture.md) | System design with C4 diagrams |
| [`docs/architecture/decisions/`](./docs/architecture/decisions/) | Architecture Decision Records |
| [`docs/features/`](./docs/features/) | Per-feature design docs |
| [`CONTRIBUTING.md`](./CONTRIBUTING.md) | How to build, test, and contribute |

---

## Project Layout

```
crates/
├── forge-cli        # binary, clap commands, init wizard, TUI render loop
├── forge-core       # agent loop, session lifecycle, permission broker
├── forge-mesh       # model router — classification, ranking, health, failover, budget
├── forge-provider   # provider trait — Anthropic, OpenAI, Ollama, CLI bridges
├── forge-tools      # tool registry — read, write, edit, shell, search, web
├── forge-store      # SQLite persistence — sessions, messages, usage, pricing, tasks
├── forge-tui        # ratatui renderer + headless presenter
├── forge-config     # layered config + OS keyring secret resolution
├── forge-index      # Lattice — tree-sitter extraction, graph, embeddings
├── forge-mcp        # MCP client — rmcp, meta-tools, allowlist, OAuth
├── forge-skills     # skills + commands catalog, CC-format reader
└── forge-types      # shared domain types
```

---

## License

[MIT](./LICENSE) © 2026 Floris Voskamp
