# Feature: inline-scrollback TUI (Claude-Code-style)

> Single-crate rendering-architecture change in `crates/forge-tui` + the loop in
> `forge-cli::run_chat_tui`. Changes *how* the TUI renders (alternate-screen full panel →
> inline viewport with native scrollback), not *what* it shows. Visual styling is
> byte-for-byte preserved.

## 1. Problem (JTBD)

> When I'm deep in a `forge chat` session, I want to scroll back through earlier messages
> and tool output with my normal terminal/mouse scroll, so I can re-read context without
> the conversation vanishing off the top of a trapped panel.

Today `forge chat` enters the **alternate screen** and renders a fullscreen ratatui app:
the conversation is a bordered `Paragraph` with a manually-computed `.scroll()` offset
(`app.rs::render_conversation`). Consequences:

- Old content scrolls off the top and is **gone** — there is no way to see it again.
- Native terminal scrollback / mouse wheel does **nothing** (alternate screen swallows it).
- On quit, the whole session disappears from the terminal (alternate screen restores).

Reference behaviour: **Claude Code** and the **Antigravity CLI** render inline — finished
output flows into the terminal's real scrollback (scroll with the mouse like any other
command), and only a small live region (input + status) is pinned at the bottom.

Who's affected: every interactive `forge chat` user. Why it matters: scrollback is the
single most-used affordance in a long agent session; its absence is the biggest UX gap.

## 2. Scope (MoSCoW)

**Must have**
- As a user, finalized conversation content (my messages, the assistant's completed
  replies, tool start/result lines, warnings) is written into the terminal's **native
  scrollback**, so I scroll it with the mouse/terminal exactly like any CLI output.
- As a user, a small **live region pinned at the bottom** shows the input box, the
  statusline, the permission bar (when asked), and the **in-flight streaming reply** with
  the spinner — and updates in place as tokens arrive.
- As a user, when a streamed reply finishes, its **full text** is committed to scrollback
  (nothing is lost even if it was longer than the live preview).
- As a user, the **visual styling is unchanged**: same palette/orange brand, same `you` /
  `⚒ forge` blocks, same `↳`/`✓`/`✗` tool lines, same statusline segments + spinner.
- As a user on quit, the conversation **stays in my terminal** (not wiped by an alternate
  screen).

**Should have**
- As a user, the ASCII **FORGE** welcome banner is printed once into scrollback at start
  (so the session opens with the brand moment, then normal output flows beneath it).
- As a user on a narrow terminal, the banner and live region degrade exactly as today
  (compact wordmark; statusline segment drop-out).

**Could have**
- Dynamic live-region height (grow/shrink the pinned viewport as the streaming reply
  grows) instead of a fixed-height tail preview — only if ratatui exposes a clean resize.
- A subtle "── new ──" rule between turns in scrollback.

**Won't have (this iteration)**
- In-app scroll keys (PgUp/PgDn) — we delegate scrolling to the **terminal**, which is the
  whole point. No custom scroll state.
- Reflowing already-emitted scrollback on terminal resize (native scrollback never reflows
  past output — same as Claude Code).
- Search / copy-mode / selection (terminal-native).

## Non-goals
- This feature does **not** change the agent loop, the Presenter seam, the event set, or
  any provider/core/store/mesh code.
- It does **not** change message *content* or styling — only the surface they're drawn on.
- It does **not** add new `PresenterEvent` variants.

## 3. Acceptance criteria

```
Given a fresh `forge chat` session on a tty
When the TUI starts
Then the ASCII FORGE wordmark + tagline is printed once at the top of the terminal
And a live region (input box + statusline) is pinned at the bottom
And the rest of the terminal is the normal scrollback buffer

Given I submit a message
When it is accepted
Then a styled "you" block for my message appears in scrollback (above the live region)
And the input box clears

Given the assistant is streaming a reply
When tokens arrive
Then the in-flight reply is shown live in the pinned region with the ▌ cursor and spinner
And it updates in place (it does NOT spam new scrollback lines per token)

Given the assistant finishes a reply
When AssistantDone is applied
Then the complete reply is written to scrollback as a styled "⚒ forge" block
And the live preview area is cleared

Given a tool runs
When ToolStart / ToolResult are applied
Then a styled "↳ name args" line and a "✓/✗ name summary" line appear in scrollback

Given the model asks permission for a side effect
When the prompt is shown
Then the permission bar appears in the live region (yellow), the conversation is untouched
And answering y/N removes the bar and the turn continues

Given I scroll my terminal/mouse wheel up mid-session
When there is earlier content
Then I see the earlier messages in native scrollback (the live region stays pinned)

Given I quit (esc / ctrl-c / /quit)
When the TUI exits
Then raw mode is disabled and the full conversation remains visible in my terminal
(no alternate-screen wipe)

Given a terminal narrower than the wordmark (< ~46 cols)
When the banner is printed
Then the compact single-line wordmark is printed instead (no wrap garbage)

Given a non-tty (piped) invocation
When `forge chat` runs
Then it uses the existing plain/headless path (inline viewport is tty-only)
```

