# Forge — Feature Roadmap & Backlog

> The single prioritized list of what Forge is, what's actually done, and what's next.
> Derived from three inputs, not opinion:
> 1. **Product vision** — the Helm note *Custom AI Coding Harness & CLI* (the differentiators).
> 2. **Real usage evidence** — analysis of the owner's Claude Code session history (what he
>    actually relies on, by frequency) — see [Appendix A](#appendix-a--usage-evidence).
> 3. **Honest completeness audit** — what the shipped v0.1 crates really do vs. what the
>    requirements claim — see [Appendix B](#appendix-b--existing-feature-audit).
>
> Deep specs live in `docs/features/`. This file is the index + priority order. No feature
> below is "in progress" — this is the design backlog; build order is the wave structure.

---

## How to read this

- **Status**: `done` (shipped + tested), `partial` (works but incomplete/mislabelled),
  `planned` (spec written, not built), `vision` (roadmap entry, no deep spec yet).
- **Priority**: P0 (foundational / a core promise is broken), P1 (high-value, unblocks the
  way the owner works), P2 (parity polish), P3 (moonshot differentiator / monetization).
- **Wave**: suggested build order respecting dependencies. Don't start a feature before its
  dependencies.

---

## 0. Existing-feature debt (all resolved)

The v0.1 audit found three real gaps. All are now fixed:

| # | Gap | Status |
|---|-----|--------|
| D1 | **Budget cap was per-session** — real daily/monthly aggregation across sessions now enforced. | **fixed** (#19) |
| D2 | **Real provider path untested** — 3-layer test strategy: unit mapping, mock-server contract, gated Ollama live. | **fixed** (#20) |
| D3 | **Permission rules half-built** — full allow/ask/deny rule engine (per-tool, path/command globs), 'a' key writes always-allow rules to config. | **fixed** (#92) |
| D4 | **Stale doc comments** — keyring "planned", rules "planned". | **fixed** |

### Current FR status

| Req | Feature | Status | Note |
|-----|---------|--------|------|
| FR-1 | Agent loop (stream, tools, iterate) | **done** | 10+ core tests |
| FR-2 | Tool system (read/write/edit/list/search/shell) | **done** | shell tool + denylist + error interceptor |
| FR-3 | Multi-provider (genai) | **done** | contract-tested; 9+ providers + CLI bridge + free-tier auto-discovery |
| FR-4 | Model Mesh routing | **done** | heuristic + `/model` in-session pin + failover + health + quota-aware |
| FR-5 | Cost + budget | **done** | real day+month cap across sessions, hard stop, downshift, warn |
| FR-6 | TUI | **done** | inline-scrollback, markdown rendering, syntax highlighting, diff-before-apply, banner, statusline |
| FR-7 | Session persistence | **done** | list + resume + compacted-view persistence + checkpoints + undo |
| FR-8 | Config (layered) | **done** | figment; wizard; live key injection |
| FR-9 | Secrets (env + keyring) | **done** | OAuth tokens also in keyring (ADR-0007) |
| FR-10 | Permission modes | **done** | 4 modes + full rule engine + 'a' always-allow writeback |

---

## 1. Wave 1 — Foundation & fixes (P0) — ALL DONE

| Feature | Status | PR |
|---------|--------|----|
| **Shell / bash tool** | **done** | stream output, denylist, safety-gated, Windows cross-platform |
| **Fix budget cap** (D1) | **done** | real day+month aggregation, hard stop, downshift, warn |
| **Provider test strategy** (D2) | **done** | 3-layer: unit + contract + gated Ollama live |
| **Fine-grained permission rules** (D3) | **done** | full allow/ask/deny engine + glob matching + 'a' always-allow writeback |
| **TUI rich rendering** | **done** | markdown, syntax highlighting, diff-before-apply (accept/reject) |

---

## 2. Wave 2 — Power primitives (P1) — ALL DONE

| Feature | Status | Notes |
|---------|--------|-------|
| **Subagent orchestration** | **done** | `spawn_agents` virtual tool, mesh-routed children, depth-1 guard, live TUI tree |
| **Assay — analysis mode** ⚒ | **done** | parallel critics + adversarial verify + ranked report + git scopes (--diff/--branch/--since) + --only/--skip lens selection + auto-diff vs prior run + `forge assay list/compare`; per-critic live TUI panel (U9 done PR #97); deferred: budget pre-estimate scope-down (U8) |
| **Commands + skills** | **done** | palette + `forge-skills`, CC-compatible, `use_skill` virtual tool, `@path` file-path completion popup |
| **MCP client** | **done** (+OAuth PR #93) | stdio + HTTP/SSE, deferred loading, allowlist, OAuth 2.0 Authorization Code + PKCE, `forge mcp login/logout` |
| **Lattice** ⚒ | **done** | tree-sitter Rust + multi-language, resolved edges, impact/path, auto-retrieval injection, hybrid semantic+structural embeddings, file watcher, `/lattice`, `forge lattice why` |
| **Web tools** | **done** | `web_search` + `web_fetch`, SSRF-guarded, Brave backend |
| **Task / todo tracking** | **done** | `update_tasks`, live TUI checklist, persisted+resume, CLI bridge |

---

## 3. Wave 3 — Harness parity & polish (P2) — ALL DONE

| Feature | Status | Notes |
|---------|--------|-------|
| **Hooks system** | **done** | PreToolUse (block + arg-rewrite) + PostToolUse (observe), direct + CLI-bridge paths; deferred: MCP tool hooks |
| **Context compaction** | **done** | `/compact` manual + auto-trigger at 80% context gauge + compacted-view persisted on resume |
| **Interactive clarification** | **done** | `ask_user` virtual tool — TUI selector + headless fallback + non-interactive sentinel |
| **Model selection UX** | **done** | `/model <id>` in-session pin (clears with `/model`), mesh still classifies for tier stats |
| **Statusline / banner** | **done** | ASCII wordmark welcome banner, statusline (spinner/tier/model/cost/mode/hints), narrow-terminal fallback |
| **Token counter + context gauge** | **done** | live ↑in/↓out totals + context-window fill gauge, threshold-colored, honest `None` for unknown windows |
| **Plan mode** | **deprioritized** | 6 sessions / `/plan` 1 — Assay is the higher-value "mode" |

---

## 4. Wave 4 — Moonshot differentiators & monetization (P3)

The Helm-note vision that makes developers switch. Each needs its own deep spec when its
wave approaches.

> **Note:** the four former Wave-4 code-intelligence items — *persistent semantic code
> memory*, *AI archaeology*, *git-native context*, *cross-repo intelligence* — have been
> **consolidated and promoted** into the single **Lattice** spec
> ([code-intelligence.md](features/code-intelligence.md)) at Wave 2 (P1). They are no longer
> separate Wave-4 entries.

| Feature | What | Source |
|---------|------|--------|
| **Skills/agents marketplace** | Publish/import skills, commands, agents — "npm for AI workflows" (25% rev share). | Helm note; designed to plug into the command/skill system |
| **Session replay** — **done** | Record prompts + model versions + outputs; replay + diff; auditable, reproducible AI. **Shipped:** `forge replay <id>` (turn-by-turn transcript) + `forge replay <a> <b>` (summary diff + per-turn content diff) + `/replay` in-session chat command + `forge replay <id> --json` (JSON export). **Deferred:** true model re-execution. | [session-replay.md](features/session-replay.md); Helm note |
| **Import / migration layer** | **Claude + Codex done** (`forge import claude` / `codex`) | Auto-detect + import from Claude Code (skills/commands/agents), Codex CLI. **Shipped:** `forge import claude [--project]` copies `~/.claude/{commands,skills,agents}` (agents share the same .md format so it's a direct copy), and `forge import codex [--project]` copies `~/.codex/prompts/*.md` as commands. **Deferred:** Claude hooks/memory/settings import; Aider/Cursor/Continue. | Helm note; prerequisite for CC-compat in skills + MCP specs |
| **Natural-language shell** | "show me what changed performance-wise since last week" → runs the right commands, diffs, explains. | Helm note |
| **Shell error interceptor** — **done** | Command fails → AI auto-explains + offers a fix. **Shipped:** trivial-tier diagnosis on shell failure, transcript injection (model sees hint on next turn), usage recorded against budget. **Deferred:** one-key apply-fix, pattern cache. | [shell-error-interceptor.md](features/shell-error-interceptor.md); Helm note |
| **Voice interface** | whisper.cpp local STT, no cloud. | Helm note |
| **More providers** | Gemini, Mistral, Cohere, OpenRouter, Groq, llama.cpp / LM Studio. | Helm note (genai already covers several) |
| **Team layer (monetization)** | Shared team memory + skills registry, team session history/replay, admin/audit/SSO (~$15-20/seat); hosted relay for cross-machine sync. | Helm note |

---

## 5. Build-order summary

All Wave 1–3 features are shipped. Wave 4 items are the remaining moonshot / monetization work.

```
Wave 1 (P0) ✓  shell-tool · permission-rules · budget-cap · provider-tests · tui-rich-rendering
Wave 2 (P1) ✓  subagent-orchestration · Assay · commands/skills · MCP+OAuth · Lattice (full) · web-tools · tasks
Wave 3 (P2) ✓  hooks · compaction+persist · ask_user · /model pin · banner/statusline · token-gauge
Wave 4 (P3)    marketplace · session-replay (MVP ✓) · import/migration (Claude+Codex ✓) ·
               NL shell · shell-error-interceptor (MVP ✓) · voice · more-providers · team-tier
```

---

## Appendix A — Usage evidence

From analysis of the owner's Claude Code session history (1,440 sessions). Tool counts are
global `tool_use` occurrences; these justify the priorities above.

| Tool | Count | → Forge status |
|------|------:|----------------|
| Bash | **5,909** | **done** (shell tool, denylist, error interceptor) |
| Read | 2,650 | done (read_file) |
| Edit | 2,027 | done (edit_file + diff-before-apply review) |
| Write | 1,299 | done (write_file + diff-before-apply review) |
| WebSearch | 373 | done (web_search + Brave backend) |
| Agent (subagents) | 233 | done (spawn_agents, mesh-routed, live TUI tree) |
| Skill | 212 | done (forge-skills + use_skill + CC-compatible) |
| WebFetch | 187 | done (web_fetch, SSRF-guarded) |
| AskUserQuestion | 118 | done (ask_user virtual tool, TUI selector) |
| TodoWrite + Task* | ~196 | done (update_tasks, persisted, TUI checklist) |
| MCP (all servers) | ~270 | done (forge-mcp, stdio+HTTP/SSE, OAuth, deferred loading) |

Top commands: `/orchestrate` 62, `/model` 39, `/compact` 21, `/mcp` 12. Permission modes:
**auto 3,599** vs plan 6 (he runs YOLO-with-rails — validates P0 permission rules + P2
plan-mode deprioritization). Environment is hook-driven (rtk token proxy ≈ 75% savings,
graphify injection, auto-title) — validates P2 hooks.

## Appendix B — Existing-feature audit (verdict)

> Mostly real, not vaporware: 9/9 crates build clean (0 warnings), no `todo!()`/stubs, 65
> tests green, all FRs wired end-to-end through `main.rs`. But **not** "feature-complete and
> fully tested": FR-5's budget cap is mislabelled per-session (the core differentiator isn't
> enforced), FR-10's fine-grained rules are unbuilt, and FR-3's real `GenAiProvider` has zero
> automated tests. Biggest risk: the untested provider path. → Wave 1 fixes D1–D3.

## Appendix C — Naming

The analysis/cleanup mode is named **Assay** (`forge assay`) — metallurgy: determining the
composition and purity of a metal. On-brand with Forge (⚒), distinctive in the AI-CLI space,
professional. Chosen over Audit / Critique / Inquest.

The built-in code-intelligence subsystem is named **Lattice** (`forge lattice`) — a metal's
crystal lattice is its fundamental atomic structure, and "lattice" also literally means a
graph/network: the structural graph underlying the code. On-brand, and it beats the external
graphify on every axis (native, incremental, agent-automatic retrieval, hybrid
structural+semantic, impact analysis) — see [code-intelligence.md](features/code-intelligence.md).
