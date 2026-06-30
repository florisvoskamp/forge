# Feature: configurable keybinds + interactive configurator

> **Status: shipped.** Every TUI action key is remappable via `[keybinds.binds]` in
> `config.toml`. An in-app configurator (Ctrl-,) edits binds live, persists them per-action, and
> resets to defaults. Production defaults cover interrupt, tier control, model skip, reasoning
> toggle, undo, compact, model/effort/temper cycling, copy, scroll, help, checkpoint, new session.

## 1. Problem (JTBD)

> When I drive Forge in the terminal, I want the keyboard shortcuts to match my muscle memory and
> not collide with my terminal/multiplexer — so I can rebind any action without recompiling.

## 2. Config surface

`[keybinds.binds]` maps an **action name** to a **key combo**. A combo is a `key` plus modifier
flags. `key` is a single character (`"c"`, `"k"`, `","`) or a named key: `up`, `down`, `left`,
`right`, `enter`, `esc`, `backspace`, `delete`, `pageup`, `pagedown`, `home`, `end`, `tab`,
`f1`–`f12`.

```toml
[keybinds.binds.toggle_reasoning]
key = "x"
ctrl = true
alt = false
shift = false
```

Overrides **deep-merge** over the built-in defaults — writing one bind never unbinds the others.

### Default binds

| Action | Default | Notes |
|---|---|---|
| `interrupt` | Ctrl-C | Stop current turn (idle: quit) |
| `command_palette` | / | Open slash-command palette |
| `skip_model` | Ctrl-K | Mid-turn: abort so you can retry on another model |
| `tier_up` | Ctrl-↑ | Escalate tier (placeholder note for now) |
| `tier_down` | Ctrl-↓ | De-escalate tier (placeholder note for now) |
| `toggle_reasoning` | Ctrl-R | Show/hide reasoning blocks inline |
| `undo` | Ctrl-Z | Undo last file write (`/undo`) |
| `compact` | Ctrl-L | Compact/summarize conversation (`/compact`) |
| `model_picker` | Alt-M | Open model picker overlay |
| `effort_cycle` | Alt-E | Cycle effort low→medium→high→xhigh→unset |
| `temper_cycle` | Alt-T | Cycle temper/mode |
| `keybind_config` | Ctrl-, | Open this configurator |
| `new_session` | Ctrl-N | Start a fresh session (`/new`) |
| `copy_last` | Ctrl-Shift-C | Copy last response to clipboard (`/copy`) |
| `scroll_up` | PageUp | Scroll transcript up |
| `scroll_down` | PageDown | Scroll transcript down |
| `help` | F1 | Show keybind reference overlay |
| `checkpoint` | Ctrl-S | Save session checkpoint (`/checkpoint`) |
| `reload` | Alt-R | Hot-reload config (incl. keybinds) |

## 3. Implementation

- **`forge-config`** — `KeyCombo`, `KeybindsConfig` (`BTreeMap<String, KeyCombo>`), a `Default`
  with the 19 binds above, and `write_keybind()` (rewrites one bind under `[keybinds.binds]`,
  preserving all other config keys). `Config` gains a `keybinds` field.
- **`forge-tui/keybinds.rs`** — `matches(combo, key_event)` compares a combo against a crossterm
  `KeyEvent`; `resolve_action()` returns the `KeyKind` for the first matching configured action.
  Shift is checked explicitly once Ctrl/Alt is held, so Ctrl-C (interrupt) and Ctrl-Shift-C
  (copy) don't collide. `key_event_to_combo()` captures a fresh binding in the configurator.
- **`forge-tui/keybind_configurator.rs`** — a fullscreen takeover (run via `Tui::run_fullscreen`)
  listing actions, current binding, and description; rebind (Enter → capture), reset one (r),
  reset all (R/Shift-R), save (s/Esc), discard (q).
- **`forge-tui/driver.rs`** — `poll_event()` calls `resolve_action()` **before** the static
  defaults, so configured binds win. The `Tui` carries the loaded `KeybindsConfig`.
- **`forge-cli/run.rs`** — the chat loop dispatches each new `KeyKind`. Global action keys
  (toggle reasoning, configurator, help, effort cycle, copy, reload, tier up/down, skip model)
  work in any state; the rest run idle, several by dispatching the equivalent slash command.

## 4. Notes / limits

- A configured plain-character bind with no modifiers will fire while typing — keep action binds
  modified (Ctrl/Alt) or use named keys.
- `tier_up`/`tier_down` currently emit a placeholder note; the routing-tier pin is future work.
- Because `resolve_action` runs first, Ctrl-K maps to `skip_model` (not the old kill-line-forward)
  and Ctrl-R to `toggle_reasoning` by default; remap them if you want the editing shortcut back.
