//! Full-screen activity transcript viewer (Ctrl+O in the chat). Runs on the ALTERNATE screen so it
//! never pollutes the chat's inline scrollback, and a caller-supplied `refresh` closure is polled
//! every frame (it drains pending activity) so the selected entry AUTO-UPDATES while open. One
//! viewer themed per kind — main chat, subagents, and assay critics all render through it, using
//! the same pre-styled lines as the main chat, wrapped to the terminal width. The line layout
//! ([`transcript_lines`]) is pure → unit-tested without terminal I/O.

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

use crate::app::{ActivityKind, ActivityStatus, TranscriptView};

/// Run the full-screen activity transcript viewer on the alternate screen (so it never pollutes the
/// chat's scrollback). `initial_selected` opens at that entry index (0-based). `refresh` is called
/// every frame to drain pending activity and return the current views — so the selected entry's log
/// AUTO-UPDATES while open. Keys: ↑↓/j/k scroll, space/PgDn + u/d page, g/G top/tail, ←→/Tab switch
/// entry, Esc/q close.
pub fn run_transcript_viewer<F>(initial_selected: usize, mut refresh: F) -> io::Result<()>
where
    F: FnMut() -> Vec<TranscriptView>,
{
    crate::driver::install_panic_restore();
    enable_raw_mode()?;
    crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
    let res = browse(&mut refresh, initial_selected);
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    res
}

