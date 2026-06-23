//! A standalone, animated full-screen multi-select list — used by `forge mcp import` to pick
//! which discovered servers to import. Self-contained (its own alt-screen + raw-mode lifecycle)
//! so it works as a one-shot CLI prompt, independent of the chat TUI's inline viewport.
//!
//! Cross-platform: crossterm drives the terminal on Linux, macOS, and Windows alike.

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Terminal, TerminalOptions, Viewport};

const ORANGE: Color = Color::Rgb(255, 145, 60);
const DIM: Color = Color::Rgb(110, 110, 120);
const OKGREEN: Color = Color::Rgb(120, 210, 140);
const FG: Color = Color::Rgb(205, 205, 215);

/// One selectable row: a primary `label` and a dim `hint` (e.g. transport + source).
pub struct SelectItem {
    pub label: String,
    pub hint: String,
    /// Whether the row starts checked.
    pub preselected: bool,
}

/// Run the animated multi-select. Returns the indices the user confirmed (Enter), or `None` if
/// they cancelled (Esc / `q` / Ctrl-C). Restores the terminal on every exit path.
pub fn select_multi(title: &str, items: &[SelectItem]) -> io::Result<Option<Vec<usize>>> {
    if items.is_empty() {
        return Ok(Some(vec![]));
    }
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;

    let mut state = State {
        cursor: 0,
        checked: items.iter().map(|i| i.preselected).collect(),
        tick: 0,
    };

    let result = run_loop(&mut terminal, title, items, &mut state);

    // Always restore, even on error.
    disable_raw_mode().ok();
    terminal.backend_mut().execute(LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

/// Run an animated single-select menu (same look as [`select_multi`], no checkboxes). Returns the
/// chosen index, or `None` on cancel (Esc / `q` / Ctrl-C). Restores the terminal on every path.
pub fn select_one(title: &str, items: &[SelectItem]) -> io::Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    stdout.execute(EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )?;
    let mut cursor = 0usize;
    let mut tick = 0u64;
    let result = loop {
        terminal.draw(|f| draw_single(f, title, items, cursor, tick))?;
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => break Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break Ok(None)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        cursor = cursor.checked_sub(1).unwrap_or(items.len() - 1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => cursor = (cursor + 1) % items.len(),
                    KeyCode::Enter => break Ok(Some(cursor)),
                    _ => {}
                }
            }
        }
        tick = tick.wrapping_add(1);
    };
    disable_raw_mode().ok();
    terminal.backend_mut().execute(LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

fn draw_single(
    f: &mut ratatui::Frame,
    title: &str,
    items: &[SelectItem],
    cursor: usize,
    tick: u64,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(f.area());
    let pulse = if (tick / 6) % 2 == 0 { ORANGE } else { DIM };
    let header = Line::from(vec![
        Span::styled("⚒ ", Style::default().fg(pulse)),
        Span::styled(
            title.to_string(),
            Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let on = i == cursor;
        let caret = if on {
            if (tick / 4) % 2 == 0 {
                "▸ "
            } else {
                "▹ "
            }
        } else {
            "  "
        };
        let label_style = if on {
            Style::default().fg(FG).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(caret, Style::default().fg(ORANGE)),
            Span::styled(format!("{:<26}", item.label), label_style),
            Span::styled(item.hint.clone(), Style::default().fg(DIM)),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(DIM)),
        ),
        chunks[1],
    );
    let footer = Line::from(vec![
        Span::styled("↑/↓", Style::default().fg(ORANGE)),
        Span::styled(" move   ", Style::default().fg(DIM)),
        Span::styled("enter", Style::default().fg(OKGREEN)),
        Span::styled(" select   ", Style::default().fg(DIM)),
        Span::styled("esc", Style::default().fg(Color::Rgb(240, 110, 110))),
        Span::styled(" cancel", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);
}

struct State {
    cursor: usize,
    checked: Vec<bool>,
    tick: u64,
}

fn run_loop(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    title: &str,
    items: &[SelectItem],
    state: &mut State,
) -> io::Result<Option<Vec<usize>>> {
    loop {
        terminal.draw(|f| draw(f, title, items, state))?;
        // ~12fps: poll keeps the accent animation ticking even with no input.
        if event::poll(Duration::from_millis(80))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None)
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        state.cursor = state.cursor.checked_sub(1).unwrap_or(items.len() - 1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        state.cursor = (state.cursor + 1) % items.len();
                    }
                    KeyCode::Char(' ') => {
                        let c = &mut state.checked[state.cursor];
                        *c = !*c;
                    }
                    KeyCode::Char('a') => state.checked.iter_mut().for_each(|c| *c = true),
                    KeyCode::Char('n') => state.checked.iter_mut().for_each(|c| *c = false),
                    KeyCode::Enter => {
                        let chosen: Vec<usize> = state
                            .checked
                            .iter()
                            .enumerate()
                            .filter_map(|(i, &c)| c.then_some(i))
                            .collect();
                        return Ok(Some(chosen));
                    }
                    _ => {}
                }
            }
        }
        state.tick = state.tick.wrapping_add(1);
    }
}

fn draw(f: &mut ratatui::Frame, title: &str, items: &[SelectItem], state: &State) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(f.area());

    // Header — the ⚒ pulses between orange and dim so the screen reads as "live".
    let pulse = if (state.tick / 6) % 2 == 0 {
        ORANGE
    } else {
        DIM
    };
    let n = state.checked.iter().filter(|&&c| c).count();
    let header = Line::from(vec![
        Span::styled("⚒ ", Style::default().fg(pulse)),
        Span::styled(
            title.to_string(),
            Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   {n}/{} selected", items.len()),
            Style::default().fg(DIM),
        ),
    ]);
    f.render_widget(Paragraph::new(header), chunks[0]);

    // The list.
    let mut lines: Vec<Line> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let on_cursor = i == state.cursor;
        let checked = state.checked[i];
        let marker = if checked { "◉" } else { "○" };
        let marker_color = if checked { OKGREEN } else { DIM };
        // Animated caret on the focused row.
        let caret = if on_cursor {
            if (state.tick / 4) % 2 == 0 {
                "▸ "
            } else {
                "▹ "
            }
        } else {
            "  "
        };
        let label_style = if on_cursor {
            Style::default().fg(FG).add_modifier(Modifier::BOLD)
        } else if checked {
            Style::default().fg(FG)
        } else {
            Style::default().fg(DIM)
        };
        lines.push(Line::from(vec![
            Span::styled(caret, Style::default().fg(ORANGE)),
            Span::styled(format!("{marker} "), Style::default().fg(marker_color)),
            Span::styled(format!("{:<18}", item.label), label_style),
            Span::styled(item.hint.clone(), Style::default().fg(DIM)),
        ]));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(DIM)),
        ),
        chunks[1],
    );

    // Footer keymap.
    let footer = Line::from(vec![
        Span::styled("↑/↓", Style::default().fg(ORANGE)),
        Span::styled(" move   ", Style::default().fg(DIM)),
        Span::styled("space", Style::default().fg(ORANGE)),
        Span::styled(" toggle   ", Style::default().fg(DIM)),
        Span::styled("a/n", Style::default().fg(ORANGE)),
        Span::styled(" all/none   ", Style::default().fg(DIM)),
        Span::styled("enter", Style::default().fg(OKGREEN)),
        Span::styled(" import   ", Style::default().fg(DIM)),
        Span::styled("esc", Style::default().fg(Color::Rgb(240, 110, 110))),
        Span::styled(" cancel", Style::default().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(footer), chunks[2]);
}
