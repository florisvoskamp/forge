# Feature: context compaction (`/compact`)

> **Status: MVP shipped.** `/compact` summarizes the older part of the transcript into one
> system message via a cheap model call, shrinking the live context sent on subsequent turns.
> Pairs with the context-window gauge (tui-token-counter.md), which shows when to do it.

## 1. Problem (JTBD)
> When a session gets long, I want to fold the early history into a summary so I stop paying to
> resend it every turn and don't overflow the model's context window — without losing the
> decisions and facts that matter.

The gauge surfaces the fill level; compaction is the action that lowers it.

## 2. Scope (MoSCoW)
**Must have (shipped)**
- `/compact` (TUI command + palette entry) summarizes all but the most recent
  `COMPACT_KEEP_RECENT` (6) messages into a single `Role::System` summary, prepended ahead of
  the kept tail. No-op when there are fewer than `KEEP_RECENT + COMPACT_MIN_OLDER` messages.
- The summary is produced by one **trivial-tier** model call (cheap, mesh-routed) with a fixed
  system prompt that preserves decisions, facts, file paths, names, and open threads.
- Runs as a **background task** like a turn (the spinner ticks; doesn't block the render loop).

**Deferred**
- **Auto-trigger** when the context gauge crosses a threshold (manual `/compact` only for now).
- **Persisting** the compacted view: compaction edits the *live* in-memory transcript only; the
  full history stays in the store, so a resumed session reloads the uncompacted transcript.
- Pinning/protecting specific messages; configurable keep-count; summary-of-summaries.

## Non-goals
- No change to cost math, the agent loop, or persistence schema. Compaction only reshapes the
  in-memory `transcript` that the next turn sends.

## 3. Acceptance criteria
```
Given a transcript longer than KEEP_RECENT + COMPACT_MIN_OLDER
When /compact runs
Then the older messages become one system summary, the recent KEEP_RECENT are kept verbatim,
 and transcript length drops to KEEP_RECENT + 1

Given a short transcript
When /compact runs
Then it is a no-op (no model call, length unchanged)

Given /compact is invoked
When it runs
Then it runs in the background (spinner animates) and emits a "compacted N → M" note
```

## 4. Design
`Session::compact()` (forge-core): splits the transcript at `len - COMPACT_KEEP_RECENT`, renders
the older messages as `role: content` text, routes a trivial-tier model
(`route_hinted(..., Some(Trivial))`), calls `provider.complete` once with a fixed
summary system prompt, then sets `transcript = [system summary, ...recent]`. Returns
`(before, after)` and emits a `Warning` note. `/compact` → `CommandAction::Compact` →
`DispatchOutcome::RunCompact` → `spawn_compact` (background task, busy/done machinery), gated
while a turn is in flight.

## 5. Definition of done
- [x] `Session::compact()` folds older → summary, keeps recent, no-op when short.
- [x] Trivial-tier model call; fixed information-preserving prompt.
- [x] `/compact` command + palette entry; runs as a background task.
- [x] Unit tests (fold + no-op); `cargo fmt` + `clippy -D warnings` clean.
- [ ] (Deferred) auto-trigger on gauge threshold; persist across resume.
