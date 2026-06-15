# Feature: AskUserQuestion — interactive questions the agent can ask mid-task

> Status: **DESIGN + BUILD** (2026-06-15).

## 1. Problem (JTBD)

> **As** the Forge agent, **I want** to pause and ask the user a focused question with good
> suggested answers, **so that** I resolve a genuine fork (a value choice, a missing decision)
> instead of guessing — the way Claude's AskUserQuestion works.

Today the only interactive prompt is the **permission** y/n (`Presenter::confirm`). The agent has
no way to ask the user a real question with curated options mid-turn.

### Non-goals
- A general forms engine. Multi-question wizards. Rich widgets beyond a single-select list with
  an optional open-ended answer. (One question per call; the model can ask again.)

## 2. Scope (MoSCoW)

**Must**
- M1 — A core-owned **virtual tool** `ask_user` (same mechanism as `spawn_agents`: it needs the
  presenter, which ordinary tools can't reach). Schema: `{ question: string, options: [{label,
  description?}], allow_other?: bool }`.
- M2 — **Interactive TUI**: render the question + a numbered/﻿arrow-selectable option list; Enter
  selects; if `allow_other` (default true) a final "Other…" entry lets the user type a free
  answer. Returns the chosen label (or the typed text) to the agent as the tool result.
- M3 — **Headless fallback**: print the question + numbered options, read a line; a number picks
  that option, anything else is the free-text answer (if allowed) else re-prompt once.
- M4 — Reuses the existing turn↔UI reply-channel plumbing (`UiMsg`/blocking `recv`) that
  `confirm()` already uses, so a question blocks only the turn task, not the render loop.

**Should**
- S1 — Good defaults: if the model omits options, treat as open-ended (free text).
- S2 — Non-interactive/headless-no-tty: return a sentinel ("(no answer — non-interactive)") so the
  agent can proceed rather than hang.

**Won't** — multiSelect, nested questions, per-option side effects.

## 3. Acceptance criteria

```
Given the model calls ask_user with a question + 3 options (allow_other default)
When rendered in the TUI and the user selects option 2
Then the tool result is option 2's label, and the turn continues with that answer

Given allow_other and the user picks "Other…" and types "use postgres"
Then the tool result is "use postgres"

Given the headless presenter and the user types "3"
Then the tool result is the 3rd option's label

Given a non-interactive (piped) session
When ask_user is called
Then it returns a non-interactive sentinel immediately (never blocks forever)
```

## 4. Design

- **Tool spec** advertised to the model alongside the registry tools (like `spawn_agents`):
  `ask_user`. `Session::invoke_tool` intercepts `call.name == "ask_user"`, parses
  `{question, options, allow_other}`, calls `self.presenter.ask(...)`, returns the answer string
  as the tool result (recorded as a tool_call for transcript fidelity).
- **Presenter trait** gains:
  `fn ask(&mut self, q: &str, options: &[QChoice], allow_other: bool) -> String;`
  - `HeadlessPresenter`: numbered list + `read_line`; number→label, else free text (if allowed).
    Non-tty → sentinel.
  - `ChannelPresenter` (TUI): send `UiMsg::Question { question, options, allow_other, reply:
    Sender<String> }`; block on `reply.recv()` (mirrors `confirm()`).
- **TUI app/loop**: a `Question` view (like the permission bar but a list). `App` holds an
  optional `question: Option<QuestionView>` with cursor state; keys: digits/↑↓ move, Enter
  selects, on "Other…" switch to text entry (reuse the input line). On submit, send the reply
  over the channel and clear the view.
- **QChoice** type in forge-tui (label + optional description), shared with the tool's parsed args.

### Impact
| Layer | Change |
|-------|--------|
| forge-tui | `PresenterEvent`/`UiMsg::Question`; `Presenter::ask`; `App` question view + render + key handling; `QChoice` |
| forge-core | `ask_user` virtual tool spec + `invoke_tool` intercept → `presenter.ask` |
| forge-cli | TUI loop: handle `UiMsg::Question` (show selector, capture answer, reply) |

## 5. Definition of done
- [ ] §3 criteria pass (TUI select, Other→free text, headless number/text, non-interactive sentinel).
- [ ] `ask_user` advertised to the model; intercepted in core; result flows back into the turn.
- [ ] Reuses the reply-channel pattern (no render-loop blocking).
- [ ] fmt + clippy `-D warnings` + green; TUI render tested via TestBackend.
