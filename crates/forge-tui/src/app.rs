//! Pure, testable TUI state and rendering for the inline-scrollback model.
//!
//! `App` folds [`PresenterEvent`]s into two kinds of state: *transient* state rendered
//! every frame in the small pinned live region (input, statusline, the in-flight reply,
//! the permission bar), and a *flush* outbox of finalized scrollback lines that the I/O
//! shell drains and pushes into the terminal's native scrollback (`insert_before`). The
//! line builders and `render_live` are free of terminal I/O so they stay TestBackend-able.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Wrap};
use ratatui::Frame;

use crate::PresenterEvent;

// Palette.
const ORANGE: Color = Color::Rgb(255, 145, 60); // brand accent
const USER: Color = Color::Rgb(125, 180, 255); // user messages
const DIM: Color = Color::Rgb(110, 110, 120); // secondary text
const OKGREEN: Color = Color::Rgb(120, 210, 140);
const ERRRED: Color = Color::Rgb(240, 110, 110);
const WARNYEL: Color = Color::Rgb(235, 200, 110);
const TOOLCYAN: Color = Color::Rgb(120, 200, 215);
const STATUSBG: Color = Color::Rgb(28, 28, 34); // status bar background

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// ANSI-Shadow block wordmark printed once into scrollback as the welcome banner.
const FORGE_WORDMARK: &[&str] = &[
    "███████╗ ██████╗ ██████╗  ██████╗ ███████╗",
    "██╔════╝██╔═══██╗██╔══██╗██╔════╝ ██╔════╝",
    "█████╗  ██║   ██║██████╔╝██║  ███╗█████╗  ",
    "██╔══╝  ██║   ██║██╔══██╗██║   ██║██╔══╝  ",
    "██║     ╚██████╔╝██║  ██║╚██████╔╝███████╗",
    "╚═╝      ╚═════╝ ╚═╝  ╚═╝ ╚═════╝ ╚══════╝",
];
const WORDMARK_WIDTH: u16 = 42;
const TAGLINE: &str = "model-mesh coding agent · type a task to begin";

/// Pinned live-region geometry. Fixed at terminal creation (ratatui inline viewports do
/// not resize at runtime), so kept small: only the in-flight reply edge, the permission
/// bar, the input box, and the statusline are pinned — finalized lines flow to scrollback.
pub const STREAM_PREVIEW_H: u16 = 3;
pub const PERMISSION_H: u16 = 1;
pub const INPUT_H: u16 = 3;
pub const STATUS_H: u16 = 1;
pub const LIVE_H: u16 = STREAM_PREVIEW_H + PERMISSION_H + INPUT_H + STATUS_H;

/// The Mesh routing decision currently displayed.
#[derive(Debug, Clone, Default)]
pub struct RoutingView {
    pub tier: String,
    pub model: String,
    pub rationale: String,
}

/// All state the TUI needs to render the pinned live region, plus the scrollback outbox.
#[derive(Debug, Clone, Default)]
pub struct App {
    pub session_id: String,
    pub routing: Option<RoutingView>,
    pub cost_usd: f64,
    pub done: bool,
    /// A pending permission question shown while the loop blocks on the user's y/n.
    pub prompt: Option<String>,
    /// The current input-line buffer (shown in the input box).
    pub input: String,
    /// The current *partial* (un-flushed, newline-free) line of the streaming reply.
    pub streaming: String,
    /// Accumulated reasoning/thinking text, flushed as a dim block before the answer.
    reasoning: String,
    /// True once the `⚒ forge` header for the in-flight reply has been flushed.
    streaming_active: bool,
    /// True while a turn is running (drives the thinking spinner).
    pub busy: bool,
    /// Animation tick, advanced by the render loop while busy.
    pub tick: usize,
    /// Finalized scrollback lines, in arrival order; drained by the I/O shell.
    flush: Vec<TextLine<'static>>,
}

/// A keystroke, decoupled from crossterm so input handling is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Char(char),
    Backspace,
    Enter,
    Esc,
}

/// The result of feeding a keystroke to the input line.
#[derive(Debug, PartialEq, Eq)]
pub enum InputOutcome {
    Editing,
    Submit(String),
    Quit,
}

