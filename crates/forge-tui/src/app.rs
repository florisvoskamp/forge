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

use crate::{PresenterEvent, QChoice};

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
    /// The active operating temper label (e.g. "Guarded"), shown in the statusline.
    pub temper: String,
    /// An in-flight AskUserQuestion: the choices + whether free text is allowed. The question
    /// text + options are already in scrollback; the input line collects the answer.
    question: Option<(Vec<QChoice>, bool)>,
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
    /// Subagents in the current `spawn_agents` batch (RFC subagent-orchestration). Running rows
    /// animate with a spinner in the live preview; on completion each becomes a scrollback
    /// branch line, and the whole group folds (header + branches + footer) when all finish.
    subagents: Vec<SubRow>,
    /// The inline slash-command palette (RFC session-management-and-commands). Open while the
    /// input line starts with `/`.
    pub palette: crate::commands::Palette,
    /// The interactive session/checkpoint picker (RFC session-management-and-commands). Modal
    /// while open; reused for `/sessions`, `/resume`, and `/checkpoints`.
    pub picker: crate::commands::Picker,
    /// For the `/models` browser only: `Some(provider)` when drilled into a provider's models,
    /// `None` at the top-level provider list. Lets Esc step back a level instead of closing.
    pub models_drilled: Option<String>,
}

/// One subagent's live row in the TUI.
#[derive(Debug, Clone)]
struct SubRow {
    id: String,
    agent: String,
    task: String,
    /// Trailing edge of the child's streamed activity (RFC subagent-orchestration Phase 3b).
    last: String,
    done: bool,
    cost: f64,
}

