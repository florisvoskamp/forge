# Changelog

All notable changes to Forge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and Forge adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.2.0] - 2026-07-01

### Added
- **Mesh routing**: `Router::route`/`route_hinted` now take a `ProjectContext`, so routing
  decisions can account for whether Forge is self-hosting the current project.
- **Mesh inspector**: the `/mesh` overlay's candidate list now supports real cursor navigation
  (↑/↓ moves an independent cursor with its own auto-scrolling viewport) instead of a static,
  non-scrolling list.
- **Ctrl+K**: skipping a model mid-turn now retries the *same* turn on the next model in mesh
  routing order instead of aborting it, and remembers every model skipped so far in that prompt's
  retry chain so it can't cycle back to one already skipped.

### Fixed
- **Mesh routing**: Claude context-window figures corrected — Opus and Sonnet (current CLI-bridge
  versions) are now reported as 1M tokens instead of a stale 200k; Haiku remains 200k.
- **Mesh inspector**: candidate `usable` flags (both the TUI overlay and the `forge mesh`/`models`
  CLI output) now match `decide()`'s real filter (credit-mode + context-fit, not just raw
  availability), so a model shown as usable is always actually routable.
- **TUI**: a large block of global hotkeys no longer fires underneath an open modal (palette,
  usage overlay, mesh inspector, `@`-picker, model picker) — they now correctly capture all input
  while open.
- **TUI**: `/quit` (and remote prompt-as-command quit) no longer deadlocks on a pending
  permission/question reply channel.
- **TUI**: two render-loop paths (remote-snapshot publish, usage-overlay periodic refresh) held a
  blocking lock on the session mutex every frame, which could freeze the entire render loop
  — including Esc/Ctrl-C — for a turn's full duration; both now use a non-blocking `try_lock`.
- **TUI**: tier up/down, the effort slider/cycle, and model pinning silently stopped working mid-turn
  because the session lock is held for a turn's entire duration (not just during a permission
  prompt) — they now read/update a render-loop-local mirror of the relevant state instead of
  requiring the session lock for their core compute step.
- **TUI**: Ctrl+K's model-exclusion bench is now applied inside the same lock acquisition as the
  retried turn itself, closing a race that could let it skip and immediately re-route to the same
  model.
- **TUI**: the slash-command palette no longer flashes open/closed while typing arguments after the
  command name (a second, unfiltered token-detection call site was missing the cursor-position
  filter the first one already had).

## [2.1.0] - 2026-07-01

### Added
- **Remote control**: full mobile control parity — installable PWA (manifest + service worker),
  live task/subagent/queued-prompt panels, tappable `AskUserQuestion` options, wire protocol
  versioning, default-on `[remote] auto` startup config, and a 256 KiB inbound-frame cap.

### Changed
- **Packaging**: crates renamed from `adforge-*` to `forge-agent-*` (binary install is now
  `cargo install forge-agent`); commit co-author trailers now attribute to `forge@adulari.dev`.
- **License**: switched from MIT to AGPL-3.0-only (Homebrew formula corrected to match).

### Fixed
- **Mesh routing**: niche/stale 70B+ models no longer outrank Opus on the Complex tier;
  `score_for()` no longer cross-matches an unrated new model version against an old one;
  `minimax-m3` size misclassification and incomplete Ollama size-tag coverage fixed; action-verb
  matching now respects word boundaries (`"port"`/`"test"` no longer false-match inside
  "report"/"latest"); version-digit accumulation capped against overflow; strict-mode fallback
  rationale now respects credit-mode.
- **MCP**: `forge mcp agent` no longer injects itself as a self-MCP server (the fork-bomb root
  cause), including explicit self-MCP entries previously persisted into config; sessions left
  empty by that bug are pruned from the store; post-handshake discovery is now timeout-bounded,
  an SSE task leak is fixed, and reconnects are serialized.
- **CLI**: symlink-following and path-traversal escapes closed in skill install (`import`,
  `marketplace`); the remote-control pairing token is now CSPRNG-derived instead of time/pid-derived.
- **Core**: undo/restore failures are surfaced instead of silently swallowed; a failing
  `spawn_agents` child no longer orphans the rest of the batch; four independently-drifted
  `truncate` helpers deduped into one char-safe implementation.
- **Lattice (index)**: path-boundary scope check (a sibling directory sharing a name prefix no
  longer false-matches); per-file update/prune errors are logged instead of aborting or being
  silently swallowed; the embedder client no longer panics on init failure; definition-enclosure
  lookup is O(n) instead of O(n²).
- **LSP**: `initialize` is timeout-bounded, message size is capped, URIs are percent-encoded, and a
  resource leak is plugged.
- **Config**: the `/config` editor's field tables match the renamed budget-cap keys; a malformed
  `[mesh]`/`[keybinds]` section now self-heals instead of silently dropping the write.
- **Store**: soft-deleted sessions are no longer treated as empty (a data-loss fix); tool-arg and
  memory growth are capped; `LIKE`-pattern metacharacters are escaped.
- **Security hardening**: `search`/`glob`/`list_dir` tools confined to the workspace, IP-literal
  SSRF blocked, and large file reads capped.
- **TUI**: the slash/`@` token scanner no longer panics on multi-byte UTF-8.
- **Types**: `PermissionMode::from_label` accepts the canonical `"accept-edits"` key; stale
  temper-mode doc comments corrected.
- **Provider**: four CLI-bridge isolation/cleanup bugs closed.
- **Skills**: bracket-wrapped LLM-generated descriptions are now quoted so assembled `SKILL.md`
  frontmatter can't silently misparse them as a YAML list.
- **xtasks**: `bench_lattice` no longer pollutes results with failed reps, no longer panics on
  non-UTF-8 paths, and drops a dead parameter.
- **Distribution**: Homebrew formula updated to a real released version with real checksums (was
  pinned to a stale version with placeholder zero hashes); a Homebrew auto-merge deadlock and an
  Assay-injection issue in CI are fixed.
- **Docs**: 25 instances of stale crate names, versions, and design-vs-implementation drift
  corrected across feature docs, architecture docs, and root docs.

## [2.0.0] - 2026-06-30

The "ready for the world" release. A full robustness, UX, and ecosystem pass synthesized from
three deep audits (see `docs/ROADMAP-v2.md` / `docs/ROADMAP-v2-DELIVERED.md` for the 1:1
item→PR mapping). Every change shipped CI-green across Linux, macOS, and Windows.

### Packaging
- **Crates published as `adforge-*`** (the `forge-*` names are taken on crates.io by unrelated
  projects). `cargo install adforge` installs the `forge` binary. Lib names are preserved, so
  there are no source-level changes. curl / Homebrew / AUR / Scoop installs are unchanged.

### Added
- **Robustness**: non-PTY shell no longer hangs on a backgrounded process; malformed `retry-after`
  bodies can't panic the failover path; truncated streams are retried instead of returned as
  complete; `web_fetch` is byte-capped (no OOM); store hardening (IMMEDIATE txns, busy-retry,
  `UNIQUE(session_id, seq)`, `PRAGMA user_version` migrations, retention + VACUUM); file-tool
  workspace confinement; explicit per-child checkpoint env (no process-global `set_var`).
- **UX**: onboarding reach for `forge run`/`nl`; `/keys` + discoverability cues; inline-stream
  markdown rendering; red error banner; `adforge`-aware help; configurable keybinds + interactive
  configurator + mid-turn `skip_model` / `tier_up` / `tier_down` / `/reload`; customizable
  statusline; +10 polish fixes.
- **Ecosystem**: runtime custom OpenAI-compatible endpoints (LM Studio/vLLM/llama.cpp); Azure,
  Bedrock, Vertex, Together, Fireworks, Perplexity providers; MCP OAuth Dynamic Client
  Registration, sampling/roots, `tools/list_changed`, HTTP server transport, SSE client transport;
  Claude-Code-compatible hooks (with enforced `stop`/`subagent_stop`) + `forge import` of
  permissions/hooks/MCP; `forge run --output-format stream-json`; skills marketplace + `update` +
  version pinning; crates.io readiness, Homebrew Linux-ARM, AUR, Scoop, shell completions, man page.
- **Quality gates**: `cargo audit` + `cargo deny` CI (caught and fixed two real CVEs — quinn-proto
  RUSTSEC-2026-0185 and an anyhow unsoundness), Dependabot, CODEOWNERS, release-profile PR build.

### Fixed
- Three real cross-platform production bugs surfaced by the new tri-platform CI: Windows
  `path_to_uri` emitted malformed `file://` URIs; the lattice deleted-file prune keyed on a
  non-canonical path so macOS/Windows users kept phantom symbols.

### Changed
- Unified `run`/`chat` resume + TUI conventions; one shared `--scope` enum across subcommands
  (back-compat `--project` preserved); 9 major dependency upgrades (tree-sitter ×5, toml 1.1,
  pulldown-cmark 0.13, clap_mangen 0.3, sha2 0.11).

## [1.8.4] - 2026-06-30

### Added
- **Linux aarch64 (ARM64) prebuilt binary.** Release now ships `forge-aarch64-unknown-linux-gnu.tar.gz`
  built on a native `ubuntu-24.04-arm` runner. The one-line installer (`curl … | sh`) now
  auto-selects the ARM64 binary on Linux ARM hosts. (`install.sh`,
  `.github/workflows/release.yml`)

## [1.8.3] - 2026-06-29

### Added
- **CLI bridge context windows.** `claude-cli` (opus=1M, sonnet/haiku=200k), `codex-cli` GPT
  models, and `agy-cli` Gemini models now have accurate context windows stored at session start,
  derived from the same native API data already fetched for their underlying providers.
  (`crates/forge-cli/src/context_windows.rs`)
- **Context bar updates after every tool step.** The context-window gauge now refreshes on every
  tool execution, not just at turn end — zero API cost (local token estimate).
  (`crates/forge-core/src/lib.rs`)

### Fixed
- **MCP server startup messages no longer corrupt the TUI input box.** `TokioChildProcess::new()`
  routes through a builder that defaults stderr to `Stdio::inherit()`, silently overriding any
  `.stderr(Stdio::null())` set before spawning. Now using the builder API explicitly so the GitHub
  MCP server's "running on stdio" line (and any other startup text) is suppressed.
  (`crates/forge-mcp/src/transport.rs`)
- **Context windows now populated for all providers.** Anthropic's `/v1/models` (`context_window`
  field) and Gemini's `/v1beta/models` (`inputTokenLimit`) are fetched natively. OpenRouter is
  always fetched (keyless) and cross-mapped to native namespaces: `openai::`, `xai::`,
  `deepseek::`, `mistral::`, and `nvidia::`. NVIDIA NIM and other custom providers whose model
  listing doesn't include context info now get windows via an OR basename lookup (e.g.
  `nvidia::meta/llama-3.1-405b-instruct` is matched to OR's `meta-llama/llama-3.1-405b-instruct`
  by model basename). (`crates/forge-cli/src/context_windows.rs`)
- **Token gauge hides stale ↑0 ↓0 for CLI bridge sessions.** Bridge models don't report API
  token usage; the turn gauge now shows only the timer (`⧖ 1.2s`) rather than misleading zero
  counts. (`crates/forge-tui/src/app.rs`)
- **Session Σ gauge hidden on first turn.** When `turn_base_in=0`, both gauges showed identical
  values. Σ gauge is now suppressed whenever it equals the turn delta.
  (`crates/forge-tui/src/app.rs`)
- **`forge migrate push` no longer fails when `forge` isn't on remote PATH.** Falls back to
  `~/.local/bin/forge` then `~/.cargo/bin/forge` before erroring.
  (`crates/forge-cli/src/cli/commands/migrate.rs`)

## [1.8.2] - 2026-06-29

### Added
- **Context-window-aware mesh routing.** The router now filters models whose stored context
  window is smaller than the current transcript token count, preventing routes to models that
  would overflow mid-turn. Fail-open: models with no stored window pass through and the
  conservative 32k floor still guards transcript trimming. (`crates/forge-mesh/src/lib.rs`,
  `crates/forge-mesh/src/pricing.rs`)
- **Effort multiplier for context-window selection.** HIGH effort inflates the minimum context
  requirement by 1.5×; XHIGH/Max by 2×, so large-output turns prefer models with sufficient
  headroom for extended thinking chains. (`crates/forge-mesh/src/lib.rs`)
- **Dynamic context-window fetch for Groq + all custom providers.** Context windows are now
  fetched from Groq's `/openai/v1/models` and every configured custom OpenAI-compatible
  endpoint (NVIDIA NIM, Cerebras, SambaNova, …) at session start and persisted to the store.
  All hardcoded family heuristics stripped from `context_limit()` — only CLI bridges remain
  (no queryable API). (`crates/forge-cli/src/context_windows.rs`,
  `crates/forge-mesh/src/pricing.rs`)
- **Context-window badge in model pickers.** `/model` pin picker and `/models` provider
  drill-in now show "128k ctx", "32k ctx" etc. per model row, read from the persisted store.
  (`crates/forge-cli/src/cli/commands/run/pickers.rs`)

## [1.8.1] - 2026-06-29

