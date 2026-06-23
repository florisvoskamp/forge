//! The friendly `/config` editor overlay: settings grouped into labeled sections, each shown with a
//! human label and a type-appropriate control — bools toggle, enums cycle, numbers/text/secrets are
//! typed. Fuzzy search filters across all of them; a detail line shows help, type, default, and
//! source for the selected row. State, input handling, and rendering live here (no `forge-config`
//! dependency); the I/O shell feeds it rows and performs the validated writes via [`ConfigAction`].

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::KeyKind;

use ratatui::style::Color;
const ORANGE: Color = Color::Rgb(255, 145, 60);
const DIM: Color = Color::Rgb(110, 110, 120);
const USER: Color = Color::Rgb(125, 180, 255);
const OKGREEN: Color = Color::Rgb(120, 210, 140);
const WARN: Color = Color::Rgb(235, 120, 110);
const VERY_DIM: Color = Color::Rgb(80, 80, 90);

/// The editing control a row uses.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum RowKind {
    Bool,
    Int,
    Float,
    /// A fixed set of valid values, cycled with ←/→.
    Enum(Vec<String>),
    #[default]
    Text,
    /// A secret (API key): masked, edits route to the keyring.
    Secret,
}

/// One editable setting row (the TUI-side mirror of `forge_config::SettingDescriptor`).
#[derive(Debug, Clone, Default)]
pub struct SettingRow {
    pub path: String,
    pub group: String,
    pub label: String,
    pub help: Option<String>,
    pub kind: RowKind,
    /// Current value, rendered.
    pub value: String,
    /// Built-in default, rendered (for the detail line).
    pub default: String,
    /// Overridden from a config file.
    pub modified: bool,
    /// "user" | "project" | "default".
    pub source: String,
}

impl SettingRow {
    fn is_secret(&self) -> bool {
        self.kind == RowKind::Secret
    }
    /// The next/previous enum value, or `None` for non-enum rows.
    fn cycled(&self, forward: bool) -> Option<String> {
        let RowKind::Enum(opts) = &self.kind else {
            return None;
        };
        if opts.is_empty() {
            return None;
        }
        let cur = opts.iter().position(|o| o == &self.value).unwrap_or(0);
        let n = opts.len();
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        Some(opts[next].clone())
    }
}

/// What the I/O shell should do after a keystroke.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigAction {
    None,
    /// Persist `value` at `path` (config.toml, or the keyring for secrets), then refresh.
    Save {
        path: String,
        value: String,
    },
    /// Reset `path` to its default (clear the override), then refresh.
    Reset {
        path: String,
    },
    /// Scope toggled — reload rows.
    Reload,
    /// Editor closed.
    Close,
}

/// The `/config` editor overlay state.
#[derive(Debug, Clone, Default)]
pub struct ConfigEditor {
    pub open: bool,
    /// false = user config, true = project.
    pub project_scope: bool,
    pub rows: Vec<SettingRow>,
    pub filter: String,
    /// Index into the FILTERED row list.
    pub selected: usize,
    /// `Some(buffer)` while typing a number/text/secret value.
    pub editing: Option<String>,
    pub status: Option<String>,
}

impl ConfigEditor {
    pub fn open_with(&mut self, rows: Vec<SettingRow>) {
        self.open = true;
        self.rows = rows;
        self.filter.clear();
        self.selected = 0;
        self.editing = None;
        self.status = None;
    }

