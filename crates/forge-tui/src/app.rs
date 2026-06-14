//! Pure, testable TUI state and rendering. `App` folds [`PresenterEvent`]s into state;
//! `render` draws that state with ratatui. Both are free of terminal I/O so they can be
//! exercised offline with ratatui's `TestBackend`.

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

/// ANSI-Shadow block wordmark shown as the welcome banner.
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

/// The Mesh routing decision currently displayed.
#[derive(Debug, Clone, Default)]
pub struct RoutingView {
    pub tier: String,
    pub model: String,
    pub rationale: String,
}

/// One rendered line in the conversation transcript.
#[derive(Debug, Clone)]
pub enum Line {
    User(String),
    Assistant(String),
    ToolStart {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        summary: String,
    },
}

/// All state the TUI needs to render a session.
#[derive(Debug, Clone, Default)]
pub struct App {
    pub session_id: String,
    pub routing: Option<RoutingView>,
    pub lines: Vec<Line>,
    pub cost_usd: f64,
    pub warnings: Vec<String>,
    pub done: bool,
    /// A pending permission question shown while the TUI blocks on the user's y/n.
    pub prompt: Option<String>,
    /// The current input-line buffer (shown in the input bar during chat).
    pub input: String,
    /// The assistant reply currently streaming in (committed to `lines` when done).
    pub streaming: String,
    /// True while a turn is running (drives the thinking spinner).
    pub busy: bool,
    /// Animation tick, advanced by the render loop while busy.
    pub tick: usize,
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
    /// Fold one presenter event into the view state.
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
            PresenterEvent::AssistantText(text) => self.lines.push(Line::Assistant(text)),
            PresenterEvent::AssistantDelta(delta) => self.streaming.push_str(&delta),
            PresenterEvent::AssistantDone => {
                if !self.streaming.is_empty() {
                    self.lines
                        .push(Line::Assistant(std::mem::take(&mut self.streaming)));
                }
            }
            PresenterEvent::Warning(msg) => self.warnings.push(msg),
            PresenterEvent::ToolStart { name, args } => {
                self.lines.push(Line::ToolStart { name, args })
            }
            PresenterEvent::ToolResult { name, ok, summary } => {
                self.lines.push(Line::ToolResult { name, ok, summary })
            }
            PresenterEvent::Cost { session_total_usd } => self.cost_usd = session_total_usd,
            PresenterEvent::Done { .. } => self.done = true,
        }
    }
}

/// Draw the whole UI for the current state.
pub fn render(frame: &mut Frame, app: &App) {
    // Welcome state: nothing has happened yet -> show the brand banner instead of an
    // empty transcript, and hide the compact header (the banner is the brand).
    let welcome = app.lines.is_empty() && app.streaming.is_empty() && app.warnings.is_empty();
    let header_h = u16::from(!welcome);
    let prompt_h = app.prompt.is_some() as u16;
    let areas = Layout::vertical([
        Constraint::Length(header_h), // compact brand header (hidden on welcome)
        Constraint::Min(1),           // banner (welcome) or conversation
        Constraint::Length(prompt_h), // permission bar (0 when none)
        Constraint::Length(3),        // input box
        Constraint::Length(1),        // statusline
    ])
    .split(frame.area());

    if !welcome {
        render_header(frame, areas[0], app);
    }
    if welcome {
        render_banner(frame, areas[1]);
    } else {
        render_conversation(frame, areas[1], app);
    }
    if app.prompt.is_some() {
        render_permission(frame, areas[2], app);
    }
    render_input(frame, areas[3], app);
    render_statusline(frame, areas[4], app);
}

