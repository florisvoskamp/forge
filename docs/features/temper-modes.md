# Feature: Temper — SHIFT+TAB operating-mode switcher

> Status: **DESIGN + BUILD** (2026-06-15).
>
> An interactive switcher that cycles Forge's operating **temper** — how forward-leaning the
> agent is allowed to act — live in the TUI with **SHIFT+TAB**, shown in the statusline.
> "Temper" is the forge/metallurgy framing (heat-treatment hardness *and* disposition/restraint),
> and it gives Forge a distinct, unique vocabulary instead of Claude Code's "permission mode".

---

## 1. Problem (JTBD)

> **As** a developer mid-session, **I want** to flip between "just look / plan", "ask me first",
> and "go ahead and edit" **with one keystroke**, **so that** I can tighten or loosen the leash
> as the task changes without restarting Forge or editing config.

Today the permission mode is fixed at session start (`config.permission_mode` / `--mode`). The
four modes already exist in the engine (`PermissionMode::{Default, AcceptEdits, Bypass, Plan}`,
ADR-0008) but there is **no way to change it during a session** and **no on-screen indicator**
of which is active. Power users expect Claude-Code-style SHIFT+TAB cycling.

---

## 2. The temper vocabulary

Internal enum variants and config/CLI keys are **unchanged** (back-compat; security-critical
names stay stable). Temper is a **display + UX layer** plus runtime switching. Canonical key
(accepted by `--mode`/config) → temper label → meaning:

| Canonical key | Temper label | Behaviour | In SHIFT+TAB cycle? |
|---------------|--------------|-----------|---------------------|
| `plan` | **Survey** | Read-only: investigate & propose, **no** side effects (hard contract). | yes |
| `default` | **Guarded** | Ask before each side effect. | yes |
| `accept-edits` | **Smith** | Auto-apply file edits; still ask for shell. | yes |
| `bypass` | **Unfettered** | All permission guards off (the unoverridable safety denylist still applies). | **no — explicit opt-in only** |

`--mode` and `[permission] mode = …` accept **either** the canonical key **or** the temper
label (e.g. `--mode survey` == `--mode plan`). Survey/Guarded/Smith/Unfettered are themed but
each keeps an obvious, unambiguous meaning — clarity wins over cuteness for a security control.

**Cycle order** (SHIFT+TAB): `Guarded → Smith → Survey → Guarded …`. Unfettered is deliberately
**excluded** from the cycle: landing on "all guards off" by tapping a key is a footgun, so it is
reachable only via `--mode unfettered` / config. (Claude Code likewise never cycles into bypass.)

**Reserved:** `Assay` (read-only analysis crew) is the next feature and will slot in beside
Survey as a read-only sibling. Not wired into the cycle this turn.

---

## 3. Scope (MoSCoW)

**Must**
- M1 — SHIFT+TAB in the chat TUI cycles temper through Guarded → Smith → Survey (wrapping).
- M2 — the active temper is shown in the statusline (themed label) at all times.
- M3 — switching takes effect on the **next** turn's tool calls (the permission engine reads the
  session's current mode); a switch mid-flight is only allowed when **idle** (not during a turn).
- M4 — each switch leaves a one-line scrollback note (`temper → Smith`) so history is legible.
- M5 — `--mode` / config accept the temper labels as aliases for the canonical keys.

**Should**
- S1 — persist the new mode to the session row so `forge sessions` / resume reflect it.

**Won't (this turn)**
- Assay temper (next feature). Cycling into Unfettered. Headless/`forge run` switching (no TUI).

---

## 4. Acceptance criteria

```
Given an idle chat-TUI session in Guarded temper
When the user presses SHIFT+TAB
Then the statusline shows "Smith" and a "temper → Smith" line is added to scrollback
And the next turn auto-applies file edits without a permission prompt

Given temper Smith
When the user presses SHIFT+TAB twice
Then temper is Survey (Smith → Survey), and a write tool in the next turn is denied (read-only)

Given temper Survey
When the user presses SHIFT+TAB
Then temper wraps to Guarded (never to Unfettered)

Given a turn is running (busy)
When the user presses SHIFT+TAB
Then the temper does not change (no mid-turn switch); the keystroke is ignored

Given `forge run --mode survey`
Then it behaves exactly as `--mode plan` (read-only)
```

---

## 5. Design / impact

| Layer | File | Change |
|-------|------|--------|
| Types | `forge-types/src/lib.rs` | `PermissionMode::label()` (themed), `cycle_next()` (safe 3-cycle, excludes Bypass), serde `alias` for the temper labels |
| Store | `forge-store/src/lib.rs` | `update_session_mode(id, mode)` (S1 persistence) |
| Core | `forge-core/src/lib.rs` | `Session::temper()` getter, `set_mode`, `cycle_temper() -> PermissionMode` (advance + persist) |
| TUI | `forge-tui/src/app.rs` | `KeyKind::CycleTemper`; `App.temper` label; render in `render_statusline`; `set_temper()` (label + scrollback note) |
| TUI | `forge-tui/src/driver.rs` | map `KeyCode::BackTab` → `KeyKind::CycleTemper` |
| CLI | `forge-cli/src/main.rs` | chat-TUI loop: on `CycleTemper` when idle, `session.cycle_temper()` + `app.set_temper()`; `--mode` value aliases |

**Vertical slice:** SHIFT+TAB → driver yields `KeyKind::CycleTemper` → (idle only) main locks the
session, `cycle_temper()` advances the mode, persists it, returns the new `PermissionMode` →
`app.set_temper(mode)` updates the statusline label + queues a scrollback note → next
`run_turn` reads `self.mode` in `permission::decide` as before.

**Why runtime mode lives on `Session`:** the permission engine already reads `self.mode`
(`forge-core/src/lib.rs` `invoke_tool`). Making `mode` mutable + adding a setter is the whole
mechanism; no change to `permission::decide`.

---

## 6. Definition of done

- [ ] All §4 acceptance criteria pass (unit + TUI render tests).
- [ ] `PermissionMode::label()`/`cycle_next()` unit-tested incl. the Bypass exclusion + wrap.
- [ ] `--mode survey|guarded|smith|unfettered` accepted (alias) and equal to canonical.
- [ ] Statusline shows the themed temper; SHIFT+TAB note appears in scrollback.
- [ ] Switch persisted to the session row.
- [ ] fmt + clippy `-D warnings` + full workspace green.