## 4. Impact analysis

Single crate (`forge-tui`) + the loop in `forge-cli`. No core/store/provider/mesh changes.
The Presenter seam, `PresenterEvent`, `ChannelPresenter`, and `run_chat_tui`'s async
structure are preserved.

| Layer | Insertion point | Change |
|-------|-----------------|--------|
| Terminal setup | `forge-tui::driver::Tui::new` | Drop `EnterAlternateScreen`/`LeaveAlternateScreen`; create the terminal with `Viewport::Inline(LIVE_H)`. Keep raw mode. |
| Scrollback flush | `forge-tui::driver::Tui` (new `insert_lines`) | New method wrapping `terminal.insert_before(height, …)` to push finalized blocks into native scrollback. |
| Live render | `forge-tui::app` (new `render_live`) | Renders only the pinned region: streaming preview + permission + input + statusline. Reuses existing `render_input`/`render_statusline`/`render_permission` verbatim. |
| Block builder | `forge-tui::app` (new `block_lines`) | Pure fn: one finalized item (`Line` / warning / banner) → `Vec<ratatui::text::Line<'static>>` using the *existing* styling from `render_conversation`. Used by `insert_lines`. |
| Outbox | `forge-tui::app::App` | `App::apply` pushes finalized items into a `flush: Vec<Flush>` queue; loop drains it via `App::drain_flush()`. Transient fields (`input`, `streaming`, `prompt`, `routing`, `cost_usd`, `busy`, `tick`) stay for the live render. |
| Loop | `forge-cli::run_chat_tui` | After applying events, `for block in app.drain_flush() { tui.insert_lines(block) }`; `tui.draw` now renders the live region only. Banner inserted once before the loop. |

**Regression risk.** The biggest risk is the existing `app.rs` render tests, which call
the full-screen `render` and assert content appears on an 80×24 `TestBackend`. After the
split, finalized content is no longer in the live frame — those assertions move to test
`block_lines` output instead. Streaming/input/statusline/spinner tests stay (they target
the live region). See §6.

## 5. Technical design

### Rendering model

```
┌─ terminal scrollback (native, mouse-scrollable) ───────────────┐
│  ███████╗ ██████╗ ... FORGE  (banner, printed once at start)   │
│  model-mesh coding agent · type a task to begin                │
│                                                                │
│    you                                                         │
│    fix the failing test                                        │
│                                                                │
│    ⚒ forge                                                     │
│    Looking at the test now…                                    │
│    ↳ read_file  {"path":"src/lib.rs"}                          │
│    ✓ read_file  42 lines                                       │
│  … (scrolls up into history as it grows) …                     │
├─ pinned live viewport (Viewport::Inline, LIVE_H rows) ─────────┤
│    ⚒ forge                          ← streaming preview (tail) │
│    The fix is to change `<` to `<=`▌   + spinner while busy    │
│  ╭ message ─────────────────────────────────────────────────╮ │
│  │ › ▌                                                       │ │   input (3 rows)
│  ╰───────────────────────────────────────────────────────────╯ │
│  ⠙ working · [complex] ollama::llama3.2 · $0.0033   ↵ send · …  │   statusline (1)
└────────────────────────────────────────────────────────────────┘
```

### Vertical slice — submit → scrollback → stream → commit

```
key Enter (handle_key → Submit(line))           [forge-cli loop]
   ↓ app.lines? no — push finalized "you" block to app.flush; clear input
   ↓ spawn turn (unchanged)
loop drains app.flush → tui.insert_lines(block_lines(User(line)))   → native scrollback
   ↓
turn emits Routing/Cost (transient: update app.routing/app.cost_usd; NO flush) → statusline
turn emits AssistantDelta*                       [ChannelPresenter → rx]
   ↓ app.streaming.push_str(delta)   (transient)
   ↓ live region re-renders the streaming tail + spinner in place
turn emits ToolStart/ToolResult/Warning
   ↓ pushed to app.flush → insert_lines → scrollback (above the live region)
turn emits AssistantDone
   ↓ push finalized "⚒ forge" block (full app.streaming) to app.flush; clear app.streaming
   ↓ insert_lines → full reply committed to scrollback; live preview clears
```

