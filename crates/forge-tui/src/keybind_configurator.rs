//! Interactive keybind configurator overlay. Runs as a fullscreen takeover (via `Tui::run_fullscreen`).
//! The user can navigate binds, rebind individual actions, reset to defaults, and save.

use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table};
use ratatui::Terminal;

use forge_config::KeybindsConfig;

use crate::keybinds::{combo_display, key_event_to_combo};

const ACCENT: Color = Color::Rgb(82, 162, 255);
const DIM: Color = Color::Rgb(82, 87, 108);
const TEXT: Color = Color::Rgb(208, 213, 224);
const OKGREEN: Color = Color::Rgb(92, 208, 122);
const WARNYEL: Color = Color::Rgb(238, 188, 82);
const ERRRED: Color = Color::Rgb(243, 92, 92);
const SELECT_BG: Color = Color::Rgb(40, 70, 132);
const ORANGE: Color = Color::Rgb(255, 138, 48);

/// Description for each action.
fn action_desc(action: &str) -> &'static str {
    match action {
        "interrupt" => "Stop current turn",
        "command_palette" => "Open slash-command palette",
        "skip_model" => "Mid-turn: abort + retry next model",
        "tier_up" => "Escalate to next tier (mid-turn)",
        "tier_down" => "De-escalate tier (mid-turn)",
        "toggle_reasoning" => "Show/hide reasoning blocks",
        "undo" => "Undo last file write",
        "compact" => "Compact/summarize conversation",
        "model_picker" => "Open model picker overlay",
        "effort_cycle" => "Cycle effort level (low→max)",
        "temper_cycle" => "Cycle temper (default→bypass)",
        "keybind_config" => "Open this keybind configurator",
        "new_session" => "Start a fresh session",
        "copy_last" => "Copy last response to clipboard",
        "scroll_up" => "Scroll transcript up",
        "scroll_down" => "Scroll transcript down",
        "help" => "Show keybind reference (this screen)",
        "checkpoint" => "Save session checkpoint",
        "reload" => "Hot-reload config",
        _ => "",
    }
}

/// Whether a binding only applies mid-turn.
fn is_mid_turn_only(action: &str) -> bool {
    matches!(action, "skip_model" | "tier_up" | "tier_down" | "interrupt")
}

/// All actions in display order.
fn all_actions() -> Vec<&'static str> {
    vec![
        "interrupt",
        "command_palette",
        "skip_model",
        "tier_up",
        "tier_down",
        "toggle_reasoning",
        "undo",
        "compact",
        "model_picker",
        "effort_cycle",
        "temper_cycle",
        "keybind_config",
        "new_session",
        "copy_last",
        "scroll_up",
        "scroll_down",
        "help",
        "checkpoint",
        "reload",
    ]
}

enum OverlayMode {
    Browse,
    Capture { action: String },
}

