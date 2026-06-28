<div align="center">

# ⚒ Forge

### The AI coding agent that isn't locked to one model — and out-codes the ones that are.

**Run any model — or your existing Claude / Codex / Gemini subscription — through one fast Rust
harness that routes every task to the cheapest capable model, fails over across providers when one
is down, and is *measurably* more reliable than the raw vendor CLIs.**

[![CI](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml/badge.svg)](https://github.com/florisvoskamp/forge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/florisvoskamp/forge?color=orange)](https://github.com/florisvoskamp/forge/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Built with Rust](https://img.shields.io/badge/built_with-Rust-dea584.svg)](https://www.rust-lang.org/)
[![Conformance tests](https://img.shields.io/badge/harness_conformance-324_tests-brightgreen.svg)](docs/harness/why-forge-is-a-better-harness.md)

<br>

**[🚀 Install](#install)** &nbsp;·&nbsp; **[⚡ Quickstart](#quick-start)** &nbsp;·&nbsp; **[🆓 Free Setup](#free-providers)** &nbsp;·&nbsp; **[🧠 Why Forge](#why-forge)** &nbsp;·&nbsp; **[📊 Benchmarks](#benchmarks)** &nbsp;·&nbsp; **[⚔️ vs. Others](#comparison)** &nbsp;·&nbsp; **[✨ Features](#feature-overview)** &nbsp;·&nbsp; **[📚 Docs](#documentation)**

</div>

<p align="center">
  <!-- TODO(demo): replace with a recorded terminal cast. To record:
       vhs docs/assets/demo.tape   (or: asciinema rec, then agg to gif)
       See docs/assets/README.md for the exact script. -->
  <img src="docs/assets/demo.gif" alt="Forge in action — full-screen TUI, mesh routing, live progress" width="820">
</p>

---

```bash
forge chat                                   # full-screen TUI, multi-turn
forge run "add pagination to the user list"  # one-shot task
forge run --model claude-cli::sonnet "…"      # run your Claude subscription THROUGH Forge
forge models --probe                          # discovered models, ranked, health-checked
forge lattice impact "UserRepository"         # code-graph blast radius
```

## Why Forge

You don't pick a model. **Forge picks the cheapest model that can do each task well** — trivial edits
to a local or free model, hard reasoning to a frontier model — under a budget you set, falling over
automatically when one is rate-limited or down. And because Forge can drive the *same* model you'd run
with `claude` or `codex` directly, the **harness is the only variable** — so its reliability layer is
a measurable, not marketing, advantage.

- 🧠 **Model Mesh** — one agent, every provider. Task-tier routing (trivial / standard / complex) to
  the cheapest capable model, benchmark-ranked, with cross-provider capability-aware failover.
- 🔌 **Bring your subscription** — run your Claude Code / Codex / Antigravity (free Gemini) plan
  *through* Forge and get mesh routing, failover, and the reliability layer on top of it. No other
  agent does this.
- 🛡️ **A harness that doesn't lie** — an objective, tool-grounded completion gate, doom-loop and
  repeated-failure guards, and recovery of tool calls a model writes as prose. It never reports a
  phantom success — and there's a `cargo test` behind every one of those claims (**324 conformance tests**).
- 🔬 **Built-in code intelligence** — Lattice: a tree-sitter symbol graph (9 languages) with
  blast-radius, call-chains, and semantic retrieval, auto-injected before each turn.
- 💭 **Cross-session memory** — Forge remembers durable, typed facts per project, auto-captured at
  turn end and **relevance-ranked** back into context next session (keyword, or semantic when an
  embedder is configured) — only the relevant ones, not a dump. Curate with `/remember` + `forge memory`.
- ⚡ **One fast static binary** — Rust, no Node/Python/Bun runtime, no Electron. Installs in one line.

---

<a id="free-providers"></a>

## 🆓 Recommended free providers

Forge is **free to run with zero paid keys.** These providers all offer a genuine free tier with
high-quality models and usable rate limits — connect a few and the mesh routes across them, failing
over automatically when one is throttled. Keys are stored in your **OS keyring**, never in config.

> Connect each with `forge auth <name>` (reads the key from stdin). The model catalog is discovered
> live, so new free models appear automatically.

### Start here (fast + frontier, best limits)

| Provider | Best free models | Free limits | Get a key | Connect |
|----------|------------------|-------------|-----------|---------|
| **Groq** | Llama-3.3-70B, Qwen3-32B, GPT-OSS-120B | ~30 RPM · 1K/day | [console.groq.com/keys](https://console.groq.com/keys) | `forge auth groq` |
| **NVIDIA NIM** | DeepSeek-R1, Llama-3.1-405B, Nemotron-Ultra-550B, GPT-OSS-120B (100+ models) | ~40 RPM | [build.nvidia.com](https://build.nvidia.com/) | `forge auth nvidia` |
| **Cerebras** | GPT-OSS-120B, Qwen-3-Coder-480B, Llama-3.3-70B | ~30 RPM · 14.4K/day | [cloud.cerebras.ai](https://cloud.cerebras.ai/) | `forge auth cerebras` |

### Add for breadth (big context, more frontier models)

| Provider | Best free models | Free limits | Get a key | Connect |
|----------|------------------|-------------|-----------|---------|
| **Google Gemini** (AI Studio) | Gemini Flash (1M context), Gemma | 15 RPM · 1.5K/day | [aistudio.google.com/apikey](https://aistudio.google.com/apikey) | `forge auth gemini` |
| **SambaNova** | DeepSeek-V3.1, Llama-4 Maverick, Llama-3.3-70B | ~20 RPM | [cloud.sambanova.ai](https://cloud.sambanova.ai/) | `forge auth sambanova` |
| **Mistral** | Mistral Large 3, Codestral, Magistral | ~1 RPS · 500K TPM | [console.mistral.ai/api-keys](https://console.mistral.ai/api-keys) | `forge auth mistral` |

### Optional (aggregators + niche)

| Provider | Best free models | Free limits | Get a key | Connect |
|----------|------------------|-------------|-----------|---------|
| **OpenRouter** | rotating `:free` models (Qwen3-Coder, DeepSeek-R1, Llama-3.3) | 20 RPM · 200/day | [openrouter.ai/keys](https://openrouter.ai/keys) | `forge auth openrouter` |
| **GitHub Models** | GPT-4.1, o4-mini, Llama-4-Scout | 10 RPM · 50/day | [github.com/marketplace/models](https://github.com/marketplace/models) | `forge auth github_copilot` |
| **Cohere** | Command A (218B), Command R+ | 20 RPM · 1K/month | [dashboard.cohere.com/api-keys](https://dashboard.cohere.com/api-keys) | `forge auth cohere` |

**A solid all-free setup:** `groq` + `nvidia` + `gemini` — fast small-task routing, 100+ frontier
models, and a 1M-context model, all at \$0. Add `cerebras`/`sambanova` for more headroom under load.

**Stack multiple keys per provider to multiply free limits.** Run `forge auth <provider>` again to add
another key — Forge round-robins across all of them, so two free Groq keys ≈ double the requests/min,
and a throttled key's retry lands on the next one. Works for every provider (except CLI bridges).

```bash
forge auth groq            # add a key
forge auth groq            # add another → Forge rotates across both
forge auth groq --list     # "groq: 2 key(s) configured — …aB3x, …9kQ2"  (masked)
forge auth groq --replace  # overwrite all with one key
```

You can also stack keys via env: `GROQ_API_KEY="k1,k2"` or numbered `GROQ_API_KEY_2`, `GROQ_API_KEY_3`.

> Rate limits and free model lists shift month-to-month — these are mid-2026 figures; check each
> provider's page for current terms. See [docs/features/free-models.md](docs/features/free-models.md)
> for tier config and how to add any other OpenAI-compatible provider in one line.

---

<a id="benchmarks"></a>

## 📊 Proof: same model, better results

The honest test of a harness: run the **same model** Forge bridges (`claude sonnet`) *through* Forge
vs. the raw `claude` CLI on **SWE-bench Lite** (real GitHub bug fixes), scored by the **official
`swebench` Docker evaluator**. The only difference is the harness.

| Same `sonnet` model · SWE-bench Lite | Bugs fixed | Tokens / fix |
|---|--:|--:|
| Raw `claude` CLI | 4 / 10 | 3.57M |
| **Forge** (loop-gated completeness) | **6 / 10** | **2.83M** |

**Forge fixes 50% more bugs (6 vs 4) at ~21% lower cost per fix** — and *strictly dominates* (every
bug the raw CLI fixed, Forge also fixed, plus two more; zero the other way). Total tokens are at
**parity** — Forge does more work because it solves more, not because it's wasteful. Re-confirmed on
the current build; the larger N=20 run holds the same direction (11 vs 9).

> Every number here is reproducible (`forge bench swe` + the official evaluator) and every reliability
> claim has a test. Full method, the larger-N run, **and an explicit "where Forge does *not* win yet"
> section**: **[Why Forge is a better harness →](docs/harness/why-forge-is-a-better-harness.md)**

---

<a id="comparison"></a>

## ⚔️ Forge vs. the alternatives

| | **Forge** | Claude Code | Codex CLI | Cursor (CLI) | OpenCode | Aider |
|---|:--:|:--:|:--:|:--:|:--:|:--:|
| Any model / any provider | ✅ | Anthropic only | OpenAI only | Cursor's set | ✅ | ✅ |
| **Auto cost-tier routing** (cheapest capable model per task) | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| **Cross-provider failover** (down → next ranked, whole catalog) | ✅ | ❌ | ❌ | ❌ | same-model retry¹ | ❌ |
| **Run your *subscription* through it** (Claude/Codex/Gemini) | ✅ | — | — | ❌ | ❌ | ❌ |
| Anti-phantom-success completion gate, test-pinned | ✅ | internal | internal | ❓ | ❓ | ❌ |
| Parallel adversarial code-review crew | ✅ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Queryable code-graph (blast-radius / call-chain) | ✅ | ❌ | ❌ | index² | ❌ | repo-map² |
| Local LLMs first-class (Ollama) | ✅ | ❌ | ❌ | ❌ | ✅ | ✅ |
| Open source | ✅ | ❌ | source-available | ❌ | ✅ | ✅ |
| Single static binary (no runtime) | ✅ Rust | Node | Node | closed | Bun | Python |

<sub>¹ OpenCode retries the *same* model on error (credential fallback only) — no task-aware,
cross-provider failover (repo recon, 2026-06). ² Cursor/Aider have repo indexing / repo-maps, not a
queryable symbol graph with blast-radius + call-chains. Competitor capabilities as last verified
mid-2026 and evolving — corrections welcome via an issue.</sub>

The shared rows (model-agnostic, open source, local LLMs) put Forge level with OpenCode/Aider; the
**bold rows are Forge-only** — no other terminal agent does cost-tier routing, cross-provider
failover, subscription bridging, *and* a test-pinned reliability layer in one binary.

---

## Feature Overview

| Category | Features |
|----------|----------|
| **Model Mesh** | Auto-discovery, cost-tiered routing, benchmark ranking, health-aware failover, subscription bridges, daily/weekly/monthly budget caps, credit-conservation modes (strict = free + subscription only), reset-aware rate-limit handling (waits out a short per-minute reset + retries the best model instead of degrading), multiple API keys per provider with round-robin rotation, `/effort` slider that steers the whole mesh route (benchmark-driven), not just the provider's reasoning param |
| **Providers** | Anthropic, OpenAI, Ollama, Claude Code CLI, Codex CLI, Antigravity CLI (free Gemini), Groq, Gemini, DeepSeek, OpenRouter, NVIDIA NIM, SambaNova, Mistral, Cohere, xAI, Cerebras, and more — any OpenAI-compatible endpoint in one config row |
| **Local LLMs** | `forge local` detects your hardware, recommends a Gemma model that fits, installs + runs it via Ollama (auto-installing Ollama if needed), opt-in autostart; animated picker menu |
| **Planning mode** | `/plan` investigates read-only and proposes a plan; `/execute` approves it and carries it out |
| **Code Intelligence** | Lattice: tree-sitter symbol graph (9 languages), semantic embeddings, hybrid retrieval, blast-radius, call-chain, git provenance |
| **LSP feedback** | Live diagnostics from a language server fed back after edits so the model self-corrects (`[lsp]`, opt-in; rust/ts/js/python/go) |
| **Autofix loop** | Run lint/test after edits and self-heal on failure, up to N iterations (`[autofix]`, opt-in) |
| **Architect mode** | Dual-model turns — strong planner drafts a plan, cheaper editor applies it (`[mesh] architect_mode`, opt-in) |
| **Harness reliability** | Objective completion gate — a model can't claim "done" without a real tool-grounded state check (same authority on direct-API **and** subscription-bridge turns); doom-loop + repeated-failure guards; text-leaked tool-call recovery; never reports a phantom success |
| **Context** | `@file` mentions inject file contents; project file auto-loaded from `.forge/AGENTS.md` (scaffold with `/init`); Lattice auto-injection |
| **Auto-memory** | Built-in cross-session memory: typed durable facts (preference/decision/fact/reference), per-project (or global), auto-captured at turn end + relevance-ranked recall at session start (keyword, or semantic when an embedder is configured), auto-dedup + salience; `/remember`, `/memories`, and the `forge memory` CLI (`[mesh] auto_memory`, on by default) |
| **Vision** | Attach images by `/image <path>` or paste them straight into the input bar as inline blocks |
| **Assay** | Parallel critic crew, adversarial verification, ranked findings, git scopes (diff/branch/since), lens selection, auto-diff vs prior run; opt-in auto-review gate over a turn's diff (`[assay] auto_review`, warn/block) |
| **MCP** | Client for external MCP servers (stdio + HTTP/SSE), OAuth 2.0 + PKCE, deferred loading, allowlist gating |
| **TUI** | Full-screen (alternate-screen) by default with a scrollable transcript + pinned panels (`--inline` to opt out); ratatui live progress, cost meter, context-window token gauge, fuzzy command palette, dynamic `/config` settings editor (every setting, searchable), unified activity viewer (subagents + critics), session/checkpoint pickers, `/usage` + `/mesh` overlays, `/model` picker, `/effort` reasoning knob |
| **Skills & Commands** | Markdown prompt templates + skill methodology injection; Claude Code format compatible; `forge import <tool>` ↔ `forge skill export` round-trip for moving/sharing your library |
| **Subagents** | Parallel fan-out (`spawn_agents`), mesh-routed children, live TUI tree, depth-limited, per-provider concurrency cap so a burst can't drain one subscription's quota, opt-in git-worktree isolation for write-capable children |
| **Session Management** | Checkpoints, `/undo` with file restore, session replay + JSON export, transcript diff, assay run history |
| **Remote control** | Drive a session from a phone/desktop browser (`/remote`) — LAN, loopback, or public tunnel |
| **Hooks** | Pre/post tool-use shell hooks — block, observe, **rewrite args**, or **inject model-visible context** (`{action: rewrite\|inject\|block\|allow}`) on any tool call; fires on both direct and CLI-bridge paths, including MCP tool calls |
| **Cost** | Prompt caching, per-model pricing fetched from OpenRouter (cache-read aware), persistent cross-restart usage store |
| **Git** | Optional model-aware co-author attribution on commits + PR bodies |
| **Safety** | Permission broker, per-tool rules, diff preview before write, shadow file snapshots, unoverridable denylist; opt-in OS shell sandbox (Linux Landlock, `[shell] sandbox`) |

> **Why a harness, not just a CLI?** Forge can run the *same* model you'd run with `claude`/`codex`
> directly — so the harness is the only difference. Real models loop, write tool calls as prose that
> never execute, emit malformed output, and claim "done" without checking; a raw CLI loop mostly
> spins, crashes, or phantom-succeeds on these. Forge turns each into a bounded, test-pinned outcome.
> The honest, runnable case (with a `cargo test` behind every claim, and a clear note on where Forge
> does *not* win): **[Why Forge is a better harness](docs/harness/why-forge-is-a-better-harness.md)**.

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

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/florisvoskamp/forge/main/install.ps1 | iex
```

Downloads the x86-64 release binary (verifying its SHA-256), installs `forge.exe`
to `%LOCALAPPDATA%\Programs\forge`, and adds it to your user `PATH`. Override with
`$env:FORGE_VERSION` or `$env:FORGE_INSTALL_DIR`. After install, `forge update`
keeps it current.

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
# Guided setup: API keys + subscription plans + optional local LLM
# (runs automatically on first launch; re-run anytime)
forge setup

# Optional: run a local model that fits your machine (via Ollama)
forge local                 # animated menu; or: forge local install

# Interactive chat (full-screen TUI; --inline for native scrollback)
forge chat
# In chat: /config edits any setting · /model picks a model · /effort sets the reasoning/route knob
#          /remember <fact> saves a memory · /memories lists them · /init writes .forge/AGENTS.md

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

**Robust under real free-tier conditions:**
- **Reset-aware rate limits** — a per-minute free-tier 429 (NVIDIA NIM / Groq / Gemini) is waited out and the *best* model retried, instead of immediately degrading to a worse one; transient 5xx/blips retry the same model before failover; permanent errors (no tool support / 402 payment-required) fail over at once.
- **Multiple keys per provider** — run `forge auth <provider>` again to stack keys; the mesh round-robins across them to multiply a free tier's per-key rate limit.
- **`credit_mode = "strict"`** keeps routing + failover to **free + subscription only** — a paid model is never silently used.
- **`/effort`** (low / medium / high / xhigh) steers the *whole* route, benchmark-driven: higher effort biases toward stronger-benchmarked models (only when the score gap is real), lower toward cheaper; medium = unchanged.

Inspect any routing decision live with `/mesh [task]` or `forge mesh "<task>"`.

### Supported Providers

| Provider | Mode | Notes |
|----------|------|-------|
| Anthropic | Direct (API key) | Claude family |
| OpenAI | Direct (API key) | GPT-5 family |
| Ollama | Local (no key) | Any local model |
| Claude Code CLI | Subscription bridge | Uses your Claude subscription |
| Codex CLI | Subscription bridge | Uses your OpenAI subscription |
| Antigravity CLI (`agy`) | Subscription bridge | **Free Gemini** (3.5 Flash / 3.1 Pro) + proxied Claude/GPT; $0 |
| Groq | Direct (API key) | Free tier available |
| Gemini | Direct (API key) | Free tier (Flash) |
| DeepSeek | Direct (API key) | |
| OpenRouter | Direct (API key) | Routes to many providers; `:free` models |
| **NVIDIA NIM** | Direct (API key) | **Free dev tier** — DeepSeek-R1, Llama-3.1-405B, Nemotron-70B |
| **SambaNova** | Direct (API key) | **Free tier** — DeepSeek-V3.1, Llama-3.3-70B, Llama-4 Maverick |
| **Mistral** | Direct (API key) | **Free Experiment tier** — Mistral Large 3, Codestral |
| Cohere | Direct (API key) | Command A (218B), free trial |
| xAI, MiniMax, MiMo, Cerebras | Direct (API key) | Cerebras free tier (very fast) |

Store keys in your OS keyring — never in plaintext config:

```bash
forge auth anthropic    # reads from stdin
forge auth nvidia       # free frontier models, instantly routable via the mesh
forge auth --remove openai
```

**Adding a new OpenAI-compatible provider is one row.** Any provider with a standard
`/chat/completions` endpoint (NVIDIA NIM, SambaNova, Mistral, … and most "free LLM API"
gateways) is wired by appending a `CustomProvider` entry to `CUSTOM_OPENAI_PROVIDERS` in
`crates/forge-config/src/lib.rs` — namespace, endpoint, key env var, free flag, and a few
seed model ids. That single row wires `forge auth`, env injection, mesh discovery, cost-tier
routing, and cross-provider failover end-to-end. No native SDK adapter required.

---

## CLI Reference

### `forge chat`

Interactive multi-turn session with the full TUI.

```bash
forge chat
forge chat --resume abc123                      # resume a previous session
forge chat --continue                           # resume the most recent session
forge chat --model anthropic::claude-opus-4-8   # pin a model
forge chat --mode accept-edits                  # auto-allow file writes
forge chat --inline                             # inline scrollback instead of full-screen
forge chat --plain                              # headless / CI mode
```

The TUI is full-screen by default (scrollable transcript, pinned panels, mouse-wheel
scroll). Use `--inline` (or `[tui] fullscreen = false`) for the classic inline-scrollback
mode. `Ctrl+O` opens the activity viewer (main chat + subagents + critics).

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
| `/config` | Dynamic settings editor — fuzzy-search + edit any setting (and API keys); Tab toggles user/project scope |
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

### `forge setup`

```bash
forge setup              # guided: API keys + subscription plans + optional local LLM
forge init               # alias for `forge setup`
```

Runs automatically on first launch when nothing is configured.

### `forge doctor`

```bash
forge doctor             # diagnose config, providers/keys, bridges, Ollama, git, terminal
```

One command to check your whole setup, with an actionable fix for anything broken. Exits non-zero
when something's wrong (handy in CI) — and it's the first thing to paste into a bug report.

### `forge local`

Run local LLMs via [Ollama](https://ollama.com) (also a first-class mesh provider, `ollama::<tag>`).
`forge local` detects your hardware (RAM / VRAM), discovers the **current** model list from Ollama's
library at runtime (so newly-released models appear automatically), and ranks everything that fits by
**real Artificial Analysis benchmark scores** — falling back to a built-in multi-family catalog
offline. Models AA hasn't rated are shown "unrated" and ranked by size, never given a borrowed score.

```bash
forge local              # animated menu, benchmark-ranked (install / start / status)
forge local detect       # specs + every model that fits, ranked by benchmark score
forge local install      # install the top-ranked model (installs Ollama if missing)
forge local install qwen2.5-coder:14b   # install a specific tag (or catalog key)
forge local start [tag]  # ensure the runtime + model are up
forge local list         # models already pulled
forge local status       # runtime, models, autostart config
```

Enable `[local] autostart = true` (with `[local] model = "gemma4:12b"`) to start your local model when `forge chat` launches.

### `forge models`

```bash
forge models            # catalog overview + auto-pick per tier
forge models --probe    # recheck only benched/excluded models (cheap)
forge models --probe --all  # re-ping every model (costs money on paid providers)
forge models --clear    # forget all benched/rate-limited marks
```

### `forge memory`

Cross-session auto-memory — durable facts Forge remembers per project.

```bash
forge memory                    # list this project's memories (id · kind · text · salience)
forge memory add "use 4-space indent" --kind decision   # add a fact by hand
forge memory search indent      # keyword search
forge memory rm 19f07a65        # remove one (id prefix)
forge memory clear              # delete all in this scope
forge memory --global           # operate on the cross-project (global) scope
```

In chat: `/remember <text>` saves a memory on the spot; `/memories` lists them. Capture + recall are
automatic (`[mesh] auto_memory`, on by default).

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
forge skill export ./bundle  # export your commands/skills/agents (inverse of import)
forge mcp                    # MCP server status
forge mcp import             # wizard: scan installed AI CLIs
forge auth anthropic         # store an API key in the OS keyring
```

### Move your install to another machine — `forge migrate`

Copy a full Forge install (config + skills + commands + MCP servers + hooks + model
metadata) to another PC or server. The bundle is a plain **directory** — move it with
`scp -r`, `rsync`, or a USB stick, then import on the other side.

```bash
# On the old machine — write a bundle:
forge migrate export ./forge-bundle                       # config + skills + MCP + model metadata
forge migrate export ./forge-bundle --include-sessions    # + full session history & usage
forge migrate export ./forge-bundle --include-keys        # + API keys (PLAINTEXT — see below)

# Move it, then on the new machine:
forge migrate import ./forge-bundle                        # restores into this machine's config
forge migrate import ./forge-bundle --force                # also replace existing session history

# Or do it in one step over SSH (forge must be installed on the target):
forge migrate push user@server --include-keys
```

What's included:

| Data | Default | Flag |
| --- | --- | --- |
| config, skills, commands, MCP servers, hooks | ✅ always | — |
| model metadata (health / context windows / pricing) | ✅ always | — |
| session history + usage | ❌ | `--include-sessions` |
| API keys | ❌ | `--include-keys` |

- **`--include-keys` writes your API keys in PLAINTEXT** into `secrets.json` inside the
  bundle. Forge prints a warning, restores them into the new machine's keyring on import,
  and reminds you to delete the bundle afterwards. Move it only over a trusted channel — or
  omit the flag and re-run `forge auth <provider>` on the new machine.
- Import **never clobbers existing session history** without `--force`; an incoming db is
  saved alongside as `forge.imported.db` instead.
- The model-metadata export is an explicit **allow-list** (health/context/pricing only), so
  a session-free bundle can never leak transcripts.

See [docs/features/migrate.md](docs/features/migrate.md) for the bundle layout and details.

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

## `@file` Context & Memory

- **`@file` mentions** — type `@` to fuzzy-pick a file; on submit the file's contents are read and injected into the turn as context (size- and binary-capped). The `@path` stays in your prompt; the contents ride along behind the scenes.
- **Project file (AGENTS.md)** — `/init` scans the repo and writes `.forge/AGENTS.md` (overview, build/test/run, layout, conventions). On every future session Forge auto-loads `.forge/AGENTS.md` (or a top-level `AGENTS.md`) as a standing system prompt.

### Built-in auto-memory

A persistent, **cross-session** memory of durable facts, scoped per project (or `--global`), stored
in the local DB.

- **Auto-capture** — at the end of a turn, a cheap classify-tier call extracts up to a few *durable*
  facts (preferences, decisions, conventions — not transient task detail) and stores them, **typed**
  (`preference` / `decision` / `fact` / `reference`). Repeated facts de-duplicate and bump salience
  instead of piling up.
- **Recall** — at the start of a session, the **most relevant** memories are injected (ranked by
  overlap with your prompt, then salience + recency — **not** a dump of every note). When an embedder
  is configured (`[lattice.embeddings]`), recall ranks by **semantic similarity**; otherwise it falls
  back to keyword overlap.
- **You stay in control** — `/remember <text>` saves a fact on the spot, `/memories` lists what Forge
  knows, and the `forge memory` CLI (`list` / `add` / `search` / `rm` / `clear`, `--global`) manages
  it. On by default (`[mesh] auto_memory`); best-effort, never blocks a turn.

> Honest scope: the capture→recall loop is verified end-to-end (a stated preference is captured + recalled
> in a later session). Semantic recall and head-to-head comparisons vs other agents' memory are not yet
> benchmarked — it's a differentiated design (typed, de-duped, relevance-ranked, semantic-capable), not a
> measured "best".

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
    version: v1.6.1          # any recent release (its binary has `forge assay run`)
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
| [**Why Forge is a better harness**](./docs/harness/why-forge-is-a-better-harness.md) | The test-backed case — incl. where Forge does *not* win |
| [**Benchmark results**](./docs/benchmarks/results.md) | Measured SWE-bench numbers, method, and honest caveats |
| [`docs/harness/competitive-analysis.md`](./docs/harness/competitive-analysis.md) | Recon of competing harnesses + what Forge leads on |
| [`docs/benchmarks/swe-bench.md`](./docs/benchmarks/swe-bench.md) | Reproduce the benchmark yourself (`forge bench swe`) |
| [`docs/architecture/02-architecture.md`](./docs/architecture/02-architecture.md) | System design with C4 diagrams |
| [`docs/architecture/decisions/`](./docs/architecture/decisions/) | Architecture Decision Records |
| [`docs/features/`](./docs/features/) | Per-feature design docs |
| [`docs/features/persistent-bridge-transport.md`](./docs/features/persistent-bridge-transport.md) | The long-lived subscription-bridge transport |
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