### Added
- **429 key-retry before mesh failover.** When a provider has ≥2 API keys configured, a
  rate-limit error now retries once with the next key (via the round-robin pool's `AtomicUsize`)
  before escalating to mesh failover. Single-key providers are unaffected.
  (`crates/forge-provider/src/genai_provider.rs`)
- **Interactive model-pin picker (`/model`).** Bare `/model` opens an animated ranked picker
  (mesh auto first, then subscription → frontier → paid → free). `/model <partial>` opens it
  pre-filtered; `/model provider::model` pins directly; `/model` bare in active-pin state clears
  the pin and restores mesh auto-routing.
  (`crates/forge-cli/src/cli/commands/run/pickers.rs`, `crates/forge-tui/src/app.rs`)
- **Animated effort slider.** `/effort` (bare) opens an interactive slider above the input bar
  with per-level animation: HIGH pulses orange→gold→red, XHIGH cycles a 12-colour rainbow with
  sparkle characters. (`crates/forge-tui/src/app.rs`)

### Fixed
- **MCP stdio stderr leak into TUI.** `TokioChildProcess::new` internally defaults to
  `stderr=inherit`, overriding any `Command::stderr(null())` we set. Switched to
  `TokioChildProcess::builder().stderr(null()).spawn()` so child startup text (e.g. "GitHub MCP
  Server running on stdio") no longer appears in the raw-mode TUI.
  (`crates/forge-mcp/src/transport.rs`)
- **MCP double-announce on startup.** The initial `announce_mcp()` ("reconnecting" placeholder)
  is now suppressed in TUI mode; a background task waits for `connect_active()` to complete via
  a `tokio::sync::watch` channel and then emits the final connected/failed state only once.
  (`crates/forge-mcp/src/lib.rs`, `crates/forge-cli/src/cli/commands/run.rs`)

## [1.8.0] - 2026-06-29

### Added
- **Auto-detect lint/test commands from project structure.** `forge_autofix` now inspects
  `package.json`, `Cargo.toml`, `pyproject.toml`, etc. to determine the correct check command
  instead of falling back to a hard-coded default. (`crates/forge-core/src/lib.rs`)
- **Tool-failure loop guard + doom-loop detection.** Repeated identical tool-call failures trigger
  a hard stop with a user-readable error instead of looping indefinitely. Doom-loop heuristic
  detects N consecutive identical error payloads and breaks out. (`crates/forge-core/src/lib.rs`)
- **TUI live observer mode for MCP-driven sessions.** When Forge is driven by an external MCP
  client, the TUI can attach as a read-only observer: streaming progress, tool calls, and model
  output in real time without interfering with the running session.
  (`crates/forge-tui/src/app.rs`, `crates/forge-cli/src/cli/commands/run.rs`)
- **`forge_assay` MCP tool.** New MCP tool that runs a bounded static analysis pass over a
  source bundle and returns structured findings; exposes Forge's assay engine to external
  MCP clients. (`crates/forge-cli/src/mcp_agent.rs`)

### Changed
- **SQLite WAL mode + pragmas.** Enabled WAL journal mode, `synchronous=NORMAL`, and
  `busy_timeout=5000` on the session store for faster concurrent writes.
  (`crates/forge-store/src/lib.rs`)
- **Transaction batching for bulk inserts.** Multi-message DB writes now execute inside a single
  transaction, cutting fsync overhead by up to 10× on turn-end flushes.
  (`crates/forge-store/src/lib.rs`)
- **`prepare_cached` on hot write paths.** `add_message_full`, `record_routing`, and
  `record_tool_call` now use `prepare_cached` instead of re-compiling SQL on every call.
  (`crates/forge-store/src/lib.rs`)
- **Background update check in TUI mode.** The once-daily GitHub version probe is now spawned
  off the startup path and delivers its notice via the TUI warning channel instead of blocking
  before the interface appears. (`crates/forge-cli/src/update_check.rs`)
- **Deduplicated config + catalog loads at startup.** `forge_config::load()` and
  `forge_skills::Catalog::load()` are each called once at TUI startup; the TUI now reuses the
  session's already-loaded `Arc<Catalog>` instead of constructing a second copy.
  (`crates/forge-cli/src/cli/commands/run.rs`)
- **Async codex bridge rollout.** Codex CLI-bridge turns now use async streaming, matching the
  claude bridge, for lower latency and better cancellation.
  (`crates/forge-provider/src/cli_provider.rs`)

### Fixed
- **Full-codebase security + stability sweep.** Four real bugs fixed: unvalidated shell-injection
  vector in tool argument handling, integer overflow in token accounting, unchecked slice index
  in transcript rewind, and a race in the MCP session lock during interrupt.
  (`crates/forge-core/src/lib.rs`, `crates/forge-tools/src/shell.rs`)
- **`/undo` on turns with autofix injections.** When `cargo check` triggered a synthetic
  `Role::User` message mid-turn, `undo()` found the injected message instead of the real prompt,
  leaving the snapshot range empty. Fixed by using a `-1` sentinel for "no turn yet" and
  recomputing the transcript index correctly. (`crates/forge-core/src/lib.rs`)

## [1.7.0] - 2026-06-28

### Added
- **`forge mcp agent` — Forge-as-MCP-server.** Expose a persistent Forge session over stdio MCP so
  any MCP-aware agent (Claude Code, another Forge instance) can drive it as a stateful coding agent.
  Four tools: `forge_chat` (send prompt, stream progress notifications), `forge_status` (inspect
  session), `forge_set_mode` (switch permission mode at runtime), `forge_interrupt` (abort an
  in-flight turn mid-flight via `tokio::sync::Notify` without touching the session lock).
  (`crates/forge-cli/src/mcp_agent.rs`)
- **`LoopOutcome` — explicit turn stop reason.** `run_turn` / `run_turn_with` now return
  `Result<LoopOutcome, CoreError>` instead of `Result<String, CoreError>`. `LoopOutcome` carries
  `text: String` and `stop_reason: StopReason` (FinalAnswer / MaxSteps / BudgetExhausted /
  Interrupted) so callers know why the turn ended without re-parsing warning text. Implements
  `Deref<Target=str>`, `Display`, and `PartialEq<str/&str/String>` so existing test assertions
  compile unchanged. (`crates/forge-types/src/lib.rs`, `crates/forge-core/src/lib.rs`)
- **TUI statusline stop_reason indicator.** When a turn ends with `MaxSteps`, the statusline shows
  an amber `⚠ step limit — send 'continue'`; `BudgetExhausted` shows a red `✕ budget cap`.
  `PresenterEvent::Done` now carries `stop_reason` so the App stores and renders it.
  (`crates/forge-tui/src/app.rs`, `crates/forge-tui/src/lib.rs`)
- **`forge_interrupt` MCP tool.** Abort the in-flight `forge_chat` turn at its next await point via
  `tokio::sync::Notify`; the session state is preserved and a partial result returned. The handler
  never holds the session lock, so it runs concurrently with the ongoing turn.
  (`crates/forge-cli/src/mcp_agent.rs`)
- **`remember` virtual tool on MCP bridge path.** `mcp_serve.rs` now advertises and handles the
  `remember` tool so bridge models (claude-cli, codex-cli) can write cross-session memories
  mid-turn, at parity with the direct path. (`crates/forge-cli/src/mcp_serve.rs`)
- **Embeddings default-on** with free Gemini `text-embedding-004` backend (falls back to OpenAI
  then ollama then no-op). Fixed `#[serde(default)]` on `EmbeddingsConfig.enabled` that silently
  kept embeddings off when the config section was partially specified.
  (`crates/forge-config/src/lib.rs`)

### Docs
- **README synced to the v1.6.x wave:** documented built-in auto-memory (capture/recall, `/remember`,
  `/memories`, `forge memory` CLI — with an honest "differentiated design, not benchmarked-best" note),
  the `/effort` mesh-wide slider, reset-aware rate-limit handling, multi-key rotation, and
  `credit_mode=strict`; refreshed the stale Action `version:` example to v1.6.1.

## [1.6.1] - 2026-06-28

### Added
- **Semantic memory recall is now live end-to-end.** The v1.6.0 engine is wired into the turn loop:
  when an embedder is configured (`[lattice.embeddings]`), end-of-turn capture stores each fact's
  embedding (`add_memory_with_embedding`) and start-of-session recall ranks memories by cosine
  similarity to the prompt (`recall_semantic`) instead of keyword overlap. Falls back to keyword
  recall transparently when no embedder is available — so it's a pure upgrade, never a regression.
  Implemented as a free `embed_one(&EmbeddingsConfig, …)` function (not a `&self` method) so the
  `.await` doesn't hold a `&Session` borrow — the fix for the non-`Send` turn future that blocked the
  v1.6.0 wiring.

## [1.6.0] - 2026-06-28

### Added
- **Semantic memory recall engine** (store layer). The auto-memory store now supports embedding-based
  recall: `add_memory_with_embedding` persists a fact's f32 vector in the (already-reserved)
  `embedding` column, and `recall_semantic` ranks a scope's memories by **cosine similarity** to a
  query embedding (memories with no/empty/mismatched embedding sort last), with little-endian f32 ↔
  blob helpers. Fully unit-tested (round-trip, cosine ranking, no-embedding fallback). *(Built by
  Forge itself via the mesh — and the v1.5.1 truncation guard proved itself live: the same task that
  corrupted the file last time landed as 175 purely-additive lines this time.)*

  The end-of-turn capture + start-of-session recall still use keyword overlap; wiring this semantic
  engine into them (best-effort, behind the existing embeddings config, keyword fallback) is the next
  increment — it needs a `Send`-safe embed path, since calling the embedder directly in the turn loop
  makes the turn future non-`Send`.

## [1.5.1] - 2026-06-28

### Fixed
- **`edit_file` now rejects a truncated replacement instead of corrupting the file.** When a model's
  `new` argument is cut off mid-string (its output got truncated), the edit used to apply and leave
  the file with an unterminated string literal / unbalanced delimiters. `apply_edit` now detects a
  likely-truncated replacement (an odd number of unescaped `"`) up front and returns an actionable
  error — "looks truncated … make a smaller edit, or use write_file" — before touching the file.
  New `looks_truncated` helper + tests. *(Built by Forge itself via the mesh — the deeper fix for
  the edit-tool truncation that previously corrupted large edits during dogfooding; pairs with the
  v1.4.1 error-message improvements.)*

## [1.5.0] - 2026-06-28

### Added
- **`/remember` and `/memories` in-session commands.** `/remember <text>` saves a durable memory for
  the current project on the spot (explicit curation alongside the automatic end-of-turn capture);
  `/memories` lists what Forge knows about the project. The start-of-session recall now also prints a
  one-line "💭 recalled N memories from past sessions" note so it's visible. Builds on the v1.4.0
  auto-memory system. *(Built by Forge itself via the mesh — dogfooding.)*

## [1.4.1] - 2026-06-28

### Improved
- **`edit_file` failures are now self-correcting instead of churning.** When `old` doesn't match
  after the whitespace + block-anchor fallbacks, the error now tells the model the file may have
  changed and to **re-read the exact lines with `read_file` and copy them verbatim**; if `old` is
  large (>40 lines) it suggests **splitting into smaller edits or using `write_file`** for a new
  file. A whitespace-insensitive *ambiguous* match (several near-matches) is now reported distinctly
  ("matches N places — add surrounding context") instead of looking like a plain miss. New
  `flex_match_count` + 3 tests. *(Built by Forge itself via the mesh — the first of the
  dogfooding-friction fixes that make Forge better at building everything.)*

## [1.4.0] - 2026-06-28

### Added
- **Built-in auto-memory — Forge now remembers durable facts across sessions, per project.** A new
  `memory` table in the store holds typed facts (preference | decision | fact | reference) scoped to
  each project (or `global`). At the end of a turn Forge makes one cheap trivial-tier call to extract
  0–3 durable facts (preferences, project decisions/conventions, key constraints — not transient task
  detail) and stores them; repeated facts auto-dedup (Jaccard) and bump salience instead of piling
  up. At the start of a session it **recalls only the few most relevant** memories (keyword overlap
  with the prompt, then salience + recency) and injects them into context — the edge over a
  dump-everything memory file. On by default (`[mesh] auto_memory`).
  - New `forge memory` command: list (no subcommand), `add <text> [--kind]`, `search <query>`,
    `rm <id>`, `clear`; `--global` for the cross-project scope.
  - Store API: `add_memory` / `list_memories` / `recall_memories` / `search_memories` /
    `delete_memory` / `clear_memories`, fully unit-tested (dedup, relevance ranking, scope
    isolation). Capture/recall are gated, best-effort, and never derail a turn.

  *(The schema + initial store layer were drafted by Forge itself via the mesh — dogfooding; Claude
  Code completed the store methods, CLI, capture/recall wiring, and tests after Forge hit edit-tool
  limits on the large files.)*

## [1.3.1] - 2026-06-28

### Fixed (mesh robustness — surfaced by dogfooding)
- **Empty-response models now fail over instead of dead-ending the turn.** A model that streams an
  empty final chunk (e.g. some NIM models like kimi-k2.6) was nudged twice and then the turn
  *stopped* — even with healthy fallback models (incl. the reliable subscription bridge) untried.
  It now benches the empty model and fails over down the chain, so the turn reaches a working model.
- **402 "Payment Required" via the streaming path is now treated as permanent (Capability).** A
  paid model surfaced through streaming (e.g. SambaNova: "A payment method is required to use
  `<model>`") was misclassified as a transient outage — retried twice and benched on a short
  cooldown instead of excluded. Added payment-required markers to the capability classifier so such
  a model is excluded immediately and failover moves on cleanly.

## [1.3.0] - 2026-06-28

### Added
- **The `/effort` slider now steers the whole mesh, not just the provider's reasoning param.** Effort
  is threaded from `Session.pinned_effort` into the routing decision (Router trait → `decide` →
  `ranked_seeded`), benchmark-driven:
  - **Medium / unset** — routing is byte-identical to before (no change).
  - **High / xHigh** — biases ranking toward the higher Artificial-Analysis-benchmarked model, but
    ONLY when the score gap is meaningful (≥1.0); equal/unknown scores keep the normal order.
  - **Low** — keeps the better model on a real quality gap, and prefers the cheaper/faster one only
    on a near-tie.
  Provider reasoning-effort is still sent, but only escalated for models that actually benefit
  (`model_benefits_from_effort`: o-series/gpt-5/thinking/reasoning/DeepSeek-R1). The active effort is
  shown as a colored, per-tier label in the TUI chat (`PresenterEvent::Effort`). New mesh tests pin
  all three behaviors. *(Implemented by Forge itself via the mesh — dogfooding.)*

## [1.2.0] - 2026-06-28

### Added
- **In-turn wait-for-reset on rate limits.** When the best-ranked model is rate-limited with a SHORT
  reset (per-minute free tiers — NIM/Groq/Gemini), Forge now waits out the reset and retries the SAME
  model instead of immediately degrading to a lower-ranked (or paid) one. Bounded by a per-turn wait
  budget (2) and a cap on the reset length (75s) — a long/hourly/daily quota still falls through to
  normal failover, so the turn never blocks indefinitely. Completes the rate-limit handling from
  v1.1.2 (short cooldown) and v1.1.1 (strict-credit).

### Changed
- **Non-chat models are excluded from the routable catalog.** Image/video generation (imagen, veo,
  lyria), audio/TTS (voxtral, orpheus, whisper), embeddings, reranking, OCR, and moderation/safety
  classifiers can't serve `/chat/completions`, so they only churned failover and showed as a
  heuristic "—". They're now filtered from EVERY discovery source (genai list, OpenRouter, the custom
  `/v1/models` listers) via `forge_config::is_non_chat_model`. Multimodal CHAT models (…-vision,
  …-vl, flash) are kept.
- **The benchmark cache is now global** (`~/.local/share/forge/benchmarks.json`) instead of
  project-local (`.forge/benchmarks.json`), so Artificial Analysis scores — which are model-wide —
  are shared across every project and refreshed once, not re-fetched per repo. The legacy project
  file is still read as a fallback so existing scores aren't lost on upgrade.

## [1.1.2] - 2026-06-28

### Fixed
- **Rate-limited free models come back in ~1 min, not 5.** The default bench cooldown when a 429
  carried no `Retry-After` was 300s, so a per-minute free tier (NVIDIA NIM, Groq, Gemini RPM) got
  parked for five minutes — long enough that routing abandoned all of that provider's high-ranked
  free models and degraded to a weaker (or paid) one. Default cooldown is now **60s**; providers
  that send `Retry-After` still use their exact value. (Pairs with the v1.1.1 strict-credit fix so
  failover can't fall onto paid models in the meantime.)
- **Transient errors retry the same model before failing over.** A one-off 5xx / dropped stream /
  network blip used to bench the model and switch immediately; it now retries the SAME model up to
  twice with backoff first (these usually succeed on a second attempt), only failing over if it
  keeps failing. Rate-limits (respect the cooldown) and permanent incapabilities (no tool support /
  402) still fail over at once — no pointless retries.

## [1.1.1] - 2026-06-28

### Fixed
- **`credit_mode = "strict"` now actually keeps paid models out of routing and failover.** It was
  wired only to the output-token cap, so its documented "free + subscription only" promise was never
  enforced — a free pick that failed over (or a declined smaller-model fallback) could land on a
  PAID model (e.g. `openrouter::google/gemini-2.5-pro`) and silently spend credit. Strict now drops
  every paid/metered model (including priced "free-tier" models that could bill once their quota is
  gone) from the auto-route candidate set and the whole failover chain; only free + subscription
  models remain. An explicit `--model` pin still overrides (it bypasses the auto path), so a
  deliberate paid choice works. Normal/Frugal are unchanged.

## [1.1.0] - 2026-06-28

### Fixed
- **The "working" spinner now stops the instant the response is done — it no longer waits on the
  end-of-turn recap.** The recap (a separate trivial-tier summary call) was awaited inside the turn,
  so the spinner kept ticking and the input/session stayed locked until it finished. The recap now
  runs on a detached task (via a clonable presenter sink) after the turn returns: the spinner stops
  and the next prompt is ready immediately, and the recap line streams in a moment later. Headless /
  non-interactive runs keep the inline behaviour.
- **The release's Homebrew-formula PR now actually opens.** `release.yml` granted only
  `contents: write`, so the `gh pr create` in the formula-update step was denied (it pushed the
  `brew/v<version>` branch but couldn't open the PR — masked by `continue-on-error`). Added
  `pull-requests: write` so future releases self-open the brew bump PR.

### Added
- **Multiple API keys per provider, with round-robin rotation.** Every key-based provider (all except
  the CLI bridges) can hold several keys — run `forge auth <provider>` again to stack another. The
  provider client round-robins across them per request, multiplying a free tier's per-key rate limit
  and landing a 429-retry on the next key. New `forge auth` flags: `--list` (masked count),
  `--replace` (overwrite), and the default now APPENDS instead of overwriting. Keys also come from
  the env as `VAR="k1,k2"` or numbered `VAR_2`/`VAR_3`/…. Rotation engages only at ≥2 keys, so
  single-key (and cache-sensitive paid) providers are unchanged. See
  [docs/features/free-models.md](docs/features/free-models.md#multiple-keys-per-provider-rotation).
- **🆓 Recommended free providers** — a new README section (linked from the top nav) tiering every
  free-tier provider with best models, rate limits, direct signup links, and the exact `forge auth`
  command.
- **NVIDIA NIM, SambaNova, and Mistral providers — free frontier models in the mesh.** Three new
  OpenAI-compatible providers wired through a generalized custom-endpoint resolver: `nvidia::`
  (DeepSeek-R1, Llama-3.1-405B, Nemotron-70B — free dev tier), `sambanova::` (DeepSeek-V3.1,
  Llama-3.3-70B, Llama-4 Maverick), `mistral::` (Mistral Large 3, Codestral). Plus `cohere::` via
  genai's native adapter (Command A 218B). Authenticate with `forge auth <provider>`; models are
  seeded into discovery (these endpoints can't be model-listed live) and route + fail over like any
  other free provider. Their **full live catalog is listed** via the provider's OpenAI `/v1/models`
  endpoint (`list_custom_models`) — generic over the registry, so NVIDIA NIM surfaces 100+ models,
  not a hand-seeded few; the `seed_models` list is now only a fallback when the live call fails.
  Embedding / reranking ids are filtered out (they can't serve chat).
- **One-row provider extensibility.** New `CUSTOM_OPENAI_PROVIDERS` registry in `forge-config` is the
  single source of truth (namespace, endpoint, key env var, free flag, seed models) for every
  OpenAI-compatible provider genai lacks an SDK adapter for. Adding a provider is a single struct
  literal — it wires auth, env injection, mesh discovery, cost-tier routing, the free/paid flag, and
  cross-provider failover end-to-end. The Cerebras integration was migrated onto it. See
  [docs/features/free-models.md](docs/features/free-models.md#adding-an-openai-compatible-provider).

## [1.0.0] - 2026-06-28

**Forge 1.0.** A model-agnostic AI coding agent for the terminal whose harness is *measurably* better
than the raw vendor CLIs: on SWE-bench Lite, same `sonnet` model, **Forge fixes 6/10 vs the raw
`claude` CLI's 4/10 at ~21% lower tokens per fix** — strictly dominating, re-confirmed on this build.

### Docs (v1.0 marketing + currency pass)
- **README rebuilt for v1.0:** new hero (proven harness + model freedom), a "same model, better
  results" benchmark section, a head-to-head comparison vs Claude Code / Codex / Cursor / OpenCode /
  Aider, a demo-GIF slot (`docs/assets/demo.tape` + recording guide), refreshed docs links, and the
  stale `v0.1.0` Action example bumped.
- **Docs currency sweep:** `known-issues.md` store-connection-pool backlog marked RESOLVED (#308);
  `roadmap.md` "Road to v1.0.0" updated (must-haves done, post-1.0 Next); benchmark + persistent-transport
  claims already corrected in `results.md` / `why-forge-is-a-better-harness.md` (#311).

### The road here (v0.4.44 → 1.0.0)
Robustness + correctness from 7 verify-first bug-hunts (28+ real bugs); the persistent
subscription-bridge transport; `forge-store` connection pool; the two final TUI bugs; Homebrew
auto-update; and the benchmark re-confirmation. Honest scope unchanged: Forge resolves more and is
cheaper per resolve; total-token efficiency is parity (not a win) — stated plainly in the proof doc.

## [0.4.71] - 2026-06-28

### Fixed (release — Homebrew auto-update now works on protected main)
- **The release's Homebrew-formula update opens a PR instead of pushing to `main`.** The v0.4.70
  release proved the direct-push approach silently no-ops on a branch-protected `main` (the push is
  rejected, and `continue-on-error` masked it as "success"). The step now pushes a `brew/v<version>`
  branch and opens an auto-merge PR — which respects branch protection. Formula brought current to
  **v0.4.70**.

## [0.4.70] - 2026-06-28

### Docs (benchmark re-confirmation on the current build + stale-claim sweep)
- **Re-confirmed the headline SWE-bench result on the current build (v0.4.65).** The standing
  comparison was from v0.4.39 (~26 releases old); re-ran the same setup (10 SWE-bench Lite instances,
  `claude sonnet`, same model both arms, loop-gated config, official `swebench` evaluator):
  **Forge 6/10 vs raw claude-cli 4/10**, Forge **strictly dominating** (every CLI solve is also
  Forge's, +2 Forge-only, 0 CLI-only) at **~21% lower tokens/resolve** (2.83M vs 3.57M). N=10 is
  small and `requests-2317` hung on both arms (excluded fairly) — it confirms no regression, not a
  fresh large-sample proof. Written up in `docs/benchmarks/results.md` + `why-forge-is-a-better-harness.md`.
- **Corrected a stale claim** in `why-forge-is-a-better-harness.md` §3: the persistent stream-json
  transport was described as "not a shipped result" — it **shipped in v0.4.63**. Now stated
  accurately (ships the ~0.88s/turn spawn-cost saving; tokens unchanged; the deeper "Forge drives the
  inner loop" variant is still open).

## [0.4.69] - 2026-06-28

### Fixed (release — Homebrew formula)
- **The Homebrew formula auto-updates per release.** `homebrew/forge.rb` was pinned at **v0.4.4** —
  `brew` users had been getting a months-old binary because the "update version + sha256 by hand
  after the tag" step never happened. `release.yml` now runs `scripts/update-brew-formula.sh` after
  publishing, reading the **just-built** `checksums.txt` (so there's no race against assets that
  don't exist yet) and committing the bumped formula to `main` (best-effort — a protected branch or
  push hiccup never fails the release; the script is also runnable by hand). Formula brought current
  to v0.4.65 in this PR.

## [0.4.68] - 2026-06-27

### Tests (harness quality)
- **Pinned that `mesh.self_review` still fires when enabled.** The self-review pass is off by default
  (it regressed when on-by-default) but must stay wired; added `self_review_runs_after_an_edit_turn_when_enabled`
  so the gated feature can't silently rot. (Also confirmed the autofix iteration-cap is already
  test-pinned by `autofix_iteration_cap_halts_the_self_heal_loop` — the v1.0 plan's "TODO" for it was stale.)

## [0.4.67] - 2026-06-27

### Changed (store — connection pool)
- **`forge-store` moved from a single `Mutex<Connection>` to an `r2d2` connection pool**, so the TUI
  run loop, subagents, and the lattice indexer no longer serialize every read behind one lock —
  WAL-backed file DBs now serve concurrent reads from separate pooled connections. Writes still
  serialize (SQLite's one-writer rule) but wait on the existing `busy_timeout` instead of failing.
  - To avoid `r2d2_sqlite` (which pins an older `rusqlite`/`libsqlite3-sys` and would link a second
    bundled SQLite), the pool uses a hand-rolled `ManageConnection` over our `rusqlite` 0.40 that
    applies the per-connection pragmas on every open.
  - In-memory stores (tests) pin to a single never-recycled connection — each `:memory:` open is a
    fresh empty DB, so recycling would silently lose data. All 35 store tests pass, plus a new
    `pool_handles_concurrent_threads_on_a_file_db` (8 threads × 20 writes land with no
    "database is locked"). The public `Store` API is unchanged.

## [0.4.66] - 2026-06-27

### Fixed (TUI — the two deferred bug-hunt-6 bugs)
- **Inline activity viewer re-enables follow at the tail.** `Down`/`j`/`space`/`PageDown` scrolled but
  never re-armed auto-follow, so returning to the bottom left new activity un-tailed until you pressed
  `End`/`G`. The render path now records the viewer's scroll geometry `(wrapped_len, body_h)`; a
  downward scroll that reaches the last page clamps and re-arms follow (matching the full-screen
  browser). Test: `viewer_down_at_tail_reenables_follow`.
- **Text selection spans the committed/streaming boundary.** `selection_text` only read the committed
  wrap cache, so a copy that ran into the live streaming reply was cut off at the boundary. It now
  pulls rows from the streaming edge too (the rendered transcript is `wrap_cache` + `stream_cache`).
  Test: `selection_spans_committed_and_streaming_rows`.

## [0.4.65] - 2026-06-27

### Changed (disk hygiene)
- **`[profile.dev] debug = "line-tables-only"`.** Full debuginfo made each debug/test binary
  ~480 MB; cargo writes a fresh one per build-hash and never GCs the old ones, so a long session of
  `cargo test` runs accumulated hundreds of GB in `target/debug/deps`. Line-tables-only keeps
  file:line in backtraces/panics at ~⅓ the size (measured: forge-core test binary 484 MB → 163 MB).
- **Subagent worktrees share the parent `target/`.** `WorktreeGuard::create` now writes
  `<worktree>/.cargo/config.toml` pointing `build.target-dir` at the parent repo's `target/`, so a
  build inside an isolated child worktree reuses the main build cache instead of compiling its own
  multi-GB copy — which both wasted disk/time and leaked if the process was killed before the guard's
  `Drop`. Best-effort (falls back to a private target on write failure); the untracked config is
  never merged back (`git diff HEAD..branch` drives merge-back). Test extended.

## [0.4.64] - 2026-06-27

### Docs
- **Persistent bridge transport scope documented** (`docs/features/persistent-bridge-transport.md`):
  why P1 is claude-only. Probed all three CLIs — **agy** 1.0.12 has no streaming-input mode (only
  one-shot `--print`); **codex** 0.141 `exec-server --listen stdio` speaks JSON-RPC but is an
  unimplemented STUB (`initialize` works, but every turn method returns `-32601 "exec-server stub
  does not implement … yet"`), so a persistent codex transport is blocked upstream, not on Forge.
  Recorded the same on the `CliProvider::persistent` field so it reads as deliberate.

## [0.4.63] - 2026-06-27

### Added (P1 — persistent bridge transport)
- **Long-lived claude `--input-format stream-json` transport.** The claude bridge now keeps ONE
  process alive across turns/re-drives and writes each turn's delta to its stdin, instead of
  re-spawning (and re-`--resume`-ing) the CLI every turn. Removes the per-turn process-spawn +
  session-reload cost that one-shot paid on every re-drive. On by default for claude;
  `FORGE_PERSISTENT_BRIDGE=0` (or `CliProvider::with_persistent(false)`) opts out.
  - **Safety:** falls back to the one-shot path whenever the live session can't be established
    *before* any turn output ran (spawn failure, first-turn stdin-write failure, immediate exit with
    no tool executed), so a tool can never double-execute. Once a turn has started, errors propagate
    (retryable) rather than re-running.
  - **Correctness:** respawns on model change, transcript shrink (compaction), and a
    `FORGE_CHECKPOINT_SEQ` change (a new user turn) — so re-drives *within* a turn reuse the process
    (the win) while bridge-edit `/undo` snapshots stay turn-accurate.
  - **Proven:** deterministic test `persistent_transport_reuses_one_process_across_turns` (a 2nd turn
    served by the same process answers "reply 2"; a fresh spawn would answer "reply 1"), live e2e
    against real claude (recalls a codeword across two turns on one process), and a measured fixed
    overhead of **≈0.88s spawn→init per one-shot turn** that persistent now pays once per session
    instead of once per re-drive. Honest scope: model inference dominates total turn time, so this is
    a real per-re-drive saving that compounds, not a headline multiplier.

## [0.4.62] - 2026-06-27

### Fixed (bug-hunt batch 7 — provider + store + tools)
- **A nameless `<parameter>` no longer drops a whole recovered tool call.** `tool_recovery::parse_span`
  used `attr_value(after, "name")?`, so a `<parameter>` with no `name` aborted the entire `<invoke>` —
  the recovered call vanished and the bridge phantom-succeeds (the exact failure prose-recovery exists
  to prevent). It now skips the malformed tag and continues, matching `parse_parameter_tags`' contract.
  Test: `nameless_parameter_tag_does_not_drop_the_whole_call`.
- **Session token / step counts exclude soft-deleted messages.** `session_tokens` and
  `session_step_count` joined `usage`→`message` on `session_id` only, so undone/compacted (`active=0`)
  turns still inflated the live TUI token counter and the Lattice "steps" benchmark metric. Added
  `AND m.active = 1`, consistent with every other session-scoped query. Test:
  `session_tokens_and_step_count_exclude_deactivated_messages`.
- **`strip_ansi` no longer leaks a byte from 3-byte charset-designation escapes.** `ESC ( B` / `ESC ) 0`
  (emitted by ncurses/box-drawing tools — tree, pagers, top) are 3 bytes; only 2 were consumed, leaking
  the final byte into model-facing output. The final byte is dropped when the post-`ESC` byte is an
  intermediate `0x20..=0x2f`; 2-byte escapes (`ESC c`, `ESC =`) are unaffected. Test:
  `strip_ansi_drops_three_byte_charset_sequences`.

(A 4th candidate — `watch.rs should_reindex` treating `./`-prefixed paths as dot-dirs — was triaged
real-but-lower-certainty and deferred.)

## [0.4.61] - 2026-06-27

### Fixed (bug-hunt batch 6 — TUI)
- **Input box no longer hides the cursor on wide glyphs.** `input_text_rows` estimated wrapped rows
  from `chars().count()`, but ratatui wraps on terminal cells. A CJK/emoji glyph is 2 cells but 1
  char, so a line of wide glyphs under-counted its rows and the cursor dropped below the visible
  input box. Now counts `unicode_width::UnicodeWidthStr::width`. Test:
  `input_text_rows_counts_cell_width_not_chars`.
- **Full-screen transcript browser re-follows at the visual bottom, not a page past it.** The browse
  loop clamped scroll to `wrapped_len - 1` and re-enabled auto-follow only there, but `transcript_lines`
  renders a `body_h = height - 3` window and clamps display scroll to `wrapped_len - body_h`. After
  scrolling up, returning to the tail left follow OFF for a full extra page (the bottom already showed,
  but new content wouldn't tail). The loop now mirrors the render's `body_h` math for both the clamp
  and the re-follow threshold.

Two further bug-hunt-6 TUI items (inline-viewer follow re-enable; selection across the
committed-cache/streaming-tail boundary) need wider plumbing and are deferred to a follow-up rather
than rushed.

## [0.4.60] - 2026-06-27

### Fixed (bug-hunt batch 5 — provider + store)
- **Claude `overloaded` errors now trigger failover.** The in-band error classifier (v0.4.52) caught
  rate-limit/auth but NOT `overloaded`/server errors, so a transient overload surfaced as a
  non-retryable `Request` and the mesh wouldn't bench or fail over. Now mapped to `Unavailable`
  (retryable). Test: `in_band_overloaded_is_retryable_for_failover`.
- **Session message counts no longer include soft-deleted rows.** `message_count` and the
  `list_sessions` count subquery omitted `AND active = 1`, so undone/compacted messages inflated the
  counts in the session picker / `forge sessions`. Test extended:
  `deactivate_excludes_messages_from_load_but_keeps_earlier_ones`.
- **`load_findings` is actually ranked now.** The query had no `ORDER BY` despite the doc promising
  (severity, confidence) ordering, so the assay UI showed the least-important finding first. Added the
  ordering.
- **`parse_secs` reads retry-after hints from pretty-printed JSON.** A newline/tab between key and
  value (`"retryDelay":
  "37s"`) aborted the parse; it now skips all whitespace.

## [0.4.59] - 2026-06-27

### Fixed (bug-hunt batch 4 — subagent routing)
- **Subagent provider concurrency cap could be bypassed after a failover.** `orchestrate` routed each
  child to size/acquire its per-provider permit, then `run_subagent` routed AGAIN internally — and if a
  concurrent child benched the first model in between, the second route returned a different provider,
  so the child held provider A's permit while running every call against provider B (silently
  exceeding B's cap, the guard that stops a burst from draining one subscription). Each child is now
  routed ONCE and the `RoutingDecision` is threaded into `run_subagent`.

## [0.4.58] - 2026-06-27

### Fixed (bug-hunt batch 4 — worktree, subagent, assay, compaction)
- **Subagent worktree merge-back silently dropped file deletions and renames.** `merge_worktree_back`
  used `--diff-filter=ACM`, so a write-capable child that deleted or renamed a file produced no patch
  entry and the change vanished with a clean-looking merge. Now `ACDMR`. Test:
  `merge_worktree_back_applies_a_deletion`.
- **A subagent that hit the step cap reported empty SUCCESS.** It returned `{final_text:"", ok:true}`,
  so the parent assembled a blank `[agent N]` block and proceeded as if the child finished. Now
  `ok=false` with a clear "hit the N-step limit" message.
- **`assay --scope diff` silently dropped STAGED changes.** It ran `git diff` (working tree vs index),
  so a fully-staged change looked like "no uncommitted changes". Now `git diff HEAD`.
- **Compaction summary couldn't see tool calls.** Tool-call assistant messages (empty content) rendered
  as blank lines, so the summarizer never knew which files/commands a turn touched. Tool calls (name +
  args) are now included in the summary input.

## [0.4.57] - 2026-06-27

### Fixed (bug-hunt batch 4 — rewind/seq model)
- **`/checkpoints` picker rewound to the wrong turn after compaction** (regression from v0.4.51).
  v0.4.51 made `rewind_to` take a transcript INDEX (to fix the `/undo` data-loss), but the checkpoint
  picker passes a DB SEQ — so after compaction (where index and seq diverge) it double-offset and
  rewound to the wrong turn or no-op'd. `rewind_to` now takes a DB seq (the stable identity both
  `/undo` and the picker use); `undo()` maps its transcript index to a seq. Test:
  `checkpoint_rewind_by_db_seq_after_compaction_targets_the_right_turn`.
- **`reload_full_context` set `self.seq` to the loaded row count** (the same class of bug v0.4.51 fixed
  in `resume`, but this spot was missed). `load_all_messages` includes soft-deleted rows from prior
  rewinds, so the count exceeded `MAX(seq)+1`, reusing seqs / corrupting the rewind offset. Now uses
  `next_seq_for_session`.

## [0.4.56] - 2026-06-27

### Fixed (bug-hunt batch 3 — config)
- **A stdio MCP server's extra secret env vars were silently dropped.** `server_from_json` keyed the
  captured secret by the server NAME and `McpAuth` has one keyring slot per server (`mcp:<name>`), so
  a stdio server declaring two secret env vars had the first silently overwritten by the second
  (last-wins). Now the first is kept deterministically and any extra is flagged loudly in the import
  notes — and never dropped into plain env (which would expose it). Multiple secrets need separate
  servers. Test: `stdio_server_with_two_secret_envs_keeps_one_and_flags_the_extra`.

## [0.4.55] - 2026-06-27

### Fixed (bug-hunt batch 3 — index cross-repo contamination)
- **Lattice PageRank + repo-map were not scoped to the project.** The store is global (one DB across
  every project), but `recompute_pagerank` and the repo-map ran UNSCOPED queries
  (`lattice_node_ids_and_names`, `lattice_ref_edges`, `lattice_nodes_ranked`) that scanned every
  project's rows. So a reference to `foo` in projectA distributed PageRank to all `foo` nodes across
  ALL projects, and projectB's map could list projectA's symbols. All three queries now take a
  `repo_root` and `JOIN lattice_file … WHERE f.repo_root = ?` — matching the already-scoped impact/ref
  queries. Test: `map_and_pagerank_are_scoped_to_their_own_repo_root`.

## [0.4.54] - 2026-06-27

### Fixed (bug-hunt batch 3 — mcp, index, skills)
- **MCP lazy reconnect was permanently unreachable after a mid-session drop.** `classify_call_error`
  marked the server `Reconnecting` but never cleared the dead peer, so `peer_for` kept returning the
  stale handle and the `reconnect()` path never ran — every later call failed. The peer is now cleared.
- **Dotfile source files were never refreshed by the watcher.** `should_reindex` applied the
  skip-directory test to the FILENAME component, so `.eslintrc.js` / a hidden `.foo.rs` (which the
  initial `update()` walk DOES index) were excluded. Only directory components are skip-tested now.
- **Frontmatter split leaked a BOM into the prompt body.** `split()` stripped the BOM into `s` but
  returned the raw (BOM-prefixed) string on the no-fence paths, so U+FEFF reached the model. Fixed.

## [0.4.53] - 2026-06-27

### Fixed (bug-hunt batch 3 — forge-tools)
- **`read_file` panicked on an inverted line range.** `start_line` > `end_line` (e.g. 8/3) sliced
  `lines[7..3]` and crashed the whole turn on untrusted tool input. Start is now clamped to `end` (an
  inverted range yields an empty result). Test: `read_file_inverted_line_range_does_not_panic`.
- **`strip_ansi` leaked OSC escape payloads into model output.** A non-CSI escape dropped only one
  char, so `ESC]0;title BEL` (window titles) and `ESC]8;;url ST` (hyperlinks) — emitted constantly by
  color-enabled programs in PTY mode — leaked their content. OSC sequences are now consumed to their
  BEL/ST terminator. Test: `strip_ansi_consumes_whole_osc_sequence`.

## [0.4.52] - 2026-06-27

### Fixed (bug-hunt batch 2 — verified logic bugs in mesh + provider)
- **In-band CLI rate-limit errors now trigger failover.** A subscription that hit its quota mid-turn
  emitted an in-band rate-limit error wrapped as a non-retryable `Request`, so the mesh surfaced a hard
  failure instead of benching the model and failing over to a fallback. Now classified `RateLimited`/
  `Auth` (retryable). Test: `in_band_rate_limit_is_retryable_for_failover`.
- **`record_session(None)` no longer preserves a stale session id.** A fresh-transcript bridge turn that
  produced no session handle left the prior id in place, so the NEXT turn `--resume`d the PRIOR session
  and skipped the current turn's context. `None` now clears it.
- **`/mesh` explain uses the routed tier, not the classified one.** `ranked_rows`/`spread_probability`
  ran before `decide()` could downshift the tier, so the explanation showed the routed pick ranked among
  the wrong tier's rows with the wrong conservation probability.
- **Cross-tier fallback rationale reports the real reason** (no usable key / benched / quota exhausted)
  instead of always "no usable key".

## [0.4.51] - 2026-06-27

### Fixed
- **P0 data loss: `/undo` after a compacted resume wiped pre-compaction history**
  (`crates/forge-core/src/lib.rs`, `crates/forge-store/src/lib.rs`). After compaction, a resumed
  session's in-memory transcript is just the active tail (+ a synthetic summary), but the DB seqs
  start high. `self.seq` was set to the loaded message COUNT (e.g. 7) instead of `MAX(seq)+1` (e.g.
  16), and `rewind_to` used the transcript INDEX directly as the DB seq — so undoing the next turn ran
  `deactivate_messages_from(low_index)` and soft-deleted the surviving pre-compaction messages. Fixed
  with `Store::next_seq_for_session` (MAX+1) on resume and an index→seq offset in `rewind_to` (0 when
  not compacted, so no behavior change for normal sessions). Test:
  `undo_after_compacted_resume_does_not_wipe_survivors`.
- **Doom-loop nudge was dropped on the concurrent read-only batch path.** The "change approach" nudge
  is queued in `pending_hints`, but only the serial tool path drained them — so a model looping on a
  concurrent batch was halted "after a nudge" it never actually received. The concurrent path now
  drains the hints too. Test: `concurrent_batch_doom_nudge_is_delivered_to_the_model`.

### Found by
A verify-first multi-agent bug-hunt over the core run-loop, mesh, store, and provider paths.

## [0.4.50] - 2026-06-27

### Fixed (diagnostics — clearer harness output, from a verified UX/observability audit)
- **Direct-model continue-nudge exhaustion is no longer silent.** A model that narrates forever with a
  task still open got nudged a bounded number of times, then the turn ended with NO warning (the
  bridge path always warned). Now surfaces a "giving up — send `continue` to resume" warning. Test
  `direct_continue_nudge_exhaustion_warns_when_giving_up`.
- **Oscillation guard says "alternating", not "repeated".** An A,B,A,B oscillation emitted the same
  "repeated the same tool call" message as a true A,A,A repeat; now distinguished.
- **Bridge stdin-write failure surfaces its cause.** A failed prompt write (child died before reading)
  was logged but not shown; the stall message now appends the real cause instead of reading as an
  unexplained 300s timeout.
- **Setup hint no longer printed twice** when a bridge CLI exits non-zero with empty stderr. Test
  extended to assert no duplication.
- **Plain-mode slash-command hint fixed** — said "use `forge chat`" when the user was already in
  `forge chat`; now "run `forge chat` (without --plain)".

## [0.4.49] - 2026-06-27

### Added (tests)
- `autofix_iteration_cap_halts_the_self_heal_loop` — pins the last untested run-loop backstop: the
  lint/test self-heal loop stops at `max_iterations` when checks never pass (drives a real turn that
  edits a file to arm autofix, with a lint command that always fails). With this, **every reliability
  guard in the run-loop now has a deterministic test**; the conformance table in `results.md` §2 gains
  the row.

## [0.4.48] - 2026-06-27

### Docs
- **`docs/harness/why-forge-is-a-better-harness.md`** — the honest, test-backed case that Forge's
  harness beats the loop inside the raw CLI it wraps: a failure-mode table where every row cites a
  deterministic conformance test, the fair-accounted N=20 SWE-bench resolve numbers, the routing/
  failover/permission features the raw CLIs lack, and a plain statement of where Forge does NOT win
  (raw token efficiency). Linked from the README. No new claims — every row maps to an existing test
  or measured result.

## [0.4.47] - 2026-06-27

### Added
- **Bridge stream-resilience tests** (`crates/forge-provider/src/cli_provider.rs`):
  `truncated_stream_line_is_skipped_not_fatal` (a corrupt NDJSON line between valid lines is skipped,
  not fatal) and `orphan_tool_result_without_started_does_not_panic_or_phantom` (a `tool_result` with
  no preceding `tool_use` neither panics nor synthesizes a phantom call).

### Docs
- **forge-lsp public API documented** — module doc + `///` on `LspRegistry`, `from_config`,
  `diagnostics_for`, `lang_from_ext`, `repo_root`, `which` (v1.0 surface polish).

## [0.4.46] - 2026-06-27

### Fixed
- **Concurrent read-only batch now feeds the failure-loop guard** (`crates/forge-core/src/lib.rs`).
  A batch of ≥2 read-only calls ran concurrently but its results never reached `failure_counts`, so a
  model issuing two `read_file`s with different missing paths every step evaded BOTH the identical-call
  doom-loop (signature changes) and the failure-loop (concurrent path untracked) — burning the
  step/token budget to the cap. `run_readonly_batch` now returns per-call failure classifications and
  the caller folds them in, exactly like the serial path.

### Added (tests — proving existing guards)
- `step_cap_halts_a_runaway_turn` pins the primary infinite-loop backstop (`max_steps`).
- `concurrent_batch_failure_loop_is_caught` proves the fix above.
- `completion_gate_covers_its_four_outcomes` — pure decision table for the completion authority.
- `docs/benchmarks/results.md` §2 conformance table extended with every new guard (oscillation, bridge
  prose-recovery + double-exec guard, concurrent-batch failure-loop, step cap, completion-gate,
  malformed-`<parameter>` panic-safety), each row backed by a deterministic test.


## [0.4.45] - 2026-06-27

### Fixed
- **P0 panic: malformed `<parameter>` tag crashed the whole turn** (`crates/forge-provider/src/tool_recovery.rs`).
  When a model emitted a `<parameter>` open tag missing its `>`, the first `>` landed inside the closing
  `</parameter>`, making `gt > val_end` so the byte-range slice (`after[gt..val_end]`) panicked — on
  untrusted model output, with no failover. This was reachable on the bridge after v0.4.44 routed prose
  recovery there. Both slice sites (`parse_invoke_span`, `parse_parameter_tags`) now use guarded
  `.get(gt..val_end)` and stop parsing params on a malformed tag. The fuzz corpus
  (`recovery_never_panics_on_adversarial_input`) gained `<parameter>` fragments that reproduce it, plus
  a direct test `malformed_parameter_tag_does_not_panic`.
- **P1 panic: resume mutex poisoning bricked every later turn** (`crates/forge-provider/src/cli_provider.rs`).
  `self.resume.lock().unwrap()` panicked permanently once the mutex was poisoned by any prior panic. Now
  poison-tolerant (`unwrap_or_else(PoisonError::into_inner)`) — a poisoned lock degrades to a fresh,
  full-transcript turn instead of a sticky brick.
- **P1: prose-recovery could double-execute a tool the CLI already ran.** Recovery now only fires when the
  turn streamed ZERO native tool events (`tool_names` empty) — the pure prose-fallback case. If the CLI
  executed a tool natively, a tool-shaped fragment in the final text is treated as prose, not re-executed
  (guards against double-running a destructive `shell`/write). Test:
  `prose_recovery_skipped_when_cli_ran_a_native_tool`.

## [0.4.44] - 2026-06-27

### Fixed
- **Bridge prose-tool-call recovery — stops a 553× spiral** (`crates/forge-provider/src/cli_provider.rs`).
  A bridged claude/codex model sometimes writes a tool call as TEXT
  (`<function_calls><invoke name="mcp__forge__read_file">…`) instead of a native `tool_use`. The CLI
  doesn't execute text, so the call landed in the final content, ran nowhere, and the model — seeing no
  result — repeated it until the turn died (observed live: **553 unexecuted `<function_calls>` on a
  single SWE-bench instance**, contaminating a measurement run). The bridge now runs
  `recover_text_tool_calls` on its output (the same recovery the direct/genai path already had), so the
  run-loop executes the recovered call and re-drives with a real result. Only fires on actual tool-call
  markup; native calls the CLI already ran stream as events (not text), so there's no double-execution.
  Test: `recovers_prose_tool_call_the_bridge_did_not_execute`.
- **Reverted the v0.4.41 "Exploring efficiently" harness-preamble nudge.** Pushing the bridge model to
  batch reads via `read_file paths[]` / `search context:N` measurably increased how often it emitted
  those calls as *prose* (the trigger for the spiral above), and its benefit was unproven (round-trips
  fell ~21% but total tokens were flat at N=2). The batch capabilities themselves remain available for
  native use; Forge just no longer steers the model toward them. With the nudge gone AND prose-recovery
  in place AND the v0.4.42 oscillation guard as a backstop, the bridge is robust to this failure mode.

## [0.4.43] - 2026-06-27

### Changed
- **CI now runs `cargo clippy --locked`**, so a `Cargo.lock` out of sync with `Cargo.toml` fails a PR
  instead of slipping through to the release (`.github/workflows/ci.yml`). The lock had silently
  drifted since v0.4.37 — the bump step ran `cargo update -p forge`, a no-op (the root crate is
  forge-cli) — and only the release build's `--locked` caught it, failing v0.4.42's first release in
  14s. `--locked` on the clippy job catches that drift on every PR from now on.

## [0.4.42] - 2026-06-27

### Fixed
- **Doom-loop oscillation guard** (`crates/forge-core/src/lib.rs`). The consecutive doom-loop guard
  missed an `A,B,A,B,…` ping-pong (every step differs from the one before, so its repeat counter kept
  resetting), and the failure-loop missed it too (an interleaved *successful* call clears the per-tool
  failure streak). So a model alternating a failing/empty call with a trivial successful one — observed
  live: an empty `shell({})` alternating with `ls -la` after a mid-run failover to a local model — ran
  to the step cap / timeout instead of halting. Added a sliding-window oscillation count: a signature
  recurring ≥ threshold times within the last 6 steps trips the same two-stage nudge-then-halt.
  Conformance test `doom_loop_halts_a_model_oscillating_between_two_calls`.
- **Recover `<function=…>` tool calls that carry `<parameter>` sub-tags** (`crates/forge-provider/src/
  tool_recovery.rs`). Some local models (observed: ollama qwen3-coder on failover) emit a mixed format
  — a Llama-style `<function=NAME>` tag whose body is not JSON but Anthropic-style `<parameter …>`
  sub-tags. Recovery extracted the name but returned empty args → an empty no-op call that looped.
  Now parses the sub-tags as a fallback (both `<parameter=key>` and `<parameter name="key">`). Both
  bugs were found during the v0.4.41 batch-tool measurement.

## [0.4.41] - 2026-06-27

### Added
- **`search` gains optional `context` lines (grep -C)** (`crates/forge-tools/src/core_tools.rs`).
  `context: N` prints N lines around each match (match lines as `path:lineno:`, context as
  `path:lineno-`, `--` between hunks; adjacent hits merge into one hunk), so a search result is often
  enough to understand a hit WITHOUT a follow-up `read_file`. Bounded by a 64 KiB output cap.
- **Harness preamble nudges the bridge model to batch exploration** (`crates/forge-provider/src/
  cli_provider.rs`). An "Exploring efficiently" clause tells the bridged claude/codex to read several
  files in one `read_file` `paths` call and to pass `search context:N` instead of search-then-read.
  Without this the new batch affordances went unused; with it the model adopts them immediately.

### Measured (small, honest)
- A 2-instance clean A/B (same instances, same model, old vs new binary; a third instance was
  discarded after a mid-run rate-limit failover to a local model corrupted it) shows the nudge drives
  **100% of searches to use `context`** (vs 0% before) and **~21% fewer tool round-trips** (23 → 18) —
  a direct hit on the structural per-step-MCP-latency gap. **Tokens were flat** (617k → 619k): the
  context lines cost about what the saved round-trips would have, so this is a **latency win, not a
  token-efficiency win** at this sample. Reported straight; N=2 is a mechanism check, not a proof.

## [0.4.40] - 2026-06-26

### Added
- **Batch `read_file` — read several files in one tool call** (`crates/forge-tools/src/core_tools.rs`).
  `read_file` now accepts an optional `paths` array in addition to single `path`; the files come back
  in one response under `===== <path> =====` headers. This directly attacks the harness's structural
  cost on the bridge — each MCP tool round-trip re-processes the growing context, and the explore phase
  was the round-trip-heaviest (500+ tool calls on the worst SWE-bench instances). Batching the read
  phase collapses N round-trips into one. A missing/unreadable file in a batch becomes an inline
  `[error: …]` block (partial context still helps) rather than failing the whole call; per-file
  (64 KiB) and total (256 KiB) caps bound context, with remaining files noted, not silently dropped.
  Single-path behavior (incl. `start_line`/`end_line`) is unchanged.

## [0.4.39] - 2026-06-26

### Changed
- **SWE-bench firming run extended to N=20** (`docs/benchmarks/results.md`). The N=10 Forge-on-bridge
  vs claude-cli result was re-measured on 20 instances (the original 10 + 10 fresh Lite instances),
  same model, same official evaluator. The win **holds and gets cleaner**: Forge loop-gated resolves
  **11/20 vs claude-cli's 9/20** at **~11% lower cost per resolve** (1.20M vs 1.35M tokens/resolve),
  and total tokens fall to **1.08×** (near parity, down from N=10's 1.39×). Reported straight,
  including the honest caveat that the *new* 10 instances tied 5/5 — the +2 net edge comes from the
  first batch, so the resolve advantage is real but modest.

## [0.4.38] - 2026-06-26

### Added
- **Conformance tests for the opt-in loop-gated completeness re-drive.** Two deterministic run-loop
  tests with a scripted bridge provider (runs a read-only tool, then yields) lock the behavior shipped
  in 0.4.37: the completeness re-drive fires **exactly once** when `mesh.verify_completeness` is on (the
  one-shot guard prevents a loop), and **never** when it's off (the default path is unchanged)
  (`crates/forge-core/src/lib.rs`).

## [0.4.37] - 2026-06-26

### Changed
- **Completeness verification is now loop-gated — Forge beats the raw CLI on resolve *and* cost-per-
  resolve.** `mesh.verify_completeness` previously appended an always-on clause to the harness preamble
  (the model carried completeness pressure through the whole turn). It now fires **once** at turn-end
  from the core run-loop: the model works the turn normally, then does a single bounded `git diff`
  review against the request's requirements. Measured (N=10 SWE-bench Lite, same model): holds the
  **6/10 resolve win over claude-cli's 4/10** at **5.53M tokens** — the cost premium fell 3× → 1.85× →
  **1.39×** across the three forms. Headline: **922k tokens per resolve vs claude-cli's 993k (~7%
  cheaper) while solving 50% more bugs** — Forge-on-bridge is now genuinely better on both axes
  (still opt-in, default off; total tokens 1.39× because it does more total work). Removes the preamble
  clause + the temporary A/B env seam (`crates/forge-core/src/lib.rs`,
  `crates/forge-provider/src/cli_provider.rs`).

## [0.4.36] - 2026-06-26

### Changed
- **`mesh.verify_completeness` now uses a bounded one-pass review — same resolve win, 39% cheaper.**
  v0.4.35's completeness clause was open-ended ("re-verify every requirement") and cost ~3× tokens. The
  clause is now a **single bounded final-diff review** (run `git diff` once, check it against the
  request's listed requirements, make targeted fixes only — no re-exploration). Measured on the same
  N=10 SWE-bench Lite set: it **holds the full 6/10 resolve** (still beats claude-cli's 4/10) at
  **6.86M tokens vs 11.3M** — strictly better than the open-ended form it replaces. Honest remaining
  cost: ~1.85× claude-cli's total tokens (~15% higher per *resolve*) — a real solve-rate win at a
  modest premium, still default-off (`docs/benchmarks/results.md` updated with the numbers).

## [0.4.35] - 2026-06-26

### Added
- **`mesh.verify_completeness` — opt-in "max-resolve" mode that beats the raw CLI on resolve rate.**
  When on, the CLI-bridge harness preamble gains a completeness clause: re-read the request and verify
  the change against EVERY requirement before finishing. On SWE-bench Lite (N=10, same model, only the
  clause changed) this took Forge-on-bridge from **4/10 → 6/10 resolved — beating claude-cli's 4/10** —
  by catching under-scoped fixes (e.g. a flask change that handled the blueprint-name dot but missed
  the endpoint dot). The cost is honest: **~3× the tokens** (more re-reading + re-verification), so it
  is **default OFF** — a deliberate quality-for-cost trade you turn on when solve rate matters more than
  spend. Documented with the measured numbers in `docs/benchmarks/results.md`
  (`crates/forge-config`, `crates/forge-provider/src/cli_provider.rs`).

## [0.4.34] - 2026-06-26

### Added
- **`lattice.inject_body_hits` — tunable context front-loading.** Controls how many top-ranked symbol
  *bodies* get injected into a turn's prompt (default 3; previously hardcoded). Raising it front-loads
  more task-relevant code so the model reads from context instead of `search`/`read_file`-ing for it.
  Measured on SWE-bench Lite: aggressive front-loading (14 bodies) took Forge-on-bridge from ~1.9×
  *worse* tokens than the raw `claude` CLI to **rough parity** (tied 4/10 resolve, ~equal tokens, at
  N=10). Honest caveat documented in `docs/benchmarks/results.md`: on a 3-instance light-repo subset it
  looked like a 44% token win that did **not** generalize — small-N benchmarks mislead.

### Fixed
- **`forge bench swe` now bounds the in-process Forge agent by `--timeout-secs`.** Only the external
  CLI path was bounded; the Forge path ran `run_turn` unwrapped, so a non-converging run could spin for
  20+ minutes (observed 500+ tool calls). It now times out like the external agents and submits the
  partial patch (`crates/forge-cli/src/bench.rs`).

## [0.4.33] - 2026-06-26

### Fixed
- **Bridge token accounting dropped cache-read/cache-write tokens — fixed (and corrected a false
  "bridge is more efficient" claim).** The CLI bridge recorded only claude/codex's *uncached*
  `input_tokens`, discarding `cache_read_input_tokens` + `cache_creation_input_tokens`. Forge's
  `Usage.input_tokens` is defined as the *full* input the model processed (cached is a subset), so
  this undercounted input **everywhere** — the statusline token gauge, cost display, and especially
  the SWE-bench efficiency comparison, where the raw-CLI metric *did* count cache reads. That made
  Forge-through-the-bridge look ~10–150× cheaper than the raw CLI purely as a counting artifact. Now
  `cli_provider::usage_from` sums uncached + cache-read + cache-write, and `bench::parse_external_usage`
  counts cache writes too, so both sides are apples-to-apples. A fair re-measure shows the *opposite*
  of the old claim — Forge's MCP-per-turn harness currently costs **more** tokens than the native CLI
  loop; `docs/benchmarks/results.md` is corrected accordingly with the real numbers
  (`crates/forge-provider/src/cli_provider.rs`, `crates/forge-cli/src/bench.rs`).

## [0.4.32] - 2026-06-26

### Fixed
- **Watcher setup no longer leaks a thread (and watcher) per `build_session`.** 0.4.31's fire-and-
  forget watcher parked a thread holding the handle for the *process* lifetime — fine for `forge
  chat` (one session per process) but a leak when `build_session` runs repeatedly in one process:
  `forge bench swe` builds a session **per instance** (hundreds in a run), and `forge replay` too, so
  each leaked a parked thread plus the watcher's own background thread + inotify/poll resources. The
  watcher is now delivered to the session through an `mpsc` channel and **owned by the session** (the
  held `Receiver` keeps it alive; it's dropped when the session ends), and the setup thread exits
  after sending. Still fully non-blocking — no filesystem op gates startup. A test proves a watcher
  held only through the channel (never received) still reindexes (`crates/forge-core/src/lib.rs`,
  `crates/forge-cli/src/cli/commands/run.rs`, `crates/forge-index/src/watch.rs`).

## [0.4.31] - 2026-06-26

### Changed
- **File watching now WORKS on WSL2 `/mnt/*` (9p) and other remote filesystems — via polling instead
  of being disabled.** 0.4.27 stopped the hang by *skipping* the watcher on a non-native filesystem
  (with a "move the project onto the Linux filesystem" caveat). Now, instead of disabling, the watcher
  transparently switches to a **polling backend** on those filesystems (`9p`/`v9fs`, `fuse*`,
  `cifs`/`smb*`, `nfs*`): it stat-walks the project tree on a 2s timer (ordinary file ops that work
  over 9p) rather than registering recursive inotify watches (the per-entry RPCs that block
  uninterruptibly on 9p). So auto-reindex works on a Windows-drive project with **no caveat and no
  manual `forge lattice update`**. Content-comparison is on so even same-size edits are caught. Native
  filesystems keep the efficient inotify backend unchanged.
- **Watcher setup is now fully fire-and-forget**, so neither inotify registration nor the polling
  backend's synchronous initial tree scan (slow over a remote link) can gate TUI startup at all — a
  detached thread owns the watcher for the process lifetime. Removes the previous 5s setup deadline.
  The polling backend is unit-tested end-to-end (an external edit is reindexed) alongside the inotify
  one (`crates/forge-index/src/watch.rs`, `crates/forge-cli/src/cli/commands/run.rs`).

## [0.4.30] - 2026-06-26

### Changed
- **MCP servers now connect in the background instead of gating `forge chat` startup.** The interactive
  path awaited `McpManager::connect_all` before the session was built, so a slow or unreachable MCP
  server stalled TUI startup by up to the per-server connect timeout (20s default) — the same
  "a startup op blocks the UI" class as the 9p watcher hang. It now uses the non-blocking pattern
  `mcp-serve` already uses: `connecting()` marks every active server `Reconnecting` and advertises the
  MCP meta-tools immediately (so the tool surface is ready at once), then a detached task connects them
  — each flips to connected/failed in the `/mcp` panel as it resolves, and the first `mcp_call` lazily
  waits on its own server. Reuses infrastructure already covered by
  `connecting_advertises_meta_tools_without_any_network_io` (`crates/forge-cli/src/cli/commands/run.rs`).

## [0.4.29] - 2026-06-26

### Changed
- **The watch-&-reindex watcher now scopes to the project root and refuses to watch all of `$HOME`.**
  Follow-up to 0.4.27's WSL fix: the recursive watch was rooted at the raw CWD, so launching `forge
  chat` in a home directory tried to recursively `inotify` the entire `$HOME` tree — `.cargo`, cloned
  `.git` repos, caches — thousands of watches and a slow initial walk even on a native filesystem
  (the original bug report watched the user's whole home dir, pulling in `.cargo/.git`). Startup now
  resolves the watch root to the nearest enclosing project marker (`.git`/`.forge`/`AGENTS.md`,
  climbing no higher than `$HOME`) and, when that resolves to the home directory itself, skips the
  watcher with a clear line ("launched in the home directory with no project root — open a project
  folder…") instead of watching everything. A marker-less subdirectory still watches just that dir.
  Pure resolver unit-tested (project-root climb, home refusal, dotfiles-`.git`-in-`$HOME`, fail-open
  when `$HOME` is unknown) (`crates/forge-index/src/watch.rs`, `crates/forge-cli/src/cli/commands/run.rs`).

## [0.4.28] - 2026-06-26

### Security
- **Built-in secret-read denylist now covers the common non-`cat` read/exfil verbs.** The shell deny
  rules blocked `cat`/`less`/`head`/`tail`/`type`/`more` on `.env`/keys/credentials, but an agent
  could still read a secret by shelling out via the OTHER obvious verbs: text tools
  (`grep`/`egrep`/`rg`/`awk`/`sed`/`nl`/`sort`/`cut`), binary dumps/encoders for exfil
  (`xxd`/`od`/`strings`/`base64`), or `source`/`.` which executes a dotenv straight into the
  environment. Those are now denied too (defense-in-depth on top of the `read_file`/`list_dir` tool
  block, which already stops the non-shell path). Verified each is denied even under `bypass` while
  ordinary uses of those verbs on non-secret files (`grep TODO src/main.rs`, `base64 logo.png`, …)
  stay allowed (`crates/forge-config/src/lib.rs`, test in `crates/forge-core/src/permission.rs`).

## [0.4.27] - 2026-06-26

### Fixed
- **`forge chat` no longer hangs on a blank screen in a WSL2 `/mnt/*` (9p/DrvFs) directory.** At
  startup Forge sets up a recursive inotify watcher (watch-&-reindex) rooted at the working dir, and
  the registration was on the TUI-init critical path. On a 9p mount (WSL2's Windows-drive DrvFs) the
  recursive watch issues a per-entry RPC to the Windows host for the whole tree, some of which block
  uninterruptibly (`D` state in `p9_client_rpc`) — so the TUI never rendered and the process hung
  until Ctrl-C. `forge run` (no watcher) and projects on the Linux filesystem (ext4 `~`) were fine.
  Two fixes: **(1)** the watcher now detects a non-native filesystem via `/proc/self/mountinfo`
  (`9p`/`v9fs`, `fuse*`, `cifs`/`smb*`, `nfs*`) and skips the recursive watch with one clear line
  ("working dir is on a Windows drive (9p/DrvFs) — file watching disabled … move the project onto the
  Linux filesystem") instead of blocking — so it works for ALL WSL `/mnt/*` paths, not just the
  reported one; **(2)** watcher setup now runs on a detached thread with a 5s deadline, so *no*
  filesystem stall (remote mount, locked dir) can ever gate TUI startup again. Retrieval is
  unaffected when the watcher is skipped — re-run `forge lattice update` to reindex after edits
  (`crates/forge-index/src/watch.rs`, `crates/forge-cli/src/cli/commands/run.rs`).

## [0.4.26] - 2026-06-26

### Added
- **Property test locking the permission broker's security invariants.** `permission::decide` is the
  single chokepoint that gates every dangerous tool (shell, `.env`/secret reads, untrusted MCP), and
  its layered ordering is easy to break in a refactor. A deterministic seeded-LCG property test runs
  5000 random combinations of mode × side-effect × tool × args × rule-set and asserts the three
  guarantees that must never regress: (1) ANY matching Deny rule wins over any allow, in every mode;
  (2) a Builtin Deny holds even under `Bypass` (the unoverridable `.env`/secret floor); (3) with no
  matching deny, `Plan` mode denies every non-ReadOnly side effect (the hard read-only contract no
  allow rule can escape). Confirms no bypass hole exists today and guards the boundary going forward
  (`crates/forge-core/src/permission.rs`).

## [0.4.25] - 2026-06-26

### Added
- **Deterministic fuzz for `clamp_to_chars` (prompt-cap boundary contract).** The function that trims
  an over-long bridge prompt to `codex exec`'s `input_too_large` cap does raw char-index arithmetic on
  a `Vec<char>` — the exact shape that produced char-boundary panics before (v0.3.10) — and carries a
  hard contract: the result must never EXCEED `max_chars` (codex rejects the turn otherwise) and must
  stay valid UTF-8. A seeded-LCG fuzz throws 6000 random multi-byte/emoji/combining-char strings at
  random caps (biased toward the degenerate 0/1/around-marker-length region where boundary bugs live)
  and asserts: no panic, result char-count ≤ cap, and an already-fitting prompt returned unchanged.
  Completes the P0.1 fuzz triad with 0.4.23/0.4.24 (`crates/forge-provider/src/cli_provider.rs`).

## [0.4.24] - 2026-06-26

### Added
- **Deterministic adversarial fuzz for the bridge stdout parsers.** Every bridge turn streams the CLI
  subprocess's stdout line-by-line through `parse_line` (claude/codex/antigravity) and, in harness
  mode, `parse_sink_line` — UNTRUSTED input that drifts with each CLI version, where a panic crashes
  the turn mid-stream (worse than a clean failure: partial/inconsistent state). A seeded-LCG fuzz test
  throws 6000 pathological JSON-event lines (truncated/unbalanced JSON, wrong-typed fields, real event
  `type`s with missing payloads, control chars, huge repeats, unicode) at all three bridge parsers +
  the sink parser and asserts no panic + determinism on every one. No new dependency; identical corpus
  on every CI box (pairs with 0.4.23's tool-recovery fuzz) (`crates/forge-provider/src/cli_provider.rs`).

## [0.4.23] - 2026-06-26

### Added
- **Deterministic adversarial fuzz for tool-call recovery (untrusted-input hardening).**
  `recover_text_tool_calls` parses UNTRUSTED model output, and a panic there crashes the whole turn
  (it can't even fail over — the worst failure mode). A new seeded-LCG fuzz test throws 5000
  pathological strings (unbalanced braces, truncated JSON, real tool-call markers spliced mid-prose,
  control chars, deep nesting, huge repeats, unicode) at both recovery entry points and asserts: no
  panic, every recovered call has a non-empty name (no silently-undispatchable phantom call), and
  determinism. No new dependency; the corpus is identical on every CI box (P0.1 "fuzz tool-recovery")
  (`crates/forge-provider/src/tool_recovery.rs`).

## [0.4.22] - 2026-06-26

### Changed
- **`forge doctor` runs its live probes concurrently.** The two LIVE diagnostics each looped
  sequentially: `provider_reachability_checks` awaited each keyed provider's `list_models` one at a
  time (N × 8s) and `bridge_roundtrip_checks` launched each CLI bridge one at a time (3 × 30s ≈ 90s
  worst case). Both now `join_all` their independent probes — doctor pays the *slowest single* probe
  per section instead of the sum — so a multi-provider, multi-bridge environment finishes in seconds
  instead of minutes. Same checks, same severities, same stable output order (`forge-cli/doctor.rs`).

## [0.4.21] - 2026-06-26

### Changed
- **Model discovery probes every provider concurrently.** Startup queried each keyed provider's
  model list in a sequential loop, paying the *sum* of per-provider timeouts (3 keyed providers ×
  8s ≈ 24s worst case on a slow/cold network). It now `join_all`s the probes — startup pays the
  *slowest single* provider's budget (~8s). Same logging, same deterministic catalog order; the
  per-provider logic moved into a `discover_provider_models` helper.

## [0.4.20] - 2026-06-26

### Added
- **`docs/benchmarks/results.md`: recorded the verified prediction pipeline + a single-task token
  smoke.** Both `forge bench swe` agent paths (`--agent forge`, `--agent claude-code`) are verified
  end-to-end on a real instance. A single trivial task on the same model produced 793 tokens via the
  Forge bridge vs 46,124 via Claude Code's own CLI — documented with explicit caveats (overhead-
  dominated on a trivial task, NOT a resolve-rate result; the representative figure needs the full
  Docker-scored run). Confirms the comparison harness works and the direction of the efficiency thesis.

## [0.4.19] - 2026-06-26

### Fixed
- **SWE-bench predictions now capture NEW files (the harness was undercounting Forge).** `forge bench
  swe` built the `model_patch` with a plain `git diff`, which **ignores untracked files** — so a
  solution that *adds* a file (very common in SWE-bench: new modules, regression tests) produced an
  **empty patch** and was scored *unresolved* even though the agent did the work. Now it stages
  everything (`git add -A` with the same excludes) then `git diff --cached`, capturing additions,
  modifications, and deletions alike. Found by running the prediction pipeline end-to-end on a real
  instance for the first time (the agent created the file; the captured patch was empty); fixed +
  regression-tested + re-run end-to-end (now an 8-line patch) (`crates/forge-cli/src/bench.rs`).

## [0.4.18] - 2026-06-26

### Added
- **`docs/benchmarks/results.md` — the measured "proven with metrics" record.** Consolidates the
  harness's evidence: the bridge-resume efficiency result (~92% fewer prompt bytes over a 6-re-drive
  turn, with the deterministic test + live e2e to reproduce), the harness conformance matrix (each
  reliability guard + how to run it), and where the Docker-gated SWE-bench resolve-rate run plugs in.
  Separates in-repo CI proofs from the external gold-standard run.

## [0.4.17] - 2026-06-26

### Fixed
- **Transcript text now wraps on terminal cell width, not char count (wide-glyph overflow).** The
  line wrappers (`wrap_lines`, `wrap_words`) counted characters per row, but a CJK ideograph / emoji
  is 2 cells — so a line of wide glyphs over-filled each row and the renderer overflowed or truncated
  it. Both now measure `unicode-width` cells and break a wide glyph to the next row instead of
  splitting it. Pairs with 0.4.16's selection fix: wide glyphs now both render AND select correctly
  (`crates/forge-tui/src/transcript.rs`, `crates/forge-tui/src/app.rs`).

## [0.4.16] - 2026-06-26

### Fixed
- **Mouse text-selection no longer drifts on wide glyphs (CJK / emoji).** A selection records the
  screen **cell** column, but the extractor sliced the line's `[char]` using that cell offset as a
  char index — and a CJK ideograph or emoji is 2 cells but 1 char, so every boundary shifted right by
  one per wide glyph before it (and could slice past the string). Added a width-aware
  `cell_to_char_index` (walks chars summing their `unicode-width`) and convert the selection bounds
  before slicing, so the copied text matches what was highlighted (`crates/forge-tui/src/app.rs`).

## [0.4.15] - 2026-06-26

### Added
- **Test coverage for the `forge bench report` comparison logic.** The headline "Forge bridging model
  X vs X's own CLI" report (#227) had no test for its arithmetic or its honesty gates. Extracted the
  aggregation into pure `summarize_agent()` + `tok_per_success_cell()` and unit-tested them: an
  instance counts as resolved only if the official scorer put its id in `resolved_ids`; token totals
  include **only** `metrics_complete` rows (a partial capture can't understate tokens-per-success and
  flatter Forge); and tokens-per-success prints a real number **only** with eval results + ≥1 resolved
  + complete capture, else `incomplete`/`n/a`. Locks down the integrity of the proven-with-metrics
  comparison (`crates/forge-cli/src/bench.rs`).

## [0.4.14] - 2026-06-26

### Fixed
- **More HTTPS-on-a-CA-less-host panics swept.** Following 0.4.13 (forge-mcp), the remaining
  `reqwest::Client::new()`/`builder()` sites that trust the OS cert store and panic at construction
  on a bare container / minimal image are fixed: the ollama embedder (`forge-index/embed.rs` — it
  panics even though ollama is plain HTTP, since the panic is in TLS-backend setup, not at connect)
  and the three `web_fetch`/`web_search` clients (`forge-tools/web.rs`). Each crate gets a local
  bundled-CA helper seeded with `webpki-root-certs` (mirrors forge-provider/forge-mcp, which they
  can't depend on). Forge now does HTTPS everywhere without a system trust store.

## [0.4.13] - 2026-06-26

### Fixed
- **MCP HTTPS no longer panics on a CA-less host.** `forge-mcp` built its OAuth-refresh client with
  `reqwest::Client::new()` and its streamable-http transport client with a plain
  `reqwest::Client::builder()` — both trust the OS certificate store and **panic internally** on a
  bare container / minimal image that has none (the same landmine fixed for the API client in
  0.4.1/#226, which `forge-mcp` didn't share because it can't depend on `forge-provider`). Added a
  `bundled_client_builder()` seeded with Mozilla's `webpki-root-certs` and routed both the OAuth flow
  and the transport through it, so connecting to a remote (HTTP/SSE) MCP server works without a system
  trust store (`crates/forge-mcp/src/transport.rs`, `crates/forge-mcp/src/oauth.rs`).

## [0.4.12] - 2026-06-26

### Fixed
- **No more misleading "model discovery failed — check your key" warning for completion-only
  providers (Cerebras).** Cerebras has no native genai adapter, so `list_models()` can't enumerate
  it and auto-discovery logged a scary keyed-provider WARN claiming its models "won't be routable
  (check the key / network)". But Cerebras *completion* works fine via the custom service-target
  resolver — it's just config-only (no model-listing API). Added `forge_provider::is_discoverable`
  and the discovery loop now skips such providers quietly with accurate guidance (pin
  `cerebras::<model>` or add it under `[mesh.models]`) instead of the alarming, inaccurate warning.
  Verified live (`crates/forge-provider/src/genai_provider.rs`, `crates/forge-cli/src/cli/commands/models.rs`).

## [0.4.11] - 2026-06-26

### Added
- **A reproducible metric for the bridge-resume efficiency win.** claude's own token accounting hides
  the saving (it prompt-caches the repeated transcript), so a deterministic test
  (`resume_sends_dramatically_fewer_prompt_bytes_over_a_turn`) measures what Forge actually controls:
  the prompt bytes streamed to the CLI's stdin across a multi-re-drive turn. Result: **~92% fewer
  prompt bytes** (a 4000-char system preamble + accumulating assistant/tool turns over 6 re-drives:
  ~59.7 KB sent without resume vs ~4.2 KB with). This is the concrete "proven with metrics" backing
  for the 0.4.9/0.4.10 claude + codex session resume (`crates/forge-provider/src/cli_provider.rs`).

## [0.4.10] - 2026-06-26

Bridge efficiency, part 2: codex session resume.

### Added
- **codex bridge session resume** — extends the 0.4.9 claude resume to the codex (ChatGPT) bridge, so
  both major subscription bridges now send only the NEW messages per turn instead of re-streaming the
  whole transcript. codex resumes via the `exec resume <id>` SUBCOMMAND (keyed by its `thread_id`),
  which — unlike claude's `--resume` flag — **rejects `--sandbox`** on resume (the recorded session's
  sandbox is inherited), so the codex arg path is rewritten `exec …` → `exec resume <id> …` with the
  sandbox pair dropped and every other harness flag kept. The model-match gate prevents codex's
  model-change warning. Verified with unit tests (the `exec resume` rewrite + `--sandbox` drop) and an
  `#[ignore]` live e2e (`e2e_codex_resume_preserves_context_across_calls`) that drives two real `codex`
  turns and asserts the resumed turn recalls a fact from turn 1 (`crates/forge-provider/src/cli_provider.rs`).

## [0.4.9] - 2026-06-26

Bridge efficiency: claude session resume (the bridge-superiority lever).

### Added
- **CLI-bridge session resume (claude `--resume`) — Forge through the bridge now sends only the NEW
  messages per turn instead of re-rendering the whole transcript every call.** A bridge `complete()`
  is a fresh process each time and used to re-stream the entire conversation; for a multi-step turn
  that re-drives several times, that re-sends the full history on every call — the biggest source of
  bridge token waste. Now Forge captures claude's own `session_id` from its `stream-json` output and,
  on the next call, spawns `claude -p --resume <id>` streaming only the delta (a `continue` nudge or
  the new user turn). claude reloads the prior context from its own session store — fewer tokens in
  *and* a prompt-cache hit on its side. Safety: a transcript shrink (compaction) or model change
  (failover) forces a fresh session, and any failed resumed turn optimistically resets so the retry
  is fresh; claude-only (`with_session_resume(false)` is the escape hatch); codex/agy unchanged.
  Verified with unit tests + an `#[ignore]` live e2e that drives two real `claude` turns and asserts
  the resumed turn recalls a fact from turn 1 while only the delta was sent
  (`crates/forge-provider/src/cli_provider.rs`).

## [0.4.8] - 2026-06-26

More harness conformance tests — empty-response and tool-call-as-text guards.

### Added
- **End-to-end conformance tests for two more reliability guards:** the empty-response
  nudge-then-stop guard (an `EmptyResponseProvider` that returns nothing must be nudged a bounded
  number of times, then stop — never loop forever) and the tool-call-as-text honest-failure guard (a
  `ToolCallAsTextProvider` that writes `<invoke …>` markup with no structured call must be nudged to
  actually execute, then end LOUDLY rather than report a phantom success). With 0.4.6/0.4.7 this
  brings end-to-end coverage to the full guard set — verification gate (direct + bridge), doom-loop,
  failure-loop, empty-response, continue/stall, and tool-call-as-text (`crates/forge-core/src/lib.rs`).

## [0.4.7] - 2026-06-26

Harness conformance tests for the runaway-prevention guards.

### Added
- **End-to-end conformance tests for the doom-loop and failure-loop guards** — the two quota-critical
  runaway guards previously had no loop-level test. A `DoomLoopProvider` (same call every step) proves
  the identical-call guard halts loudly; a `FailureLoopProvider` (a unique non-existent path each call
  → same `NotFound` kind, differing signatures) proves the failure-by-kind guard halts where the
  doom-loop can't see it. Both assert the turn actually STOPS rather than burning the step budget
  (`crates/forge-core/src/lib.rs`). Part of the "prove the harness with deterministic mock-provider
  tests" effort that already surfaced the 0.4.6 direct-gate bug.

## [0.4.6] - 2026-06-26

Make the direct-path completion-verification gate actually work.

### Fixed
- **The direct-API completion-verification gate now genuinely detects whether the model inspected real state**, where before it could not. Two root causes: (1) `inspect_ran`/`tools_ran` were only incremented from the provider's `ToolStarted` *stream events* — which the CLI bridge emits (its tool loop runs inside one `complete()`) but a direct genai provider does **not** (it returns tool calls in the response, executed separately), so the counters were always 0 on the direct path; (2) the gate read a step-local "inspected this step?" signal, but a direct model runs tools in steps *separate* from the text "done" claim, so that signal was always false at the gate. Result: a direct model that correctly verified could be wrongly flagged `UNVERIFIED`. Now the loop counts the tools a direct model runs, and the gate measures inspection **since the verification was requested**. Backed by two end-to-end conformance tests (inspect-during-verification → accepted; never-inspect → flagged `UNVERIFIED`) (`crates/forge-core/src/lib.rs`).

## [0.4.5] - 2026-06-26

Harness reliability + import/export round-trip.

### Added
- **`forge skill export <dir>` and `forge skill import <dir>`** — a portable bundle round-trip for your commands/skills/agents (the inverse of `forge import`), so a library can be moved to another machine or shared. Reuses the import copy machinery; idempotent (existing files kept) (`crates/forge-cli/src/cli/commands/skill.rs`).
- **Structured hook directive protocol** — a `PreToolUse`/`PostToolUse` hook can emit `{action: rewrite|inject|block|allow}` on stdout. `inject` feeds model-visible context (lint output, "this file is generated") via `pending_hints` — the first way a hook can *teach* the model, not just gate it. Back-compatible with the bare-object rewrite (`crates/forge-core/src/hooks.rs`).
- **Per-provider subagent fan-out cap** — `[mesh.subagents] max_per_provider` (default 2): each child also acquires a per-provider permit, so a burst routed to one subscription/key can't drain a single quota in parallel (`crates/forge-core/src/subagent.rs`).

### Changed
- **The objective completion-verification gate now guards the direct-API path too**, not just the CLI bridge — extracted into one shared `completion_gate` authority. A direct model that marks every task Done without a real tool-grounded state check is gated identically to a bridge (`crates/forge-core/src/lib.rs`).

### Fixed
- **Command namespace is preserved when copying** (import/export): a subdir command `git/commit.md` (name `git:commit`) no longer flattens to `commit.md`, which dropped the namespace and silently clobbered same-named commands from different namespaces (`crates/forge-cli/src/cli/commands/import.rs`).

## [0.4.4] - 2026-06-25

Patch release: mesh routing now factors tool-call reliability to avoid auto-selecting tool-leaky models.

### Fixed
- **Mesh routing now factors tool-call reliability: a `capability::tool_reliability_penalty` demotes models that emit tool calls as TEXT instead of structured calls (the Gemini flash family) below comparable tool-reliable peers**, so a high-bench-but-tool-leaky model is no longer auto-selected #1 for a tool-driven turn; it stays in the fallback chain. Pairs with the tool_recovery `<function=NAME>` salvage from v0.4.3 (`crates/forge-mesh/src/capability.rs`, `catalog.rs`).

## [0.4.3] - 2026-06-25

Patch release: tool_recovery now handles the Llama/Groq `<function=NAME>{json}</function>` format.

### Fixed
- **tool_recovery now recovers the Llama/Groq `<function=NAME>{json}</function>` tool-call format** (bare and wrapped in `<tool_call>`), in addition to `<invoke>` and `<tool_call>{json}`; a groq llama-3.x model routed by the mesh had leaked this as text and stalled the turn. Normalizes `mcp__forge__`/`default_api:` prefixes and recovers even an empty body so a degenerate call can't be mistaken for a final answer (`crates/forge-provider/src/tool_recovery.rs`).

## [0.4.2] - 2026-06-25

Patch release: bounded poll mode for the shell tool so the model can wait out long external jobs.

### Added
- **`shell` tool: `poll_until_exit_zero` + `poll_interval_secs` — wait for long external jobs without blocking.** A single blocking `gh run watch` is killed at the ~120s shell cap, making it impossible to wait out a multi-minute CI run or release build. The new poll mode re-runs `command` every `poll_interval_secs` (default 5, max 60) until it exits 0 or the per-call budget elapses (capped at 100s, well under the request cap). On budget exhaustion it returns a resumable "call again to keep waiting" result instead of a killed timeout, so the model waits by calling again rather than guessing. PTY mode is incompatible and rejected. The bridge preamble is updated to use this instead of a single blocking `gh run watch` (`crates/forge-tools/src/shell.rs`, `crates/forge-provider/src/cli_provider.rs`).

## [0.4.1] - 2026-06-25

Patch release: bridge-completion reliability, HTTPS on bare systems, and a security fix for
per-tool MCP permission rules.

### Security
- **`mcp_call` now enforces per-tool permission rules on the direct path.** On the
  `forge-core` direct path, `mcp_call` forwarded calls to `invoke_mcp` after checking only
  the outer wrapper name (`"mcp_call"`). A configured allow/ask/deny rule targeting the
  actual inner tool (e.g. `deny = ["myserver__dangerous"]`) was silently ignored because
  the broker never saw the inner tool name. Fix: after the outer wrapper passes, extract the
  inner tool name and arguments from the `mcp_call` arguments and run a second
  `permission::decide` against the real tool identity — deny or ask outcomes block execution
  (`crates/forge-core/src/lib.rs`).
- **`rule_matches` now fires on `deny = "*"` for MCP tool names.** For non-shell/non-path
  tools (all MCP server tools), a wildcard pattern `"*"` was matching zero args instead of
  any args, so `deny = "*"` on an MCP tool name never fired. Without this fix the per-tool
  gate above was unusable from config even after the `invoke_mcp` fix
  (`crates/forge-core/src/permission.rs`).

### Fixed
- **A bridge turn can no longer end with the plan half-done or falsely "done."** A CLI bridge
  (claude-cli/codex) is a one-shot subprocess that runs its own loop and exits, so a long plan
  could stop partway — the bridge did a few steps (merged + tagged), exited after launching the
  async release build, and the dependent steps (brew sha, verify) never ran; forge accepted the
  exit as "done." Completion is now defined by the TASK LIST and verified, not by the subprocess
  exiting (`crates/forge-core/src/lib.rs`):
  - **Task-driven re-drive.** While tracked tasks remain unfinished, forge re-invokes the bridge to
    continue (a clean new process — what the user typing `continue` does). Bounded.
  - **Progress gate (anti-spiral).** A re-run must make real progress — start a tool or close a
    task — or the turn HALTS loudly instead of re-driving. This is the guard the earlier
    bridge-nudge lacked: a bridge that just re-narrates can't loop.
  - **Objective verification gate.** When the bridge reports every task Done, forge forces a
    tool-grounded verification turn ("prove each task is actually complete by checking real state —
    git, gh, files — and reopen anything that isn't") before the turn can end. The verification must
    run a real INSPECTION tool (not just re-mark `update_tasks`); if the model never inspects, forge
    re-prompts then ends flagged UNVERIFIED rather than reporting success. Self-reported "done" is
    never trusted on its own — which is what produced the phantom release.
  - The bridge preamble now mandates completing the whole task and WAITING for any async job it
    launches (a release build, CI) rather than treating "launched" as "done"
    (`crates/forge-provider/src/cli_provider.rs`).
  - **Invariant:** forge never reports a phantom success — incomplete work is completed+verified,
    re-driven, halted loudly, or flagged UNVERIFIED. Documented in `docs/harness/bridge-completion.md`
    with the end-to-end test method (`scripts/bridge-e2e.sh` drives real subscription bridges and
    asserts on the real filesystem + run log).
- **HTTPS no longer requires a system trust store.** `build_reqwest_client()` now seeds
  Mozilla's bundled `webpki-root-certs` as the sole trust store, bypassing
  `rustls-platform-verifier`. Forge works on bare containers and minimal CI images with no
  `ca-certificates` package installed. Applied to both the API client and model auto-discovery
  (`crates/forge-provider/src/genai_provider.rs`).
- **Failover now follows the mesh ranking exactly.** When a model failed, Forge advanced to the
  *next-ranked* model — except the failover chain was secretly re-ordered by a provider
  round-robin (`interleave_by_provider`), so the second model tried was the top model of the
  *second provider*, not the second-best-ranked model overall. The chain is now in strict rank
  order. Storm protection is preserved differently: only a rate-limit (429) lazily skips that
  provider's remaining chain entries; every other failure keeps strict rank order
  (`crates/forge-mesh/src/lib.rs`, `crates/forge-core/src/lib.rs`).
- **A model that writes a tool call as *text* no longer "succeeds" without doing anything.**
  Some providers' native adapters (notably genai's Gemini adapter) don't decode function calls
  into structured tool calls — the call leaks into the assistant's text as `<invoke …>` /
  `default_api:` markup and never executes. Two defenses: (1) a **text tool-call recovery
  pass** reconstructs and executes the call from the markup
  (`crates/forge-provider/src/tool_recovery.rs`); (2) an **honest-failure guard** detects
  un-executed tool-call text, nudges the model to call the tool, and — if it persists — fails
  loudly instead of silently accepting the narration (`crates/forge-core/src/lib.rs`).
- **"database is locked" under concurrent Forge processes.** The SQLite store set WAL mode
  but no `busy_timeout`, so a second Forge process (TUI + `mcp-serve` bridge sharing one
  global db) hit `SQLITE_BUSY` immediately. The connection now waits up to 5 s for the write
  lock (`crates/forge-store/src/lib.rs`).

## [0.4.0] - 2026-06-24

Reliability release: every fix here came out of Forge attempting (and botching) its own release,
which exposed how a routed turn could silently do nothing yet report success.

### Fixed
- **Failover now follows the mesh ranking exactly.** When a model failed, Forge advanced to the
  *next-ranked* model — except the failover chain was secretly re-ordered by a provider round-robin
  (`interleave_by_provider`), so the second model tried was the top model of the *second provider*,
  not the second-best-ranked model overall. That is how release-critical turns ended up on a
  low-ranked free model after a higher-ranked provider's first model failed over. The chain is now
  in strict rank order. Storm protection is preserved differently: only when a model fails with a
  **rate limit** does Forge skip that provider's remaining chain entries (a 429 is usually
  provider-wide) — every other failure keeps strict rank order
  (`crates/forge-mesh/src/lib.rs`, `crates/forge-core/src/lib.rs`).
- **A model that writes a tool call as *text* no longer "succeeds" without doing anything.** Some
  providers' native adapters (notably genai's Gemini adapter on newer models) don't decode a
  model's function calls into structured tool calls — the call leaks into the assistant's text as
  `<invoke …>` / `default_api:` markup and never executes. Forge saw no tool calls, accepted the
  narration as the final answer, and reported success having merged no PR and pushed no tag. Two
  defenses now: (1) a **text tool-call recovery pass** reconstructs the call from the markup
  (`<invoke>`, `<tool_call>` JSON; `default_api:`/`mcp__forge__` namespaces normalized) and executes
  it (`crates/forge-provider/src/tool_recovery.rs`); (2) an **honest-failure guard** detects
  un-executed tool-call text a direct model emits, nudges it to actually call the tool, and — if it
  persists — fails loudly instead of silently accepting the narration (`crates/forge-core/src/lib.rs`).
- **"database is locked" under concurrent Forge processes.** The SQLite store set WAL mode but no
  `busy_timeout`, so a second Forge process (the TUI plus the `mcp-serve` bridge sharing one global
  db) hit `SQLITE_BUSY` immediately and could crash a turn mid-run. The connection now waits up to
  5s for the write lock (`crates/forge-store/src/lib.rs`).

### Changed
- Workspace version and internal dependency constraints bumped to `0.4.0`; the Homebrew formula is
  bumped in lockstep (its sha256 values are filled from `checksums.txt` after the release build).
- Added `RELEASING.md` — a fixed cut-a-release checklist (the missed Homebrew-version bump that
  shipped stale `brew` installs was a recurring symptom of having no written process).

## [0.3.10] - 2026-06-24

### Fixed
- **Submitting a prompt with multi-byte whitespace no longer panics the turn.** Pasted text often
  carries a non-breaking space (`U+00A0`) or other multi-byte Unicode whitespace. The `@file`
  expansion scanned the prompt byte-by-byte, cast each byte to a `char`, and sliced the string on
  the result — which lands mid-character for any multi-byte whitespace and panicked with
  `end byte index … is not a char boundary` (`crates/forge-cli/src/cli/commands/run.rs`), crashing
  the whole session. It now splits on Unicode whitespace (`split_whitespace`), which is UTF-8-correct.

## [0.3.9] - 2026-06-24

### Fixed
- **`/copy` now actually copies on Wayland (and over SSH).** It reported "✓ copied" but the
  clipboard stayed empty: `arboard`'s Wayland backend silently no-ops from a terminal app (it needs
  an owned window/surface a TUI doesn't have). `/copy` (and mouse-selection copy) now also emit an
  **OSC 52** escape so the *terminal* sets the clipboard — reliable on Wayland, over SSH, and in
  Windows Terminal / kitty / iTerm with no display server (tmux/screen get the passthrough form).
  `arboard` is kept for X11 / macOS / Windows-native.

### Added
- **Cross-platform real-turn E2E you can run yourself, no VM.** `scripts/e2e-docker.sh` drives a
  real `forge run` turn across Ubuntu/Debian/Fedora containers against your host ollama (builds the
  *current* code in a glibc container so you test what you're editing), and reproduces the
  no-Secret-Service condition that hung `forge chat` on WSL — asserting startup stays bounded. A new
  `e2e` GitHub Actions workflow runs the same headless real turn + the probing `forge doctor` on
  **windows-latest** / ubuntu / macOS (manual or weekly) and uploads the logs, so cross-platform
  breakage is diagnosable without owning a Windows machine. (Set repo var `E2E_MODEL` + a provider
  secret, e.g. `GROQ_API_KEY`.)

## [0.3.8] - 2026-06-24

### Added
- **`/copy [N]` — copy an assistant response to the clipboard, with a code-block picker.** `/copy`
  copies the most recent assistant response; `/copy N` copies the Nth-latest (1-based from the most
  recent — `/copy 2` is the second-to-last). `/yank` is an alias. When the response contains fenced
  code blocks, `/copy` opens an interactive picker to copy the **full response** or any **individual
  block** (shown with its language + size); ↑↓ select, **Enter** copies to the clipboard, **`w`**
  writes the selection to a timestamped file in the cwd (useful over SSH, where the clipboard can't
  reach your local machine), **Esc** cancels. A response with no code blocks copies directly.

## [0.3.7] - 2026-06-24

### Changed
- **Failover shows one animated indicator instead of a per-hop warning wall.** When the mesh fails
  over between models (e.g. through rate-limited free models), it no longer prints a
  `{model} {reason} — failing over` line to the chat for every hop. Instead a single animated
  `⠋ finding a model` indicator appears in the status bar (the model being tried shows on the
  routing line) and clears the instant real output begins — so you see *that* it's searching and
  *what* it settled on, without the scrollback spam. A genuinely exhausted failover chain still
  surfaces a clear warning.

## [0.3.6] - 2026-06-24

### Fixed
- **Architect mode no longer dispatches a keyless `groq` for its planner/editor.** With
  `architect_mode` on (it's off by default — opt-in), the plan and edit phases resolved their model
  via the first configured tier candidate, and the built-in defaults lead with
  `groq::llama-3.3-70b-versatile`. On a box with no groq key every turn dispatched groq: the plan
  phase wasted a failover hop (`architect plan failing over to claude-cli::sonnet`) and the edit
  phase — which runs with mid-turn failover disabled — hard-failed (`turn failed: no API key
  configured for provider 'groq'`). The planner/editor now pick the first candidate whose provider
  has a key (keyless bridges / ollama qualify), so they land on e.g. `claude-cli` / `gemini`, never
  a keyless default. (A different path than the v0.3.5 last-resort leak — same symptom.)
- **Built-in default model lists no longer lead with `groq`.** Every tier's default candidate list
  started with `groq::llama-3.3-70b-versatile`, a free model that needs a key many users don't have
  — so any code path taking "the first candidate" landed on groq and failed. The defaults now lead
  with a keyless/bridge option (ollama / `claude-cli`) and keep groq last. Routing is unchanged
  (the cost-ranker picks the cheapest *usable* model regardless of list order); this only hardens
  the first-candidate fallback paths. A regression test asserts a config that omits `architect_mode`
  keeps it `false`.
- **`forge doctor` no longer false-warns about `TERM` on Windows.** `TERM` is a Unix concept and is
  normally unset on Windows (crossterm drives the console via the Console API regardless), so an
  interactive Windows console is simply OK. The "TUI may not render" warning now only fires on Unix,
  where an unset/`dumb` `TERM` is a real signal (e.g. WSL).

## [0.3.5] - 2026-06-24

Stability release: make Forge usable for new users on Windows / WSL / non-Arch boxes. Every fix
targets a "Forge is unusable and doesn't say why" failure reported from the field.

### Fixed
- **The mesh can no longer dispatch a provider you have no key for (the "groq for everything"
  churn).** Reported on Windows + WSL: the mesh kept trying `groq::llama-3.3-70b-versatile` despite
  no groq key — even with `--model` forced — and surfaced a raw `Resolver error`. Root cause was a
  keyless model reaching the wire via a path that wasn't key-filtered. Closed with three independent
  layers: (1) a genai "Resolver error" (adapter/auth couldn't be built — almost always a missing
  key) is now classified **permanent `Auth`** → the model is *excluded* and re-probed, not benched
  and retried forever; (2) the **last-resort fallback** (`soonest_unbenched`) now skips any provider
  with no key, so a benched keyless default can't become the pick; (3) a **pre-dispatch backstop**
  converts any keyless `active_model` into a permanent failure so failover advances to a usable
  model instead of dispatching and erroring. Keyless providers (ollama, the claude/codex bridges)
  are unaffected.
- **`forge chat` no longer hangs forever before the TUI opens (WSL).** On WSL / headless Linux an
  activatable-but-dead `org.freedesktop.secrets` made the OS-keyring call **block indefinitely** —
  and it's called per provider at startup, so the TUI never drew its first frame. The keyring is now
  probed once with an 800ms timeout and, if it doesn't answer, Forge uses its encrypted file store
  for the session (secrets stay durable). The setup menu worked because it doesn't hit that path.
- **All blocking startup steps are now time-boxed** — a command completes its preflight (or shows a
  clear error) before the TUI draws, instead of loading infinitely: model auto-discovery is capped
  at 15s (then falls back to built-in defaults + a warning), the `~/.claude` quota scan at 3s, and
  the background `claude --debug` quota probe at 20s.

### Changed
- **`forge doctor` now tests function, not just presence** — it reported "0 failures" on machines
  where Forge was unusable. It now runs two bounded live probes: a **CLI-bridge round-trip** (it
  actually launches the detected claude/codex bridge and confirms it answers — catching a bridge
  that's on PATH but can't launch, e.g. the Windows `cmd /S /C` shim case), and a **keyed-provider
  reachability** check (a `list_models` timeout means the provider won't route → the keyless-fallback
  cause). The terminal check now resolves the old `interactive (?)` (warns on an unusable `TERM`) and
  flags WSL explicitly.

## [0.3.4] - 2026-06-24

### Fixed
- **Billing safety: unpriced paid-provider models are no longer treated as "free".** A model from a
  metered API provider (OpenAI, xAI, DeepSeek, Anthropic, …) that Forge holds no bundled price for
  was classified `free` (unpriced → `$0`), so cost-aware routing would happily pick e.g.
  `gpt-5.5-pro`, `gpt-5-pro`, `o3`, or `gemini-3-pro` for a cheap tier — and **bill the user** —
  while reporting it as free. "Free" now requires positive evidence: genuinely-free providers
  (local `ollama`, free-tier `groq`/`cerebras`), an explicit `:free` variant, or a configured price
  of `0`. Everything else unpriced is **paid (unknown cost)**. **Gemini keeps its real free tier**:
  its Flash / Flash-Lite (and Gemma) models stay free, but Gemini Pro — paid-only since the free
  tier dropped it (Apr 2026) — is correctly paid. This also stops the trivial tier from routing to a
  paid model just because it had no price.
- **More non-chat models excluded from routing.** `is_routable` now also filters video (`sora`),
  realtime voice (`*realtime*`), speech-to-text (`*transcribe*`), and legacy base-completion models
  (`babbage`, `davinci`) — these leaked into the routable set from providers' full model lists (on
  top of the image/TTS/embedding/deep-research/moderation models already excluded). They remain
  visible in `forge models`, just never routed to.

## [0.3.3] - 2026-06-24

### Fixed
A multi-agent audit of the workspace surfaced these (verified) bugs, fixed here:

- **Windows: CLI bridges and MCP servers failed to launch when a path contained a space.** A `.cmd`
  shim was run as `cmd /C "<path>"`, but `cmd` strips the first/last quote of its `/C` string, so a
  quoted path broke the moment a second quoted token (an argument with a space — e.g. an
  `--mcp-config` path under `C:\Users\First Last\…`) appeared. Now launched via `cmd /S /C` with the
  whole command line individually quoted, so spaces in the path **and** in any argument survive.
  Applies to both the claude/codex bridges and stdio MCP servers. This could manifest as a bridge
  that's installed and detected yet keeps benching itself (and the mesh then routing elsewhere).
- **Bridge failures hid the CLI's own error.** A stalled or crashed bridge now includes the CLI's
  `stderr` tail in the error (so a benched bridge shows *why* it failed), and a prompt-to-stdin write
  failure is logged instead of silently dropped (which could leave the CLI waiting for EOF).
- **A configured provider whose model discovery failed vanished silently.** A keyed provider that
  errors or times out during discovery is now logged at `warn` (not `debug`), and keyed providers get
  a more forgiving discovery budget (8s vs 4s) so a slow/cold connection — e.g. OpenRouter's large
  model list — doesn't drop it and force a fallback to the built-in defaults.
- **`@path` completion was dead on Windows** outside a git repo: the fallback shelled out to Unix
  `find` (a different, incompatible tool on Windows). Replaced with a portable `std::fs` walk.
- **Architect mode reused the wrong failover chain.** When the editor model differed from the routed
  model, an editor-model failure failed over using the *routed* model's fallbacks; it now runs
  without cross-model failover (matching the self-review / autofix re-runs).
- **An empty keyring entry counted as an API key**, producing a cryptic provider 401 instead of the
  actionable "no key — run `forge auth`" message. `api_key()` now requires a non-empty value, like
  `has_api_key()`.
- **A server retry-after of the form `.5s` (leading decimal point) was ignored**, discarding the
  server's cooldown hint; the parser now accepts a leading dot.

## [0.3.2] - 2026-06-24

### Fixed
- The mesh no longer spins on a keyless provider (e.g. "keeps trying groq for everything" with no
  groq key). When routing can find no usable model — because auto-discovery came up empty, or the
  user's keys are for providers not in the built-in defaults — it previously fell back to the first
  default candidate (groq) anyway and called it, auth-failing every turn. The turn now stops with an
  actionable diagnostic instead: it names the keyless provider, lists the providers you *do* have a
  key for, and points at `forge auth` / `forge models` / `/model <id>` / `ollama serve`.
- OpenRouter is now recognised when its API key is set under the conventional `OPENROUTER_API_KEY`
  name, not only Forge/genai's canonical `OPEN_ROUTER_API_KEY`. A user who exported the standard
  name was silently treated as keyless, so the mesh skipped OpenRouter discovery and fell back to
  the groq defaults. Both names are accepted, and the conventional one is copied into the canonical
  variable the provider client authenticates with.

## [0.3.1] - 2026-06-24

### Fixed
- Failover churn on a bad/missing API key: an auth failure (HTTP 401/403) is now treated as
  **permanent** — the model is excluded from routing (24h + automatic re-probe) instead of being
  short-benched and re-tried at the top of *every* turn. A keyless or misconfigured provider no
  longer adds a failover hop + warning to each turn; it recovers automatically once the key is fixed.
- One-line turn recap no longer invents success on a stalled turn. A turn that gave up with no
  output (empty-response exhaustion / failover exhaustion) is no longer summarized — previously the
  trivial-tier summarizer leaned on the *request* and reported e.g. "Fixed the bug…" for a turn that
  did nothing. The recap prompt was also hardened to describe only what the response actually shows,
  and to say so plainly when a turn stalled, errored, or only planned.

### Changed
- The `curl … | sh` / `irm … | iex` installer can be re-run any time to update or reinstall on any
  platform **without touching your config, sessions, or API keys** — it only ever writes the binary
  and updates `PATH`. A re-run now detects the previous version and confirms your settings are
  preserved (`forge 0.3.1 -> … (was 0.3.0; your config and sessions are preserved)`).
  `scripts/test-installer-config-safe.sh` asserts a seeded config survives two installs.

## [0.3.0] - 2026-06-24

### Added
- Turn timer + token counter in the statusline (like Claude Code / Codex). While a turn runs, the
  spinner shows `⟳ working` and row 2 shows a single `⧖ 12s ↑in ↓out` segment for this turn — elapsed
  time ticking live, the per-turn token deltas filling in, both frozen at the final totals when the
  turn ends. The context gauge sits next, then the session running totals as `Σ ↑in ↓out` (clipped
  first if the row is narrow, so the gauge always stays visible).
- Mouse text selection in full-screen mode, no Shift needed. Forge now does its own click-drag
  selection (highlighted in place) and copies the text to the clipboard on release — so the wheel
  still scrolls AND plain drag selects, where kitty/most terminals otherwise force Shift+drag once an
  app reports the mouse. Drag-motion is only reported while a button is held, so there's no hover
  overhead. Disable all mouse reporting with `[tui] mouse_capture = false`.
- Floating "↓ Jump to bottom · Ctrl+End" bar in the full-screen transcript: appears only while
  scrolled up off the tail; click it or press Ctrl+End to jump back to the live tail and resume
  following.
- `scripts/tui-drive.sh` — drive `forge chat` in a real full-screen TUI inside tmux, send a
  scripted key sequence, and capture/assert on the rendered screen (the alt-screen grid, which a raw
  PTY byte-stream can't show). The `--mock` provider is now plan- and task-aware, so `/plan …`
  renders a real plan card + seeds the task panel and a task-tracking prompt renders the sticky task
  panel — reproducing the full-screen panels offline (no API/bridge). Sample scripts under
  `scripts/tui-scripts/`.
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
- The agent loop no longer ends a turn mid-task when a (often weaker/free) **direct** model narrates
  its next step without calling the tool, or signs off with tasks still open. If the tracked task
  list has unfinished items when such a model stops with plain text, Forge nudges it to continue (up
  to 4×) instead of accepting the stall as the final answer. A **CLI-bridge** turn (claude/codex) is
  exempt and treated as terminal — the bridge runs its own internal tool loop and returns the whole
  turn as one text response, so nudging it only re-runs the entire bridge in a confused state (it
  starts narrating tool calls as text and spirals). The doom-loop guard also fires a "change
  approach" nudge *before* it hard-stops, so a single repeated call no longer kills an otherwise-
  recoverable turn; it only halts if the model keeps repeating after the nudge.
- The CLI bridge (claude/codex) now gates on Forge's **live** temper, not the stale on-disk config
  mode — so switching Plan→Auto-edit (e.g. approving a plan) actually lets the bridged model write.
  `forge mcp-serve` runs as a fresh process per turn and loaded `permission_mode` from the config
  file, blind to runtime switches: after the user moved to Auto-edit, the bridge still denied
  `mcp__forge__write_file` with "denied by Forge permission policy", which the model surfaced as "I
  have no permissions" even though the UI said Auto-edit. The parent now exports its current temper
  (`FORGE_PERMISSION_MODE`) into the per-turn checkpoint env and forwards it to the bridge child,
  which honors it over the config. Verified live (codex 0.141): `plan` denies the write, `accept-edits`
  performs it. This also closes the opposite hole — writes are no longer silently allowed *during*
  Plan mode when the config happened to be `accept-edits`.
- Statusline no longer shows a duplicate turn timer (the elapsed time appeared on both the spinner
  and the `⧖` segment) and no longer pushes the context gauge off-screen — the gauge is now ordered
  ahead of the session totals so a narrow row clips the totals, not the gauge.
- Streaming replies now render as markdown live, not as one raw unwrapped blob. The in-flight reply
  edge was dumped into a single span (embedded newlines, headings, lists and code fences all
  collapsed into one wrapped paragraph) and only re-rendered as markdown once the turn finished. It's
  now markdown-rendered on every update — matching the finalized block — and memoized on content
  length so re-parsing doesn't re-introduce the long-conversation lag.
- The claude CLI bridge no longer gives up on shell commands ("can't run that / tool channel broken")
  and fails to commit or open a PR. Told to "commit and push" without a named tool, claude reached
  for its (harness-disabled) native `Bash`, emitted the call as text, and hallucinated interactive-
  shell output (login banner, prompt) — then spiralled, unable to read its own results. The harness
  preamble now states plainly that native tools are disabled, that EVERY shell command goes through
  `mcp__forge__shell` (a clean non-interactive `sh -c`), that garbled/empty output means re-verify
  rather than "channel broken," and that it must never claim it cannot run a command, commit, or open
  a PR. Verified live (claude 2.1.186): a vague "commit and push" now routes through `mcp__forge__shell`
  and lands the commit.
- The codex bridge no longer stalls a plan build claiming it has "no write permissions." codex's
  own shell is sandboxed read-only **by design** (every write is meant to go through Forge's gated
  `mcp__forge__*` tools, which run outside that sandbox), but codex would run `test -w .`, see the
  read-only sandbox + `approval_policy=never`, and quit without ever trying the MCP write tools. The
  harness preamble now states plainly that the read-only shell is expected, that file changes go
  through `mcp__forge__write_file`/`edit_file`/… , and that it must never probe writability or refuse
  a build over it. Verified live (codex 0.141): an implementation turn now calls `write_file` instead
  of bailing.
- `forge lattice impact` output now flags that it is **name-based**: it appends how many definitions
  share the queried name and warns that same-named symbols in unrelated modules/crates are included,
  so a hit must be confirmed before it's treated as a cross-module blocker. A planning agent (notably
  on the codex bridge) was taking impact's same-name collisions at face value — `impact run` reports
  21 definitions across the workspace — and hard-stopping a refactor plan on phantom external
  references instead of presenting it.
- The `present_plan` plan card now word-wraps long step titles, details, and notes to the frame width
  instead of overflowing the right border and mangling the box. Each wrapped continuation line is
  indented under its step and padded so every row meets the border at the same column.
- `forge lattice` now honors `.gitignore`/`.ignore` (via ripgrep's `ignore` walker) instead of a raw
  directory walk, so it no longer indexes gitignored trees — most importantly Forge's own
  `.forge/bench/repos/<astropy|django>/…` SWE-bench clones and `target/`. Those swamped `impact`/
  `query` with hundreds of name-collision hits from unrelated code (e.g. `impact Command` → "323
  sites" across django/astropy), which is why a `/plan` refactor on the codex bridge kept *stopping*:
  the agent dutifully halted on the prompt's "stop if anything outside the crate references this"
  rule when impact reported that vendored noise.
- Lattice queries (`impact`/`query`/`path`) are now scoped to the current repo root. The store is
  global (one DB across every project and bench clone), so an unscoped name match cross-contaminated
  across repos; impact for the project no longer returns symbols from a different checkout.
- `forge lattice update` prunes orphan roots from the global index: a root whose directory is gone
  (deleted scratch clone), or one nested under the root being indexed (stale sub-root, e.g. an old
  bench clone) — reclaiming a badly bloated index (here 101k → 3k nodes).
- Mouse-selection copy no longer spams the chat or breaks the TUI. It's silent now (no `📋 copied`
  scrollback note), and reuses one long-lived clipboard handle — creating one per copy made arboard's
  X11 backend relinquish the selection immediately ("clipboard dropped") and write to the terminal,
  which corrupted the full-screen layout.
- **The plan card and task list now actually appear on the codex bridge.** codex hands its stdio
  MCP servers a *curated* environment (only `PATH`/`HOME`/`LANG`/… survive — verified live), so the
  `FORGE_SUBAGENT_SINK` and checkpoint-context env that the parent sets for `forge mcp-serve` never
  reached it. The served `present_plan`/`update_tasks` wrote to a dead sink, so the parent TUI never
  got the events — the model *did* call the tools (visible in the live stream), nothing rendered.
  Forge now injects that env explicitly into the MCP config (`-c mcp_servers.forge.env.*` for codex,
  the `env` object in claude's `--mcp-config`). Same gap silently broke `/undo` snapshots of
  codex-made edits (the checkpoint env was stripped too); both are fixed. Verified end-to-end: a
  live codex turn's `update_tasks` + `present_plan` now round-trip to the parent sink.
- Full-screen chat is no longer laggy on long conversations. The transcript was re-wrapped
  character-by-character in full every frame (~60×/sec while streaming), which is O(transcript) and
  showed up as input/scroll lag once the log grew. The wrap is now memoized (re-wrapped only when the
  log or width changes) and each frame clones just the visible window, not the whole transcript.
- Full-screen mouse wheel scroll and native text selection now BOTH work. The wheel-scroll support
  used crossterm's full mouse capture, which turns on motion tracking and disables the terminal's
  native click-drag selection. Forge now enables only **minimal** mouse reporting (button + wheel,
  no motion tracking), so the wheel scrolls the transcript while drag-to-select keeps working.
  Configurable via `[tui] mouse_capture` (default on); set false to disable mouse reporting entirely
  and scroll with PgUp/PgDn/Home/End.
- Plan mode now works on CLI bridges: the harness tool-preamble names `mcp__forge__present_plan`
  (and notes that hosts like codex load MCP tools lazily and won't pre-list them). codex 0.141 only
  surfaces a subset of MCP tools up front, so a bridged model told to "present a plan" couldn't find
  `present_plan` and fell back to its read-only shell — the plan never rendered. Verified end-to-end
  against codex 0.141: a bare-name "present a plan" instruction now resolves and calls the tool.
- `forge lattice update` now PRUNES files that vanished from the walk — deleted files and, crucially,
  ones now under a skipped/nested-git/vendored tree. Previously the nested-git skip stopped *new*
  indexing but left already-indexed vendored symbols (e.g. a SWE-bench `django/` clone) in the graph,
  so `impact`/`query` stayed swamped with unrelated hits and the store ballooned. Re-running update
  purges them (symbols/edges/refs cascade-deleted).
- The CLI bridge (`forge mcp-serve`) now uses the SAME global session store as the parent instead of
  a relative `.forge/forge.db`. The divergent path created spurious empty sessions and meant a bridge
  turn's `update_tasks` was written to a different database than the parent's post-turn reload read —
  so bridge task updates didn't round-trip.
- Chat input is more responsive: the event loop now paces adaptively, looping back quickly while you
  type or navigate the palette/picker/approve prompts instead of always waiting a full frame.
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

[Unreleased]: https://github.com/Adulari/forge/compare/v2.2.0...HEAD
[2.2.0]: https://github.com/Adulari/forge/compare/v2.1.0...v2.2.0
[2.1.0]: https://github.com/Adulari/forge/compare/v2.0.0...v2.1.0
[1.8.1]: https://github.com/Adulari/forge/compare/v1.8.0...v1.8.1
[1.8.0]: https://github.com/Adulari/forge/compare/v1.7.0...v1.8.0
[1.7.0]: https://github.com/Adulari/forge/compare/v1.6.1...v1.7.0
[0.4.4]: https://github.com/Adulari/forge/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/Adulari/forge/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/Adulari/forge/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/Adulari/forge/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/Adulari/forge/compare/v0.3.10...v0.4.0
[0.3.10]: https://github.com/Adulari/forge/compare/v0.3.9...v0.3.10
[0.3.9]: https://github.com/Adulari/forge/compare/v0.3.8...v0.3.9
[0.3.8]: https://github.com/Adulari/forge/compare/v0.3.7...v0.3.8
[0.3.7]: https://github.com/Adulari/forge/compare/v0.3.6...v0.3.7
[0.3.6]: https://github.com/Adulari/forge/compare/v0.3.5...v0.3.6
[0.3.5]: https://github.com/Adulari/forge/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/Adulari/forge/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/Adulari/forge/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/Adulari/forge/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/Adulari/forge/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/Adulari/forge/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Adulari/forge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Adulari/forge/releases/tag/v0.1.0
