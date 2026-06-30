//! Runtime keybind matching: map a `KeyCombo` from config to a crossterm `KeyEvent`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use forge_config::{KeyCombo, KeybindsConfig};

use crate::app::KeyKind;

/// Returns true when `ev` matches `combo`.
pub fn matches(combo: &KeyCombo, ev: &KeyEvent) -> bool {
    let code_matches = match combo.key.as_str() {
        "up" => ev.code == KeyCode::Up,
        "down" => ev.code == KeyCode::Down,
        "left" => ev.code == KeyCode::Left,
        "right" => ev.code == KeyCode::Right,
        "enter" => ev.code == KeyCode::Enter,
        "esc" => ev.code == KeyCode::Esc,
        "backspace" => ev.code == KeyCode::Backspace,
        "delete" => ev.code == KeyCode::Delete,
        "pageup" => ev.code == KeyCode::PageUp,
        "pagedown" => ev.code == KeyCode::PageDown,
        "home" => ev.code == KeyCode::Home,
        "end" => ev.code == KeyCode::End,
        "tab" => ev.code == KeyCode::Tab,
        "f1" => ev.code == KeyCode::F(1),
        "f2" => ev.code == KeyCode::F(2),
        "f3" => ev.code == KeyCode::F(3),
        "f4" => ev.code == KeyCode::F(4),
        "f5" => ev.code == KeyCode::F(5),
        "f6" => ev.code == KeyCode::F(6),
        "f7" => ev.code == KeyCode::F(7),
        "f8" => ev.code == KeyCode::F(8),
        "f9" => ev.code == KeyCode::F(9),
        "f10" => ev.code == KeyCode::F(10),
        "f11" => ev.code == KeyCode::F(11),
        "f12" => ev.code == KeyCode::F(12),
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap();
            ev.code == KeyCode::Char(c) || ev.code == KeyCode::Char(c.to_ascii_uppercase())
        }
        _ => false,
    };
    if !code_matches {
        return false;
    }
    let mods = ev.modifiers;
    let ctrl_ok = combo.ctrl == mods.contains(KeyModifiers::CONTROL);
    let alt_ok = combo.alt == mods.contains(KeyModifiers::ALT);
    // Shift handling. For a PLAIN char key (no ctrl/alt) the terminal encodes shift in the
    // character's case, so the SHIFT modifier is unreliable and we don't require it. But once
    // ctrl/alt is held the char is NOT case-transformed, so the SHIFT modifier is the only signal
    // — and it must be checked, otherwise `Ctrl-C` (interrupt) and `Ctrl-Shift-C` (copy_last)
    // collide and the alphabetically-first bind wins.
    let is_plain_char = combo.key.chars().count() == 1 && !combo.ctrl && !combo.alt;
    let shift_ok = if is_plain_char {
        true
    } else {
        combo.shift == mods.contains(KeyModifiers::SHIFT)
    };
    ctrl_ok && alt_ok && shift_ok
}

/// Map a configurable action name to the `KeyKind` variant it should produce.
pub fn action_to_key_kind(action: &str) -> Option<KeyKind> {
    match action {
        "interrupt" => Some(KeyKind::Esc),
        "skip_model" => Some(KeyKind::SkipModel),
        "tier_up" => Some(KeyKind::TierUp),
        "tier_down" => Some(KeyKind::TierDown),
        "toggle_reasoning" => Some(KeyKind::ToggleReasoning),
        "undo" => Some(KeyKind::UndoWrite),
        "compact" => Some(KeyKind::CompactSession),
        "model_picker" => Some(KeyKind::ModelPicker),
        "effort_cycle" => Some(KeyKind::EffortCycle),
        "temper_cycle" => Some(KeyKind::TemperCycle),
        "keybind_config" => Some(KeyKind::OpenKeybindConfig),
        "new_session" => Some(KeyKind::NewSession),
        "copy_last" => Some(KeyKind::CopyLast),
        "scroll_up" => Some(KeyKind::PageUp),
        "scroll_down" => Some(KeyKind::PageDown),
        "help" => Some(KeyKind::ShowHelp),
        "checkpoint" => Some(KeyKind::SaveCheckpoint),
        "reload" => Some(KeyKind::ReloadConfig),
        _ => None,
    }
}

/// Given a raw crossterm key event and the configured keybinds, return the matching `KeyKind`
/// if any configured action matches.
pub fn resolve_action(keybinds: &KeybindsConfig, ev: &KeyEvent) -> Option<KeyKind> {
    for (action, combo) in &keybinds.binds {
        if matches(combo, ev) {
            if let Some(kind) = action_to_key_kind(action) {
                return Some(kind);
            }
        }
    }
    None
}

