# Requirements — Forge

> Status: **HISTORICAL v0.1 baseline** (confirmed 2026-06-14, Floris Voskamp; repo is now at
> v2.0.0). Kept as the original baseline contract for context; the "Later (planned, not in v0.1)"
> list in §3 below is now largely shipped (MCP client/server, tree-sitter code memory, native
> multi-agent orchestration, skills/commands system, session replay & diff, import from Claude
> Code/Codex) — see [`docs/roadmap.md`](../roadmap.md) and
> [`docs/ROADMAP-v2-DELIVERED.md`](../ROADMAP-v2-DELIVERED.md) for what actually shipped and when.

## 1. Purpose

- **Problem:** Existing AI coding CLIs (Claude Code, Codex CLI, Aider, etc.) lock you to
  one provider and one model choice per task, so you either overpay by sending trivial
  edits to a frontier model, or underperform by sending hard reasoning to a cheap one.
  Switching models is manual, cost is invisible until the bill arrives, and the harness
  itself (hooks, context, tool system) is constrained by the host vendor's choices.
- **Goal / definition of success (v0.1):** A native Rust CLI + TUI that runs a real
  agentic coding loop against multiple providers (Anthropic, OpenAI, local Ollama),
  where a **Model Mesh** automatically routes each task to the cheapest model that can
  do it well, shows cost live, and enforces a budget — daily-usable by the author for
  real coding work, installable as a single binary on Linux, macOS, and Windows.

## 2. Users & roles

| Role | Who they are | What they need to do |
|------|--------------|----------------------|
| Solo developer (primary) | Individual engineer using their own API keys (BYOK) | Run coding tasks from the terminal, see/control cost, pick or auto-route models |
| OSS contributor | Community developer extending Forge | Build/test locally, add providers/tools, follow a clear architecture |
| (Later) Team member | Dev on a team sharing memory/skills | Out of v0.1 scope — see roadmap |

Approximate user count at launch: 1 (author) → early OSS adopters. External, open source.

## 3. Functional requirements (what it must do)

### MVP (must have for v0.1 launch)
- **FR-1 — Agent loop.** Run an interactive agentic session: user prompt → model call →
  tool calls → observations → iterate until done. Streamed output.
- **FR-2 — Tool system.** A first-class, extensible tool interface. v0.1 ships core
  coding tools: read file, write/edit file, run shell command, search (grep/glob),
  list directory. Tools declare schemas; the harness validates calls.
- **FR-3 — Multi-provider abstraction.** A provider-agnostic model interface with
  adapters for **Anthropic**, **OpenAI**, and **Ollama** (local). Streaming + tool/function
  calling normalized across providers.
- **FR-4 — Model Mesh routing.** Classify each task (trivial / standard / complex) and
  route to a configured model tier. Routing is rule-based + heuristic in v0.1, fully
  user-configurable (rules, model→tier mapping, overrides). User can also pin a model.
- **FR-5 — Cost tracking & budget.** Track token usage and cost per request, per session,
  and cumulatively. Live cost meter in the TUI. **Budget mode:** a daily/monthly cap that
  makes the router prefer cheaper tiers (and warn/stop) as the cap is approached.
- **FR-6 — TUI.** A `ratatui`-based terminal UI showing the conversation, live agent
  progress, the cost meter, and which model the mesh chose for each task. A plain
  non-interactive mode for scripting/pipes.
- **FR-7 — Session persistence.** Persist sessions (messages, tool calls, costs, routing
  decisions) to local SQLite; list and resume prior sessions.
- **FR-8 — Configuration.** Layered config (project + user) for providers, routing rules,
  budgets, and tool permissions. Secrets are never stored in plaintext config.
- **FR-9 — Secret handling.** Read provider API keys from environment variables; optionally
  store/retrieve them from the OS keyring (secret-service / Keychain / Credential Manager).
- **FR-10 — Tool permission modes.** A switchable permission *mode* governs tool side
  effects, plus fine-grained per-tool/per-project allow/ask/deny rules. v0.1 modes:
  `default` (ask before side effects), `accept-edits` (auto-allow file writes, still ask
  for shell), `bypass` (auto-allow everything — explicit opt-in), and `plan` (read-only;
  no side effects at all). Mode is selectable per session and configurable as a default.

### Later (planned, not in v0.1) — roadmap, not built now
- Persistent semantic code memory (tree-sitter AST graph).
- Native multi-agent orchestration (parallel fan-out/synthesize).
- MCP client/server support.
- Skills / commands / agents system + community marketplace.
- Session replay & diff.
- Migration import from Claude Code / Codex / Aider / Cursor / Continue.
- Additional providers: Google Gemini, Mistral, Cohere, OpenRouter, Groq, llama.cpp/LM Studio.
- Voice interface (whisper.cpp), natural-language shell, shell error interceptor,
  AI archaeology, cross-repo intelligence.
