//! Full-screen subagent transcript browser (Ctrl+O in the chat). Runs on the ALTERNATE screen so
//! it never pollutes the chat's inline scrollback, and a caller-supplied `refresh` closure is
//! polled every frame (it drains pending activity) so the selected child's log AUTO-UPDATES while
//! open. The line layout ([`transcript_lines`]) is pure → unit-tested without terminal I/O.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Terminal;

use crate::app::SubagentView;

/// Run the full-screen subagent transcript browser on the alternate screen (so it never pollutes
/// the chat's scrollback). `refresh` is called every frame to drain pending activity and return
/// the current views — so the selected child's log AUTO-UPDATES while open. Keys: ↑↓/j/k scroll,
/// space/PgDn + u/d page, g/G top/bottom, ←→/Tab switch agent, Esc/q close.
pub fn run_subagent_transcript<F>(mut refresh: F) -> io::Result<()>
where
    F: FnMut() -> Vec<SubagentView>,
{
    crate::driver::install_panic_restore();
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    let res = browse(&mut refresh);
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    res
}

fn browse<F: FnMut() -> Vec<SubagentView>>(refresh: &mut F) -> io::Result<()> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut selected = 0usize;
    let mut scroll = 0usize;
    loop {
        let views = refresh();
        let n = views.len().max(1);
        selected = selected.min(n - 1);
        let log_len = views.get(selected).map(|v| v.log.len()).unwrap_or(0);
        scroll = scroll.min(log_len.saturating_sub(1));
        term.draw(|f| {
            let a = f.area();
            f.render_widget(Clear, a);
            f.render_widget(
                Paragraph::new(transcript_lines(
                    &views, selected, scroll, a.height, a.width,
                )),
                a,
            );
        })?;
        // Short poll so the view refreshes even without keypresses (live log updates).
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
            KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
            KeyCode::PageUp | KeyCode::Char('u') => scroll = scroll.saturating_sub(10),
            KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Char(' ') => {
                scroll = scroll.saturating_add(10)
            }
            KeyCode::Home | KeyCode::Char('g') => scroll = 0,
            KeyCode::End | KeyCode::Char('G') => scroll = usize::MAX / 2,
            KeyCode::Right | KeyCode::Tab => {
                selected = (selected + 1) % n;
                scroll = 0;
            }
            KeyCode::Left | KeyCode::BackTab => {
                selected = (selected + n - 1) % n;
                scroll = 0;
            }
            _ => {}
        }
    }
}

// Brand palette (mirrors the per-module consts elsewhere in the crate).
const ORANGE: Color = Color::Rgb(255, 145, 60);
const DIM: Color = Color::Rgb(110, 110, 120);
const TOOLCYAN: Color = Color::Rgb(120, 200, 215);
const BODY: Color = Color::Rgb(205, 205, 215);

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

/// Build the transcript view for `views[selected]` scrolled to `scroll`, sized to `height`×`width`:
/// a header (which child, of how many, + status/cost), the visible slice of that child's live log,
/// and a footer with the position + key hints. Pure — unit-tested. `selected`/`scroll` are clamped.
pub fn transcript_lines(
    views: &[SubagentView],
    selected: usize,
    scroll: usize,
    height: u16,
    width: u16,
) -> Vec<TextLine<'static>> {
    let h = height as usize;
    if views.is_empty() {
        return vec![
            TextLine::from(Span::styled(
                "  ⚒ subagent transcript",
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            )),
            TextLine::from(Span::styled(
                "  no subagents in this batch yet",
                Style::default().fg(DIM),
            )),
        ];
    }
    let selected = selected.min(views.len() - 1);
    let view = &views[selected];
    let status = if view.done { "done" } else { "running" };
    let title_w = (width as usize).saturating_sub(40);
    let mut lines = vec![
        TextLine::from(vec![
            Span::styled(
                format!("  ⚒ transcript [{}/{}] ", selected + 1, views.len()),
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", view.agent),
                Style::default().fg(TOOLCYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("· {status} · ${:.4}", view.cost),
                Style::default().fg(DIM),
            ),
        ]),
        TextLine::from(Span::styled(
            format!("  {}", truncate(&view.task, title_w.max(10))),
            Style::default().fg(DIM),
        )),
    ];

    // Body: the log window. Reserve 2 header + 1 footer rows.
    let body_h = h.saturating_sub(3).max(1);
    let total = view.log.len();
    let max_scroll = total.saturating_sub(body_h);
    let scroll = scroll.min(max_scroll);
    if view.log.is_empty() {
        lines.push(TextLine::from(Span::styled(
            "  (no activity captured yet)",
            Style::default().fg(DIM),
        )));
    }
    for entry in view.log.iter().skip(scroll).take(body_h) {
        lines.push(TextLine::from(Span::styled(
            format!("  {}", truncate(entry, width.saturating_sub(2) as usize)),
            Style::default().fg(BODY),
        )));
    }
    // Pad so the footer sits at the bottom of the region.
    while lines.len() < h.saturating_sub(1) {
        lines.push(TextLine::default());
    }
    let shown_end = (scroll + body_h).min(total);
    lines.push(TextLine::from(Span::styled(
        format!(
            "  ── {}-{}/{} lines · ↑↓ scroll · ←→ switch agent · Esc close ──",
            scroll.min(total.saturating_sub(1)) + usize::from(total > 0),
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

    fn view(agent: &str, n: usize, done: bool) -> SubagentView {
        SubagentView {
            agent: agent.into(),
            task: "scan the repo".into(),
            done,
            cost: 0.0,
            log: (0..n).map(|i| format!("line {i}")).collect(),
        }
    }

    #[test]
    fn header_shows_selected_of_total_and_agent() {
        let vs = vec![view("alpha", 3, false), view("beta", 1, true)];
        let txt = render(&transcript_lines(&vs, 1, 0, 20, 80));
        assert!(txt.contains("[2/2]"), "selector: {txt}");
        assert!(txt.contains("beta"));
        assert!(txt.contains("done"));
    }

    #[test]
    fn scroll_offsets_the_log_window() {
        let vs = vec![view("a", 40, false)];
        let body: Vec<String> = transcript_lines(&vs, 0, 5, 12, 80)
            .iter()
            .filter_map(|l| l.spans.first().map(|s| s.content.trim().to_string()))
            .filter(|s| s.starts_with("line "))
            .collect();
        assert_eq!(body.first().unwrap(), "line 5");
        assert!(!body.iter().any(|s| s == "line 0"));
    }

    #[test]
    fn selected_and_scroll_are_clamped() {
        let vs = vec![view("a", 3, false)];
        // Out-of-range selected + scroll must not panic.
        assert!(!transcript_lines(&vs, 99, 999, 10, 80).is_empty());
    }

    #[test]
    fn empty_views_render_placeholder() {
        let txt = render(&transcript_lines(&[], 0, 0, 10, 80));
        assert!(txt.contains("no subagents"));
    }

    fn render(lines: &[TextLine]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}
