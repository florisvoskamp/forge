<div align="center">

# ⚒ Forge

**A fast, model-agnostic AI coding harness and CLI — built in Rust.**

*You don't pick a model. Forge routes every task to the optimal model for cost × capability.*

[![CI](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml/badge.svg)](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)

</div>

---

> **Status: pre-alpha / design phase.** Architecture is being designed and documented
> before implementation. See [`docs/architecture/`](./docs/architecture/) for the
> requirements, decision records, and system design.

## What is Forge?

Forge is a self-hosted AI coding assistant for the terminal — like Claude Code, but
not locked to one provider, and built around a **Model Mesh**: a routing engine that
automatically decomposes work and sends each task to the cheapest model that can do it
well. Trivial edits go to a local or cheap model; hard reasoning goes frontier. You set
a budget; Forge stays under it.

- **Multi-provider, BYOK** — Anthropic, OpenAI, and local Ollama in v0.1; more to follow.
- **Model Mesh** — automatic cost × capability routing with live cost metering.
- **Native Rust** — sub-millisecond startup, single static binary, no runtime.
- **Beautiful TUI** — `ratatui`-based live agent progress, cost meter, routing decisions.
- **MCP client** — connect external MCP servers (stdio + HTTP/SSE) and drive their tools
  through Forge's permission gate; declare them in `.forge/mcp.toml` or `forge mcp import`
  a Claude-Code `.mcp.json`. `forge mcp` / `/mcp` list status; tools load on demand.
- **Commands & skills** — reusable prompt templates and methodologies as markdown files in
  `./.forge/commands`, `./.forge/skills/<name>/SKILL.md` (and the same dirs under your user
  config). Type `/` in the TUI for a fuzzy palette; `/name args` expands a template (`$1`,
  `$ARGUMENTS`), `/skill <name>` injects a methodology. Claude-Code's `~/.claude/commands` and
  `skills` formats parse unchanged. `forge commands` lists them. Project-scope files are
  confirmed on first use (they can steer the model).
- **Local-first** — SQLite session state, no cloud account required to run.

The full vision (semantic code memory, multi-agent orchestration, MCP, marketplace,
voice, session replay, migration import) is on the roadmap; see the architecture docs
for what is in v0.1 versus later.

## Documentation

| Doc | What |
|-----|------|
| [`docs/architecture/01-requirements.md`](./docs/architecture/01-requirements.md) | Confirmed requirements (functional, non-functional, constraints) |
| [`docs/architecture/02-architecture.md`](./docs/architecture/02-architecture.md) | System design with C4 diagrams |
| [`docs/architecture/decisions/`](./docs/architecture/decisions/) | Architecture Decision Records |
| [`CONTRIBUTING.md`](./CONTRIBUTING.md) | How to build, test, and contribute |

## License

[MIT](./LICENSE) © 2026 Floris Voskamp
