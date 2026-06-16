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

## 0. Existing-feature debt (fix before claiming v0.1 "done")

The audit found v0.1 is **mostly real** (9 crates build clean, no stubs/`todo!()`, 65 tests
green, end-to-end wiring works) — but it is **not** "feature-complete and fully tested" as
claimed. Three real gaps, one of which breaks the headline differentiator:

| # | Gap | Why it matters | Priority | Spec |
|---|-----|----------------|----------|------|
| D1 | **Budget cap is fake** — "daily/monthly cap" is actually *per-session* cost (`spent_today_usd` = one session's total). Never aggregates across sessions; no monthly concept; no hard stop. | **The core product promise (bounded cost) is not enforced.** | **P0** | [fix-budget-cap.md](features/fix-budget-cap.md) |
| D2 | **Real provider path untested** — `GenAiProvider` (Anthropic/OpenAI/Ollama, streaming, tool-calls) has **zero** automated tests; all 65 tests hit `MockProvider`. | FR-3, a headline capability, is unverified; mapping/streaming/tool-call regressions go uncaught. | **P0** | [provider-test-strategy.md](features/provider-test-strategy.md) |
| D3 | **Permission rules half-built** — only the 4 global modes exist; FR-10's per-tool/per-project allow/ask/deny rules are unbuilt. | Blocks safe shell execution; owner relies on allow/deny lists today. | **P1** | [fix-permission-rules.md](features/fix-permission-rules.md) |
| D4 | **Stale doc comments** — `forge-config/src/lib.rs:3` says keyring is "planned" (it's shipped); `forge-core/src/permission.rs:3` calls rules "planned" (accurate — see D3). | Misleads readers about what's done. | P2 | _(trivial; fix inline — no spec)_ |

### Shipped v0.1 status (from the audit)

| Req | Feature | Status | Note |
|-----|---------|--------|------|
| FR-1 | Agent loop (stream, tools, iterate) | **done** | 10 core tests |
| FR-2 | Tool system (read/write/edit/list/search) | **done** | 10 tests; **no shell tool** (see P0-1) |
| FR-3 | Multi-provider (genai) | **done** | contract-tested (#20); 7 providers + CLI bridge (#25/#26) |
| FR-4 | Model Mesh routing | **done** | heuristic (length+keyword+hints+code+dev-verbs), `--model` pin, provider fallback; cost-aware candidate selection = future (provider-cost-routing.md) |
| FR-5 | Cost + budget | **done** | real day+month cap across sessions (#19) |
| FR-6 | TUI | **done** | inline-scrollback; plain markdown only (see P0-5) |
| FR-7 | Session persistence | **done** | list + resume |
| FR-8 | Config (layered) | **done** | figment |
| FR-9 | Secrets (env + keyring) | **done** | keyring untested (no OS service in CI) |
| FR-10 | Permission modes | **partial** | 4 modes done; **rules unbuilt** (D3) |

---

## 1. Wave 1 — Foundation & fixes (P0)

Make real coding possible and make the core promise true. Mostly within existing crates.

| Feature | Priority | What | Spec | Evidence |
|---------|----------|------|------|----------|
| **Shell / bash tool** | **P0** | Run build/test/git inline; stream output; background jobs; safety-gated. The single biggest missing capability. | [shell-tool.md](features/shell-tool.md) | Bash = owner's **#1 tool, 5,909 uses**; Forge can't run any command today |
| **Fix budget cap** (D1) | **P0** | Real daily/monthly cost aggregation + hard stop + downshift + warn. | [fix-budget-cap.md](features/fix-budget-cap.md) | Core differentiator broken |
| **Provider test strategy** (D2) | **P0** | 3-layer tests (unit mapping → mock-server contract → gated Ollama live) so the real adapter path is verified in CI with no keys. | [provider-test-strategy.md](features/provider-test-strategy.md) | Headline feature unverified |
| **Fine-grained permission rules** (D3) | **P0/P1** | allow/ask/deny rule engine (per-tool, path/command globs); precedence with global modes. Unblocks safe shell. | [fix-permission-rules.md](features/fix-permission-rules.md) | Owner runs auto-mode + allow/deny lists |
| **TUI rich rendering** | **P0** | Markdown rendering of answers + syntax highlighting + **diff view before applying edits** (accept/reject). | [tui-rich-rendering.md](features/tui-rich-rendering.md) | 115 "review" asks, 202 GitLab MR-review MCP calls — review is daily |

**Wave-1 dependency:** shell-tool's safety layer depends on the permission-rules engine →
build/land `fix-permission-rules` first (or together).

---

## 2. Wave 2 — Power primitives (P1)

The capabilities that make Forge feel like *his* tool, including the #1 requested feature.

| Feature | Priority | What | Spec | Evidence |
|---------|----------|------|------|----------|
| **Subagent orchestration** | **P1 (high)** | First-class spawn-agent primitive: parallel fan-out, Model-Mesh-routed children, budget/depth caps, synthesis. Exposed as a `task` tool + `forge orchestrate`. | [subagent-orchestration.md](features/subagent-orchestration.md) | `/orchestrate` = **#1 command (62)**; **233 subagent spawns**; Helm differentiator |
| **Assay — analysis mode** ⚒ | **P1 (top user ask)** — **MVP done** (#49) | Critical multi-agent crew that investigates code/design/architecture/docs for AI-slop, dead weight, unsafe/untested code, bad architecture; ranked findings, fixing opt-in. **Sibling to plan mode.** MVP: `forge assay [PATH]` (parallel mesh-routed critics + adversarial verification + ranked persisted report). Deferred: git scopes, live ⚒ TUI view, `/assay` chat, fix hand-off, report diffing. | [analysis-mode.md](features/analysis-mode.md) | Explicit #1 request; subagent-orchestration shipped (#34-38) |
| **Commands + skills + autosuggest** | **P1** — **done** (palette + `forge-skills`; `@path` deferred) | Slash commands + reusable skills (CC-import compatible) in `./.forge/{commands,skills}`; fuzzy command palette; `/name` template expansion + `/skill` methodology injection; `forge commands`. New `forge-skills` crate. Deferred: `@path` autocomplete. | [command-skill-system.md](features/command-skill-system.md) | **212 skill uses, ~45 custom commands** authored |
| **MCP client** | **P1** — **done** | Connect to MCP servers (stdio + HTTP/SSE via `rmcp`), expose their tools through the existing tool path + permission broker (new `SideEffect::External`, ADR-0009); deferred tool loading (`mcp_search_tools` + universal `mcp_call`, works on direct + CLI-bridge paths); allowlist; `forge mcp` + `/mcp`; auto-scan import from installed AI CLIs with secrets auto-stored in the keyring. New `forge-mcp` crate. | [mcp-client.md](features/mcp-client.md) | **~270 MCP calls** (GitLab MR review = daily) |
| **Lattice — built-in code intelligence** ⚒ | **P1 (flagship)** | Native, zero-setup semantic+structural code memory (tree-sitter + SQLite, incremental, persistent) that the agent loop **uses automatically** for ranked context retrieval — plus impact/blast-radius, git archaeology, git-native scoping, cross-repo. The "graphify but way better, built-in" feature. **Consolidates 4 former Wave-4 items.** | [code-intelligence.md](features/code-intelligence.md) | Owner relies on external graphify today (hooks-injected); wants it native + automatic + better |
| **Web search + fetch tools** | **P1** — **done** (#58) | `web_search` (BYOK, pluggable backend; Brave reference) + `web_fetch` (keyless, SSRF-guarded, HTML→text). New `SideEffect::Network`. | [web-tools.md](features/web-tools.md) | WebSearch **373** + WebFetch **187** |
| **Task / todo tracking** | **P1** — **done** | `update_tasks` tool → live styled checklist in the TUI, persisted + restored on resume; works on the direct path AND the CLI bridge (via mcp-serve). | [task-tracking.md](features/task-tracking.md) | TodoWrite **79** + Task* **~117** |

**Wave-2 dependency:** **Assay is gated on subagent-orchestration** (its critic crew IS the
subagent primitive). Build the primitive, then Assay.

---

## 3. Wave 3 — Harness parity & polish (P2)

The power-user surface that makes the harness "better than Claude Code" (the stated goal).

| Feature | Priority | What | Evidence |
|---------|----------|------|----------|
| **Hooks system** | P2 | Pre/post tool-use shell hooks (the owner's whole env runs on hooks — rtk token proxy, graphify injection, auto-title). Without it he loses ~75% token savings. | His entire setup; `/hooks` used |
| **Context compaction** | P2 | Auto-summarize long sessions (`/compact` equivalent). | `/compact` **21** |
| **Interactive clarification** | P2 | AskUserQuestion-style mid-task multiple-choice prompts. | **118** AskUserQuestion uses |
| **Model selection UX** | P2 | `--model` pin flag, `/model`, effort/usage views (Mesh exists; add the controls). | `/model` **39** |
| **Statusline / output styles / auto-title** | P2 | Configurable statusline + output styles (he invests in these). | auto-title hook, `/title`, caveman styles |
| **Token counter + context gauge** | P2 | Live session token totals + context-window fill gauge next to the spinner/cost in the statusline. | [tui-token-counter.md](features/tui-token-counter.md) — explicit request |
| **Plan mode** | **P2 (deprioritized)** | Read-only planning mode. | **Only 6 sessions / `/plan` 1** — overrated vs reputation; Assay is the higher-value "mode" |

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
| **Session replay** | Record prompts + model versions + outputs; replay + diff; auditable, reproducible AI. | Helm note |
| **Import / migration layer** | Auto-detect + import from Claude Code (skills/commands/agents/hooks/memory/settings), Codex CLI, Aider, Cursor/Windsurf, Continue.dev. | Helm note; prerequisite for CC-compat in skills + MCP specs |
| **Natural-language shell** | "show me what changed performance-wise since last week" → runs the right commands, diffs, explains. | Helm note |
| **Shell error interceptor** | Command fails → AI auto-explains + offers a fix, no prompt needed. | Helm note |
| **Voice interface** | whisper.cpp local STT, no cloud. | Helm note |
| **More providers** | Gemini, Mistral, Cohere, OpenRouter, Groq, llama.cpp / LM Studio. | Helm note (genai already covers several) |
| **Team layer (monetization)** | Shared team memory + skills registry, team session history/replay, admin/audit/SSO (~$15-20/seat); hosted relay for cross-machine sync. | Helm note |

---

## 5. Build-order summary (dependency graph)

```
Wave 1 (P0, parallel-ish):
  shell-tool ── needs ──▶ fix-permission-rules
  fix-budget-cap
  provider-test-strategy
  tui-rich-rendering (markdown + syntax + diff)

Wave 2 (P1):
  subagent-orchestration ── unblocks ──▶ Assay (analysis mode)   ◀── top user ask
  command-skill-system        web-tools        task-tracking
  mcp-client                  Lattice (built-in code intelligence)  ◀── flagship

Wave 3 (P2):  hooks · compaction · clarification · model UX · statusline ·
              token-counter+context-gauge · (plan mode)

Wave 4 (P3):  marketplace · session replay · import/migration · NL shell ·
              shell error interceptor · voice · more providers · team tier
              (semantic memory / archaeology / git-native / cross-repo → folded into Lattice)
```

Key gates: **Assay requires subagent-orchestration. Shell-tool requires permission-rules.
The diff-view (tui-rich-rendering) reuses the permission/confirm flow. CC-import compat in
the skill + MCP specs leans on the Wave-4 import layer (stub the readers until then).**

---

## Appendix A — Usage evidence

From analysis of the owner's Claude Code session history (1,440 sessions). Tool counts are
global `tool_use` occurrences; these justify the priorities above.

| Tool | Count | → Forge status |
|------|------:|----------------|
| Bash | **5,909** | **missing** → P0 shell-tool |
| Read | 2,650 | done (read_file) |
| Edit | 2,027 | done (edit_file) — add diff view |
| Write | 1,299 | done (write_file) — add diff view |
| WebSearch | 373 | missing → P1 web tools |
| Agent (subagents) | 233 | missing → P1 subagent-orchestration |
| Skill | 212 | missing → P1 command/skill system |
| WebFetch | 187 | missing → P1 web tools |
| AskUserQuestion | 118 | missing → P2 clarification |
| TodoWrite + Task* | ~196 | missing → P1 task tracking |
| MCP (all servers) | ~270 | missing → P1 MCP client (GitLab 202) |

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