/// Run the interactive keybind configurator as a fullscreen takeover.
/// Returns `Ok(true)` if changes were saved, `Ok(false)` if discarded.
pub fn run_keybind_configurator(keybinds: &mut KeybindsConfig) -> io::Result<bool> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let defaults = KeybindsConfig::default();
    let mut working = keybinds.clone();
    let actions = all_actions();
    let mut selected: usize = 0;
    let mut mode = OverlayMode::Browse;
    let mut status: Option<(String, bool)> = None;
    let mut changes = 0usize;
    let mut saved = false;

    loop {
        let actions_len = actions.len();
        terminal.draw(|f| {
            let area = f.area();
            f.render_widget(Clear, area);

            let outer = Block::default()
                .title(TextLine::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        "Keybind Configuration",
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]))
                .border_type(BorderType::Rounded)
                .borders(Borders::ALL)
                .style(Style::default().fg(DIM));

            let inner = outer.inner(area);
            f.render_widget(outer, area);

            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(5),
                    Constraint::Length(1),
                ])
                .split(inner);

            let help_text = match &mode {
                OverlayMode::Browse => {
                    "j/k Up/Down navigate  •  Enter rebind  •  r reset  •  R reset all  •  s save+close  •  q discard"
                }
                OverlayMode::Capture { .. } => "Press the new key combination...  •  Esc cancel",
            };
            f.render_widget(
                Paragraph::new(Span::styled(help_text, Style::default().fg(DIM))),
                chunks[0],
            );

            let header = Row::new(vec![
                Cell::from(Span::styled(
                    "Action",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    "Binding",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(
                    "Description",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
            ])
            .height(1);

            let rows: Vec<Row> = actions
                .iter()
                .enumerate()
                .map(|(i, action)| {
                    let combo = working.binds.get(*action);
                    let binding_text = combo
                        .map(combo_display)
                        .unwrap_or_else(|| "(unset)".to_string());
                    let is_default = defaults.binds.get(*action) == working.binds.get(*action);
                    let mid_turn = is_mid_turn_only(action);

                    let binding_display = if !is_default {
                        format!("{} [*]", binding_text)
                    } else {
                        binding_text
                    };

                    let is_selected = i == selected;
                    let row_style = if is_selected {
                        Style::default().bg(SELECT_BG).fg(Color::White)
                    } else {
                        Style::default().fg(TEXT)
                    };

                    let marker = if mid_turn { " >" } else { "  " };

                    Row::new(vec![
                        Cell::from(Span::styled(
                            format!("{}{}", marker, action),
                            row_style,
                        )),
                        Cell::from(Span::styled(
                            binding_display,
                            if is_selected {
                                row_style
                            } else if !is_default {
                                Style::default().fg(WARNYEL)
                            } else {
                                Style::default().fg(OKGREEN)
                            },
                        )),
                        Cell::from(Span::styled(
                            action_desc(action),
                            if is_selected {
                                row_style
                            } else {
                                Style::default().fg(DIM)
                            },
                        )),
                    ])
                    .height(1)
                })
                .collect();

            let table = Table::new(
                rows,
                [
                    Constraint::Length(22),
                    Constraint::Length(22),
                    Constraint::Min(20),
                ],
            )
            .header(header)
            .block(Block::default());

            f.render_widget(table, chunks[1]);

            if let Some((msg, ok)) = &status {
                let color = if *ok { OKGREEN } else { ERRRED };
                f.render_widget(
                    Paragraph::new(Span::styled(msg.clone(), Style::default().fg(color))),
                    chunks[2],
                );
            } else {
                let unsaved_note = if changes > 0 {
                    format!(
                        "  Unsaved changes: {}  •  > = mid-turn only  •  [*] = modified",
                        changes
                    )
                } else {
                    "  No unsaved changes  •  > = mid-turn only".to_string()
                };
                f.render_widget(
                    Paragraph::new(Span::styled(unsaved_note, Style::default().fg(DIM))),
                    chunks[2],
                );
            }

            if matches!(mode, OverlayMode::Capture { .. }) {
                let action_name = match &mode {
                    OverlayMode::Capture { action } => action.clone(),
                    _ => String::new(),
                };
                let popup_w = 44u16;
                let popup_h = 5u16;
                let popup_x = area.x + (area.width.saturating_sub(popup_w)) / 2;
                let popup_y = area.y + (area.height.saturating_sub(popup_h)) / 2;
                let popup_area = Rect::new(popup_x, popup_y, popup_w, popup_h);
                f.render_widget(Clear, popup_area);
                let popup = Block::default()
                    .title(Span::styled(
                        " Rebind ",
                        Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
                    ))
                    .border_type(BorderType::Rounded)
                    .borders(Borders::ALL)
                    .style(Style::default().fg(DIM));
                let popup_inner = popup.inner(popup_area);
                f.render_widget(popup, popup_area);
                let msg = format!("Press the new key for:\n  {}", action_name);
                f.render_widget(
                    Paragraph::new(Span::styled(msg, Style::default().fg(TEXT))),
                    popup_inner,
                );
            }
        })?;

        if let Ok(ev) = event::read() {
            status = None;
            match &mode {
                OverlayMode::Capture { action } => {
                    let action = action.clone();
                    match ev {
                        Event::Key(k) if k.kind == KeyEventKind::Press => {
                            if k.code == KeyCode::Esc {
                                mode = OverlayMode::Browse;
                            } else if let Some(combo) = key_event_to_combo(&k) {
                                working.binds.insert(action.clone(), combo.clone());
                                changes += 1;
                                status = Some((
                                    format!("✓ {} -> {}", action, combo_display(&combo)),
                                    true,
                                ));
                                mode = OverlayMode::Browse;
                            }
                        }
                        _ => {}
                    }
                }
                OverlayMode::Browse => match ev {
                    Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                        KeyCode::Char('q') => {
                            break;
                        }
                        KeyCode::Char('s') | KeyCode::Esc => {
                            *keybinds = working.clone();
                            saved = true;
                            break;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if selected + 1 < actions_len {
                                selected += 1;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            selected = selected.saturating_sub(1);
                        }
                        KeyCode::Enter => {
                            let action = actions[selected].to_string();
                            mode = OverlayMode::Capture { action };
                        }
                        // Shift-R (uppercase, with or without the SHIFT modifier flag depending on
                        // the terminal) resets ALL binds; plain 'r' resets only the selected one.
                        KeyCode::Char('R') => {
                            working = defaults.clone();
                            changes += 1;
                            status = Some(("✓ all keybinds reset to defaults".to_string(), true));
                        }
                        KeyCode::Char('r') if k.modifiers.contains(KeyModifiers::SHIFT) => {
                            working = defaults.clone();
                            changes += 1;
                            status = Some(("✓ all keybinds reset to defaults".to_string(), true));
                        }
                        KeyCode::Char('r') => {
                            let action = actions[selected];
                            if let Some(default_combo) = defaults.binds.get(action) {
                                working
                                    .binds
                                    .insert(action.to_string(), default_combo.clone());
                                changes += 1;
                                status = Some((format!("✓ {} reset to default", action), true));
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                },
            }
        }
    }

    crossterm::execute!(io::stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    Ok(saved)
}
