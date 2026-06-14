# Feature: rich rendering in the TUI (markdown · syntax highlighting · diff review)

> Multi-crate rendering feature: a new `forge-render` module (markdown→lines, code→highlighted
> lines, structured diff→lines), new structured `PresenterEvent` variants for code/diff payloads,
> rendering hooks in `forge-tui` (`app.rs` line builders + `render_live`), and a diff-emit +
> permission-gating seam in `forge-core` / `forge-tools`. Builds *on top of* the inline-scrollback
> model (see `docs/features/tui-inline-scrollback.md`): markdown and diff blocks finalize into
> native scrollback; only the in-flight edge stays pinned. No change to the agent loop's async
> shape or the Presenter trait's method set.

## 1. Problem (JTBD)

> When the assistant answers me or proposes a file edit, I want to **read it the way it's meant to
> be read** — headings as headings, code as highlighted code, and a *diff I can review and
> accept/reject before it touches my files* — so I can trust and steer the agent at a glance
> instead of squinting at raw markdown asterisks and unhighlighted blobs.

Today every assistant line lands in scrollback as plain text via `body_line` (`app.rs:203`):

```rust
fn body_line(text: &str) -> TextLine<'static> {
    TextLine::from(format!("  {text}"))
}
```

Consequences:

- **Markdown is shown literally.** `## Plan`, `**important**`, `` `Foo::bar` ``, bullet lists,
  tables, and fenced code blocks render as their source characters. A long technical answer is a
  wall of monospace with visible syntax noise.
- **Code is unhighlighted.** A fenced ```rust block is grey body text — no token colors, no
  language affordance. For a *coding* agent this is the most-read content type.
- **Edits are invisible until applied.** `edit_file`/`write_file` are `SideEffect::Write`
  (`core_tools.rs:47,116`). The permission prompt today (`main.rs:382`) says only
  `allow edit_file (Write)` — the reviewer is asked to approve a change **they cannot see**.
  After the fact, the only trace is a `✓ edit_file  edited path (1 replacement)` summary line.

Three connected gaps, one job: **render AI output and edits properly so the owner can review
constantly and fast.** The diff-review part is the highest-value: the owner approves edits many
times per session, currently blind.

Who's affected: every interactive `forge chat` user. The headless/piped path keeps its plain
behavior (it's for scripting/CI — see §5 fallback).

## 2. Scope (MoSCoW)

**Must have**
- As a user, assistant replies render **markdown**: ATX/setext headings, bold/italic, inline code,
  bullet + ordered lists (with nesting), blockquotes, fenced code blocks, links, thematic breaks
  (horizontal rules) — as ratatui styled spans, finalized into scrollback.
- As a user, **fenced code blocks are syntax-highlighted** by language, themed to the orange brand,
  in both assistant answers and diff bodies.
- As a user, when the assistant calls `edit_file`/`write_file`, I see a **unified diff with `+`/`-`
  gutters and syntax highlighting**, rendered **before** the change is applied, with an
  **accept/reject** prompt that gates the write through the existing permission flow.
- As a user, markdown rendering works **with streaming**: committed lines/blocks finalize into
  scrollback as they complete; the in-flight (not-yet-closed) block renders incrementally in the
  live region without corrupting partial markdown.
- As a user on a **non-tty / piped** session, I get the existing plain-text output (no ANSI, no
  diff prompt — see fallback in §5).
- As a user, the **orange brand palette is reused** (the `ORANGE/USER/DIM/...` constants in
  `app.rs:18-25`); highlighting and diff colors are chosen to harmonize, not clash.

**Should have**
- As a user, **GFM tables** render as aligned bordered tables; **task lists** (`- [ ]`) render with
  checkbox glyphs.
- As a user, the diff shows a **hunk header** (`@@ -a,b +c,d @@`) and a file path header, and
  collapses unchanged context beyond N lines (default 3).
- As a user, very large diffs are **summarized/truncated** with a `… (N more lines)` note rather
  than flooding scrollback (see edge-case table).
- As a user, I can **scroll the diff** in my terminal like any scrollback (it's finalized inline),
  and re-read it after accepting.

**Could have**
- **Side-by-side** diff view (old | new columns) as an alternate layout, width-gated; unified is the
  default.
- A keybind to **toggle** raw markdown vs rendered for the last block (debugging / copy-paste).
- **Per-language theme** tuning beyond the default theme.
- A small **language badge** on code blocks (e.g. `rust` right-aligned on the fence top border).

**Won't have (this iteration)**
- Image/inline-HTML rendering, MathJax/LaTeX, footnote/citation rendering.
- Editing the diff inline before accepting (accept-as-proposed or reject only).
- Multi-file atomic diffs in one prompt (each tool call is one file; a multi-edit is multiple
  prompts) — structurally possible later, out of scope now.
- Re-highlighting / reflowing already-emitted scrollback on resize (native scrollback never
  reflows — same constraint as the inline-scrollback feature).

## Non-goals
- Does **not** add or remove `Presenter` trait methods (`emit`/`confirm`/`read_line` unchanged); it
  adds new `PresenterEvent` *variants* and reuses the existing `confirm` gate for accept/reject.
- Does **not** change the agent loop's async structure, the Mesh router, the store, or provider code.
- Does **not** change *which* tools exist or their JSON schemas as seen by the model — only how their
  proposed effects are surfaced to the human.

## 3. Acceptance criteria

```
# Markdown