fn browse<F: FnMut() -> Vec<TranscriptView>>(
    refresh: &mut F,
    initial_selected: usize,
) -> io::Result<()> {
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut selected = initial_selected;
    // Start at the tail so the user immediately sees the most recent activity.
    let mut scroll = usize::MAX / 2;
    let mut follow = true; // auto-scroll to tail while content grows
    loop {
        let views = refresh();
        let n = views.len().max(1);
        selected = selected.min(n - 1);
        let width = term.size().map(|s| s.width).unwrap_or(80);
        let wrapped_len = views
            .get(selected)
            .map(|v| wrap_lines(&v.lines, width.saturating_sub(1) as usize).len())
            .unwrap_or(0);
        if follow {
            scroll = usize::MAX / 2;
        }
        scroll = scroll.min(wrapped_len.saturating_sub(1));
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
                if scroll >= wrapped_len.saturating_sub(1) {
                    follow = true;
                }
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                follow = false;
                scroll = scroll.saturating_sub(10);
            }
            KeyCode::PageDown | KeyCode::Char('d') | KeyCode::Char(' ') => {
                scroll = scroll.saturating_add(10);
                if scroll >= wrapped_len.saturating_sub(1) {
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
const WARNYEL: Color = Color::Rgb(235, 200, 110);
const OKGREEN: Color = Color::Rgb(120, 210, 140);
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

/// Wrap one styled line to `width` columns, preserving each span's style across the break. A blank
/// line stays one blank line. Measures terminal CELL width (CJK/emoji = 2) so wide glyphs don't
/// overflow; deterministic so the scroll math is exact.
pub(crate) fn wrap_lines(lines: &[TextLine<'_>], width: usize) -> Vec<TextLine<'static>> {
    if width == 0 {
        return lines
            .iter()
            .map(|l| {
                TextLine::from(
                    l.spans
                        .iter()
                        .map(|s| Span::styled(s.content.to_string(), s.style))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
    }
    use unicode_width::UnicodeWidthChar;
    let mut out: Vec<TextLine<'static>> = Vec::with_capacity(lines.len());
    for line in lines {
        let mut cur: Vec<Span<'static>> = Vec::new();
        let mut cur_w = 0usize;
        for span in &line.spans {
            let style = span.style;
            let mut buf = String::new();
            for ch in span.content.chars() {
                // Wrap on terminal CELL width, not char count: a CJK ideograph / emoji is 2 cells, so
                // counting it as 1 over-fills the row and the renderer overflows/truncates. A wide
                // glyph that won't fit in the remaining cells starts the next row (never split across).
                let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
                if cur_w + cw > width && cur_w > 0 {
                    if !buf.is_empty() {
                        cur.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    out.push(TextLine::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                }
                buf.push(ch);
                cur_w += cw;
                if cur_w >= width {
                    cur.push(Span::styled(std::mem::take(&mut buf), style));
                    out.push(TextLine::from(std::mem::take(&mut cur)));
                    cur_w = 0;
                }
            }
            if !buf.is_empty() {
                cur.push(Span::styled(buf, style));
            }
        }
        // Always emit a line for this logical line (even if empty → preserves blank spacing).
        out.push(TextLine::from(std::mem::take(&mut cur)));
    }
    out
}

fn kind_theme(kind: ActivityKind) -> (&'static str, Color) {
    match kind {
        ActivityKind::MainChat => ("●", TOOLCYAN),
        ActivityKind::Subagent => ("⚒", ORANGE),
        ActivityKind::AssayCritic => ("⚖", WARNYEL),
    }
}

/// Build the transcript view for `views[selected]` scrolled to `scroll`, sized to `height`×`width`:
/// a two-row header (kind glyph + title + status/cost, then subtitle + model), the visible slice of
/// that entry's wrapped lines, and a footer with position + key hints. Pure — unit-tested.
/// `selected`/`scroll` are clamped.
pub fn transcript_lines(
    views: &[TranscriptView],
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
                "  ⚒ activity",
                Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            )),
            TextLine::from(Span::styled("  no activity yet", Style::default().fg(DIM))),
        ];
    }
    let selected = selected.min(views.len() - 1);
    let view = &views[selected];
    let (glyph, color) = kind_theme(view.kind);
    let (status, status_color) = match view.status {
        ActivityStatus::Running => ("… running", DIM),
        ActivityStatus::Done => ("✔ done", OKGREEN),
        ActivityStatus::Skipped => ("⏭ skipped", DIM),
    };
    let cost = if view.cost > 0.0 {
        format!(" · ${:.4}", view.cost)
    } else {
        String::new()
    };
    let nav = if views.len() > 1 {
        format!("  [{}/{}]", selected + 1, views.len())
    } else {
        String::new()
    };
    let model = view
        .model
        .as_deref()
        .map(|m| m.split("::").last().unwrap_or(m).to_string());

    let mut header2_spans = vec![Span::styled(
        format!(
            "  {}",
            truncate(&view.subtitle, w.saturating_sub(20).max(10))
        ),
        Style::default().fg(DIM),
    )];
    if let Some(m) = model {
        header2_spans.push(Span::styled(
            format!("  [{m}]"),
            Style::default().fg(VERY_DIM),
        ));
    }

    let mut lines = vec![
        TextLine::from(vec![
            Span::styled(
                format!("  {glyph} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                view.title.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {status}"), Style::default().fg(status_color)),
            Span::styled(cost, Style::default().fg(DIM)),
            Span::styled(nav, Style::default().fg(VERY_DIM)),
        ]),
        TextLine::from(header2_spans),
    ];

    // Body: the wrapped log window. Reserve 2 header + 1 footer rows.
    let body_h = h.saturating_sub(3).max(1);
    let wrapped = wrap_lines(&view.lines, w.saturating_sub(1));
    let total = wrapped.len();
    let max_scroll = total.saturating_sub(body_h);
    let scroll = scroll.min(max_scroll);
    if wrapped.is_empty() {
        lines.push(TextLine::from(Span::styled(
            "  (no activity captured yet)",
            Style::default().fg(DIM),
        )));
    }
    for line in wrapped.into_iter().skip(scroll).take(body_h) {
        lines.push(line);
    }
    // Pad so the footer sits at the bottom of the region.
    while lines.len() < h.saturating_sub(1) {
        lines.push(TextLine::default());
    }
    let shown_end = (scroll + body_h).min(total);
    lines.push(TextLine::from(Span::styled(
        format!(
            "  ── {}-{}/{} lines · ↑↓ scroll · ←→ switch · G tail · Esc close ──",
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

    fn view(kind: ActivityKind, title: &str, n: usize, status: ActivityStatus) -> TranscriptView {
        TranscriptView {
            kind,
            title: title.into(),
            subtitle: "scan the repo".into(),
            model: Some("anthropic::opus".into()),
            status,
            cost: 0.0,
            lines: (0..n)
                .map(|i| TextLine::from(Span::raw(format!("line {i}"))))
                .collect(),
            line_count: n,
        }
    }

    fn render(lines: &[TextLine]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn header_shows_selected_of_total_title_status_and_model() {
        let vs = vec![
            view(
                ActivityKind::MainChat,
                "main chat",
                3,
                ActivityStatus::Running,
            ),
            view(ActivityKind::Subagent, "general", 1, ActivityStatus::Done),
        ];
        let txt = render(&transcript_lines(&vs, 1, 0, 20, 80));
        assert!(txt.contains("[2/2]"), "selector: {txt}");
        assert!(txt.contains("general"));
        assert!(txt.contains("done"));
        assert!(txt.contains("[opus]"), "model shown: {txt}");
    }

    #[test]
    fn scroll_offsets_the_log_window() {
        let vs = vec![view(
            ActivityKind::Subagent,
            "a",
            40,
            ActivityStatus::Running,
        )];
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
        let vs = vec![view(
            ActivityKind::Subagent,
            "a",
            3,
            ActivityStatus::Running,
        )];
        assert!(!transcript_lines(&vs, 99, 999, 10, 80).is_empty());
    }

    #[test]
    fn empty_views_render_placeholder() {
        let txt = render(&transcript_lines(&[], 0, 0, 10, 80));
        assert!(txt.contains("no activity"));
    }

    #[test]
    fn long_lines_wrap_to_width() {
        let long = "x".repeat(200);
        let view = TranscriptView {
            kind: ActivityKind::MainChat,
            title: "main chat".into(),
            subtitle: String::new(),
            model: None,
            status: ActivityStatus::Done,
            cost: 0.0,
            lines: vec![TextLine::from(Span::raw(long))],
            line_count: 1,
        };
        // One 200-char logical line wraps into several visual rows at width 40.
        let wrapped = wrap_lines(&view.lines, 39);
        assert!(wrapped.len() >= 5, "wrapped into {} rows", wrapped.len());
    }

    #[test]
    fn wide_glyph_lines_wrap_on_cell_width_not_char_count() {
        use unicode_width::UnicodeWidthStr;
        // 30 CJK ideographs = 60 CELLS. At width 10, each row must be ≤ 10 cells — a char-count
        // wrapper would pack 10 ideographs (20 cells) per row and overflow the column.
        let cjk = "日".repeat(30);
        let lines = vec![TextLine::from(Span::raw(cjk))];
        let wrapped = wrap_lines(&lines, 10);
        for row in &wrapped {
            let cells: usize = row
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                cells <= 10,
                "a wrapped row is {cells} cells — overflows width 10"
            );
        }
        assert!(
            wrapped.len() >= 6,
            "60 cells / 10 → ≥6 rows, got {}",
            wrapped.len()
        );
    }
}