/// A keystroke, decoupled from crossterm so input handling is testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Char(char),
    Backspace,
    Enter,
    Esc,
    /// Arrow up/down — navigate the command palette / pickers (ignored by the input line).
    Up,
    Down,
    /// TAB — complete the palette selection (ignored by the input line).
    Tab,
    /// SHIFT+TAB — cycle the operating temper (handled by the shell, not the input line).
    CycleTemper,
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
        // Navigation / temper keys are handled by the shell before reaching the input line.
        KeyKind::Up | KeyKind::Down | KeyKind::Tab | KeyKind::CycleTemper => InputOutcome::Editing,
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
            PresenterEvent::SubagentStart { id, agent, task } => {
                // First child of a batch opens the group box in scrollback.
                if self.subagents.is_empty() {
                    self.flush.push(subagent_header_line());
                }
                self.subagents.push(SubRow {
                    id,
                    agent,
                    task,
                    last: String::new(),
                    done: false,
                    cost: 0.0,
                });
            }
            PresenterEvent::SubagentProgress { id, snippet } => {
                if let Some(row) = self.subagents.iter_mut().find(|r| r.id == id && !r.done) {
                    // Keep only the trailing edge of the child's activity for its row.
                    row.last.push_str(snippet.replace('\n', " ").as_str());
                    let n = row.last.chars().count();
                    if n > 80 {
                        row.last = row.last.chars().skip(n - 80).collect();
                    }
                }
            }
            PresenterEvent::SubagentResult {
                id,
                agent,
                ok,
                summary,
                cost_usd,
            } => {
                self.flush
                    .push(subagent_branch_line(&agent, ok, cost_usd, &summary));
                if let Some(row) = self.subagents.iter_mut().find(|r| r.id == id) {
                    row.done = true;
                    row.cost = cost_usd;
                }
                // When every child in the batch has reported, close the box and clear the live
                // running list.
                if self.subagents.iter().all(|r| r.done) {
                    let n = self.subagents.len();
                    let total: f64 = self.subagents.iter().map(|r| r.cost).sum();
                    self.flush.push(subagent_footer_line(n, total));
                    self.subagents.clear();
                }
            }
            PresenterEvent::Diff(diff) => {
                self.flush.extend(crate::render::diff_to_lines(&diff));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::AssayProgress(msg) => {
                self.flush.push(TextLine::from(Span::styled(
                    format!("  {msg}"),
                    Style::default().fg(DIM),
                )));
            }
            PresenterEvent::AssayReport(report) => {
                self.flush
                    .extend(crate::render::assay_report_lines(&report));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Tasks(tasks) => {
                self.flush.extend(crate::render::task_list_lines(&tasks));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::McpStatus(servers) => {
                self.flush.extend(crate::render::mcp_status_lines(&servers));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Done { .. } => self.done = true,
        }
    }

    /// Update the active temper label. The colored statusline segment is the live indicator —
    /// switching no longer spams a scrollback line per change (that flooded the view on rapid
    /// SHIFT+TAB cycling).
    pub fn set_temper(&mut self, label: &str) {
        self.temper = label.to_string();
    }

    /// Begin an AskUserQuestion: render the question + numbered options into scrollback and arm
    /// the input line to collect the answer (a number picks an option; free text if allowed).
    pub fn set_question(&mut self, question: &str, options: &[QChoice], allow_other: bool) {
        self.flush.push(TextLine::from(vec![
            Span::styled("❓ ", Style::default().fg(ORANGE).bold()),
            Span::styled(question.to_string(), Style::default().fg(USER).bold()),
        ]));
        for (i, o) in options.iter().enumerate() {
            let mut spans = vec![
                Span::styled(format!("  {}) ", i + 1), Style::default().fg(ORANGE)),
                Span::raw(o.label.clone()),
            ];
            if !o.description.is_empty() {
                spans.push(Span::styled(
                    format!("  — {}", o.description),
                    Style::default().fg(DIM),
                ));
            }
            self.flush.push(TextLine::from(spans));
        }
        self.prompt = Some(if allow_other {
            "type a number, or your own answer".to_string()
        } else {
            "type the number of your choice".to_string()
        });
        self.question = Some((options.to_vec(), allow_other));
    }

    /// True while a question is awaiting an answer.
    pub fn awaiting_question(&self) -> bool {
        self.question.is_some()
    }

    /// Drop any in-flight question/permission prompt (e.g. when the turn is interrupted).
    pub fn clear_question(&mut self) {
        self.question = None;
        self.prompt = None;
    }

    /// Try to resolve a submitted line against the active question. `Some(answer)` clears the
    /// question; `None` means invalid input — keep the question open and re-prompt.
    pub fn resolve_question(&mut self, line: &str) -> Option<String> {
        let (opts, allow_other) = self.question.as_ref()?;
        let ans = crate::resolve_answer(line, opts, *allow_other)?;
        self.question = None;
        self.prompt = None;
        self.flush.push(TextLine::from(vec![
            Span::styled("  ↳ ", Style::default().fg(DIM)),
            Span::styled(ans.clone(), Style::default().fg(OKGREEN)),
        ]));
        Some(ans)
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

    /// Render a resumed session's prior transcript into scrollback (after a `/resume` swap), so
    /// the conversation reappears without restarting. User turns echo like live input; assistant
    /// turns render markdown under the `⚒ forge` header.
    pub fn replay_history(&mut self, msgs: &[(forge_types::Role, String)]) {
        for (role, content) in msgs {
            match role {
                forge_types::Role::User => self.submit_user(content),
                _ => {
                    self.flush.push(header_line("⚒ forge", ORANGE));
                    self.flush.extend(crate::render::markdown_to_lines(content));
                    self.flush.push(TextLine::default());
                }
            }
        }
    }

    /// Push a dim informational line into scrollback (command feedback, session lists, etc).
    pub fn note(&mut self, text: &str) {
        self.flush.push(TextLine::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(DIM),
        )));
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

/// Opens the subagent group box in scrollback.
fn subagent_header_line() -> TextLine<'static> {
    TextLine::from(vec![
        Span::styled("  ╭─ ", Style::default().fg(DIM)),
        Span::styled("subagents", Style::default().fg(TOOLCYAN).bold()),
        Span::styled(" ─────────────", Style::default().fg(DIM)),
    ])
}

/// One completed subagent as a branch of the group box.
fn subagent_branch_line(agent: &str, ok: bool, cost_usd: f64, summary: &str) -> TextLine<'static> {
    let (mark, color) = if ok {
        ("✓", OKGREEN)
    } else {
        ("✗", ERRRED)
    };
    TextLine::from(vec![
        Span::styled("  ├─ ", Style::default().fg(DIM)),
        Span::styled(format!("{mark} "), Style::default().fg(color)),
        Span::styled(format!("[{agent}] "), Style::default().fg(TOOLCYAN)),
        Span::styled(format!("${cost_usd:.4}  "), Style::default().fg(DIM)),
        Span::styled(truncate(summary, 44), Style::default().fg(DIM)),
    ])
}

/// Closes the subagent group box with a total.
fn subagent_footer_line(n: usize, total_usd: f64) -> TextLine<'static> {
    TextLine::from(Span::styled(
        format!("  ╰─ {n} agents · ${total_usd:.4}"),
        Style::default().fg(DIM),
    ))
}