    /// Row indices passing the fuzzy filter (matched on label OR path), in order.
    pub fn matches(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.rows.len()).collect();
        }
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                fuzzy_subseq(&self.filter, &r.label) || fuzzy_subseq(&self.filter, &r.path)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_row(&self) -> Option<&SettingRow> {
        let m = self.matches();
        m.get(self.selected).map(|&i| &self.rows[i])
    }

    fn clamp(&mut self) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// Feed a keystroke; returns the I/O action. Modal: while typing a value, keys edit the buffer;
    /// otherwise bools toggle (Enter), enums cycle (←/→/Enter), numbers/text/secrets open an input,
    /// Del resets to default, Tab switches scope, typing filters.
    pub fn handle_key(&mut self, key: KeyKind) -> ConfigAction {
        if let Some(buf) = self.editing.as_mut() {
            return match key {
                KeyKind::Char(c) => {
                    buf.push(c);
                    ConfigAction::None
                }
                KeyKind::Backspace => {
                    buf.pop();
                    ConfigAction::None
                }
                KeyKind::Esc => {
                    self.editing = None;
                    ConfigAction::None
                }
                KeyKind::Enter => {
                    let value = self.editing.take().unwrap_or_default();
                    match self.selected_row() {
                        Some(r) => ConfigAction::Save {
                            path: r.path.clone(),
                            value,
                        },
                        None => ConfigAction::None,
                    }
                }
                _ => ConfigAction::None,
            };
        }
        match key {
            KeyKind::Esc => {
                self.open = false;
                ConfigAction::Close
            }
            KeyKind::Up => {
                self.selected = self.selected.saturating_sub(1);
                ConfigAction::None
            }
            KeyKind::Down => {
                let n = self.matches().len();
                if n > 0 && self.selected + 1 < n {
                    self.selected += 1;
                }
                ConfigAction::None
            }
            KeyKind::Tab => {
                self.project_scope = !self.project_scope;
                ConfigAction::Reload
            }
            // Del resets the selected setting to its default.
            KeyKind::DeleteForward => match self.selected_row() {
                Some(r) if !r.is_secret() => ConfigAction::Reset {
                    path: r.path.clone(),
                },
                _ => ConfigAction::None,
            },
            // Enum cycle (Enter/→ next, ← prev).
            KeyKind::Left | KeyKind::Right
                if matches!(self.selected_row().map(|r| &r.kind), Some(RowKind::Enum(_))) =>
            {
                let fwd = matches!(key, KeyKind::Right);
                match self.selected_row().and_then(|r| {
                    r.cycled(fwd).map(|v| ConfigAction::Save {
                        path: r.path.clone(),
                        value: v,
                    })
                }) {
                    Some(a) => a,
                    None => ConfigAction::None,
                }
            }
            KeyKind::Enter => {
                let Some(r) = self.selected_row() else {
                    return ConfigAction::None;
                };
                match &r.kind {
                    RowKind::Bool => ConfigAction::Save {
                        path: r.path.clone(),
                        value: (r.value != "true").to_string(),
                    },
                    RowKind::Enum(_) => match r.cycled(true) {
                        Some(v) => ConfigAction::Save {
                            path: r.path.clone(),
                            value: v,
                        },
                        None => ConfigAction::None,
                    },
                    // Number / text / secret → open an input (secrets start empty).
                    _ => {
                        self.editing = Some(if r.is_secret() {
                            String::new()
                        } else {
                            r.value.clone()
                        });
                        ConfigAction::None
                    }
                }
            }
            KeyKind::Backspace => {
                self.filter.pop();
                self.clamp();
                ConfigAction::None
            }
            KeyKind::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
                ConfigAction::None
            }
            _ => ConfigAction::None,
        }
    }
}

/// Case-insensitive subsequence match.
fn fuzzy_subseq(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars().map(|c| c.to_ascii_lowercase());
    for nc in needle.chars().map(|c| c.to_ascii_lowercase()) {
        if !hay.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}

/// A rendered line of the editor body: a section header, or a setting row (by row index).
enum Disp {
    Header(String),
    Row(usize),
}

/// Build the interleaved display list (section headers + the filtered rows under them).
fn display_items(ed: &ConfigEditor, matches: &[usize]) -> Vec<Disp> {
    let mut items = Vec::new();
    let mut last_group = String::new();
    for &ri in matches {
        let g = &ed.rows[ri].group;
        if g != &last_group {
            items.push(Disp::Header(g.clone()));
            last_group = g.clone();
        }
        items.push(Disp::Row(ri));
    }
    items
}

/// Render the value control for a row: `[● on]`/`[○ off]`, `‹ value ›`, masked secret, or the value.
fn value_span(r: &SettingRow, sel: bool) -> Span<'static> {
    let style = Style::default().fg(if sel { OKGREEN } else { DIM });
    match &r.kind {
        RowKind::Bool => {
            let on = r.value == "true";
            Span::styled(
                if on {
                    "[● on]".to_string()
                } else {
                    "[○ off]".to_string()
                },
                Style::default().fg(if on { OKGREEN } else { DIM }),
            )
        }
        RowKind::Enum(_) => Span::styled(format!("‹ {} ›", r.value), style),
        RowKind::Secret => Span::styled(r.value.clone(), style),
        _ => {
            let v = if r.value.is_empty() {
                "—".to_string()
            } else {
                truncate(&r.value, 30)
            };
            Span::styled(v, style)
        }
    }
}