### Live-region height & streaming model

`LIVE_H` is fixed at terminal creation — ratatui's inline viewport height cannot change at
runtime (`Terminal::resize` reuses the height stored in `Viewport::Inline(h)`; there is no
setter). A tall fixed viewport would pin many blank rows when idle, contradicting the
"small pinned region" must-have. So we keep the viewport **small** and stream **completed
lines straight into scrollback** as they finalize — only the *in-flight partial line* lives
in the pinned region. This is exactly how Claude Code feels: text streams at the bottom and
scrolls up into history.

Composition, top to bottom (fixed):

```
STREAM_PREVIEW_H  (3; wrapped tail of the current partial line — blank when idle)
PERMISSION_H      (1; blank when no prompt)
INPUT_H           (3; bordered "message" box)
STATUS_H          (1; statusline)
------------------------------------------------
LIVE_H = 3 + 1 + 3 + 1 = 8
```

**Streaming algorithm** (`App::apply`):
- First `AssistantDelta` of a reply: flush the `⚒ forge` **header** line to scrollback,
  set `streaming_active = true`.
- Each `AssistantDelta`: append to `streaming`, then split off every **newline-terminated**
  line and flush each to scrollback (`Flush`). What remains in `streaming` is a single
  partial logical line with no `\n`.
- `render_live` shows that partial line in the preview area: a `Paragraph` wrapped to width,
  scrolled to its bottom so the freshest `STREAM_PREVIEW_H` rows + the `▌` cursor are
  visible. (One logical line, so it never needs more than the partial's own wrap height.)
- `AssistantDone`: flush the partial remainder (if any) + a blank separator; clear
  `streaming`, `streaming_active = false`.

Nothing is lost or truncated in the record — every line of the reply lands in scrollback as
it completes; the preview is only the live edge. When idle, the preview rows are blank, so
the pinned region is effectively input + statusline (small footprint).

### `insert_before` height computation

`terminal.insert_before(height, f)` needs the row count up front, which depends on
wrapping at the current width. Compute it from the built lines:

```rust
let width = self.terminal.size()?.width;
let para = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
let height = para.line_count(width) as u16;   // ratatui 0.30 Paragraph::line_count
self.terminal.insert_before(height, |buf| para.render(buf.area, buf))?;
```

If `line_count` proves unavailable/unstable in 0.30, fall back to a small wrap helper
(count wrapped rows per `Line` via `unicode-width`). Either way the height is exact so no
clipping/overlap with the viewport.

### Banner at startup

Before entering the loop, build the banner lines (reuse `FORGE_WORDMARK`/`TAGLINE`, with
the `< WORDMARK_WIDTH` compact fallback) and `tui.insert_lines(banner)` once. The welcome
state is now a one-time scrollback print, not a render branch — so `render`'s old
`welcome` switch is removed; the compact header is likewise dropped (the banner is the
brand moment, then output flows). Statusline + input render every frame.

### Data structures

A "flush" is just pre-styled scrollback content. Building the lines needs no width (the
`Paragraph` wraps at render time; only `insert_before`'s height needs width), so `App`
holds owned `ratatui::text::Line<'static>`s directly:

```rust
pub struct App {
    // transient (rendered in the pinned live region):
    pub routing: Option<RoutingView>,   // statusline tier+model
    pub cost_usd: f64,                  // statusline cost
    pub input: String,                  // input box
    pub streaming: String,              // current partial (un-flushed) reply line
    streaming_active: bool,             // header already flushed for this reply
    pub prompt: Option<String>,         // permission bar
    pub busy: bool,
    pub tick: usize,
    pub session_id: String,
    pub done: bool,
    // outbox — finalized scrollback lines in arrival order, drained by the loop:
    flush: Vec<TextLine<'static>>,
}

impl App {
    pub fn drain_flush(&mut self) -> Vec<TextLine<'static>> { std::mem::take(&mut self.flush) }
    pub fn submit_user(&mut self, line: &str) { /* push styled "you" block to flush */ }
}
```

`App::apply` changes:
- `AssistantText(t)` → push header + body lines + blank to `flush` (non-streamed path).
- `AssistantDelta` → header-once + per-line flush (see streaming algorithm above).
- `AssistantDone` → flush remaining partial + blank separator; clear streaming state.
- `ToolStart`/`ToolResult`/`Warning` → push their styled line(s) to `flush`.
- `Routing`/`Cost`/`SessionStarted`/`Done` → update transient fields (no flush).

The `lines: Vec<Line>` field and the `Line` enum's render path are removed — nothing draws
the historical transcript in-frame. Small private builders (`you_block`, `forge_header`,
`body_line`, `tool_start_line`, `tool_result_line`, `warning_line`, `banner_lines`) carry
over the **exact** span styling from today's `render_conversation` so scrollback looks
identical to what the panel showed.

### Driver changes

```rust
// Tui::new — inline viewport, no alternate screen
enable_raw_mode()?;
let backend = CrosstermBackend::new(io::stdout());
let terminal = Terminal::with_options(backend,
    TerminalOptions { viewport: Viewport::Inline(LIVE_H) })?;

// Tui::insert_lines(&mut self, lines: Vec<TextLine<'static>>) -> io::Result<()>
//   computes height (above) and calls insert_before.

// Tui::draw — renders the live region only (render_live), unchanged signature.

// Drop — disable_raw_mode + show_cursor.  NO LeaveAlternateScreen.
//   Optionally a final newline so the shell prompt starts cleanly below the viewport.
```

### Edge cases

| Edge case | Behaviour |
|-----------|-----------|
| Terminal height < LIVE_H | ratatui clamps the inline viewport to available rows; statusline+input are the last to drop. Acceptable; agent loop unaffected. |
| Streaming reply longer than STREAM_PREVIEW_H | Live shows the tail (latest lines); full text committed to scrollback on done — nothing lost. |
| Very long single token/line (no spaces) | `Wrap { trim:false }` wraps by width; `line_count` accounts for it so insert height is correct. |
| Terminal resize mid-session | crossterm Resize event; ratatui re-lays the inline viewport at the new width for future frames. Already-emitted scrollback does **not** reflow (native behaviour, same as Claude Code). |
| Permission prompt while streaming | Streaming pauses (turn task blocks on the reply channel — unchanged); permission bar shows in live region; on answer the stream resumes. |
| Banner wider than terminal | Compact single-line `⚒ FORGE — model-mesh coding agent` inserted instead (existing `< WORDMARK_WIDTH` fallback). |
| Non-tty / piped | Unchanged: `forge chat` uses the headless/plain path; inline viewport is tty-only. |
| Rapid tool spam (many lines fast) | Each finalized line is one `insert_before`; loop drains the whole `flush` queue per iteration, so no per-token churn. |
| Quit mid-stream | Turn task is detached; raw mode disabled; partial reply that was only in the live preview is **not** committed (it never finished) — acceptable (matches "only finalized content persists"). |

## 6. Definition of done

- [ ] `forge chat` renders inline: finalized blocks flow into native terminal scrollback;
      mouse/terminal scroll works; only input+statusline (+streaming preview/permission)
      are pinned at the bottom.
- [ ] Banner printed once into scrollback at start (narrow fallback intact).
- [ ] Streaming reply animates in place in the live region; full text committed to
      scrollback on done.
- [ ] Quit leaves the conversation in the terminal (no alternate-screen wipe).
- [ ] Visual styling identical: palette, `you`/`⚒ forge` blocks, tool lines, statusline
      segments, spinner cadence (the §A loop fix keeps spinner speed unchanged).
- [ ] `cargo fmt` + `clippy -D warnings` clean; workspace builds.
- [ ] Tests adapted: `block_lines` unit tests assert finalized content/styling
      (user/assistant/tool/warning/banner); `render_live` TestBackend tests assert
      streaming preview + cursor, input bar, statusline (model/cost/tier), spinner-when-
      busy, idle-no-spinner, permission bar. No test asserts historical transcript in the
      live frame.
- [ ] Verified live in a real terminal against Ollama: scroll up shows history; quit keeps
      output; styling matches the previous panel.

## Appendix — relationship to the perf fix

Shipped alongside (Part A, already applied to `run_chat_tui`): drain *all* pending
keystrokes per loop iteration (was one/frame → ~16 keys/sec cap, the input lag), redraw
only when state changed (`dirty` flag), and derive the spinner `tick` from elapsed time so
animation speed is identical at any loop frequency. The inline model compounds the win:
the live region is a handful of rows, so each frame rebuilds far fewer `Line`s than the
old full-conversation panel.
