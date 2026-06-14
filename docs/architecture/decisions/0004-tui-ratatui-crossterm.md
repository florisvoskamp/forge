# ADR-0004: TUI built on ratatui + crossterm, with a headless mode

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

FR-6 requires a "beautiful" interactive terminal UI (live agent progress, cost meter,
routing decisions) plus a plain non-interactive mode for scripting/pipes (NFR: usability,
portability across Linux/macOS/Windows). The blueprint names ratatui.

Facts as of 2026-06: ratatui 0.30.1 (2026-06-05) is the current, maintained standard for
Rust TUIs; crossterm 0.29.0 is its default cross-platform backend (the one that works on
Windows as well as Unix).

## Options considered

1. **ratatui + crossterm** — the de-facto Rust TUI stack; immediate-mode rendering;
   cross-platform via crossterm; rich widget set; async examples target tokio. Cons:
   immediate-mode means we own app state/layout; 0.x version (minor churn).
2. **cursive** — higher-level, retained-mode TUI. Cons: smaller ecosystem, less control
   over custom live widgets (cost meter, streaming panes), less momentum than ratatui.
3. **Plain stdout only (no TUI)** — simplest. Cons: fails FR-6's core "beautiful TUI"
   differentiator outright.

## Decision

Use **ratatui 0.30** with the **crossterm 0.29** backend for the interactive TUI. Keep all
rendering behind a `forge-tui` crate and drive it from the session core via a
**presenter/event abstraction**, so a **headless renderer** (line-based stdout/JSON) can
satisfy the scripting/pipe mode without the core knowing which surface is attached.

## Rationale

ratatui+crossterm is the only choice that satisfies both "beautiful + live" and
"cross-platform incl. Windows". Putting the core behind a presenter interface (not calling
ratatui directly) means TTY-vs-pipe, and future surfaces, are a swap at one seam — serving
the maintainability NFR and the headless requirement together.

## Consequences

- **Positive:** Rich live UI; Windows support via crossterm; headless mode falls out of
  the presenter seam; testable core (no terminal needed to test session logic).
- **Negative / trade-offs accepted:** We manage UI state ourselves (immediate mode);
  tracking a 0.x UI dependency.
- **Follow-ups:** Define the presenter event types (token stream, tool start/finish, cost
  update, routing decision, permission prompt) in the core, consumed by both renderers.
