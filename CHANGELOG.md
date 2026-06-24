# Changelog

All notable changes to Forge are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and Forge adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/florisvoskamp/forge/compare/v0.3.4...HEAD
[0.3.4]: https://github.com/florisvoskamp/forge/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/florisvoskamp/forge/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/florisvoskamp/forge/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/florisvoskamp/forge/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/florisvoskamp/forge/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/florisvoskamp/forge/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/florisvoskamp/forge/releases/tag/v0.1.0
