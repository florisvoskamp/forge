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
/// the chat's scrollback). `initial_selected` opens at that agent index (0-based). `refresh` is
/// called every frame to drain pending activity and return the current views — so the selected
/// child's log AUTO-UPDATES while open. Keys: ↑↓/j/k scroll, space/PgDn + u/d page, g/G
/// top/bottom, ←→/Tab switch agent, Esc/q close.
pub fn run_subagent_transcript<F>(initial_selected: usize, mut refresh: F) -> io::Result<()>
where
    F: FnMut() -> Vec<SubagentView>,
{
    crate::driver::install_panic_restore();
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    let res = browse(&mut refresh, initial_selected);
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    res
}

fn browse<F: FnMut() -> Vec<SubagentView>>(
    refresh: &mut F,
    initial_selected: usize,
) -> io::Result<()> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut selected = initial_selected;
    // Start at the tail so the user immediately sees the most recent activity.
    let mut scroll = usize::MAX / 2;
    let mut follow = true; // auto-scroll to tail while agent is running
    loop {
        let views = refresh();
        let n = views.len().max(1);
        selected = selected.min(n - 1);
        let log_len = views.get(selected).map(|v| v.log.len()).unwrap_or(0);
        if follow {
            scroll = usize::MAX / 2;
        }
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
            KeyCode::Up | KeyCode::Char('k') => {
                follow = false;
                scroll = scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                scroll = scroll.saturating_add(1);
                if scroll >= log_len.saturating_sub(1) {
                    follow = true;
                }
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                follow = false;
                scroll = scroll.saturating_sub(10);
            }
            KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Char(' ') => {
                scroll = scroll.saturating_add(10);
                if scroll >= log_len.saturating_sub(1) {
                    follow = true;
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                follow = false;
                scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                follow = true;
                scroll = usize::MAX / 2;
            }
            KeyCode::Right | KeyCode::Tab => {
                selected = (selected + 1) % n;
                follow = true;
                scroll = usize::MAX / 2;
            }
            KeyCode::Left | KeyCode::BackTab => {
                selected = (selected + n - 1) % n;
                follow = true;
                scroll = usize::MAX / 2;
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
const OKGREEN: Color = Color::Rgb(100, 200, 120);
const WARNRED: Color = Color::Rgb(220, 80, 80);
const VERY_DIM: Color = Color::Rgb(80, 80, 90);

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

/// Classify a log entry line into a display color based on its content.
fn line_color(entry: &str) -> Color {
    let t = entry.trim();
    // Tool call lines: start with a known tool name or "⚒"/"→"
    if t.starts_with("⚒") || t.starts_with("※") {
        return ORANGE;
    }
    // Success markers
    if t.starts_with("✔") || t.starts_with("✓") || t.starts_with("ok ") || t.contains(" → ok")
    {
        return OKGREEN;
    }
    // Error / warning markers
    if t.starts_with("✗")
        || t.starts_with("error:")
        || t.starts_with("Error:")
        || t.starts_with("failed")
        || t.contains("FAILED")
    {
        return WARNRED;
    }
    // Tool invocations (Read / Edit / Write / Bash / shell / etc.)
    let tool_prefixes = [
        "Read ", "Edit ", "Write ", "Bash ", "shell ", "→ ", "tool:", "Tool:", "⚙",
    ];
    if tool_prefixes.iter().any(|p| t.starts_with(p)) {
        return TOOLCYAN;
    }
    // System/meta lines (dim)
    if t.starts_with('[') || t.starts_with("//") || t.starts_with('#') || t.starts_with("--") {
        return VERY_DIM;
    }
    BODY
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
    let w = width as usize;
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
    let status_glyph = if view.done { "✔ done" } else { "… running" };
    let cost_label = if view.cost > 0.0 {
        format!(" · ${:.4}", view.cost)
    } else {
        String::new()
    };
    // Header row 1: agent selector + name + status
    let nav_hint = if views.len() > 1 {
        format!("  [{}/{}]  ", selected + 1, views.len())
    } else {
        "  ".to_string()
    };
    let mut lines = vec![
        TextLine::from(vec![
            Span::styled(
                "  ⚒ ",
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                view.agent.clone(),
                Style::default().fg(TOOLCYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {status_glyph}{cost_label}"),
                Style::default().fg(DIM),
            ),
            Span::styled(nav_hint, Style::default().fg(VERY_DIM)),
        ]),
        TextLine::from(vec![Span::styled(
            format!("  {}", truncate(&view.task, w.saturating_sub(4))),
            Style::default().fg(DIM),
        )]),
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
        let color = line_color(entry);
        lines.push(TextLine::from(Span::styled(
            format!("  {}", truncate(entry, w.saturating_sub(2))),
            Style::default().fg(color),
        )));
    }
    // Pad so the footer sits at the bottom of the region.
    while lines.len() < h.saturating_sub(1) {
        lines.push(TextLine::default());
    }
    let shown_end = (scroll + body_h).min(total);
    let follow_hint = "  G tail · g top · ←→ switch · Esc close";
    lines.push(TextLine::from(Span::styled(
        format!(
            "  ── {}-{}/{} lines{follow_hint} ──",
            scroll.min(total.saturating_sub(1)) + usize::from(total > 0),
            shown_end,
            total,
        ),
        Style::default().fg(VERY_DIM),
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
