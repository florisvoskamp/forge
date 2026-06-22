//! The dynamic `/config` editor overlay: a fuzzy-searchable, importance-ordered list of every
//! scalar setting (discovered by `forge_config::config_leaves`), edited in place. State, input
//! handling, and rendering all live here (no `forge-config` dependency); the I/O shell feeds it
//! rows and performs the validated writes via the [`ConfigAction`] it returns.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::KeyKind;

// Local brand palette (mirrors app.rs).
use ratatui::style::Color;
const ORANGE: Color = Color::Rgb(255, 145, 60);
const DIM: Color = Color::Rgb(110, 110, 120);
const USER: Color = Color::Rgb(125, 180, 255);
const OKGREEN: Color = Color::Rgb(120, 210, 140);
const WARN: Color = Color::Rgb(235, 120, 110);
const VERY_DIM: Color = Color::Rgb(80, 80, 90);

/// One editable setting row (the TUI-side mirror of `forge_config::SettingLeaf`).
#[derive(Debug, Clone, Default)]
pub struct SettingRow {
    pub path: String,
    /// Current value, rendered.
    pub display: String,
    /// Short type tag (`bool`/`int`/`float`/`text`/`secret`).
    pub type_tag: String,
    /// One-line help shown for the selected row.
    pub help: Option<String>,
    /// A secret (API key): the value is masked, edits route to the keyring not config.toml.
    pub secret: bool,
}

/// What the I/O shell should do after a keystroke (it owns the `forge-config` writes).
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigAction {
    None,
    /// Persist `value` at `path` in the current scope, then refresh rows.
    Save {
        path: String,
        value: String,
    },
    /// Scope toggled — reload the rows (effective values may differ to show).
    Reload,
    /// Editor closed.
    Close,
}

/// The `/config` editor overlay state.
#[derive(Debug, Clone, Default)]
pub struct ConfigEditor {
    pub open: bool,
    /// false = user config (`~/.config/forge`), true = project (`./.forge`).
    pub project_scope: bool,
    /// All settings (importance-ordered), populated by the I/O shell.
    pub rows: Vec<SettingRow>,
    /// Fuzzy filter text.
    pub filter: String,
    /// Index into the FILTERED list.
    pub selected: usize,
    /// `Some(buffer)` while editing the selected value.
    pub editing: Option<String>,
    /// Transient status (last save result), shown in the footer.
    pub status: Option<String>,
}

impl ConfigEditor {
    /// Open the editor with freshly-loaded rows.
    pub fn open_with(&mut self, rows: Vec<SettingRow>) {
        self.open = true;
        self.rows = rows;
        self.filter.clear();
        self.selected = 0;
        self.editing = None;
        self.status = None;
    }

