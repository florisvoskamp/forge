//! Full-screen scrollable subagent transcript browser (Ctrl+O in the chat). The inline chat
//! viewport can't grow at runtime, so reading a child's whole transcript needs an alternate-screen
//! takeover — same mechanism as the `/config` wizard ([`crate::driver::Tui::run_fullscreen`]). The
//! line layout ([`transcript_lines`]) is pure so it's unit-tested against a `TestBackend`-free model.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

use crate::app::SubagentView;

// Brand palette (mirrors the per-module consts in app.rs/render.rs/init_wizard.rs).
const ORANGE: Color = Color::Rgb(255, 145, 60);
const DIM: Color = Color::Rgb(110, 110, 120);
const TOOLCYAN: Color = Color::Rgb(120, 200, 215);

/// Truncate to `max` chars with an ellipsis (display width approximated by char count).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let take = max.saturating_sub(1);
        format!("{}…", s.chars().take(take).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Browser state: which subagent is shown and how far its transcript is scrolled.
struct State {
    selected: usize,
    scroll: usize,
    /// Visible body height of the last render, so PageUp/Down and clamping match what's on screen.
    page: usize,
}

/// Run the full-screen transcript browser over `views` until the user exits (Esc/q). Enters the
/// alternate screen + raw mode and restores them on exit (and on panic, via the shared hook). A
/// no-op for an empty slice. Keys: ↑/↓ line, PgUp/PgDn page, Home/End jump, ←/→ or Tab switch
/// subagent, Esc/q close.
pub fn run_subagent_transcript(views: &[SubagentView]) -> io::Result<()> {
    if views.is_empty() {
        return Ok(());
    }
    crate::driver::install_panic_restore();
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    let result = browse(views);
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    result
}

fn browse(views: &[SubagentView]) -> io::Result<()> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = State {
        selected: 0,
        scroll: 0,
        page: 1,
    };
    loop {
        terminal.draw(|f| {
            state.page = render(f, views, &state);
        })?;
        let view = &views[state.selected];
        let max_scroll = view.log.len().saturating_sub(1);
        if !event::poll(Duration::from_millis(120))? {
            continue;
        }
        let Event::Key(k) = event::read()? else {
            continue;
        };
        if k.kind == KeyEventKind::Release {
            continue;
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => state.scroll = state.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => state.scroll = (state.scroll + 1).min(max_scroll),
            KeyCode::PageUp => state.scroll = state.scroll.saturating_sub(state.page),
            KeyCode::PageDown => state.scroll = (state.scroll + state.page).min(max_scroll),
            KeyCode::Home => state.scroll = 0,
            KeyCode::End => state.scroll = max_scroll,
            KeyCode::Right | KeyCode::Tab => {
                state.selected = (state.selected + 1) % views.len();
                state.scroll = 0;
            }
            KeyCode::Left | KeyCode::BackTab => {
                state.selected = (state.selected + views.len() - 1) % views.len();
                state.scroll = 0;
            }
            _ => {}
        }
    }
}

/// Draw one frame; returns the body height (visible transcript rows) so the loop can page by it.
fn render(f: &mut Frame, views: &[SubagentView], state: &State) -> usize {
    let area = f.area();
    f.render_widget(Clear, area);
    let view = &views[state.selected];
    let title = format!(
        " ⚒ subagent transcript — {} of {} ",
        state.selected + 1,
        views.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ORANGE))
        .title(Span::styled(
            title,
            Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = transcript_lines(view, state.scroll, inner.height, inner.width);
    // Body height = inner minus the 2 header lines + 1 footer the layout reserves.
    let body_h = inner.height.saturating_sub(3) as usize;
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        Rect {
            height: inner.height,
            ..inner
        },
    );
    body_h.max(1)
}

/// Build the transcript lines for `view` scrolled to `scroll`, sized to a `height`×`width` area:
/// a header (agent · task · status · cost), the visible slice of the log, and a footer with the
/// position + key hints. Pure — unit-tested. `scroll` is clamped to the log length.
pub fn transcript_lines(
    view: &SubagentView,
    scroll: usize,
    height: u16,
    width: u16,
) -> Vec<TextLine<'static>> {
    let status = if view.done { "done" } else { "running" };
    let head_w = width.saturating_sub(2) as usize;
    let mut lines = vec![
        TextLine::from(vec![
            Span::styled(
                format!("[{}] ", view.agent),
                Style::default().fg(TOOLCYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate(&view.task, head_w.saturating_sub(view.agent.len() + 4)),
                Style::default().fg(DIM),
            ),
        ]),
        TextLine::from(Span::styled(
            format!("{status} · ${:.4}", view.cost),
            Style::default().fg(DIM),
        )),
    ];

    let body_h = (height as usize).saturating_sub(3).max(1);
    let total = view.log.len();
    let scroll = scroll.min(total.saturating_sub(1));
    if view.log.is_empty() {
        lines.push(TextLine::from(Span::styled(
            "(no activity captured yet)",
            Style::default().fg(DIM),
        )));
    }
    for entry in view.log.iter().skip(scroll).take(body_h) {
        lines.push(TextLine::from(Span::styled(
            entry.clone(),
            Style::default().fg(Color::Rgb(205, 205, 215)),
        )));
    }
    let shown_end = (scroll + body_h).min(total);
    lines.push(TextLine::from(Span::styled(
        format!(
            "── {}-{}/{} · ↑↓ scroll · PgUp/PgDn · ←→/Tab switch · Esc close ──",
            (scroll + 1).min(total.max(1)),
            shown_end,
            total
        ),
        Style::default().fg(DIM),
    )));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(n: usize) -> SubagentView {
        SubagentView {
            agent: "general".into(),
            task: "scan the repo".into(),
            done: false,
            cost: 0.0,
            log: (0..n).map(|i| format!("line {i}")).collect(),
        }
    }

    #[test]
    fn header_shows_agent_task_and_status() {
        let l = transcript_lines(&view(3), 0, 20, 80);
        let txt: String = l
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(txt.contains("general"));
        assert!(txt.contains("scan the repo"));
        assert!(txt.contains("running"));
    }

    #[test]
    fn scroll_offsets_the_visible_window() {
        // height 8 → body_h = 5 rows. Scrolling to 2 shows lines 2..7.
        let l = transcript_lines(&view(20), 2, 8, 80);
        let body: Vec<String> = l
            .iter()
            .filter_map(|line| line.spans.first().map(|s| s.content.to_string()))
            .filter(|s| s.starts_with("line "))
            .collect();
        assert_eq!(body.first().unwrap(), "line 2");
        assert!(!body.iter().any(|s| s == "line 0"));
    }

    #[test]
    fn scroll_is_clamped_past_the_end() {
        // Scrolling way past the end must not panic and shows the tail.
        let l = transcript_lines(&view(5), 999, 10, 80);
        assert!(!l.is_empty());
    }

    #[test]
    fn empty_log_renders_placeholder_not_panic() {
        let l = transcript_lines(&view(0), 0, 10, 80);
        let txt: String = l
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(txt.contains("no activity"));
    }
}