/// A still-running subagent row for the live preview (animated spinner). Shows the child's live
/// activity tail once it starts streaming, falling back to the task before then.
fn subagent_running_line(spin: &str, agent: &str, task: &str, last: &str) -> TextLine<'static> {
    let detail = if last.trim().is_empty() { task } else { last };
    TextLine::from(vec![
        Span::styled(format!("  {spin} "), Style::default().fg(TOOLCYAN)),
        Span::styled(format!("[{agent}] "), Style::default().fg(TOOLCYAN).bold()),
        Span::styled(truncate(detail, 50), Style::default().fg(DIM)),
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

/// Color for a temper label, by permissiveness (the at-a-glance posture cue): Read-only=blue,
/// Ask=yellow, Auto-edit=green, Full=red. Unknown → cyan.
fn temper_color(label: &str) -> Color {
    match label {
        "Read-only" => USER,
        "Ask" => WARNYEL,
        "Auto-edit" => OKGREEN,
        "Full" => ERRRED,
        _ => TOOLCYAN,
    }
}

/// Color a `/models` browser row: provider rows blue; model rows by category (subscription=green,
/// frontier=orange, free=cyan, paid=yellow). Provider vs model is told apart by the `::` in `id`.
fn models_row_color(row: &crate::commands::PickerRow) -> Color {
    if !row.id.contains("::") {
        return USER; // a provider header row
    }
    let s = row.subtitle.to_lowercase();
    if s.contains("subscription") {
        OKGREEN
    } else if s.contains("frontier") {
        ORANGE
    } else if s.contains("free") {
        TOOLCYAN
    } else {
        WARNYEL // paid
    }
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

    if app.palette.open {
        render_palette(frame, areas[0], app);
    } else if app.picker.open {
        render_picker(frame, areas[0], app);
    } else {
        render_preview(frame, areas[0], app);
    }
    if app.prompt.is_some() {
        render_permission(frame, areas[1], app);
    }
    render_input(frame, areas[2], app);
    render_statusline(frame, areas[3], app);
}

/// The inline slash-command palette: a scrolling window of filtered commands, selected row
/// highlighted, revealed by an ease-in animation (RFC session-management-and-commands).
fn render_palette(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return; // degenerate viewport (e.g. 0-height terminal) — nothing to draw, never clamp(1,0).
    }
    let matches = app.palette.matches();
    if matches.is_empty() {
        frame.render_widget(
            Paragraph::new(TextLine::from(Span::styled(
                "  no commands match",
                Style::default().fg(DIM),
            ))),
            area,
        );
        return;
    }
    let h = area.height as usize;
    // Ease-in reveal: rows appear over the first few frames after opening.
    let revealed = ((app.palette.anim * h as f32).ceil() as usize).clamp(1, h);
    // Scroll so the selected row stays visible within the window.
    let start = app.palette.selected.saturating_sub(h.saturating_sub(1));
    let lines: Vec<TextLine> = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(revealed)
        .map(|(i, c)| {
            let selected = i == app.palette.selected;
            let marker = if selected { "▸ " } else { "  " };
            let name_style = if selected {
                Style::default().fg(ORANGE).bold()
            } else {
                Style::default().fg(USER)
            };
            TextLine::from(vec![
                Span::styled(format!("  {marker}/{}", c.name), name_style),
                Span::styled(format!("  {}", c.desc), Style::default().fg(DIM)),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// The interactive session/checkpoint picker: a heading + a scrolling, filter-narrowed window of
/// rows, the selected one highlighted, revealed by the same ease-in as the palette. Constrained
/// to the (fixed-height) inline live region, so it scrolls rather than growing.
fn render_picker(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return; // degenerate viewport — never clamp(1, 0).
    }
    let p = &app.picker;
    let matches = p.matches();
    let h = area.height as usize;
    let mut lines: Vec<TextLine> = Vec::with_capacity(h);

    // Heading: title · live filter (or hint) · position.
    let mut head = vec![Span::styled(
        format!("  {} ", p.heading),
        Style::default().fg(ORANGE).bold(),
    )];
    if p.query.is_empty() {
        head.push(Span::styled("(type to filter)", Style::default().fg(DIM)));
    } else {
        head.push(Span::styled(
            format!("/{}", p.query),
            Style::default().fg(USER),
        ));
    }
    if !matches.is_empty() {
        head.push(Span::styled(
            format!("  {}/{}", p.selected + 1, matches.len()),
            Style::default().fg(DIM),
        ));
    }
    lines.push(TextLine::from(head));

    if matches.is_empty() {
        lines.push(TextLine::from(Span::styled(
            "  no matches",
            Style::default().fg(DIM),
        )));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let list_h = h.saturating_sub(1); // rows below the heading
    let revealed = ((p.anim * list_h as f32).ceil() as usize).clamp(1, list_h.max(1));
    let start = p.selected.saturating_sub(list_h.saturating_sub(1));
    let tempers = p.kind == Some(crate::commands::PickerKind::Tempers);
    let models = p.kind == Some(crate::commands::PickerKind::Models);
    for (i, row) in matches.iter().enumerate().skip(start).take(revealed) {
        let selected = i == p.selected;
        let marker = if selected { "▸ " } else { "  " };
        // In the mode picker, color each row by its temper; in the models browser, by category.
        let base = if tempers {
            temper_color(&row.title)
        } else if models {
            models_row_color(row)
        } else {
            USER
        };
        let title_style = if selected {
            Style::default().fg(base).bold()
        } else {
            Style::default().fg(base)
        };
        lines.push(TextLine::from(vec![
            Span::styled(format!("  {marker}{}", row.title), title_style),
            Span::styled(
                format!("  {}", truncate(&row.subtitle, 44)),
                Style::default().fg(DIM),
            ),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The in-flight streaming reply's trailing edge, scrolled to its bottom so the freshest
/// text and the `▌` cursor stay visible.
fn render_preview(frame: &mut Frame, area: Rect, app: &App) {
    if app.streaming_active {
        let line = TextLine::from(vec![
            Span::raw(format!("  {}", app.streaming)),
            Span::styled("▌", Style::default().fg(ORANGE)),
        ]);
        let para = Paragraph::new(line).wrap(Wrap { trim: false });
        let count = para.line_count(area.width) as u16;
        let scroll = count.saturating_sub(area.height);
        frame.render_widget(para.scroll((scroll, 0)), area);
        return;
    }

    // While a spawn_agents batch runs (and nothing is streaming), animate the still-running
    // children here in the live region; finished ones have already flowed to scrollback.
    let running: Vec<&SubRow> = app.subagents.iter().filter(|r| !r.done).collect();
    if running.is_empty() {
        return;
    }
    let spin = SPINNER[app.tick % SPINNER.len()];
    let h = area.height as usize;
    let mut lines: Vec<TextLine> = running
        .iter()
        .take(h)
        .map(|r| subagent_running_line(spin, &r.agent, &r.task, &r.last))
        .collect();
    if running.len() > h {
        lines.pop();
        lines.push(TextLine::from(Span::styled(
            format!("  … +{} more running", running.len() - h + 1),
            Style::default().fg(DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
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
    // The active temper (operating mode), color-coded by how permissive it is so the current
    // posture reads at a glance: Read-only=blue, Ask=yellow, Auto-edit=green, Full=red.
    if !app.temper.is_empty() && w >= 46 {
        left.push(sep());
        left.push(Span::styled(
            format!("◆ {}", app.temper),
            Style::default()
                .fg(temper_color(&app.temper))
                .bold()
                .bg(STATUSBG),
        ));
    }

    if w >= 70 {
        let cols = Layout::horizontal([Constraint::Min(0), Constraint::Length(24)]).split(area);
        frame.render_widget(Paragraph::new(TextLine::from(left)).style(bg), cols[0]);
        let hint = if app.busy {
            "esc stop "
        } else if app.done {
            "done · esc quit "
        } else {
            "⇧⇥ temper · esc quit "
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
    fn subagent_batch_animates_live_then_folds_into_a_scrollback_box() {
        let mut app = App::default();
        app.apply(PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "reviewer".into(),
            task: "review the diff".into(),
        });
        app.apply(PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "find call sites".into(),
        });

        // Both children animate in the live region while running.
        let live = screen(&app);
        assert!(
            live.contains("reviewer"),
            "running child shown live: {live}"
        );

        // A streamed activity delta shows in that child's row (Phase 3b live streaming).
        app.apply(PresenterEvent::SubagentProgress {
            id: "a".into(),
            snippet: "inspecting auth".into(),
        });
        assert!(
            screen(&app).contains("inspecting auth"),
            "child's live activity tail shows in its row"
        );

        app.apply(PresenterEvent::SubagentResult {
            id: "a".into(),
            agent: "reviewer".into(),
            ok: true,
            summary: "2 issues".into(),
            cost_usd: 0.001,
        });
        // Box stays open (b still running) → b animates, a has flowed to scrollback.
        assert!(
            screen(&app).contains("general"),
            "remaining child still live"
        );

        app.apply(PresenterEvent::SubagentResult {
            id: "b".into(),
            agent: "general".into(),
            ok: true,
            summary: "5 sites".into(),
            cost_usd: 0.002,
        });

        // Once all done the live list clears and the group box is in scrollback.
        assert!(
            !screen(&app).contains("reviewer"),
            "running list cleared after all complete"
        );
        let sb = flush_text(&mut app);
        assert!(sb.contains("subagents"), "group header: {sb}");
        assert!(
            sb.contains("reviewer") && sb.contains("2 issues"),
            "branch: {sb}"
        );
        assert!(sb.contains("2 agents"), "footer with count: {sb}");
    }

    #[test]
    fn temper_shows_in_statusline_and_switching_does_not_spam_scrollback() {
        let mut app = App {
            temper: "Ask".into(),
            ..App::default()
        };
        // Wide enough that the temper segment renders.
        assert!(
            screen_wh(&app, 90, LIVE_H).contains("Ask"),
            "active temper shown in the statusline"
        );

        app.set_temper("Auto-edit");
        assert_eq!(app.temper, "Auto-edit");
        assert!(
            screen_wh(&app, 90, LIVE_H).contains("Auto-edit"),
            "statusline reflects the new temper"
        );
        // Switching updates the (colored) statusline indicator only — no per-switch scrollback
        // line (rapid SHIFT+TAB cycling used to flood the view).
        assert!(
            flush_text(&mut app).is_empty(),
            "switching the temper queues nothing to scrollback"
        );
    }

    #[test]
    fn temper_indicator_is_color_coded_by_posture() {
        // Each temper renders in its own color so the current posture reads at a glance.
        assert_eq!(temper_color("Read-only"), USER);
        assert_eq!(temper_color("Ask"), WARNYEL);
        assert_eq!(temper_color("Auto-edit"), OKGREEN);
        assert_eq!(temper_color("Full"), ERRRED);
    }

    #[test]
    fn question_renders_options_to_scrollback_and_resolves_an_answer() {
        let mut app = App::default();
        let options = vec![
            QChoice {
                label: "Postgres".into(),
                description: "relational".into(),
            },
            QChoice {
                label: "SQLite".into(),
                description: String::new(),
            },
        ];
        app.set_question("which database?", &options, true);
        assert!(app.awaiting_question());
        let sb = flush_text(&mut app);
        assert!(sb.contains("which database?"), "question shown: {sb}");
        assert!(
            sb.contains("1) Postgres") && sb.contains("2) SQLite"),
            "options numbered: {sb}"
        );

        // A number selects; the question clears.
        assert_eq!(app.resolve_question("2").as_deref(), Some("SQLite"));
        assert!(!app.awaiting_question());

        // Invalid input keeps the question open (None).
        app.set_question("again?", &options, false);
        assert_eq!(app.resolve_question("not-a-number"), None);
        assert!(
            app.awaiting_question(),
            "invalid answer keeps the question open"
        );
    }

    #[test]
    fn shift_tab_is_a_cycle_temper_key_not_an_edit() {
        let mut input = String::new();
        assert_eq!(
            handle_key(&mut input, KeyKind::CycleTemper),
            InputOutcome::Editing
        );
        assert!(input.is_empty(), "temper key never edits the input line");
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
    fn command_palette_renders_filtered_commands() {
        let mut app = App::default();
        app.palette.open_with("");
        app.palette.anim = 1.0; // fully revealed
        let text = screen(&app);
        assert!(text.contains("/help"), "palette shows commands: {text}");
        assert!(text.contains("▸"), "selected row marked");
    }

    #[test]
    fn picker_renders_heading_rows_and_selection() {
        use crate::commands::{PickerKind, PickerRow};
        let mut app = App::default();
        app.picker.open_with(
            PickerKind::Sessions,
            "resume a session",
            vec![PickerRow {
                id: "aaa".into(),
                title: "aaa  $0.01  2 msgs".into(),
                subtitle: "fix the auth bug".into(),
            }],
        );
        app.picker.anim = 1.0;
        let text = screen(&app);
        assert!(text.contains("resume a session"), "heading shown: {text}");
        assert!(text.contains("fix the auth bug"), "row subtitle shown");
        assert!(text.contains('▸'), "selected row marked");
    }

    #[test]
    fn picker_zero_height_does_not_panic() {
        use crate::commands::PickerKind;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::default();
        app.picker.open_with(PickerKind::Sessions, "resume", vec![]);
        let mut term = Terminal::new(TestBackend::new(80, 0)).unwrap();
        let _ = term.draw(|f| render_live(f, &app));
    }

    #[test]
    fn command_palette_zero_height_does_not_panic() {
        // Regression: clamp(1, 0) panicked on a 0-height viewport.
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::default();
        app.palette.open_with("");
        let mut term = Terminal::new(TestBackend::new(80, 0)).unwrap();
        // Must not panic.
        let _ = term.draw(|f| render_live(f, &app));
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