    /// Indices (into `rows`) passing the fuzzy filter, in display order.
    pub fn matches(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.rows.len()).collect();
        }
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| fuzzy_subseq(&self.filter, &r.path))
            .map(|(i, _)| i)
            .collect()
    }

    /// The currently-highlighted row, if any.
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

    /// Feed a keystroke; returns the I/O action to perform. Modal: while editing, keys edit the
    /// value buffer; otherwise they filter / navigate / open an edit / toggle scope / close.
    pub fn handle_key(&mut self, key: KeyKind) -> ConfigAction {
        if let Some(buf) = self.editing.as_mut() {
            match key {
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
            }
        } else {
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
                KeyKind::Enter => {
                    if let Some(r) = self.selected_row() {
                        // Secrets start empty (never prefill the masked value); bools pre-fill the
                        // toggle for a one-keystroke flip; everything else pre-fills its value.
                        self.editing = Some(if r.secret {
                            String::new()
                        } else if r.type_tag == "bool" {
                            (r.display != "true").to_string()
                        } else {
                            r.display.clone()
                        });
                    }
                    ConfigAction::None
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
}

/// Case-insensitive subsequence match (the chars of `needle` appear in order in `haystack`).
fn fuzzy_subseq(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars().map(|c| c.to_ascii_lowercase());
    for nc in needle.chars().map(|c| c.to_ascii_lowercase()) {
        if !hay.any(|hc| hc == nc) {
            return false;
        }
    }
    true
}

/// Render the editor as a full-frame overlay (called last, on top of everything).
pub fn render_config_overlay(frame: &mut Frame, ed: &ConfigEditor) {
    if !ed.open {
        return;
    }
    let area = frame.area();
    if area.height < 4 || area.width < 20 {
        return;
    }
    frame.render_widget(Clear, area);

    let w = area.width as usize;
    let scope = if ed.project_scope {
        "project (./.forge)"
    } else {
        "user (~/.config/forge)"
    };
    let matches = ed.matches();

    let mut lines: Vec<TextLine> = Vec::with_capacity(area.height as usize);
    lines.push(TextLine::from(vec![
        Span::styled("  ⚒ config  ", Style::default().fg(ORANGE).bold()),
        Span::styled(format!("scope: {scope}"), Style::default().fg(USER)),
        Span::styled("  ·  Tab switch scope", Style::default().fg(VERY_DIM)),
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

    // Body: reserve 3 header + 3 footer rows (help + status + hints); window to the selection.
    let body_h = (area.height as usize).saturating_sub(6).max(1);
    let start = ed.selected.saturating_sub(body_h.saturating_sub(1));
    for (row, &ri) in matches.iter().enumerate().skip(start).take(body_h) {
        let r = &ed.rows[ri];
        let sel = row == ed.selected;
        let marker = if sel { "▸ " } else { "  " };
        let editing_here = sel && ed.editing.is_some();
        let value_span = if editing_here {
            let buf = ed.editing.as_deref().unwrap_or("");
            // Secrets are masked while typing.
            let shown = if r.secret {
                "•".repeat(buf.chars().count())
            } else {
                buf.to_string()
            };
            Span::styled(
                format!("{shown}▌"),
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                truncate(&r.display, 28),
                Style::default().fg(if sel { OKGREEN } else { DIM }),
            )
        };
        let path_style = if sel {
            Style::default().fg(ORANGE).bold()
        } else {
            Style::default().fg(USER)
        };
        let path_col = w.saturating_sub(44).clamp(16, 56);
        lines.push(TextLine::from(vec![
            Span::styled(format!("  {marker}"), path_style),
            Span::styled(
                format!("{:<width$}", truncate(&r.path, path_col), width = path_col),
                path_style,
            ),
            Span::styled(
                format!("  {:<6} ", r.type_tag),
                Style::default().fg(VERY_DIM),
            ),
            value_span,
        ]));
    }
    while lines.len() < (area.height as usize).saturating_sub(3) {
        lines.push(TextLine::default());
    }

    // Help line for the selected setting (the "what does this do" the editor was missing).
    let help = ed
        .selected_row()
        .and_then(|r| r.help.clone())
        .unwrap_or_else(|| "(no description)".to_string());
    lines.push(TextLine::from(Span::styled(
        format!("  {}", truncate(&help, w.saturating_sub(4))),
        Style::default().fg(USER),
    )));

    // Footer: status (if any) + key hints.
    if let Some(s) = &ed.status {
        let color = if s.starts_with('✓') { OKGREEN } else { WARN };
        lines.push(TextLine::from(Span::styled(
            format!("  {}", truncate(s, w.saturating_sub(4))),
            Style::default().fg(color),
        )));
    } else {
        lines.push(TextLine::default());
    }
    let hint = if ed.editing.is_some() {
        "  type a value · Enter save · Esc cancel"
    } else {
        "  ↑↓ move · Enter edit · Tab scope · Esc close"
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

    fn row(path: &str, display: &str, type_tag: &str) -> SettingRow {
        SettingRow {
            path: path.into(),
            display: display.into(),
            type_tag: type_tag.into(),
            ..Default::default()
        }
    }

    fn rows() -> Vec<SettingRow> {
        vec![
            row("tui.fullscreen", "true", "bool"),
            row("local.autostart", "false", "bool"),
            row("mesh.daily_cap_usd", "10", "float"),
        ]
    }

    #[test]
    fn secret_row_starts_empty_and_masks() {
        let mut ed = ConfigEditor::default();
        ed.open_with(vec![SettingRow {
            path: "key.openai".into(),
            display: "● set".into(),
            type_tag: "secret".into(),
            secret: true,
            ..Default::default()
        }]);
        ed.handle_key(KeyKind::Enter); // begin editing the secret
        assert_eq!(ed.editing.as_deref(), Some("")); // never prefilled with the value
    }

    #[test]
    fn fuzzy_filter_narrows_rows() {
        let mut ed = ConfigEditor::default();
        ed.open_with(rows());
        ed.filter = "autos".into();
        let m = ed.matches();
        assert_eq!(m.len(), 1);
        assert_eq!(ed.rows[m[0]].path, "local.autostart");
    }

    #[test]
    fn enter_on_bool_prefills_the_toggle() {
        let mut ed = ConfigEditor::default();
        ed.open_with(rows()); // selected=0 → tui.fullscreen=true
        assert_eq!(ed.handle_key(KeyKind::Enter), ConfigAction::None);
        assert_eq!(ed.editing.as_deref(), Some("false")); // flipped
    }

    #[test]
    fn editing_then_enter_emits_save() {
        let mut ed = ConfigEditor::default();
        ed.open_with(rows());
        ed.filter = "daily".into();
        ed.handle_key(KeyKind::Enter); // begin edit (prefill "10")
        ed.handle_key(KeyKind::Backspace);
        ed.handle_key(KeyKind::Backspace);
        ed.handle_key(KeyKind::Char('2'));
        ed.handle_key(KeyKind::Char('5'));
        assert_eq!(
            ed.handle_key(KeyKind::Enter),
            ConfigAction::Save {
                path: "mesh.daily_cap_usd".into(),
                value: "25".into()
            }
        );
    }

    #[test]
    fn tab_toggles_scope_and_requests_reload() {
        let mut ed = ConfigEditor::default();
        ed.open_with(rows());
        assert!(!ed.project_scope);
        assert_eq!(ed.handle_key(KeyKind::Tab), ConfigAction::Reload);
        assert!(ed.project_scope);
    }

    #[test]
    fn esc_closes() {
        let mut ed = ConfigEditor::default();
        ed.open_with(rows());
        assert_eq!(ed.handle_key(KeyKind::Esc), ConfigAction::Close);
        assert!(!ed.open);
    }
}