/// Apply one keystroke to the input buffer (pure; no terminal I/O).
pub fn handle_key(input: &mut String, key: KeyKind) -> InputOutcome {
    match key {
        KeyKind::Char(c) => {
            input.push(c);
            InputOutcome::Editing
        }
        KeyKind::Backspace => {
            input.pop();
            InputOutcome::Editing
        }
        KeyKind::Enter => {
            if input.trim().is_empty() {
                InputOutcome::Editing
            } else {
                InputOutcome::Submit(std::mem::take(input))
            }
        }
        KeyKind::Esc => InputOutcome::Quit,
    }
}

impl App {
    /// Fold one presenter event into the view state, queueing any finalized scrollback.
    pub fn apply(&mut self, event: PresenterEvent) {
        match event {
            PresenterEvent::SessionStarted { id } => self.session_id = id,
            PresenterEvent::Routing {
                tier,
                model,
                rationale,
            } => {
                self.routing = Some(RoutingView {
                    tier,
                    model,
                    rationale,
                })
            }
            // A complete (non-streamed) assistant message: render markdown into scrollback.
            PresenterEvent::AssistantText(text) => {
                self.flush.push(header_line("⚒ forge", ORANGE));
                self.flush.extend(crate::render::markdown_to_lines(&text));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Reasoning(delta) => self.reasoning.push_str(&delta),
            PresenterEvent::AssistantDelta(delta) => {
                if !self.streaming_active {
                    self.flush_reasoning();
                    self.flush.push(header_line("⚒ forge", ORANGE));
                    self.streaming_active = true;
                }
                // Accumulate the whole reply; it's rendered as markdown on AssistantDone so
                // multi-line blocks (lists, code fences) stay whole. The growing tail shows
                // live (plain) in the preview. (Per-block streaming finalize is a follow-up.)
                self.streaming.push_str(&delta);
            }
            PresenterEvent::AssistantDone => {
                if self.streaming_active {
                    let rest = std::mem::take(&mut self.streaming);
                    if !rest.is_empty() {
                        self.flush.extend(crate::render::markdown_to_lines(&rest));
                    }
                    self.flush.push(TextLine::default());
                    self.streaming_active = false;
                } else {
                    // Reasoning arrived but no answer text streamed — still show the thinking.
                    self.flush_reasoning();
                }
            }
            PresenterEvent::Warning(msg) => self.flush.push(warning_line(&msg)),
            PresenterEvent::ToolStart { name, args } => {
                self.flush.push(tool_start_line(&name, &args))
            }
            PresenterEvent::ToolResult { name, ok, summary } => {
                self.flush.push(tool_result_line(&name, ok, &summary))
            }
            PresenterEvent::Cost { session_total_usd } => self.cost_usd = session_total_usd,
            PresenterEvent::Diff(diff) => {
                self.flush.extend(crate::render::diff_to_lines(&diff));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Done { .. } => self.done = true,
        }
    }

    /// Flush accumulated reasoning into scrollback as a dim "thinking" block (once), if any.
    fn flush_reasoning(&mut self) {
        if self.reasoning.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.reasoning);
        let dim = Style::default().fg(DIM);
        self.flush
            .push(TextLine::from(Span::styled("✱ thinking", dim)));
        for l in text.lines() {
            self.flush
                .push(TextLine::from(Span::styled(l.to_string(), dim)));
        }
        self.flush.push(TextLine::default());
    }

    /// Echo a just-submitted user message into scrollback.
    pub fn submit_user(&mut self, line: &str) {
        self.flush.push(header_line("you", USER));
        for l in line.lines() {
            self.flush.push(body_line(l));
        }
        self.flush.push(TextLine::default());
    }

    /// Take the finalized scrollback lines queued since the last call.
    pub fn drain_flush(&mut self) -> Vec<TextLine<'static>> {
        std::mem::take(&mut self.flush)
    }
}

// ---- Scrollback line builders (own their text; identical styling to the old panel). ----

fn header_line(label: &str, color: Color) -> TextLine<'static> {
    TextLine::from(Span::styled(
        format!("  {label}"),
        Style::default().fg(color).bold(),
    ))
}

fn body_line(text: &str) -> TextLine<'static> {
    TextLine::from(format!("  {text}"))
}

fn warning_line(msg: &str) -> TextLine<'static> {
    TextLine::from(Span::styled(
        format!("  ⚠ {msg}"),
        Style::default().fg(WARNYEL),
    ))
}

