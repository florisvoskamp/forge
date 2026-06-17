<div align="center">

# ⚒ Forge

**A fast, model-agnostic AI coding assistant — built in Rust.**

*You don't pick a model. Forge routes every task to the optimal model for cost × capability.*

[![CI](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml/badge.svg)](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

</div>

---

Forge is a self-hosted AI coding assistant for the terminal — like Claude Code, but not locked to one provider and not locked to one model. Its **Model Mesh** automatically classifies every task by complexity and routes it to the cheapest model that can handle it well. Trivial edits go to a local or free model; hard reasoning goes frontier. You set a budget; Forge stays under it.

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
| **Model Mesh** | Auto-discovery, cost-tiered routing, health-aware failover, subscription bridges, budget caps |
| **Providers** | Anthropic, OpenAI, Ollama, Claude Code CLI, Codex CLI, Groq, Gemini, DeepSeek, OpenRouter, and more |
| **Code Intelligence** | Lattice: tree-sitter symbol graph, semantic embeddings, hybrid retrieval, blast-radius, call-chain |
| **MCP** | Client for external MCP servers (stdio + HTTP/SSE), static bearer auth, deferred loading, allowlist gating |
| **TUI** | ratatui live progress, cost meter, token gauge, command palette, session picker, diff preview |
| **Skills & Commands** | Markdown prompt templates, skill methodology injection, Claude Code format compatible |
| **Subagents** | Parallel fan-out (`spawn_agents`), mesh-routed children, live TUI tree, depth-limited |
| **Session Management** | Checkpoints, `/undo` with file restore, session replay, transcript diff |
| **Hooks** | Pre/post tool-use shell hooks — block (pre) or observe (post) any tool call; fires on both direct and CLI-bridge paths |
| **Safety** | Permission broker, per-tool rules, diff preview before write, shadow file snapshots |

---

## Quick Start

```bash
# Build
cargo build --release

# First-run wizard (API keys + subscription plans)
forge init

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

The mesh is **health-aware**: rate-limited or unavailable models are benched with a cooldown and the next fallback is tried automatically. Under budget pressure the mesh downshifts tiers.

### Supported Providers

| Provider | Mode | Notes |
|----------|------|-------|
| Anthropic | Direct (API key) | Claude family |
| OpenAI | Direct (API key) | GPT-4 family |
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
forge chat --resume abc123      # resume a previous session
forge chat --model anthropic::claude-opus-4-8  # pin a model
forge chat --mode accept-edits  # auto-allow file writes
forge chat --plain              # headless / CI mode
```

**In-session slash commands:**

| Command | Description |
|---------|-------------|
| `/new` | Start a fresh session |
| `/resume [id]` | Resume a previous session |
| `/sessions` | Browse + pick a past session |
| `/undo` | Revert last turn (restores edited files) |
| `/checkpoints [label]` | Browse + rewind to any checkpoint |
| `/compact` | Summarize older context to free the window (also auto-triggers at 80% gauge) |
| `/mode` | Switch permission mode interactively |
| `/models` | Browse all discovered models |
| `/mcp [tools <server>]` | Show MCP server status |
| `/assay` | Run code-quality analysis crew |
| `/goal <objective>` | Set a persistent goal the agent tracks |
| `/loop <task>` | Run autonomously until complete (≤25 turns) |
| `/lattice <symbol>` | Query code intelligence inline |
| `/config` | Open configuration wizard |
| `/` | Open command palette (fuzzy-find skills + commands) |

**Keyboard shortcuts:**

| Key | Action |
|-----|--------|
| `SHIFT+TAB` | Cycle permission mode (Read-only → Ask → Auto-edit → Full) |
| `Ctrl+O` | Open subagent viewer (↑↓ select agent, Enter open transcript, Esc close) |
| `Esc` | Close palette / cancel |
| `↑ / ↓` | Navigate palettes and pickers |

### `forge run`

Single agent turn, non-interactive.

```bash
forge run "add tests for the payment service"
forge run --tui "debug the startup crash"      # with live TUI
forge run --mode bypass "apply all the diffs"  # no prompts
```

### `forge models`

Show all discovered models and the mesh's auto-pick per tier.

```bash
forge models            # catalog overview
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

The index is injected automatically as context before each agent turn (hybrid semantic + structural retrieval, budgeted).

### `forge sessions` / `forge replay`

Audit past work.

```bash
forge sessions              # list sessions, newest first (cost, messages, preview)
forge replay abc123         # reconstruct turn-by-turn transcript
forge replay abc123 def456  # diff two session summaries
```

### `forge mcp`

Manage external MCP server connections.

```bash
forge mcp                   # show server status
forge mcp tools myserver    # list tools on a specific server
forge mcp import            # wizard: scan installed AI CLIs, pick servers to import
forge mcp import path/to/.mcp.json  # import a specific file
```

### `forge commands`

List all discovered slash commands and skills with scope and conflict markers.

```bash
forge commands
```

### `forge import`

Migrate commands and skills from other AI CLIs.

```bash
forge import claude          # copy ~/.claude/commands + ~/.claude/skills/
forge import codex           # copy ~/.codex/prompts/ as commands
forge import claude --project  # import to ./.forge instead of user config
```

### `forge init`

First-run setup wizard — API keys, subscription plans, provider selection.

```bash
forge init
```

### `forge auth`

Manage API keys in your OS keyring.

```bash
forge auth anthropic     # store key (read from stdin)
forge auth --remove groq # remove key
```

---

## Lattice — Code Intelligence

Lattice is Forge's built-in code graph. It parses your repo with **tree-sitter** (40+ languages), stores the symbol graph in SQLite, computes **semantic embeddings**, and injects relevant context before each agent turn automatically.

```
forge lattice update .
✓  Indexed 312 files — 4 217 symbols, 18 923 edges (2.1s)

forge lattice impact "UserRepository"
→  UserService  (calls, 3 refs)
→  AuthMiddleware  (uses, 1 ref)
→  SessionStore  (inherits, 1 ref)

forge lattice path "main" "persist"
→  main → run_session → agent_loop → tool_registry → persist
```

**Context injection** is hybrid: embedding-based semantic neighbors + structural references, scaled by budget pressure (full context budget → reduced under cost pressure).

---

## Subagent Orchestration

The agent can spawn concurrent child agents via `spawn_agents` for decomposed parallel work. Each child gets its own mesh-routed session, persisted to the store, with live progress visible in the TUI as an expandable tree.

Depth is bounded (`max_depth = 2` by default). Subscription CLI bridges (claude, codex) can also fan out via `spawn_agents` — they receive Forge's tools through MCP.

---

## Skills & Commands

Forge supports reusable prompt templates (commands) and methodology guides (skills) as markdown files. Claude Code's `~/.claude/commands` and `~/.claude/skills` format is compatible — import them directly with `forge import claude`.

**Command** — expands a template with your args:
```markdown
---
title: Refactor
description: Clean up code structure
args:
  - name: scope
    required: true
tier: standard
---

Refactor $1 for readability and maintainability.
```

**Skill** — injects a methodology into the agent's context:
```markdown
---
title: TDD
description: Red-green-refactor workflow
---

Write a failing test first. Then make it pass with the simplest code possible. Then refactor.
```

Type `/` in the TUI to open the fuzzy command palette. Use `/skill <name>` to load a skill explicitly, or `/command args` to expand a command.

**Scope precedence:** Project (`./.forge`) shadows User (`~/.config/forge`) shadows Builtin. Project-scope commands are confirmed on first use.

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

Server tools are exposed to the agent via meta-tools (`mcp_search_tools`, `mcp_call`) — the server's full tool list is never loaded upfront, keeping the model's tool space small. All MCP calls pass through Forge's permission broker. Secrets come from env vars or the OS keyring, never from the config file.

**Import existing configs** from Claude Code, Cursor, Windsurf, VS Code, or Codex:

```bash
forge mcp import   # wizard: auto-scan installed AI CLIs
```

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

`pre_tool_use` hooks can **block** a call by exiting non-zero (stderr becomes the reason shown to the model). `post_tool_use` hooks observe only. Both receive the tool call as JSON on stdin, time-bounded.

Hooks run on **both** the direct path (`forge chat` / `forge run`) and the CLI-bridge path (`forge mcp-serve` + claude/codex). On Windows, hooks use `cmd /C`; on Unix, `sh -c`.

---

## Session Safety

- **Permission broker** — every side-effecting tool call requires confirmation (or is auto-allowed based on mode + per-tool rules)
- **Diff preview** — file writes show a unified diff *before* the permission prompt
- **Shadow snapshots** — pre-edit bytes are captured before each permitted write; `/undo` restores them
- **Checkpoints** — every turn creates a checkpoint; rewind to any point with `/checkpoints`
- **Audit trail** — all tool calls, routing decisions, and permission outcomes are persisted

**Permission modes** (cycle with `SHIFT+TAB` or `/mode`):

| Mode | File writes | Shell | External |
|------|-------------|-------|----------|
| **Read-only** | Denied | Denied | Denied |
| **Ask** | Prompt | Prompt | Prompt |
| **Auto-edit** | Allowed | Prompt | Prompt |
| **Full** | Allowed | Allowed | Allowed |

---

## Budget Control

Set daily and monthly caps in config:

```toml
[mesh]
daily_budget_usd = 5.0
monthly_cap_usd = 50.0
```

Forge tracks spend across both axes. At 80% it warns; at the cap it stops (overridable with `FORGE_BUDGET_OVERRIDE=1`). Under budget pressure, the mesh automatically downshifts to cheaper models and reduces Lattice context injection.

**Context auto-compaction:** when the context gauge reaches 80% of the model's window at turn-end, Forge automatically runs `/compact` — no manual action needed. A note is shown in the TUI.

---

## Configuration

Forge uses layered config — defaults → user → project → env vars:

| Layer | Path |
|-------|------|
| User | `~/.config/forge/config.toml` |
| Project | `./.forge/config.toml` |
| MCP servers | `./.forge/mcp.toml` |
| Agent types | `./.forge/agents/<name>.toml` |
| Env override | `FORGE_*` prefix |
| API keys | OS keyring (via `forge auth`) |

Key config sections: `[mesh]` (routing, budget, failover), `[[permissions]]` (per-tool rules), `[lattice]` (indexing + embeddings), `[shell]` (error interceptor), `[[hooks]]`, `[mcp]`.

---

## Documentation

| Doc | What |
|-----|------|
| [`docs/architecture/01-requirements.md`](./docs/architecture/01-requirements.md) | Confirmed requirements |
| [`docs/architecture/02-architecture.md`](./docs/architecture/02-architecture.md) | System design with C4 diagrams |
| [`docs/architecture/decisions/`](./docs/architecture/decisions/) | Architecture Decision Records |
| [`docs/features/`](./docs/features/) | Per-feature design docs (30+ features) |
| [`CONTRIBUTING.md`](./CONTRIBUTING.md) | How to build, test, and contribute |

---

## Project Layout

```
crates/
├── forge-cli        # binary, clap commands, init wizard
├── forge-core       # agent loop, session lifecycle, permission broker
├── forge-mesh       # model router — classification, health, failover, budget
├── forge-provider   # provider trait — Anthropic, OpenAI, Ollama, CLI bridges
├── forge-tools      # tool registry — read, write, edit, shell, search, web
├── forge-store      # SQLite persistence — sessions, messages, usage, tasks
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