/// Human-readable description of a `KeyCombo` for the configurator UI.
pub fn combo_display(combo: &KeyCombo) -> String {
    let mut parts = Vec::new();
    if combo.ctrl {
        parts.push("Ctrl");
    }
    if combo.alt {
        parts.push("Alt");
    }
    if combo.shift {
        parts.push("Shift");
    }
    let key_label = match combo.key.as_str() {
        "up" => "↑",
        "down" => "↓",
        "left" => "←",
        "right" => "→",
        "pageup" => "PgUp",
        "pagedown" => "PgDn",
        "enter" => "Enter",
        "esc" => "Esc",
        "backspace" => "Bksp",
        "delete" => "Del",
        "tab" => "Tab",
        "home" => "Home",
        "end" => "End",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        s => s,
    };
    parts.push(key_label);
    parts.join("-")
}

/// Try to parse a raw key event into a `KeyCombo`. Returns `None` for unrepresentable events.
pub fn key_event_to_combo(ev: &KeyEvent) -> Option<KeyCombo> {
    let (key, shift) = match ev.code {
        KeyCode::Char(c) => {
            let lower = c.to_ascii_lowercase().to_string();
            let is_upper = c.is_ascii_uppercase();
            (lower, is_upper)
        }
        KeyCode::Up => ("up".to_string(), false),
        KeyCode::Down => ("down".to_string(), false),
        KeyCode::Left => ("left".to_string(), false),
        KeyCode::Right => ("right".to_string(), false),
        KeyCode::Enter => ("enter".to_string(), false),
        KeyCode::Esc => ("esc".to_string(), false),
        KeyCode::Backspace => ("backspace".to_string(), false),
        KeyCode::Delete => ("delete".to_string(), false),
        KeyCode::PageUp => ("pageup".to_string(), false),
        KeyCode::PageDown => ("pagedown".to_string(), false),
        KeyCode::Home => ("home".to_string(), false),
        KeyCode::End => ("end".to_string(), false),
        KeyCode::Tab => (
            "tab".to_string(),
            ev.modifiers.contains(KeyModifiers::SHIFT),
        ),
        KeyCode::F(n) => (format!("f{n}"), false),
        _ => return None,
    };
    Some(KeyCombo {
        key,
        ctrl: ev.modifiers.contains(KeyModifiers::CONTROL),
        alt: ev.modifiers.contains(KeyModifiers::ALT),
        shift: ev.modifiers.contains(KeyModifiers::SHIFT) || shift,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// Ctrl-C must resolve to interrupt (Esc), NOT copy_last — they share key `c`+ctrl and differ
    /// only by shift. A regression here makes Ctrl-C copy instead of stopping the turn.
    #[test]
    fn ctrl_c_is_interrupt_not_copy() {
        let kb = KeybindsConfig::default();
        let plain = ev(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(resolve_action(&kb, &plain), Some(KeyKind::Esc));

        let with_shift = ev(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );
        assert_eq!(resolve_action(&kb, &with_shift), Some(KeyKind::CopyLast));
    }

    #[test]
    fn named_keys_and_modifiers_match() {
        let kb = KeybindsConfig::default();
        // Ctrl-Up → tier_up
        let up = ev(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(resolve_action(&kb, &up), Some(KeyKind::TierUp));
        // Plain Up is not a configured action
        let plain_up = ev(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(resolve_action(&kb, &plain_up), None);
        // F1 → help
        let f1 = ev(KeyCode::F(1), KeyModifiers::NONE);
        assert_eq!(resolve_action(&kb, &f1), Some(KeyKind::ShowHelp));
        // Alt-M → model_picker
        let altm = ev(KeyCode::Char('m'), KeyModifiers::ALT);
        assert_eq!(resolve_action(&kb, &altm), Some(KeyKind::ModelPicker));
    }

    /// Alt-R (reload) and Ctrl-R (toggle_reasoning) share key `r` but differ by modifier.
    #[test]
    fn ctrl_r_and_alt_r_are_distinct() {
        let kb = KeybindsConfig::default();
        let ctrl_r = ev(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(resolve_action(&kb, &ctrl_r), Some(KeyKind::ToggleReasoning));
        let alt_r = ev(KeyCode::Char('r'), KeyModifiers::ALT);
        assert_eq!(resolve_action(&kb, &alt_r), Some(KeyKind::ReloadConfig));
    }

    #[test]
    fn round_trips_through_combo() {
        let e = ev(KeyCode::Char('k'), KeyModifiers::CONTROL);
        let combo = key_event_to_combo(&e).unwrap();
        assert_eq!(combo.key, "k");
        assert!(combo.ctrl && !combo.alt && !combo.shift);
        assert!(matches(&combo, &e));
        assert_eq!(combo_display(&combo), "Ctrl-k");
    }
}