fn tool_start_line(name: &str, args: &str) -> TextLine<'static> {
    TextLine::from(vec![
        Span::styled("  ↳ ", Style::default().fg(TOOLCYAN)),
        Span::styled(name.to_string(), Style::default().fg(TOOLCYAN).bold()),
        Span::styled(
            format!("  {}", truncate(args, 48)),
            Style::default().fg(DIM),
        ),
    ])
}

fn tool_result_line(name: &str, ok: bool, summary: &str) -> TextLine<'static> {
    let (mark, color) = if ok {
        ("  ✓ ", OKGREEN)
    } else {
        ("  ✗ ", ERRRED)
    };
    TextLine::from(vec![
        Span::styled(mark, Style::default().fg(color)),
        Span::styled(format!("{name}  "), Style::default().fg(color)),
        Span::styled(truncate(summary, 56), Style::default().fg(DIM)),
    ])
}

/// The welcome banner, printed once into scrollback. Centered via leading padding (so the
/// generic, left-aligned `insert_before` path renders it correctly). Narrow fallback.
pub fn banner_lines(width: u16) -> Vec<TextLine<'static>> {
    let center = |text: &str, text_w: usize, color: Color, bold: bool| -> TextLine<'static> {
        let pad = (width as usize).saturating_sub(text_w) / 2;
        let mut style = Style::default().fg(color);
        if bold {
            style = style.bold();
        }
        TextLine::from(vec![
            Span::raw(" ".repeat(pad)),
            Span::styled(text.to_string(), style),
        ])
    };

    let mut lines = vec![TextLine::default()];
    if width < WORDMARK_WIDTH {
        lines.push(center("⚒ FORGE", 7, ORANGE, true));
        lines.push(center("model-mesh coding agent", 23, DIM, false));
    } else {
        for row in FORGE_WORDMARK {
            lines.push(center(row, WORDMARK_WIDTH as usize, ORANGE, true));
        }
        lines.push(TextLine::default());
        lines.push(center(TAGLINE, TAGLINE.chars().count(), DIM, false));
    }
    lines.push(TextLine::default());
    lines
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s
    }
}

// ---- Live region (pinned at the bottom; rendered every frame). ----

/// Draw the pinned live region for the current state (the only thing in the viewport).
pub fn render_live(frame: &mut Frame, app: &App) {
    let areas = Layout::vertical([
        Constraint::Length(STREAM_PREVIEW_H), // in-flight reply edge (blank when idle)
        Constraint::Length(PERMISSION_H),     // permission bar (blank when none)
        Constraint::Length(INPUT_H),          // input box
        Constraint::Length(STATUS_H),         // statusline
    ])
    .split(frame.area());

    render_preview(frame, areas[0], app);
    if app.prompt.is_some() {
        render_permission(frame, areas[1], app);
    }
    render_input(frame, areas[2], app);
    render_statusline(frame, areas[3], app);
}

/// The in-flight streaming reply's trailing edge, scrolled to its bottom so the freshest
/// text and the `▌` cursor stay visible.
fn render_preview(frame: &mut Frame, area: Rect, app: &App) {
    if !app.streaming_active {
        return;
    }
    let line = TextLine::from(vec![
        Span::raw(format!("  {}", app.streaming)),
        Span::styled("▌", Style::default().fg(ORANGE)),
    ]);
    let para = Paragraph::new(line).wrap(Wrap { trim: false });
    let count = para.line_count(area.width) as u16;
    let scroll = count.saturating_sub(area.height);
    frame.render_widget(para.scroll((scroll, 0)), area);
}

fn render_permission(frame: &mut Frame, area: Rect, app: &App) {
    if let Some(p) = &app.prompt {
        frame.render_widget(
            Paragraph::new(TextLine::from(Span::styled(
                format!(" » {p}   [y]es / [N]o "),
                Style::default().fg(Color::Black).bg(WARNYEL).bold(),
            ))),
            area,
        );
    }
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ORANGE))
        .padding(Padding::horizontal(1))
        .title(Span::styled(" message ", Style::default().fg(ORANGE)));
    let line = TextLine::from(vec![
        Span::styled("› ", Style::default().fg(ORANGE).bold()),
        Span::raw(app.input.clone()),
        Span::styled("▌", Style::default().fg(ORANGE)),
    ]);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