/// Render the editor as a full-frame overlay (called last, on top of everything).
pub fn render_config_overlay(frame: &mut Frame, ed: &ConfigEditor) {
    if !ed.open {
        return;
    }
    let area = frame.area();
    if area.height < 6 || area.width < 24 {
        return;
    }
    frame.render_widget(Clear, area);
    let w = area.width as usize;
    let h = area.height as usize;
    let matches = ed.matches();
    let scope = if ed.project_scope { "project" } else { "user" };

    let mut lines: Vec<TextLine> = Vec::with_capacity(h);
    lines.push(TextLine::from(vec![
        Span::styled("  ⚒ config  ", Style::default().fg(ORANGE).bold()),
        Span::styled(
            format!("writing to {scope} scope"),
            Style::default().fg(USER),
        ),
        Span::styled("  ·  Tab switch", Style::default().fg(VERY_DIM)),
    ]));
    lines.push(TextLine::from(vec![
        Span::styled("  search ", Style::default().fg(DIM)),
        Span::styled(
            if ed.filter.is_empty() {
                "(type to filter)".to_string()
            } else {
                ed.filter.clone()
            },
            Style::default().fg(if ed.filter.is_empty() { VERY_DIM } else { USER }),
        ),
        Span::styled(
            format!("   {} settings", matches.len()),
            Style::default().fg(VERY_DIM),
        ),
    ]));
    lines.push(TextLine::default());

    // Body: interleave headers + rows; window around the selected row. Reserve 3 header + 4 footer.
    let items = display_items(ed, &matches);
    let sel_disp = items
        .iter()
        .position(|d| matches!(d, Disp::Row(ri) if matches.get(ed.selected) == Some(ri)))
        .unwrap_or(0);
    let body_h = h.saturating_sub(7).max(1);
    let start = sel_disp
        .saturating_sub(body_h / 2)
        .min(items.len().saturating_sub(body_h));
    let label_col = w.saturating_sub(40).clamp(14, 40);
    for d in items.iter().skip(start).take(body_h) {
        match d {
            Disp::Header(g) => lines.push(TextLine::from(Span::styled(
                format!("  {g}"),
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            ))),
            Disp::Row(ri) => {
                let r = &ed.rows[*ri];
                let sel = matches.get(ed.selected) == Some(ri);
                let marker = if sel { "▸" } else { " " };
                let editing_here = sel && ed.editing.is_some();
                let value = if editing_here {
                    let buf = ed.editing.as_deref().unwrap_or("");
                    let shown = if r.is_secret() {
                        "•".repeat(buf.chars().count())
                    } else {
                        buf.to_string()
                    };
                    Span::styled(
                        format!("{shown}▌"),
                        Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
                    )
                } else {
                    value_span(r, sel)
                };
                let label_style = if sel {
                    Style::default().fg(ORANGE).bold()
                } else {
                    Style::default().fg(USER)
                };
                let mods = if r.modified {
                    Span::styled(" ●", Style::default().fg(WARN))
                } else {
                    Span::raw("")
                };
                lines.push(TextLine::from(vec![
                    Span::styled(format!("    {marker} "), label_style),
                    Span::styled(
                        format!(
                            "{:<width$}",
                            truncate(&r.label, label_col),
                            width = label_col
                        ),
                        label_style,
                    ),
                    Span::raw("  "),
                    value,
                    mods,
                ]));
            }
        }
    }
    while lines.len() < h.saturating_sub(4) {
        lines.push(TextLine::default());
    }

    // Detail line: the selected setting's identity (type · default · source).
    if let Some(r) = ed.selected_row() {
        let kind = match &r.kind {
            RowKind::Bool => "on/off".to_string(),
            RowKind::Int => "number".to_string(),
            RowKind::Float => "number".to_string(),
            RowKind::Enum(o) => format!("one of: {}", o.join(" / ")),
            RowKind::Secret => "secret (keyring)".to_string(),
            RowKind::Text => "text".to_string(),
        };
        let dflt = if r.is_secret() {
            String::new()
        } else {
            format!(
                " · default: {}",
                if r.default.is_empty() {
                    "—"
                } else {
                    &r.default
                }
            )
        };
        lines.push(TextLine::from(Span::styled(
            format!(
                "  {} · {kind}{dflt} · source: {}",
                truncate(&r.path, w.saturating_sub(40).max(10)),
                r.source
            ),
            Style::default().fg(VERY_DIM),
        )));
        let help = r.help.clone().unwrap_or_default();
        lines.push(TextLine::from(Span::styled(
            format!("  {}", truncate(&help, w.saturating_sub(4))),
            Style::default().fg(USER),
        )));
    } else {
        lines.push(TextLine::default());
        lines.push(TextLine::default());
    }

    // Status (last save/reset result).
    if let Some(s) = &ed.status {
        let color = if s.starts_with('✓') { OKGREEN } else { WARN };
        lines.push(TextLine::from(Span::styled(
            format!("  {}", truncate(s, w.saturating_sub(4))),
            Style::default().fg(color),
        )));
    } else {
        lines.push(TextLine::default());
    }

    // Key hints — context-sensitive.
    let hint = if ed.editing.is_some() {
        "  type a value · Enter save · Esc cancel"
    } else {
        match ed.selected_row().map(|r| &r.kind) {
            Some(RowKind::Bool) => "  Enter toggle · Del reset · Tab scope · Esc close",
            Some(RowKind::Enum(_)) => "  ←/→ change · Del reset · Tab scope · Esc close",
            _ => "  Enter edit · Del reset · Tab scope · Esc close",
        }
    };
    lines.push(TextLine::from(Span::styled(
        hint,
        Style::default().fg(VERY_DIM),
    )));

    frame.render_widget(Paragraph::new(lines), area);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brow(path: &str, label: &str, group: &str, value: &str) -> SettingRow {
        SettingRow {
            path: path.into(),
            label: label.into(),
            group: group.into(),
            kind: RowKind::Bool,
            value: value.into(),
            ..Default::default()
        }
    }

    fn ed_with(rows: Vec<SettingRow>) -> ConfigEditor {
        let mut ed = ConfigEditor::default();
        ed.open_with(rows);
        ed
    }

    #[test]
    fn bool_enter_toggles_and_saves() {
        let mut ed = ed_with(vec![brow(
            "tui.fullscreen",
            "Full-screen TUI",
            "Interface",
            "true",
        )]);
        assert_eq!(
            ed.handle_key(KeyKind::Enter),
            ConfigAction::Save {
                path: "tui.fullscreen".into(),
                value: "false".into()
            }
        );
        assert!(ed.editing.is_none()); // no text editing for bools
    }

    #[test]
    fn enum_cycles_with_arrows() {
        let mut ed = ed_with(vec![SettingRow {
            path: "mesh.credit_mode".into(),
            label: "Credit conservation".into(),
            group: "Mesh & Cost".into(),
            kind: RowKind::Enum(vec!["normal".into(), "frugal".into(), "strict".into()]),
            value: "frugal".into(),
            ..Default::default()
        }]);
        assert_eq!(
            ed.handle_key(KeyKind::Right),
            ConfigAction::Save {
                path: "mesh.credit_mode".into(),
                value: "strict".into()
            }
        );
        assert_eq!(
            ed.handle_key(KeyKind::Left),
            ConfigAction::Save {
                path: "mesh.credit_mode".into(),
                value: "normal".into()
            }
        );
    }

    #[test]
    fn del_resets_to_default() {
        let mut ed = ed_with(vec![brow(
            "tui.fullscreen",
            "Full-screen TUI",
            "Interface",
            "true",
        )]);
        assert_eq!(
            ed.handle_key(KeyKind::DeleteForward),
            ConfigAction::Reset {
                path: "tui.fullscreen".into()
            }
        );
    }

    #[test]
    fn text_row_opens_input_then_saves() {
        let mut ed = ed_with(vec![SettingRow {
            path: "local.model".into(),
            label: "Model".into(),
            group: "Local LLM".into(),
            kind: RowKind::Text,
            value: String::new(),
            ..Default::default()
        }]);
        ed.handle_key(KeyKind::Enter); // open input
        assert!(ed.editing.is_some());
        ed.handle_key(KeyKind::Char('g'));
        ed.handle_key(KeyKind::Char('4'));
        assert_eq!(
            ed.handle_key(KeyKind::Enter),
            ConfigAction::Save {
                path: "local.model".into(),
                value: "g4".into()
            }
        );
    }

    #[test]
    fn secret_starts_empty_and_masks() {
        let mut ed = ed_with(vec![SettingRow {
            path: "key.openai".into(),
            label: "OpenAI key".into(),
            group: "Providers & Keys".into(),
            kind: RowKind::Secret,
            value: "● set".into(),
            ..Default::default()
        }]);
        ed.handle_key(KeyKind::Enter);
        assert_eq!(ed.editing.as_deref(), Some("")); // never prefilled
    }

    #[test]
    fn fuzzy_filter_matches_label_or_path() {
        let ed = ed_with(vec![
            brow("tui.fullscreen", "Full-screen TUI", "Interface", "true"),
            brow(
                "local.autostart",
                "Auto-start on launch",
                "Local LLM",
                "false",
            ),
        ]);
        let mut e = ed;
        e.filter = "autostart".into(); // matches the path
        assert_eq!(e.matches().len(), 1);
        e.filter = "full".into(); // matches the label
        assert_eq!(e.matches().len(), 1);
    }
}