Given the assistant replies with "## Plan\n\n- step **one**\n- step `two`\n"
When the reply finalizes
Then scrollback shows "Plan" as a bold heading line (no literal "##")
And two bullet lines with "• " markers, "one" bold and "two" in inline-code style
And no raw markdown punctuation (**, `, -) is visible

Given the assistant streams a reply token by token
When a markdown block (e.g. a list) is only partially received
Then the completed lines above it are finalized into scrollback
And the in-flight partial line renders in the live preview as plain-but-safe text
And no half-parsed markdown artifact is committed to scrollback

# Fenced code

Given an assistant reply contains a ```rust fenced block
When it finalizes
Then the block renders with a subtle framed background and syntax colors
And the language is shown (badge or fence label)

Given a fenced block with an unknown/absent language tag (``` or ```wat-lang)
When it renders
Then it renders as plain monospace inside the frame (no crash, no color), with a dim "text" label

Given the assistant streams a code fence whose closing ``` has not yet arrived
When tokens arrive
Then the open fence is treated as in-flight: its received lines render highlighted in the live
     region, and nothing is finalized to scrollback until the fence closes (or the reply ends)

# Diff review

Given the assistant calls edit_file{path,old,new} in Default permission mode
When the tool is about to run
Then a unified diff (path header, @@ hunk, +/- gutters, syntax-highlighted bodies) is shown in
     scrollback BEFORE any file write
And a permission bar shows "apply edit to <path>?  [y]es / [N]o"
And the file on disk is unchanged at this point

Given that diff prompt
When I press y / Enter
Then the edit is applied and a "✓ edit_file" result line appears

Given that diff prompt
When I press N / Esc
Then the edit is NOT applied, the file is unchanged, and a "✗ edit_file  rejected by user" line appears

Given write_file{path,content} where the file already exists
When the prompt is shown
Then the diff is computed against the current file contents (old = on-disk, new = content)

Given write_file for a NEW path (file does not exist)
When the prompt is shown
Then the diff shows all lines as additions (old = empty) with a "(new file)" path header

Given a diff larger than the large-diff threshold (default 500 changed lines)
When it renders
Then it shows the first N hunks + a "… (M more changed lines — full diff in result)" note
And the accept/reject prompt still gates the whole change

Given the target of write_file is a binary / non-UTF-8 file
When the prompt is shown
Then no textual diff is attempted; the prompt shows "binary file <path> (N → M bytes)" and the
     accept/reject gate still applies

# Permission modes & fallback

Given AcceptEdits mode (Write auto-allowed; see permission.rs:24-28)
When edit_file is proposed
Then the diff is still rendered to scrollback (for the record) but NO prompt is shown and the
     edit applies automatically

Given Plan mode (all writes denied)
When edit_file is proposed
Then the diff is rendered with a dim "(plan mode — not applied)" note and the edit is skipped

Given a non-tty / piped invocation (HeadlessPresenter)
When the assistant replies with markdown or proposes an edit
Then output is plain text (current behavior): markdown source is printed as-is, the diff is printed
     as a plain unified-diff text block, and confirm() uses the existing stdin y/N path
```

## 4. Impact analysis

Touches four crates. The seam that makes diff-before-apply possible is `forge-core::invoke_tool`
(`lib.rs:291-337`): permission `confirm` already runs **before** `tool.run()`, so the diff must be
produced **before** `run()`. Two viable placements, recommended choice in §5.

| Layer | Insertion point | Change |
|-------|-----------------|--------|
| Render lib | **new** `forge-render` crate (or `forge-tui::render` module) | Pure functions: `markdown_to_lines(&str, width) -> Vec<Line<'static>>`, `highlight_code(src, lang) -> Vec<Line<'static>>`, `diff_to_lines(&FileDiff, opts) -> Vec<Line<'static>>`. No terminal I/O; unit-testable on strings. Owns the syntax/theme assets. |
| Presenter events | `forge-tui::lib.rs` `PresenterEvent` (`lib.rs:18-50`) | Add `AssistantText`/streaming stays, plus **new** variants: `Diff(FileDiff)` (proposed edit payload) and optionally `CodeBlock { lang, src }` if core ever emits standalone code. Markdown does **not** need a new event — it's a *rendering* of `AssistantText`/the streamed reply. |
| Diff payload type | `forge-types` (shared) | **new** `FileDiff { path, kind: Created|Modified|Deleted, old: Option<String>, new: Option<String> }` (or a precomputed unified-patch string). Lives in `forge-types` so core/tools/tui all see it. |
| Tools emit diff | `forge-tools::core_tools.rs` `EditFileTool`/`WriteFileTool` | Add a way to produce the *proposed* new content **without writing**: a `preview(&Value) -> Result<FileDiff>` method on a `PreviewableTool` sub-trait (read old from disk, compute new in memory). `run()` still does the actual write on accept. |
| Core gates on diff | `forge-core::lib.rs::invoke_tool` (`lib.rs:311-321`) | Before the `confirm` call for a `Write` tool: if the tool is previewable, compute `FileDiff`, `emit(PresenterEvent::Diff(diff))`, *then* `confirm`. `confirm` return value still decides whether `run()` executes. No new trait method on `Presenter`. |
| TUI renders markdown | `forge-tui::app.rs` apply paths (`AssistantText` `lib? :137`, `AssistantDelta` `:144`, `AssistantDone` `:157`) | Route finalized assistant text through `markdown_to_lines` instead of raw `body_line`. Streaming: maintain a markdown line-accumulator; finalize completed blocks, keep the in-flight block in `streaming`. |
| TUI renders diff | `forge-tui::app.rs` `apply` + new builder | Handle `PresenterEvent::Diff` → `diff_to_lines` → push to `flush`. The permission bar text for a diff prompt becomes "apply edit to <path>?". |
| Live preview | `forge-tui::app.rs::render_preview` (`:299`) | When the in-flight block is inside a code fence, render the partial through `highlight_code` (best-effort) rather than raw; otherwise plain. |
| Loop prompt wording | `forge-cli::main.rs:382` | `app.prompt` for a Write tool reads "apply edit to <path>?" when a diff was just emitted (cosmetic; the y/N plumbing at `:333-340` is unchanged). |
| Deps | workspace `Cargo.toml` | Add `pulldown-cmark` (markdown), `syntect` *or* a tree-sitter set (highlighting — see §5 eval), `similar` (diff). All confined to `forge-render`. |

**Regression risk.**
- Existing `app.rs` tests assert raw substrings in scrollback (e.g. `assistant_text_is_queued_to_scrollback` checks the text appears). Markdown rendering preserves the *visible text* (it strips only markup), so most asserts hold; asserts that depend on literal markup must change. New tests target `markdown_to_lines`/`diff_to_lines` directly.
- `summarize()` in core still produces the `✓` summary line — unchanged.
- The headless presenter must be explicitly kept on the plain path (don't route it through the renderers).

## 5. Technical design

### 5.1 Dependency choices

**Markdown — `pulldown-cmark`.** Pull-parser emitting a flat `Event` stream
(`Start(Tag)`/`End(Tag)`/`Text`/`Code`/`SoftBreak`/...). Fits the streaming model far better than a
DOM/AST: we fold events into ratatui `Line`s with a small style stack (current fg/modifiers), and we
can stop at the last *completed* block. Tiny, no_std-friendly, no runtime assets, fast startup —
matches Forge's "instant startup / tiny binary" ethos. GFM tables/task-lists/strikethrough are
behind `Options` flags. Alternatives (`comrak` — heavier, CommonMark-AST; `markdown-rs`) bring more
weight or a less stream-friendly shape.

**Syntax highlighting — recommend `syntect` with a *bundled minimal* syntax/theme set, NOT the full
default packs.** Evaluation:

| Criterion | `syntect` (default dumps) | `syntect` (curated/minified set) | tree-sitter (+ grammars) |
|-----------|---------------------------|----------------------------------|--------------------------|
| Startup cost | Loads a ~2MB `SyntaxSet` dump (`from_dumps`) lazily — measurable but one-time, behind `OnceLock` | Load only N languages we care about (rust, ts/js, python, go, json, toml, yaml, md, bash, html/css) — small, fast `OnceLock` init | Per-grammar `Parser` init is cheap; cost is *binary size* of compiled grammars |
| Binary size | +several MB (oniguruma regex engine + syntaxes) | Use `fancy-regex` (pure-Rust) backend + curated set → much smaller, no C dep | Each grammar is C compiled in; 10 langs ≈ multiple MB; +`tree-sitter` runtime |
| Language coverage | Excellent (Sublime syntaxes) | Good for our curated set; "unknown → plain" fallback | Excellent *per grammar shipped*; adding a language = adding a crate |
| Theming to orange brand | Themes are `.tmTheme`; we can ship one custom **forge-dark** theme mapping scopes → our palette | same | We map highlight *capture names* → palette ourselves (more control, more work) |
| Streaming/partial code | Stateless per-line via `HighlightLines`; trivially handles "render the lines I have so far" | same | Incremental parser is built for edits but re-highlighting a growing buffer each delta is fine at our sizes |
| Fit to ethos | Acceptable if curated | **Best balance** | Best *quality* but worst on binary size / build complexity (C toolchain) |

**Recommendation: `syntect` with the `fancy-regex` (pure-Rust) backend and a curated, minified
`SyntaxSet`/single bundled `forge-dark` theme**, loaded once via `OnceLock`. Rationale: pure-Rust (no
oniguruma C dependency → clean cross-compile, smaller binary), one-time lazy init that doesn't hit
cold-start of `forge --help`/`forge run`, and per-line stateless highlighting that maps perfectly
onto "highlight the lines I have so far" for streaming. tree-sitter gives marginally better
tokenization but costs C grammars (binary size + build toolchain) that contradict the tiny-binary
goal; revisit only if highlight quality proves insufficient.

> Brand theming: ship one `forge-dark.tmTheme` (or build the `Theme` in code) whose scopes map onto
> the existing palette — keywords/markers → `ORANGE`, strings → `OKGREEN`-ish, comments → `DIM`,
> types → `TOOLCYAN`, so highlighted code visually belongs to the same TUI.

**Diff — `similar`.** Pure-Rust, maintained, produces line-level (and intra-line word-level) diffs
with a clean `TextDiff`/`unified_diff` API and grouped hunks with configurable context. No C deps,
small. We use it to build the `FileDiff` → unified hunks → styled lines. (Alternative `dissimilar`
is word-only; `diffy` is fine too but `similar` has nicer hunk grouping + intra-line emphasis we use
for the `Should-have` word highlighting.)

All three are confined to **`forge-render`** so the heavy assets/deps never leak into
`forge-core`/`forge-provider` and headless builds.

### 5.2 New types & events

```rust
// forge-types
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffKind { Created, Modified, Deleted }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub kind: DiffKind,
    pub old: Option<String>,   // None for a created file
    pub new: Option<String>,   // None for a deleted file
    pub lang: Option<String>,  // inferred from extension; drives highlighting
    pub binary: bool,          // true → don't attempt a textual diff
}
```

```rust
// forge-tui::lib.rs  — added PresenterEvent variant(s)
pub enum PresenterEvent {
    // ... existing variants unchanged ...
    /// A proposed file change, emitted by core BEFORE the write is confirmed/applied.
    Diff(forge_types::FileDiff),
    // Optional, only if core ever needs to show standalone highlighted code outside a reply:
    // CodeBlock { lang: Option<String>, src: String },
}
```

```rust
// forge-tools — opt-in preview without writing
#[async_trait]
pub trait PreviewableTool: Tool {
    /// Compute the proposed change without touching disk.
    async fn preview(&self, args: &Value) -> Result<FileDiff, ToolError>;
}
// EditFileTool::preview: read old = fs::read_to_string(path).ok();
//   compute new = old.replacen(old_str, new_str, 1) (mirrors run() but no write);
//   lang from extension; binary=false (UTF-8 read succeeded).
// WriteFileTool::preview: old = fs::read_to_string(path).ok() (None ⇒ Created);
//   new = Some(content); binary check on existing file.
```

### 5.3 Core seam (diff before confirm)

`forge-core::invoke_tool` (`lib.rs:311`), for a `SideEffect::Write` tool that is `PreviewableTool`:

```text
side_effect = tool.side_effect()
emit(ToolStart{name,args})                                   // unchanged
if side_effect == Write && let Some(p) = tool.as_previewable():
    match p.preview(&call.args).await {
        Ok(diff) => emit(PresenterEvent::Diff(diff)),          // NEW: render before gate
        Err(_)   => { /* fall through; confirm without a diff */ }
    }
allowed = decide(mode, side_effect) → Ask ⇒ confirm(name, side_effect)   // unchanged gate
if allowed { tool.run(...).await } else { "permission denied" }          // unchanged
emit(ToolResult{...})                                                    // unchanged
```

Key property: **the diff is emitted on the same Presenter, ordered before the `confirm` blocks the
turn task.** In the TUI, the `Diff` event flushes to scrollback, then the `UiMsg::Permission`
arrives (the existing `confirm` → `ChannelPresenter::confirm` → `UiMsg::Permission{reply}` path,
`driver.rs:47-61`), the loop sets `app.prompt`, the user answers y/N (`main.rs:333-340`), and the
reply unblocks `run()`. **No new Presenter method, no new channel.** AcceptEdits/Bypass skip the
prompt (decide() returns Allow) but the `Diff` event is still emitted, so the change is on the record
in scrollback.

### 5.4 Markdown + streaming algorithm

The inline-scrollback model already splits "finalized → scrollback" vs "in-flight → live preview"
(`app.rs:144-166`). Markdown adds *block awareness* on top:

- Keep a per-reply **markdown accumulator** `String` (the full text received so far) plus the index
  up to which we've already finalized.
- On each `AssistantDelta`: append to the accumulator. Run `pulldown-cmark` over the accumulator and
  determine the **last fully-closed block boundary** (a block is "closed" when a following block
  starts or the source ends with a blank line; a fenced code block is closed only by its closing
  ```). Render every block *before* that boundary with `markdown_to_lines` and flush those lines to
  scrollback (only the newly-crossed boundary's lines, tracked by the finalized index). The tail
  (the still-open block) stays in `streaming`.
- `render_preview` shows the tail. If the tail is inside an open code fence, render it through
  `highlight_code` (best-effort, no closing fence needed); otherwise render it as plain text (we do
  **not** commit speculative markup — a half-typed `**bo` is shown literally until the block closes).
- `AssistantDone`: the source is complete ⇒ all remaining blocks are closed; render and flush them,
  clear the accumulator.

This means: **no half-parsed markdown is ever finalized**; the worst case is the live preview shows
a few characters of raw markup for the few hundred ms before a block closes — acceptable and exactly
how editors behave mid-keystroke. Re-parsing the whole accumulator per delta is O(reply size); at
chat-message sizes this is negligible (and we can re-parse only the un-finalized suffix if it ever
matters).

`markdown_to_lines` maps events → `Line<'static>`:

| Markdown | Render |
|----------|--------|
| `# … ######` heading | bold, `ORANGE`, prefixed by level (e.g. blank line + bold text; H1 may add an underline rule line) |
| `**bold**` / `*italic*` | `Modifier::BOLD` / `Modifier::ITALIC` spans |
| `` `inline code` `` | `DIM`/distinct-bg span, monospace (terminal is already mono) |
| `- item` / `1. item` | `•`/`N.` marker span (`ORANGE`) + content, 2-space indent per nest level |
| `> quote` | leading `▏` bar span (`DIM`) + italic-ish content |
| ```` ```lang ```` fenced | hand to `highlight_code(src, lang)`, wrapped in a framed background block |
| `[text](url)` | `text` styled (underline + `USER` color), URL appended dim in parens (terminals rarely OSC-8 reliably) |
| `| a | b |` table (GFM) | aligned columns with light box-drawing separators |
| `---` thematic break | a full-width `─` rule line in `DIM` |

### 5.5 Diff rendering

`diff_to_lines(&FileDiff, opts)`:

1. If `binary` → single line `"  binary file <path> (oldN → newM bytes)"`.
2. Else build `similar::TextDiff::from_lines(old.unwrap_or(""), new.unwrap_or(""))`, group into
   hunks with `opts.context` (default 3).
3. Emit a path/kind header line, then per hunk a `@@` header, then per line:
   - `+` additions: `OKGREEN` fg, `+ ` gutter; body highlighted via `highlight_code` *blended* with
     the add tint (highlight the code, keep a subtle green gutter/background).
   - `-` deletions: `ERRRED` fg, `- ` gutter.
   - ` ` context: `DIM` gutter, normal highlighted body.
4. Truncate per the large-diff threshold (Should-have), appending a `… (M more changed lines)` note.

Highlighting inside a diff reuses the same `highlight_code` (per-line, stateless) keyed on
`FileDiff.lang`; unknown lang → plain.

### 5.6 Mockups (monospace)

**Rendered markdown answer** (finalized into scrollback):

```
  ⚒ forge

  Plan
  ────────────────────────────────────────────
  Here's the fix for the off-by-one:

  • Change the loop bound from < to <=
  • Add a regression test in tests/range.rs

  > Note: this also affects the empty-range case.

  ┌ rust ───────────────────────────────────────┐
  │ for i in 0..=n {        // was 0..n           │
  │     total += weights[i];                      │
  │ }                                             │
  └───────────────────────────────────────────────┘

  See the iterator docs (https://doc.rust-lang.org/std/iter).
```
(`Plan` bold orange + underline rule; `<`/`<=` and `••` orange; blockquote `>` shown as a dim bar;
fenced block framed with a `rust` label, keywords/comments colored; link text underlined, URL dim.)

**Highlighted code block** (colors shown as annotations):

```
  ┌ python ─────────────────────────────────────┐
  │ def greet(name: str) -> str:                  │   def/return → ORANGE (keyword)
  │     return f"hello, {name}"                    │   "hello…"  → green (string)
  │                                                │   name      → cyan (type/param)
  │ # entrypoint                                   │   # …       → DIM (comment)
  │ greet("forge")                                 │
  └───────────────────────────────────────────────┘
```

**Unified diff with accept/reject prompt** (in the pinned live region over scrollback):

```
  ── scrollback ───────────────────────────────────────────────────────────
  ↳ edit_file  {"path":"src/range.rs","old":"0..n","new":"0..=n"}

  ✎ edit_file · src/range.rs (modified)
  @@ -10,6 +10,7 @@ fn weighted_sum(weights: &[f64], n: usize) -> f64
       let mut total = 0.0;
  -    for i in 0..n {
  +    for i in 0..=n {
  +        debug_assert!(i <= n);
           total += weights[i];
       }
       total
  ── pinned live region ──────────────────────────────────────────────────────
   » apply edit to src/range.rs?   [y]es / [N]o
  ╭ message ──────────────────────────────────────────────────────────────╮
  │ › ▌                                                                     │
  ╰─────────────────────────────────────────────────────────────────────────╯
  ⠙ working · [complex] ollama::llama3.2 · $0.0041            ↵ send · esc quit
```
(`-` line red, `+` lines green, `@@`/path header orange-ish, code bodies highlighted; the yellow
permission bar reuses today's `render_permission` styling, only the text differs.)

### 5.7 Performance

- **Highlighting large files / diffs.** Per-line stateless highlight is O(lines). Cap rendered diff
  size at the large-diff threshold; never highlight the full `new` content of a huge `write_file` —
  only the rendered (post-truncation) lines. Load `SyntaxSet`/`Theme` once via `OnceLock` so the cost
  is paid at most once per process and never on `forge --help`.
- **Streaming re-parse.** Markdown re-parse per delta is bounded by message size; finalize-and-drop
  keeps the live accumulator small. If a single message is pathologically large, re-parse only the
  un-finalized suffix.
- **insert_before height.** Unchanged mechanism (`driver.rs:94-104`): `Paragraph::line_count(width)`
  on the built lines gives exact height; styled spans don't change wrapping math.
- **Idle cost is zero.** Renderers run only when a block finalizes or the in-flight tail changes; the
  existing `dirty` flag (`main.rs:319`) still gates redraws.

### 5.8 Edge cases

| Edge case | Behavior |
|-----------|----------|
| Unknown / missing fence language | `highlight_code` returns plain monospace lines; fence labeled `text`; no panic. |
| Malformed markdown (stray `*`, unbalanced `` ` ``) | `pulldown-cmark` is total — it degrades to literal text; never panics, never drops content. |
| Streaming half-open code fence | Treated as in-flight: highlighted-but-not-finalized in the live preview; flushed only when the closing ``` arrives or the reply ends. |
| Streaming half-typed inline markup (`**bo`) | Shown literally in the live preview; the block isn't finalized until closed, so no broken span lands in scrollback. |
| Very large diff (> threshold changed lines) | Render first hunks + `… (M more changed lines — full diff in tool result)`; accept/reject still gates the *whole* change. |
| `write_file` to a new path | `FileDiff{kind:Created, old:None}`; all `+` lines; header `(new file)`. |
| `edit_file` where `old` not found / ambiguous | `preview()` mirrors `run()`'s checks (`core_tools.rs:137-145`) and returns `ToolError::Failed`; core falls through to a plain confirm (no diff) and the real error surfaces on `run()`. |
| Binary / non-UTF-8 target | `preview` sets `binary:true`; renderer shows `binary file <path> (N → M bytes)`; gate still applies; no textual diff attempted. |
| No-tty / headless | `HeadlessPresenter` keeps plain paths: markdown printed as source, `Diff` printed as a plain unified-diff text block, `confirm` uses stdin y/N (`lib.rs:124-136`). No ANSI, no ratatui. |
| AcceptEdits / Bypass mode | `decide()` returns Allow → no prompt; `Diff` event still emitted and rendered for the record. |
| Plan mode | `decide()` returns Deny → not applied; diff rendered with a dim `(plan mode — not applied)` note. |
| Reject mid-turn | User answers N → `confirm` returns false → `run()` not called, file untouched, `✗ … rejected by user` result line; turn continues. |
| Terminal too narrow for table / side-by-side | GFM table degrades to stacked rows; side-by-side falls back to unified below a width threshold. |
| Diff emitted but user quits before answering | Turn task is blocked in `confirm`; on quit the task is detached (same as inline-scrollback's quit-mid-stream) — no write happens. |

## 6. Definition of done

- [ ] `forge-render` (or `forge-tui::render`) exposes pure `markdown_to_lines`, `highlight_code`,
      `diff_to_lines`; all deps (`pulldown-cmark`, `syntect`+`fancy-regex` curated set, `similar`)
      confined to it.
- [ ] Assistant replies render markdown in scrollback (headings/bold/italic/lists/quotes/inline
      code/links/rules/tables); streaming finalizes completed blocks and never commits half-parsed
      markup.
- [ ] Fenced code blocks are syntax-highlighted, brand-themed, with a language label; unknown
      language falls back to plain.
- [ ] `PresenterEvent::Diff(FileDiff)` added; `FileDiff` in `forge-types`; `EditFileTool`/
      `WriteFileTool` implement `PreviewableTool::preview` (no disk write); `invoke_tool` emits the
      diff before `confirm`.
- [ ] Unified diff renders with path/`@@` headers, `+`/`-`/context gutters, highlighted bodies, and
      the accept/reject prompt gates the write through the **existing** permission flow (no new
      Presenter method).
- [ ] Reject leaves the file unchanged; accept applies; AcceptEdits auto-applies but still renders
      the diff; Plan renders + skips.
- [ ] Headless/piped path unchanged (plain text + stdin y/N).
- [ ] Large-diff truncation, binary-file, new-file, and unknown-language edge cases handled per the
      table; no panics on malformed markdown.
- [ ] One-time `OnceLock` syntax/theme init; no startup cost on `forge --help`/`forge run`.
- [ ] `cargo fmt` + `clippy -D warnings` clean; workspace builds; `graphify update .` run.
- [ ] Unit tests: `markdown_to_lines` (each element + malformed input), `highlight_code` (known +
      unknown lang), `diff_to_lines` (modified/created/binary/truncated); `app.rs` tests for
      streaming finalize-vs-live and the diff→prompt path; verified live against Ollama in a real
      terminal.
```

---

3-line summary:
- Spec covers the three connected parts — `pulldown-cmark` markdown rendering that works with the inline-scrollback finalize/streaming model, `syntect` (pure-Rust `fancy-regex` backend, curated minified syntax set + a brand-themed `forge-dark` theme) for code + diff highlighting, and a `similar`-based unified diff shown *before* apply.
- The diff-review seam reuses Forge's existing permission gate: a new `PresenterEvent::Diff(FileDiff)` (type in `forge-types`) is emitted from `forge-core::invoke_tool` right before the existing `confirm()` — so accept/reject routes through the current `UiMsg::Permission` reply channel with **no new Presenter trait method**; tools gain a `PreviewableTool::preview` that computes old/new without writing.
- Doc includes MoSCoW scope + non-goals, Given/When/Then acceptance (incl. negatives: unknown lang, malformed markdown, half-open fence, reject, headless, binary, plan mode), an impact table with file:line insertion points, dependency justification, performance/theming notes, three monospace mockups, an edge-case table, and a DoD checklist.