- Team tier, hosted relay, SSO, admin/audit (commercial layers).

### Explicitly out of scope (v0.1)
- Any hosted/cloud backend or account system (local-first only).
- Reselling tokens / managed API access (BYOK always).
- GUI / web / desktop app (terminal only).
- Fine-tuning or training models.

## 4. Scale & growth

- **Workload shape:** single-user, single-machine, interactive. Concurrency is *within*
  one session (parallel tool calls, future parallel agents), not many simultaneous users.
- **Data volume:** session history on the order of MBs–low GBs of SQLite over time; one
  active codebase per session.
- **Latency expectation:** CLI cold-start target sub-100 ms (excluding network/model
  latency); model/network latency dominates and is bounded by the provider.
- **1–2 year horizon:** more providers, code-memory graphs per repo (could reach 100k+
  AST nodes), optional team sync — architecture must not preclude these.

## 5. Non-functional requirements (the ones that matter — with targets)

| Attribute | Target / requirement | Why it matters |
|-----------|----------------------|----------------|
| Performance (startup) | CLI process start < 100 ms; TUI first paint < 150 ms | Core differentiator vs Node/Electron tools; matters for shell integration |
| Performance (streaming) | Render model tokens to TUI with < 50 ms added latency | Responsive feel during generation |
| Resource footprint | Idle RSS in low tens of MB; single static binary | "No runtime bloat" promise |
| Portability | One codebase builds + passes tests on Linux, macOS, Windows | Stated platform requirement |
| Security | No plaintext secrets; tool side-effects gated by permission policy; no telemetry without opt-in | Handles user code, shell access, and API keys |
| Privacy | Local-first; nothing leaves the machine except calls to the user's chosen providers | Trust; code is sensitive |
| Cost-correctness | Cost numbers accurate to provider pricing; budget cap never silently exceeded | The product's core promise is cost control |
| Reliability | A provider/tool failure degrades gracefully (retry/fallback tier), never corrupts session state | Long agent loops must be robust |
| Maintainability | Modular boundaries (provider, router, tools, TUI, persistence) swappable in isolation; ≥ baseline test coverage on core logic | Open source; many planned subsystems |
| Extensibility | Adding a provider or a tool = implementing a trait, no core changes | The roadmap is mostly "add another X" |
| Observability | Structured logging + a session log of every model/tool/routing decision; debug trace mode | Debuggability + future session replay |
| Usability | Sensible zero-config defaults; works after setting one API key | Adoption |

## 6. Constraints

- **Budget:** $0 infra (local-first, no servers in v0.1). Runtime AI cost is the user's
  own BYOK spend — which Forge exists to minimize.
- **Timeline:** No hard external deadline; correctness and a solid foundation prioritized
  over speed (design-first project).
- **Team:** Solo author initially (Floris), Rust-capable; open to OSS contributors.
  Implies: favor well-documented, mainstream, low-maintenance-burden crates.
- **Existing infrastructure:** None (greenfield). GitHub for repo/CI; GitHub Actions CI.
- **Integrations:** Provider HTTP APIs (Anthropic, OpenAI), local Ollama HTTP API; OS
  keyring; local shell + filesystem.
- **Licensing / lock-in:** ~~MIT for the core; all dependencies must be MIT/Apache-2.0/BSD
  compatible (no GPL/AGPL in the shipped binary).~~ **Superseded:** the project switched to
  **AGPL-3.0-only** (commit `ec20d43`, PR #408); see the root `Cargo.toml` `license` field, the
  `LICENSE` file, and README.md. No ADR records the decision. Avoid hard lock-in to any single
  provider's SDK or proprietary format.
- **Language/runtime:** Rust (stable toolchain), async runtime. Single static binary
  delivery; no required external runtime (Node/JVM/Python) for core features.

## 7. Assumptions to confirm

- **A-1:** v0.1 providers are exactly Anthropic + OpenAI + Ollama; others are post-v0.1.
- **A-2:** CONFIRMED — routing in v0.1 is rule-based/heuristic by default; an optional
  cheap-LLM classifier is a pluggable, opt-in enhancement (not on by default).
- **A-3:** "Beautiful TUI" is `ratatui`-based and is the default interactive surface, but
  a headless/pipe mode exists for scripting and CI.
- **A-4:** Local-first only for v0.1 — no account, no cloud sync, no telemetry.
- **A-5:** CONFIRMED — tool safety is governed by switchable **permission modes**
  (`default`/ask, `accept-edits`, `bypass`, `plan`/read-only) plus per-tool/project
  allow-ask-deny rules. Default mode is safe (`default`/ask).
- **A-6:** Target the current Rust **stable** channel; MSRV pinned and tested in CI.
- **A-7:** Cost/pricing tables for providers are bundled + user-overridable (pricing
  changes shouldn't require a release).