/// A real status bar: working state · mesh tier+model · cost, with right-aligned key
/// hints. Lower-priority segments drop out on narrow terminals; model+cost always show.
fn render_statusline(frame: &mut Frame, area: Rect, app: &App) {
    let bg = Style::default().bg(STATUSBG);
    let w = area.width;
    let sep = || Span::styled("  ·  ", Style::default().fg(DIM).bg(STATUSBG));

    let model = app
        .routing
        .as_ref()
        .map(|r| r.model.as_str())
        .unwrap_or("—");
    let tier = app.routing.as_ref().map(|r| r.tier.as_str());

    let mut left: Vec<Span> = vec![Span::styled(" ", bg)];
    if app.busy && w >= 40 {
        let f = SPINNER[app.tick % SPINNER.len()];
        left.push(Span::styled(
            format!("{f} working"),
            Style::default().fg(ORANGE).bg(STATUSBG),
        ));
        left.push(sep());
    }
    if let (Some(t), true) = (tier, w >= 52) {
        left.push(Span::styled(
            format!("[{t}] "),
            Style::default().fg(ORANGE).bold().bg(STATUSBG),
        ));
    }
    left.push(Span::styled(
        model.to_string(),
        Style::default().fg(Color::White).bg(STATUSBG),
    ));
    left.push(sep());
    left.push(Span::styled(
        format!("${:.4}", app.cost_usd),
        Style::default().fg(OKGREEN).bold().bg(STATUSBG),
    ));

    if w >= 70 {
        let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(22)]).split(area);
        frame.render_widget(Paragraph::new(TextLine::from(left)).style(bg), cols[0]);
        let hint = if app.done {
            "done · esc quit "
        } else {
            "↵ send · esc quit "
        };
        frame.render_widget(
            Paragraph::new(TextLine::from(Span::styled(
                hint,
                Style::default().fg(DIM).bg(STATUSBG),
            )))
            .alignment(Alignment::Right)
            .style(bg),
            cols[1],
        );
    } else {
        frame.render_widget(Paragraph::new(TextLine::from(left)).style(bg), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the pinned live region at the natural live height.
    fn screen(app: &App) -> String {
        screen_wh(app, 80, LIVE_H)
    }

    fn screen_wh(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render_live(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// Concatenated text of everything queued for scrollback.
    fn flush_text(app: &mut App) -> String {
        app.drain_flush()
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn welcome_banner_builds_ascii_wordmark() {
        let text: String = banner_lines(80)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains('█'), "ASCII wordmark in banner");
        assert!(
            text.contains("model-mesh coding agent"),
            "tagline in banner"
        );
    }

    #[test]
    fn narrow_terminal_banner_falls_back_to_compact_wordmark() {
        let text: String = banner_lines(30)
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(
            text.contains("FORGE"),
            "compact wordmark on narrow terminal"
        );
        assert!(!text.contains('█'), "no block art when too narrow");
    }

    #[test]
    fn user_message_is_queued_to_scrollback() {
        let mut app = App::default();
        app.submit_user("my own task");
        let text = flush_text(&mut app);
        assert!(text.contains("you"));
        assert!(text.contains("my own task"));
    }

    #[test]
    fn assistant_text_is_queued_to_scrollback() {
        let mut app = App::default();
        app.apply(PresenterEvent::AssistantText(
            "the workspace looks healthy".into(),
        ));
        assert!(flush_text(&mut app).contains("the workspace looks healthy"));
    }

    #[test]
    fn tool_invocation_is_queued_to_scrollback() {
        let mut app = App::default();
        app.apply(PresenterEvent::ToolStart {
            name: "read_file".into(),
            args: "{\"path\":\"Cargo.toml\"}".into(),
        });
        app.apply(PresenterEvent::ToolResult {
            name: "read_file".into(),
            ok: true,
            summary: "[workspace]".into(),
        });
        assert!(flush_text(&mut app).contains("read_file"));
    }

    #[test]
    fn budget_warning_is_queued_to_scrollback() {
        let mut app = App::default();
        app.apply(PresenterEvent::Warning(
            "approaching daily budget cap".into(),
        ));
        assert!(flush_text(&mut app).contains("approaching daily budget cap"));
    }

    #[test]
    fn streaming_accumulates_and_shows_live_until_done() {
        let mut app = App::default();
        app.apply(PresenterEvent::AssistantDelta("first line\nsecond ".into()));
        // The header is queued on the first delta; the body accumulates live (rendered as
        // markdown only on Done, so multi-line blocks stay whole).
        assert!(
            flush_text(&mut app).contains("⚒ forge"),
            "header flushed on first delta"
        );
        assert_eq!(
            app.streaming, "first line\nsecond ",
            "reply accumulates live"
        );
        assert!(screen(&app).contains("second"), "tail shown in preview");
        assert!(screen(&app).contains('▌'), "cursor shown while streaming");
    }

    #[test]
    fn assistant_done_renders_reply_to_scrollback() {
        let mut app = App::default();
        app.apply(PresenterEvent::AssistantDelta("committed text".into()));
        app.apply(PresenterEvent::AssistantDone);
        assert!(app.streaming.is_empty(), "streaming buffer cleared");
        assert!(flush_text(&mut app).contains("committed text"));
    }

    #[test]
    fn assistant_markdown_is_rendered_not_literal() {
        let mut app = App::default();
        app.apply(PresenterEvent::AssistantDelta(
            "## Plan\n\n- do **it**\n".into(),
        ));
        app.apply(PresenterEvent::AssistantDone);
        let text = flush_text(&mut app);
        assert!(
            text.contains("Plan") && !text.contains("##"),
            "heading rendered: {text:?}"
        );
        assert!(text.contains("• do it"), "bullet + stripped bold: {text:?}");
    }

    #[test]
    fn statusline_shows_model_and_cost() {
        let mut app = App {
            cost_usd: 0.0042,
            ..Default::default()
        };
        app.apply(PresenterEvent::Routing {
            tier: "standard".into(),
            model: "openai::gpt-4o-mini".into(),
            rationale: "x".into(),
        });
        let text = screen(&app);
        assert!(text.contains("openai::gpt-4o-mini"), "model in statusline");
        assert!(text.contains("$0.0042"), "cost in statusline");
        assert!(text.contains("standard"), "tier in statusline");
    }

    #[test]
    fn cost_meter_shows_running_total() {
        let mut app = App::default();
        app.apply(PresenterEvent::Cost {
            session_total_usd: 0.0033,
        });
        assert!(screen(&app).contains("$0.0033"));
    }

    #[test]
    fn input_bar_renders_when_present() {
        let app = App {
            input: "fix the bug".to_string(),
            ..Default::default()
        };
        assert!(screen(&app).contains("› fix the bug"));
    }

    #[test]
    fn busy_shows_a_spinner_frame() {
        // SPINNER[2] == "⠹"; the statusline animates while a turn runs.
        let app = App {
            busy: true,
            tick: 2,
            ..Default::default()
        };
        assert!(screen(&app).contains('⠹'), "spinner frame shown when busy");
    }

    #[test]
    fn idle_shows_no_spinner() {
        let text = screen(&App::default());
        assert!(!text.contains('⠹') && !text.contains('⠙'));
    }

    #[test]
    fn typing_a_char_appends_and_keeps_editing() {
        let mut buf = String::new();
        assert_eq!(
            handle_key(&mut buf, KeyKind::Char('h')),
            InputOutcome::Editing
        );
        assert_eq!(
            handle_key(&mut buf, KeyKind::Char('i')),
            InputOutcome::Editing
        );
        assert_eq!(buf, "hi");
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut buf = "abc".to_string();
        assert_eq!(
            handle_key(&mut buf, KeyKind::Backspace),
            InputOutcome::Editing
        );
        assert_eq!(buf, "ab");
    }

    #[test]
    fn enter_submits_and_clears_buffer() {
        let mut buf = "do it".to_string();
        assert_eq!(
            handle_key(&mut buf, KeyKind::Enter),
            InputOutcome::Submit("do it".into())
        );
        assert_eq!(buf, "", "buffer cleared after submit");
    }

    #[test]
    fn enter_on_empty_buffer_keeps_editing() {
        let mut buf = "   ".to_string();
        assert_eq!(handle_key(&mut buf, KeyKind::Enter), InputOutcome::Editing);
    }

    #[test]
    fn esc_quits() {
        let mut buf = "whatever".to_string();
        assert_eq!(handle_key(&mut buf, KeyKind::Esc), InputOutcome::Quit);
    }
}