/// The ASCII welcome banner, centered (with a narrow-terminal fallback).
fn render_banner(frame: &mut Frame, area: Rect) {
    let mut lines: Vec<TextLine> = Vec::new();
    if area.width < WORDMARK_WIDTH {
        lines.push(TextLine::default());
        lines.push(TextLine::from(Span::styled(
            "⚒ FORGE",
            Style::default().fg(ORANGE).bold(),
        )));
        lines.push(TextLine::from(Span::styled(
            "model-mesh coding agent",
            Style::default().fg(DIM),
        )));
    } else {
        let content_h = FORGE_WORDMARK.len() as u16 + 2;
        let pad = area.height.saturating_sub(content_h) / 2;
        for _ in 0..pad {
            lines.push(TextLine::default());
        }
        for row in FORGE_WORDMARK {
            lines.push(TextLine::from(Span::styled(
                *row,
                Style::default().fg(ORANGE).bold(),
            )));
        }
        lines.push(TextLine::default());
        lines.push(TextLine::from(Span::styled(
            TAGLINE,
            Style::default().fg(DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        " ⚒ FORGE ",
        Style::default().fg(Color::Black).bg(ORANGE).bold(),
    )];
    if !app.session_id.is_empty() {
        let short: String = app.session_id.chars().take(8).collect();
        spans.push(Span::styled(format!("  {short}"), Style::default().fg(DIM)));
    }
    frame.render_widget(Paragraph::new(TextLine::from(spans)), area);
}

fn render_conversation(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<TextLine> = Vec::new();
    let push_block = |label: &str, color: Color, body: &str, lines: &mut Vec<TextLine>| {
        lines.push(TextLine::from(Span::styled(
            format!("  {label}"),
            Style::default().fg(color).bold(),
        )));
        for l in body.lines() {
            lines.push(TextLine::from(format!("  {l}")));
        }
        lines.push(TextLine::default());
    };

    for line in &app.lines {
        match line {
            Line::User(t) => push_block("you", USER, t, &mut lines),
            Line::Assistant(t) => push_block("⚒ forge", ORANGE, t, &mut lines),
            Line::ToolStart { name, args } => lines.push(TextLine::from(vec![
                Span::styled("  ↳ ", Style::default().fg(TOOLCYAN)),
                Span::styled(name.clone(), Style::default().fg(TOOLCYAN).bold()),
                Span::styled(
                    format!("  {}", truncate(args, 48)),
                    Style::default().fg(DIM),
                ),
            ])),
            Line::ToolResult { name, ok, summary } => {
                let (mark, color) = if *ok {
                    ("  ✓ ", OKGREEN)
                } else {
                    ("  ✗ ", ERRRED)
                };
                lines.push(TextLine::from(vec![
                    Span::styled(mark, Style::default().fg(color)),
                    Span::styled(format!("{name}  "), Style::default().fg(color)),
                    Span::styled(truncate(summary, 56), Style::default().fg(DIM)),
                ]));
            }
        }
    }

    for w in &app.warnings {
        lines.push(TextLine::from(Span::styled(
            format!("  ⚠ {w}"),
            Style::default().fg(WARNYEL),
        )));
    }

    // The reply currently streaming in, shown live with a cursor.
    if !app.streaming.is_empty() {
        lines.push(TextLine::from(Span::styled(
            "  ⚒ forge",
            Style::default().fg(ORANGE).bold(),
        )));
        lines.push(TextLine::from(vec![
            Span::raw(format!("  {}", app.streaming)),
            Span::styled("▌", Style::default().fg(ORANGE)),
        ]));
    }

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .padding(Padding::horizontal(1))
        .title(Span::styled(" conversation ", Style::default().fg(DIM)));

    // Keep the latest content in view (approximate; wrapping may add lines).
    let inner_h = area.height.saturating_sub(2);
    let scroll = (lines.len() as u16).saturating_sub(inner_h);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
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

    fn screen(app: &App) -> String {
        screen_wh(app, 80, 24)
    }

    fn screen_wh(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| render(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn welcome_shows_ascii_banner() {
        let text = screen(&App::default());
        assert!(text.contains('█'), "ASCII wordmark shown on empty session");
        assert!(text.contains("model-mesh coding agent"), "tagline shown");
    }

    #[test]
    fn banner_gives_way_to_compact_header_when_active() {
        let mut app = App::default();
        app.lines.push(Line::User("hi".into()));
        let text = screen(&app);
        assert!(
            !text.contains("model-mesh coding agent"),
            "banner gone once active"
        );
        assert!(text.contains("FORGE"), "compact brand header shown");
        assert!(text.contains("hi"), "conversation shown");
    }

    #[test]
    fn narrow_terminal_falls_back_to_compact_wordmark() {
        let text = screen_wh(&App::default(), 30, 20);
        assert!(
            text.contains("FORGE"),
            "compact wordmark on narrow terminal"
        );
        assert!(
            !text.contains('█'),
            "no block art when too narrow (no wrap garbage)"
        );
    }

    #[test]
    fn statusline_shows_model_and_cost() {
        let mut app = App { cost_usd: 0.0042, ..Default::default() };
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
    fn routing_panel_shows_model_and_tier() {
        let mut app = App::default();
        app.apply(PresenterEvent::Routing {
            tier: "complex".into(),
            model: "anthropic::claude-opus-4-8".into(),
            rationale: "matched complex signal".into(),
        });
        let text = screen(&app);
        assert!(
            text.contains("claude-opus-4-8"),
            "missing model in:\n{text}"
        );
        assert!(text.contains("complex"), "missing tier in:\n{text}");
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
    fn assistant_text_appears_in_conversation() {
        let mut app = App::default();
        app.apply(PresenterEvent::AssistantText(
            "the workspace looks healthy".into(),
        ));
        assert!(screen(&app).contains("the workspace looks healthy"));
    }

    #[test]
    fn tool_invocation_appears_in_conversation() {
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
        let text = screen(&app);
        assert!(text.contains("read_file"), "missing tool name in:\n{text}");
    }

    #[test]
    fn budget_warning_is_displayed() {
        let mut app = App::default();
        app.apply(PresenterEvent::Warning(
            "approaching daily budget cap".into(),
        ));
        assert!(screen(&app).contains("approaching daily budget cap"));
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

    #[test]
    fn input_bar_renders_when_present() {
        let app = App {
            input: "fix the bug".to_string(),
            ..Default::default()
        };
        assert!(screen(&app).contains("› fix the bug"));
    }

    #[test]
    fn streamed_deltas_render_live_with_cursor() {
        let mut app = App::default();
        app.apply(PresenterEvent::AssistantDelta("hello ".into()));
        app.apply(PresenterEvent::AssistantDelta("world".into()));
        let text = screen(&app);
        assert!(text.contains("hello world"), "live stream shown:\n{text}");
        assert!(text.contains('▌'), "cursor shown while streaming");
    }

    #[test]
    fn assistant_done_commits_the_streamed_reply() {
        let mut app = App::default();
        app.apply(PresenterEvent::AssistantDelta("committed text".into()));
        app.apply(PresenterEvent::AssistantDone);
        assert!(app.streaming.is_empty(), "streaming buffer cleared");
        assert!(screen(&app).contains("committed text"));
    }

    #[test]
    fn user_message_is_shown() {
        let mut app = App::default();
        app.lines.push(Line::User("my own task".into()));
        let text = screen(&app);
        assert!(text.contains("you"));
        assert!(text.contains("my own task"));
    }

    #[test]
    fn busy_shows_a_spinner_frame() {
        // SPINNER[2] == "⠹"; the header animates while a turn runs.
        let app = App {
            busy: true,
            tick: 2,
            ..Default::default()
        };
        assert!(screen(&app).contains('⠹'), "spinner frame shown when busy");
    }

    #[test]
    fn idle_shows_no_spinner() {
        let app = App::default();
        let text = screen(&app);
        assert!(!text.contains('⠹') && !text.contains('⠙'));
    }
}
