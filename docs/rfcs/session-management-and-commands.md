# RFC: In-TUI command system, session management & checkpoints

| Field | Value |
|-------|-------|
| Status | ACCEPTED (scope + key forks decided with the user) |
| Author | Forge |
| Created | 2026-06-15 |
| Reviewers | florisvoskamp |
| Implements | 3 sequenced PRs (below) |

## Summary

Forge's animated chat TUI has no commands: every typed line is a model prompt, and
the only way to switch sessions is to quit and relaunch with `--resume`. This RFC
designs (1) an in-TUI **slash-command system** with an animated, keyboard-driven
palette; (2) **session commands** (`/help /sessions /resume /new /clear`) that act on
the live session without restarting; (3) **conversation checkpoints + `/undo`**
(rewind the transcript); and (4) **code checkpoints + restore** via Forge-managed
shadow snapshots (never touching the user's git). Built in three sequenced PRs.

## Problem statement

- **No commands.** The TUI input box only submits prompts. There is no `/help`,
  no way to see or switch sessions, no undo. Every "meta" action (resume, new
  session) requires killing the process and relaunching — losing the live view.
- **Resume is launch-only.** `forge chat --resume <id>` works, but mid-session a
  user who realizes they want a different (or fresh) session must exit and retype
  the command. There is no `/resume` / `/new`.
- **No undo / checkpoints.** If the model makes a bad edit or the conversation goes
  sideways, there is no way to rewind the transcript or restore changed files. The
  user's only recourse is manual `git checkout` (and only if they're in git, and
  only if they hadn't staged other work).

Cost: the harness feels closed and unforgiving versus tools like Claude Code (which
has `/`-commands and resume). For an agentic editor, **undo is a safety feature** —
without it users won't grant the auto-edit temper, undercutting the whole UX.

### Non-goals

- A scripting/macro language for commands (commands are a fixed Rust registry).
- Cloud/remote session sync (sessions stay in the local SQLite store).
- Multi-session split view. One active session per TUI at a time.
- Editing/branching history into a tree (undo is linear rewind; redo is a stretch
  goal, not required).
- Touching the user's git state in any way (explicit user decision).

## Background and context

Verified current state:

| Area | Fact |
|------|------|
| Render loop | `forge-cli::run_chat_tui` — 16fps inline ratatui viewport, `scrolling-regions`, native scrollback via `tui.insert_lines`. Owns `Arc<tokio::Mutex<Session>>`. |
| Input | `forge-tui::App` input-line buffer + `handle_key(&mut input, KeyKind) -> InputOutcome{Editing,Submit(line),Quit}`. |
| Turn | Submitting spawns a tokio task: lock the `Session`, `run_turn`. Presenter events → `std::sync::mpsc` (`ChannelPresenter`→`UiMsg`) → loop. Permission/question prompts block the turn task on a reply channel **wrapped in `tokio::task::block_in_place`** (do not re-introduce a bare blocking recv on the loop thread). |
| Commands | None in the TUI. `chat_action` handles `/quit` only in plain mode. |
| Store | `create_session`, `create_child_session`, `list_sessions()->SessionSummary{id,cwd,permission_mode,created_at,total_cost_usd,message_count,preview}`, `load_messages()->Vec<StoredMessage>`, `session_exists`, `matching_session_ids(prefix)`, `update_session_mode`. |
| Session | `forge-core::Session { id, store, provider, router, tools, presenter, config, pricing, mode, rules, transcript, seq }`. `Session::start` / `Session::resume(...)` already exist. |
| Checkpoints | None. Git work tree present but **off-limits**. |

The hard-won lesson from the freeze fix (RFC predecessor / PR #45): **never block a
tokio worker on the render-loop path.** The command system must honor this — command
handlers that do I/O (DB reads for `/sessions`, file copies for snapshots) run such
that the 16fps loop keeps ticking.

## Proposed solution

### High-level design

Three layers, one per PR:

```
PR1  forge-tui: Palette (UI state) + CommandRegistry (metadata)
     forge-cli: key routing → palette → CommandAction dispatch (Session swap)
     forge-core: Session::start_fresh / reuse Session::resume
PR2  forge-store: checkpoint table; forge-core: rewind(seq); /undo /checkpoint(s)
PR3  forge-core: ShadowSnapshot (pre-edit file copy) + restore; wire into /undo
```

A **command is metadata + an action**, not a closure that mutates the Session
directly — because the Session lives behind an `Arc<tokio::Mutex>` owned by the
render loop, and some commands must *replace* it. So a command produces a
`CommandAction` value that the render loop interprets:

```rust
// forge-tui
pub struct Command { pub name: &'static str, pub desc: &'static str, pub usage: &'static str }
pub fn commands() -> &'static [Command];           // the static registry (metadata only)

pub enum CommandAction {                            // what the loop must do
    Help,                                           // pure UI: show the palette as a list
    ListSessions,                                   // open the session picker
    Resume(String),                                 // resume by id/prefix (replace Session)
    New,                                            // fresh Session (replace)
    ClearScreen,                                    // clear on-screen scrollback only
    Undo,                                           // PR2
    Checkpoint(Option<String>),                     // PR2
    ListCheckpoints,                                // PR2
    Unknown(String),                                // show "unknown command: X"
}
pub fn parse_command(line: &str) -> CommandAction;  // "/resume ab12" -> Resume("ab12")
```

forge-tui owns the **palette UI state** and parsing (pure, testable). forge-cli's
render loop owns the **effects** (Session swap, store reads) because only it holds
the `Arc<Mutex<Session>>` and the `Tui`. This keeps forge-tui free of forge-core /
forge-store deps (layering preserved).

### Detailed design — command framework (PR1)

**Trigger & palette.** The palette is **inline** (rendered inside the existing live
region above the input line), not a modal full-screen overlay — it matches the
inline-scrollback aesthetic and avoids alternate-screen flicker. `App` gains:

```rust
pub struct Palette { pub open: bool, pub query: String, pub selected: usize, pub anim: f32 }
// in App: pub palette: Palette
```

- When the input is exactly `/` (or starts with `/` and the palette isn't dismissed),
  `App::open_palette()` sets `open=true`. As the user types, `query` = text after `/`.
- Filtering: prefix-first, then subsequence fuzzy match on command name, ranked
  (prefix matches above fuzzy), recomputed each keystroke over the static registry
  (≤ a few dozen commands → trivially fast, no async).
- Keys while open: `↑/↓` move `selected` (clamped), `Tab` completes the input to the
  selected command name, `Enter` accepts (dispatch), `Esc` dismisses (palette closes,
  input retained), `Backspace` past `/` closes it. These route in `run_chat_tui`
  **before** the normal input handling when `app.palette.open`.
- Animation: `anim` eases 0→1 on open (height/opacity reveal) driven by the existing
  per-frame tick; rows use the existing dim/bold styling. No new render thread — the
  16fps loop already redraws on `dirty`.
- A line **not** starting with `/` is unchanged: normal prompt submit. A `/` line
  whose command is unknown → `CommandAction::Unknown` → a warning line, no submit.

**Render-loop routing.** In `run_chat_tui`, the key-handling block gains a branch:
`if app.palette.open { handle_palette_key(...) }`. On accept, `parse_command(line)` →
`CommandAction` → a `dispatch_command(action, &session, &mut tui, &mut app).await`
helper. Dispatch runs on the loop's async context (it may `session.lock().await`),
but **only while not `busy`** (see Risks: commands are disabled during an in-flight
turn — the palette won't open, or accept is a no-op with a "busy" hint).

**Session swap mechanics.** The loop owns `let session: Arc<Mutex<Session>>`. To
*replace* the session (for `/resume`, `/new`) without rebuilding the loop:

```rust
// dispatch (loop side), not busy:
let mut guard = session.lock().await;
*guard = build_session_for(action, &*guard).await?;   // new Session, same Arc cell
```

`build_session_for` reuses `Session::resume(store, provider, router, tools, presenter, config, id)`
or `Session::start(...)`. **Problem:** `Session` owns `presenter: Box<dyn Presenter>`
(the `ChannelPresenter` with the live `tx`). We must move the existing presenter into
the new Session so events keep flowing to the loop. Add
`Session::reseat(self, new_id_or_resume)` OR a constructor variant that takes an
existing `Box<dyn Presenter>`. Chosen: a `Session::fork_resume`/`fork_new` that
consumes the old Session and returns a new one carrying the *same* presenter,
store, provider, router, tools, config — only `id`/`transcript`/`seq` change. The
loop does `let old = std::mem::replace(&mut *guard, placeholder); *guard = old.into_resumed(id)?;`
(use a small `Option<Session>` swap to avoid a placeholder; details in PR1).

**Redraw transcript on resume.** After swapping in a resumed Session, the loop must
repaint the prior conversation into scrollback: `Session::resume` already rehydrates
`transcript`; expose `session.transcript_lines()` (or reuse the store's
`load_messages`) → render each message to `Vec<Line>` via the existing
message-rendering in `app`/`render`, then `tui.insert_lines(...)` once. `/clear`
clears the *screen* (insert a clear / reset the viewport scrollback region) without
touching the store or transcript.

### Session commands (PR1)

| Command | Action | Behavior |
|---------|--------|----------|
| `/help` | `Help` | Open the palette listing all commands (name — desc). |
| `/sessions` | `ListSessions` | Animated picker over `store.list_sessions()`: rows `id8  $cost  N msgs  age  preview`, `↑↓` select, `Enter` resumes, `Esc` cancels. |
| `/resume <prefix>` | `Resume(prefix)` | `matching_session_ids(prefix)` → unique → swap to resumed Session + redraw transcript. Ambiguous/none → warning. No prefix → same as `/sessions`. |
| `/new` | `New` | `Session::start` fresh (same cwd/config), swap in, clear screen. |
| `/clear` | `ClearScreen` | Clear on-screen scrollback; session + transcript untouched. |

The session picker is the same inline-list widget as the palette (shared
`SelectList` UI component) parameterized by rows — write once, reuse for commands,
sessions, and (PR2) checkpoints.

### Conversation checkpoints + /undo (PR2)

**Model.** Every user prompt starts a *turn* at a known `seq`. A checkpoint marks a
turn boundary. `/undo` rewinds to the **previous user turn**: drop the current
turn's messages.

**Soft-delete, not hard-delete** (chosen — enables `/redo` later and keeps an audit
trail). Add an `active` flag (default 1) to `message`; rewind sets `active=0` for
messages with `seq >= boundary`. `load_messages`/`run_turn` read only `active=1`.
`seq` resets to the boundary so the next turn overwrites cleanly (new rows; the
inactivated ones remain for redo until a new turn supersedes them).

```sql
ALTER TABLE message ADD COLUMN active INTEGER NOT NULL DEFAULT 1;   -- idempotent migration

CREATE TABLE IF NOT EXISTS checkpoint (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
    label       TEXT,                       -- NULL = auto (per-turn)
    seq         INTEGER NOT NULL,           -- transcript boundary (messages with seq < this are kept)
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
```

- `forge-core`: `Session::rewind_to(seq)` truncates `self.transcript` to `< seq`,
  resets `self.seq`, and calls `store.deactivate_messages_from(&id, seq)`.
- `/undo` → boundary = the seq of the last user message; `rewind_to(that)`. The loop
  then **collapses scrollback** for the removed turn (re-render: clear + reinsert the
  surviving transcript, cheap for a chat).
- `/checkpoint [name]` → `store.add_checkpoint(session, label, self.seq)`.
- `/checkpoints` → picker (shared `SelectList`); `Enter` → `rewind_to(seq)`.

### Code checkpoints + restore (PR3) — shadow snapshots

**Hook point.** In `run_turn`, immediately before executing a tool whose
`side_effect == Write` (write_file/edit_file), snapshot the **target path** (from the
tool's `args.path`) into `.forge/checkpoints/<session>/<seq>/` *once per path per
turn* (first touch wins — preserves the pre-turn content). A per-turn `manifest.json`
records each path's prior state:

```json
{ "seq": 7, "files": [
  { "path": "src/main.rs", "status": "modified", "blob": "src__main.rs.0" },
  { "path": "src/new.rs",   "status": "created" }            // no blob: created this turn
]}
```

- `status: modified` → copy current bytes to `blob` before the edit; restore = copy
  blob back. `status: created` (path didn't exist pre-turn) → no blob; restore =
  delete the file.
- Path discovery: read `call.args["path"]` for the write tools (already how the
  permission/diff layer finds targets). Only files actually about to be written are
  snapshotted → cost = changed files only, not the tree.
- `/undo` (PR3-aware) restores the latest turn's snapshot (files) **and** rewinds the
  conversation (PR2) in one action — "undo the last turn" = both. `/undo --code` /
  `/undo --chat` can scope it (stretch).
- Snapshots live under `.forge/` (already git-ignored via `.forge/forge.db` etc; add
  `.forge/` to the project `.gitignore` if not already). Pruned with the session.

**Off the blocking path.** Snapshotting is small synchronous file I/O inside
`run_turn` (already on the turn task, not the render loop) — fine. Restore on `/undo`
runs in `dispatch_command` while not busy; for large restores, wrap in
`tokio::task::block_in_place` (consistent with the freeze lesson) so the loop ticks.

## Alternatives considered

### Git-backed code checkpoints — REJECTED (user decision)
Auto-commit/stash per turn to a hidden ref. **Why rejected:** mutates the user's git
state (index/refs), requires a repo, and can collide with the user's own
commits/staging — surprising and unsafe for a tool that must "run anywhere." Shadow
snapshots are self-contained and git-agnostic. (Captured for the record; not revisited.)

### Modal full-screen palette — REJECTED
A centered overlay (alternate screen) for the palette. **Why rejected:** the TUI is
deliberately inline (conversation stays in native scrollback); an alternate screen
breaks that and flickers. Inline palette above the input matches the design.

### Hard-delete messages on undo — REJECTED
Simpler, but forecloses `/redo` and loses the audit trail. Soft `active` flag costs
one column + a `WHERE active=1` and keeps the door open.

### Do nothing — REJECTED
Leaves the harness command-less and undo-less; undercuts adoption of the auto-edit
temper (no safety net) and the whole "beautiful, capable TUI" goal.

## Risks and mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Session swap races an in-flight turn (the turn task holds the `Mutex`; swap would deadlock or corrupt) | High | High | Commands that swap/rewind are **disabled while `busy`**: the palette opens but accept shows "finish or Esc the current turn first". Dispatch only runs when `!busy`. The swap takes the same `Mutex` the turn holds, so it naturally serializes; we gate in the loop to avoid waiting. |
| Re-introducing a blocking recv on the loop thread | Medium | High | Command dispatch uses async `session.lock().await`; any heavy sync work (large file restore) uses `block_in_place`. No `std::sync::mpsc::recv` on the loop. Covered by the existing deadlock regression test pattern. |
| Snapshot disk growth | Medium | Low | Only changed files copied; prune snapshots older than N turns / on session delete; document `.forge/checkpoints` size. |
| Restoring a file the user edited manually after the turn | Medium | Medium | `/undo` warns when a target's on-disk bytes differ from what Forge last wrote (hash mismatch) before overwriting; user confirms. |
| Resume redraw cost for very long sessions | Low | Low | Cap the redrawn scrollback to the last K messages (older ones summarized as "… N earlier messages"); full history stays in the store. |
| Palette intercepts a legitimate prompt starting with `/` | Low | Low | Palette only arms when the line *is* a command-like token; `Esc` dismisses and submits literally; a leading `//` escapes to a literal `/` prompt. |

## Security considerations

No new network surface. Checkpoints write under `.forge/` (local, same trust domain
as the SQLite store). `/resume <prefix>` only resolves ids that exist in the local
store (no traversal). Shadow snapshots copy file *contents* into `.forge/checkpoints`
— same data the user already has on disk; document that `.forge/` may contain
snapshots of edited files (don't commit it). No git mutation → no risk to the user's
VCS history.

## Operational considerations

`.forge/` must be git-ignored (add if missing). Snapshot pruning is the only new
housekeeping. No daemons, no background threads (the render loop already exists).

## Performance considerations

- Palette filtering is over a static in-memory registry — O(commands) per keystroke,
  negligible.
- `/sessions` / `/checkpoints` do one indexed SQLite query on open (not per frame).
- Snapshots copy only the files a turn actually writes.
- The 16fps loop is untouched; all command I/O is either async (`lock().await`) or
  `block_in_place`, so the spinner/animation never stalls (the #45 invariant).

## Phased rollout (the 3 PRs)

**PR1 — Command framework + session commands.**
DoD: typing `/` opens an animated inline palette; `↑↓/Tab/Enter/Esc` work; `/help`,
`/sessions` (picker), `/resume <prefix>`, `/new`, `/clear` function in a live TUI
**without restarting**; resumed transcript redraws into scrollback; commands disabled
during a busy turn; a non-`/` line still submits as a prompt; `parse_command` +
filtering unit-tested; a Session-swap test (resume replaces the active session,
transcript rehydrates); `cargo test --workspace` + clippy `-D warnings` clean.

**PR2 — Conversation checkpoints + /undo.**
DoD: `message.active` migration; `checkpoint` table; `Session::rewind_to`;
`/undo` rewinds to the previous user turn and collapses scrollback; `/checkpoint
[name]` + `/checkpoints` picker restore; soft-delete verified (rewind then a new turn
supersedes; inactive rows excluded from `load_messages`); tests for rewind boundary +
exclusion; gate behind `!busy`.

**PR3 — Code checkpoints + restore.**
DoD: pre-write shadow snapshot with manifest; `/undo` restores files (modified→prior
bytes, created→deleted) and rewinds the conversation together; hash-mismatch warning;
snapshot only changed files; tests for snapshot/restore round-trip incl create+delete;
`.forge/checkpoints` documented + pruned.

## Spike checklist (de-risk PR1 before full build)

1. Render an inline palette list in the existing viewport, filtered by query, with
   `↑↓` selection + ease-in `anim`, via `TestBackend` snapshot test.
2. Prove a **Session swap mid-TUI**: from a running `run_chat_tui`, dispatch
   `Resume(id)` → `*session.lock().await = old.into_resumed(id)?` carrying the same
   `ChannelPresenter` → a subsequent prompt runs against the resumed session and its
   events still reach the loop. (No process restart.)
3. Confirm dispatch never blocks the loop (lock().await only; no sync recv).

## Decision log

| Date | Decision | Rationale |
|------|----------|-----------|
| 2026-06-15 | 3 sequenced PRs | User chose "both, sequenced". |
| 2026-06-15 | Shadow snapshots for code checkpoints | User decision; git-safety + runs-anywhere. |
| 2026-06-15 | Inline palette (not modal) | Matches inline-scrollback TUI. |
| 2026-06-15 | Soft `active` flag for undo | Enables redo + audit; cheap. |
| 2026-06-15 | Commands gated while busy | Avoids Session-swap race with the turn task. |
| 2026-06-15 | PR2 + PR3 shipped together | `/undo` = rewind chat **and** restore files in one action — shipping conversation-undo without code-restore would be a half-undo. Combined in one PR. |
| 2026-06-15 | `/sessions` `/resume` `/checkpoints` are interactive pickers, not text lists | User: "all commands as interactive as possible." A shared animated, filter-narrowed `Picker` widget replaces the text dump; `Resume(prefix)` pre-fills the filter. Enter resumes/rewinds + redraws the transcript. |
| 2026-06-15 | Picker lives in the fixed-height inline live region (scrolls, doesn't grow) | ratatui inline viewports can't resize at runtime; the picker scrolls a 3-row window with a heading + position counter. Beauty comes from animation + formatting, not height. |

## Post-build status (2026-06-15)

Built on branch `feat/checkpoints-undo-and-pickers`. **PR2 + PR3 + interactive pickers** complete:
- Store: `message.active` soft-delete migration + `checkpoint` table; `deactivate_messages_from`,
  `add_checkpoint`, `list_checkpoints`; `load_messages` filters `active = 1`.
- Core: `Session::rewind_to(seq)` / `undo()` / `checkpoint(label)` / `checkpoints()`; pre-write
  shadow snapshots (`snapshot` module) hooked in `invoke_tool`, restored on rewind; hash-mismatch
  warning when a file changed since Forge wrote it.
- TUI: shared animated `Picker` (`Sessions` / `Checkpoints` kinds) + `CommandAction::{Undo,
  Checkpoint, ListCheckpoints}`; `/undo /checkpoint /checkpoints /sessions /resume` wired in the
  render loop (modal key routing, gated while busy, `lock().await` only — #45 invariant honored).
- Tests: store soft-delete/checkpoint round-trips; core undo + checkpoint-rewind + a file-restore
  integration; picker state + render + 0-height guard; a `#[ignore]`d real-pty e2e
  (`crates/forge-cli/tests/tui_e2e.rs`) that saves + restores a checkpoint through the live TUI.

## References

- PR #45 — TUI freeze fix (the `block_in_place` invariant this design must respect).
- `docs/features/auto-discovery-mesh.md`, `docs/features/ask-user-question.md`.
</content>
