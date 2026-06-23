# Changelog

All notable changes to Forge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and Forge adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Interactive plan mode: `/plan` now has the agent call a `present_plan` tool that renders a
  bordered, animated **plan card** (title + numbered steps + notes) instead of loose prose. You
  approve it interactively — approving switches to Auto-edit and **auto-builds** the plan, typing
  changes revises it, cancelling keeps you in planning. The plan's steps seed the live task list so
  build progress is visible, and every draft/revision is saved to `.forge/plans/`. Works on direct
  API models and the claude/codex bridges alike.
- Windows PowerShell installer (`install.ps1`): `irm …/install.ps1 | iex` downloads the x86-64
  release binary (SHA-256 verified), installs `forge.exe` to `%LOCALAPPDATA%\Programs\forge`, and
  adds it to the user `PATH`. `install.sh` now points Windows users to it.
- Anchored-block fuzzy tier for `edit_file` / `multi_edit`: when an `old` block's interior was
  paraphrased but its first/last lines match, the unique span between those anchors is replaced —
  guarded by uniqueness + a disproportionate-match rejection so it can't silently eat the wrong
  region.
- `docs/harness/competitive-analysis.md` — recon of competing coding-agent harnesses and a
  prioritized backlog of techniques to port.
- `forge doctor` — diagnose your setup (providers/keys, CLI bridges, Ollama, MCP, config, git,
  terminal) with actionable fixes.
- `forge update` — self-update to the latest release. A standalone binary install (curl/zip) is
  downloaded and swapped in place; Homebrew/cargo installs print the correct upgrade command instead
  of clobbering a package-managed file. `forge update --check` reports without changing anything.
- Startup update check now offers to update: on an interactive terminal it prompts "update now?" and
  runs `forge update` on yes; otherwise it prints the notice (disable with `[update] check = false`
  or `FORGE_NO_UPDATE_CHECK=1`). Never prompts in headless runs, pipes, or the `mcp-serve` bridge.
- Community infrastructure: `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, `SECURITY.md`, issue
  templates, and this changelog.

### Fixed
- Context gauge no longer reads a bogus, ever-climbing percentage (e.g. "337% — auto-compact
  imminent") on a subscription CLI bridge. `claude-cli`/`codex-cli` report *cumulative* internal
  token usage across their own tool loop, not the size of the request Forge sent, so the gauge now
  uses Forge's transcript estimate for bridge turns and the provider's real input count for direct
  API models. This also stops the phantom auto-compact banner (no real compaction was ever needed).
- The sticky task list (`update_tasks`) and spawned-subagent panels now appear during an interactive
  CLI-bridge turn. The out-of-band event sink that carries those updates from `forge mcp-serve` was
  only created in benchmark/harness mode, so a normal `forge chat` on `claude-cli`/`codex-cli` showed
  no live tasks at all; it is now created and tailed for every bridge turn. A post-turn store reload
  also no longer blanks a task list that was already shown.
- `forge lattice` no longer indexes nested git repositories (vendored deps, submodules, or scratch
  clones such as SWE-bench workdirs under the project root). Their symbols were swamping `impact`/
  `query` with unrelated hits (a generic `Command` matching across a cloned `django/` tree); the
  walker now skips any subdirectory that is its own git repo.
- `forge bench swe` no longer fails with `os error 2` when `--workdir` is relative (the default):
  the per-instance clone was double-nesting the path. The workdir is now absolutized first.
- Local Ollama models that emit Hermes/Qwen-style `<tool_call>` XML (e.g. `qwen3-coder`) are now
  driven correctly: `ollama::` is routed through Ollama's OpenAI-compatible `/v1` endpoint, which
  parses those into structured tool calls instead of leaking them as text and dead-ending the turn.
- **Windows**: the Claude Code / Codex CLI bridges and stdio MCP servers now launch correctly.
  Detection and spawning are PATH-resolved with the `.exe`/`.cmd`/`.bat` suffixes, and `.cmd`/`.bat`
  shims (how npm installs `claude`, `codex`, `npx`, and node-based MCP servers like `caveman-shrink`)
  are run through `cmd /C`. Previously these failed to detect or spawn ("program not found").

## [0.2.0] - 2026-06-23

### Added
- Full-screen (alternate-screen) TUI by default — scrollable transcript, pinned panels, mouse-wheel
  scroll; `--inline` (or `[tui] fullscreen = false`) keeps the classic inline-scrollback mode.
- Unified in-loop activity viewer (Ctrl+O): main chat + subagents + assay critics in one navigable
  full-screen view.
- Dynamic `/config` editor — grouped sections, friendly labels, type-appropriate controls (toggle /
  cycle / typed input), API-key management (keyring), per-setting help, default/modified/source,
  reset-to-default, fuzzy search, user/project scope.
- `forge local` — local LLMs via Ollama: hardware detection, live model discovery, ranking by
  Artificial Analysis benchmark scores (multi-family catalog offline), install/start/status, an
  animated picker, and opt-in autostart.
- `forge setup` — guided first-run wizard (providers/plans + optional local-LLM step); auto-runs on
  first launch. `forge init` aliases it.
- Per-turn AI recap; persisted TUI view state restored on resume.

### Changed
- `forge models --probe` rechecks only benched models by default (cheap); `--all` re-pings every
  model. Capability-exclusion window shortened to 24h.

### Fixed
- Activity-viewer crash and scrollback corruption in full-screen mode; assay per-critic model/cost
  detail; macOS release checksum.

## [0.1.0] - 2026-06-18

Initial public release: Model Mesh routing, multi-provider support, cost/budget caps, the
inline TUI, session persistence + checkpoints, permission broker, subagents, Assay analysis,
Lattice code intelligence, MCP client, web tools, hooks, skills/commands, and more.

[Unreleased]: https://github.com/florisvoskamp/forge/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/florisvoskamp/forge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/florisvoskamp/forge/releases/tag/v0.1.0
