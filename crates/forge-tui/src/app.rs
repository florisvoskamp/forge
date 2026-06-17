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
/// Fixed inline-viewport height. Large enough to give the task + subagent panels their own rows
/// (each sized dynamically within `render_live`) while keeping a small idle footprint.
/// Cannot be resized at runtime — recreating the inline viewport pollutes the terminal scrollback.
pub const LIVE_H: u16 = 14;
/// Max task / running-subagent rows shown in their sticky panels before summarizing the overflow.
const TASKS_PANEL_MAX: usize = 6;
const SUBAGENTS_PANEL_MAX: usize = 4;

/// The Mesh routing decision currently displayed.
#[derive(Debug, Clone, Default)]
pub struct RoutingView {
    pub tier: String,
    pub model: String,
    pub rationale: String,
}

/// Data for the `/usage` overlay — API spend + token breakdown across providers.
#[derive(Debug, Default, Clone)]
pub struct UsageOverlay {
    pub open: bool,
    /// Per-model rows for the last 5 hours: (model, cost_usd, input_tokens, output_tokens).
    pub by_model_5h: Vec<(String, f64, u64, u64)>,
    /// Per-model rows for today: (model, cost_usd, input_tokens, output_tokens).
    pub by_model: Vec<(String, f64, u64, u64)>,
    /// Per-model rows for this week: (model, cost_usd, input_tokens, output_tokens).
    pub by_model_week: Vec<(String, f64, u64, u64)>,
    /// This month's total spend in USD (scalar; not per-model).
    pub month_usd: f64,
    /// Session spend in USD (from the running Cost events).
    pub session_usd: f64,
    /// Session input tokens.
    pub session_in: u64,
    /// Session output tokens.
    pub session_out: u64,
    /// Daily cap (from config), None if uncapped.
    pub daily_cap: Option<f64>,
    /// Weekly cap (from config), None if uncapped.
    pub weekly_cap: Option<f64>,
    /// Monthly cap (from config), None if uncapped.
    pub monthly_cap: Option<f64>,
    /// Codex 5-hour used % (0–100), from latest local session file.
    pub codex_5h_pct: Option<f64>,
    /// Codex weekly used % (0–100), from latest local session file.
    pub codex_weekly_pct: Option<f64>,
    /// Claude 5-hour used % (0–100), from ~/.claude/.rate-limits-cache.json written by statusline.
    pub claude_5h_pct: Option<f64>,
    /// Claude weekly used % (0–100), from ~/.claude/.rate-limits-cache.json.
    pub claude_weekly_pct: Option<f64>,
    /// Claude tokens (input incl cache) used in the last 5 hours.
    pub claude_5h_in: u64,
    pub claude_5h_out: u64,
    /// Claude tokens used this ISO week.
    pub claude_weekly_in: u64,
    pub claude_weekly_out: u64,
    /// Animation tick counter (incremented each tick, used for spinner).
    pub anim_tick: u32,
}

impl UsageOverlay {
    fn totals(rows: &[(String, f64, u64, u64)]) -> (f64, u64, u64) {
        rows.iter().fold((0.0, 0, 0), |acc, r| {
            (acc.0 + r.1, acc.1 + r.2, acc.2 + r.3)
        })
    }
}

/// One subscription's quota row in the `/mesh` inspector.
#[derive(Debug, Default, Clone)]
pub struct MeshQuotaRow {
    pub provider: String,
    /// Window fraction consumed (0.0–1.0).
    pub fraction: f64,
    pub plan: String,
    /// "Ok" / "Warning" / "Exhausted".
    pub status: String,
    /// Probability a complex task spreads off this subscription (0.0–1.0).
    pub spread_complex: f64,
}

/// One scored candidate row in the `/mesh` inspector.
#[derive(Debug, Default, Clone)]
pub struct MeshCandRow {
    pub rank: usize,
    pub model: String,
    pub score: f64,
    /// "free" / "subscription" / "paid".
    pub cost_tag: String,
    pub frontier: bool,
    pub usable: bool,
    pub selected: bool,
    /// Conservation demotion applied (0.0 = none).
    pub penalty: f64,
}

/// Data for the `/mesh` overlay — a legible, animated trace of one routing decision (or the
/// per-tier overview when no prompt is given). Populated by the binary from the mesh's
/// RoutingExplanation engine; the TUI only renders the plain fields.
#[derive(Debug, Default, Clone)]
pub struct MeshOverlay {
    pub open: bool,
    /// The explained prompt ("" = overview mode).
    pub prompt: String,
    pub classified: String,
    pub routed: String,
    pub code_heavy: bool,
    pub reasons: String,
    /// Pre-rendered conservation verdict line.
    pub conserve_line: String,
    pub conserve_fired: bool,
    pub quota: Vec<MeshQuotaRow>,
    pub candidates: Vec<MeshCandRow>,
    pub pick: String,
    pub fallbacks: Vec<String>,
    pub rationale: String,
    /// Animation tick — drives the bar-fill ease and the row-by-row candidate reveal.
    pub anim_tick: u32,
}

/// All state the TUI needs to render the pinned live region, plus the scrollback outbox.
#[derive(Debug, Clone, Default)]
pub struct App {
    pub session_id: String,
    pub routing: Option<RoutingView>,
    pub cost_usd: f64,
    /// Live token counter (tui-token-counter.md): session totals + current context fill.
    pub session_in: u64,
    pub session_out: u64,
    pub context_tokens: u64,
    pub context_limit: Option<u32>,
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
    /// Per-critic rows for the live assay panel. Populated from AssayCriticRow events; cleared
    /// when the AssayReport arrives (the full report lands in scrollback instead).
    assay_critics: Vec<forge_types::AssayCriticRow>,
    /// The inline slash-command palette (RFC session-management-and-commands). Open while the
    /// input line starts with `/`.
    pub palette: crate::commands::Palette,
    /// The interactive session/checkpoint picker (RFC session-management-and-commands). Modal
    /// while open; reused for `/sessions`, `/resume`, and `/checkpoints`.
    pub picker: crate::commands::Picker,
    /// For the `/models` browser only: `Some(provider)` when drilled into a provider's models,
    /// `None` at the top-level provider list. Lets Esc step back a level instead of closing.
    pub models_drilled: Option<String>,
    /// The live task list (`update_tasks`). Kept so the sticky tasks panel stays visible during
    /// the turn (the inline scrollback copy scrolls away); cleared when the model empties the list.
    tasks: Vec<forge_types::TodoItem>,
    /// File-path picker for `@path` inline completion. Opens when the input contains `@…` at cursor.
    pub at_picker: crate::commands::AtPathPicker,
    /// A shell fix command from the last shell diagnosis. Pressing F (idle only) populates
    /// the input with this command for the user to review before submitting.
    pub pending_shell_fix: Option<String>,
    /// When true, the subagent picker overlay is shown in the stream area (opened by Ctrl+O when
    /// multiple subagents are in the current batch). ↑↓ navigate, Enter opens transcript, Esc closes.
    pub subagent_picking: bool,
    /// The currently highlighted row in the subagent picker.
    pub subagent_pick_idx: usize,
    /// The `/usage` overlay state.
    pub usage_overlay: UsageOverlay,
    /// The `/mesh` routing-inspector overlay state.
    pub mesh_overlay: MeshOverlay,
}

/// One subagent's live row in the TUI.
#[derive(Debug, Clone)]
struct SubRow {
    id: String,
    agent: String,
    task: String,
    /// Trailing edge of the child's streamed activity (RFC subagent-orchestration Phase 3b).
    last: String,
    /// Recent progress snippets, newest last, for the expandable detail view. Bounded so a chatty
    /// child can't grow the buffer without limit.
    log: Vec<String>,
    done: bool,
    cost: f64,
}

/// An owned snapshot of one subagent for the full-screen transcript browser ([`App::subagent_views`]).
#[derive(Debug, Clone)]
pub struct SubagentView {
    pub agent: String,
    pub task: String,
    pub done: bool,
    pub cost: f64,
    /// The child's full captured transcript (progress lines + the final result), oldest first.
    pub log: Vec<String>,
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
    /// CTRL+O — toggle the expanded detail view for the active subagents (shell-handled).
    ToggleSubagentDetail,
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
        KeyKind::Up
        | KeyKind::Down
        | KeyKind::Tab
        | KeyKind::CycleTemper
        | KeyKind::ToggleSubagentDetail => InputOutcome::Editing,
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
            PresenterEvent::ContextInjected {
                symbols,
                files,
                tokens,
            } => self.flush.push(lattice_line(symbols, files, tokens)),
            PresenterEvent::ShellDiagnosis {
                command,
                diagnosis,
                fix,
            } => {
                self.pending_shell_fix = fix.clone();
                self.flush
                    .extend(shell_diagnosis_lines(&command, &diagnosis, fix.as_deref()));
            }
            PresenterEvent::Cost {
                session_total_usd,
                session_in,
                session_out,
                context_tokens,
                context_limit,
            } => {
                self.cost_usd = session_total_usd;
                self.session_in = session_in;
                self.session_out = session_out;
                self.context_tokens = context_tokens;
                self.context_limit = context_limit;
            }
            PresenterEvent::SubagentStart { id, agent, task } => {
                // A new batch: the previous (all-finished) batch's rows are retained until now so
                // their transcripts stay viewable (Ctrl+O); drop them as the next batch begins.
                if !self.subagents.is_empty() && self.subagents.iter().all(|r| r.done) {
                    self.subagents.clear();
                }
                // First child of a batch opens the group box in scrollback.
                if self.subagents.is_empty() {
                    self.flush.push(subagent_header_line());
                }
                self.subagents.push(SubRow {
                    id,
                    agent,
                    task,
                    last: String::new(),
                    log: Vec::new(),
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
                    // Keep the FULL transcript for the scrollable browser (Ctrl+O), capped only by
                    // a high safety bound so a pathological child can't exhaust memory.
                    const MAX_LOG: usize = 10_000;
                    for piece in snippet.split('\n').filter(|p| !p.trim().is_empty()) {
                        row.log.push(piece.trim().to_string());
                    }
                    if row.log.len() > MAX_LOG {
                        let drop = row.log.len() - MAX_LOG;
                        row.log.drain(0..drop);
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
                    // Record the outcome at the tail of the transcript so the browser shows it.
                    row.log.push(String::new());
                    row.log.push(format!(
                        "── result ({}) ──",
                        if ok { "ok" } else { "failed" }
                    ));
                    for piece in summary.split('\n').filter(|p| !p.trim().is_empty()) {
                        row.log.push(piece.trim().to_string());
                    }
                }
                // When every child in the batch has reported, close the scrollback box. The rows
                // are KEPT (not cleared) so their full transcripts remain viewable via Ctrl+O until
                // the next batch starts; the live panel collapses on its own (no rows are running).
                if self.subagents.iter().all(|r| r.done) {
                    let n = self.subagents.len();
                    let total: f64 = self.subagents.iter().map(|r| r.cost).sum();
                    self.flush.push(subagent_footer_line(n, total));
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
            PresenterEvent::AssayCriticRow(row) => {
                // Update the live panel: insert on Queued, update status on Done/Skipped.
                if let Some(existing) = self.assay_critics.iter_mut().find(|r| r.lens == row.lens) {
                    existing.status = row.status;
                } else {
                    self.assay_critics.push(row);
                }
            }
            PresenterEvent::AssayReport(report) => {
                self.assay_critics.clear();
                self.flush
                    .extend(crate::render::assay_report_lines(&report));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Tasks(tasks) => {
                // The task list lives ONLY in the sticky panel above the input — it does not scroll
                // with the chat and is not part of history, so it is NOT flushed to scrollback.
                // An empty list collapses the panel.
                self.tasks = tasks;
            }
            PresenterEvent::McpStatus(servers) => {
                self.flush.extend(crate::render::mcp_status_lines(&servers));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Done { .. } => self.done = true,
            PresenterEvent::QuotaUpdate {
                provider,
                window,
                fraction,
            } => {
                let pct = Some(fraction * 100.0);
                match (provider.as_str(), window.as_str()) {
                    ("claude-cli", "five_hour") => self.usage_overlay.claude_5h_pct = pct,
                    ("claude-cli", "weekly") => self.usage_overlay.claude_weekly_pct = pct,
                    ("codex-cli", "five_hour") => self.usage_overlay.codex_5h_pct = pct,
                    ("codex-cli", "weekly") => self.usage_overlay.codex_weekly_pct = pct,
                    _ => {}
                }
            }
        }
    }

    /// The live task list (`update_tasks`), for the sticky tasks panel. Empty → panel hidden.
    pub fn tasks(&self) -> &[forge_types::TodoItem] {
        &self.tasks
    }

    /// Number of subagents currently running (between SubagentStart and SubagentResult). When `> 0`
    /// the sticky subagents panel is shown.
    pub fn running_subagents(&self) -> usize {
        self.subagents.iter().filter(|r| !r.done).count()
    }

    /// Rows the sticky tasks panel wants in the live region (0 = hidden). Header + up to
    /// [`TASKS_PANEL_MAX`] items + an overflow line. This is a DEDICATED region (not the stream
    /// preview) so the list stays visible even while the model is streaming.
    pub fn tasks_panel_height(&self) -> u16 {
        if self.tasks.is_empty() {
            return 0;
        }
        let shown = self.tasks.len().min(TASKS_PANEL_MAX);
        let overflow = u16::from(self.tasks.len() > TASKS_PANEL_MAX);
        1 + shown as u16 + overflow
    }

    /// Number of assay critics in the live panel (> 0 while an assay run is in flight).
    pub fn running_assay_critics(&self) -> usize {
        self.assay_critics.len()
    }

    /// Rows the live assay critics panel wants (0 = hidden). Header + one row per critic.
    pub fn assay_panel_height(&self) -> u16 {
        let n = self.assay_critics.len();
        if n == 0 {
            return 0;
        }
        1 + n as u16
    }

    /// Rows the running-subagents panel wants in the live region (0 = hidden). Counts the whole
    /// current batch (running + done) so the panel stays visible after all agents finish — without
    /// this, Start + Result events arriving in the same render drain leave the panel at height 0
    /// forever. The batch is cleared by [`on_turn_start`] when the user sends the next message.
    pub fn subagents_panel_height(&self) -> u16 {
        let n = self.subagents.len();
        if n == 0 {
            return 0;
        }
        let shown = n.min(SUBAGENTS_PANEL_MAX);
        let overflow = u16::from(n > SUBAGENTS_PANEL_MAX);
        1 + shown as u16 + overflow
    }

    /// Called at the start of each new user turn. Collapses the "done" subagent batch that the
    /// panel was holding so it doesn't bleed into the new turn's live region.
    pub fn on_turn_start(&mut self) {
        if !self.subagents.is_empty() && self.subagents.iter().all(|r| r.done) {
            self.subagents.clear();
        }
        self.pending_shell_fix = None;
    }

    /// Owned snapshot of the current batch's subagents (running + just-finished), in spawn order;
    /// empty when no batch has run yet. Feeds the full-screen transcript browser (Ctrl+O), which
    /// re-reads it as new progress is drained so a child's log updates live.
    pub fn subagent_views(&self) -> Vec<SubagentView> {
        self.subagents
            .iter()
            .map(|r| SubagentView {
                agent: r.agent.clone(),
                task: r.task.clone(),
                done: r.done,
                cost: r.cost,
                log: r.log.clone(),
            })
            .collect()
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

/// Render a symbol's scoped subgraph (the `/lattice` view) as styled scrollback lines: the
/// matching definitions, their reverse-dependents (blast radius), and a provenance line. Takes
/// plain tuples `(kind, name, rel_path, line)` so this crate needn't depend on forge-index.
#[allow(clippy::type_complexity)]
pub fn lattice_view_lines(
    query: &str,
    roots: &[(String, String, String, i64)],
    dependents: &[(String, String, String, i64)],
    why: Option<(String, String, String, String)>,
) -> Vec<TextLine<'static>> {
    let mut out = Vec::new();
    out.push(TextLine::from(vec![
        Span::styled("  ⌬ LATTICE ", Style::default().fg(TOOLCYAN).bold()),
        Span::styled(format!(" {query}"), Style::default().fg(DIM)),
    ]));
    if roots.is_empty() {
        out.push(TextLine::from(Span::styled(
            format!("    no symbols match '{query}' — run `forge lattice update`?"),
            Style::default().fg(DIM),
        )));
        return out;
    }
    for (kind, name, path, line) in roots {
        out.push(TextLine::from(vec![
            Span::styled("    ● ", Style::default().fg(ORANGE)),
            Span::styled(format!("{kind} "), Style::default().fg(DIM)),
            Span::styled(name.clone(), Style::default().fg(ORANGE).bold()),
            Span::styled(format!("  {path}:{line}"), Style::default().fg(DIM)),
        ]));
    }
    if !dependents.is_empty() {
        const MAX: usize = 20;
        out.push(TextLine::from(Span::styled(
            format!("    blast radius: {} reference(s)", dependents.len()),
            Style::default().fg(TOOLCYAN),
        )));
        for (kind, name, path, line) in dependents.iter().take(MAX) {
            out.push(TextLine::from(vec![
                Span::styled("    ← ", Style::default().fg(DIM)),
                Span::styled(format!("{kind} "), Style::default().fg(DIM)),
                Span::styled(name.clone(), Style::default()),
                Span::styled(format!("  {path}:{line}"), Style::default().fg(DIM)),
            ]));
        }
        if dependents.len() > MAX {
            out.push(TextLine::from(Span::styled(
                format!(
                    "    … +{} more (forge lattice impact)",
                    dependents.len() - MAX
                ),
                Style::default().fg(DIM),
            )));
        }
    }
    if let Some((author, date, commit, subject)) = why {
        out.push(TextLine::from(Span::styled(
            format!("    why: {author} · {date} · {commit} · {subject}"),
            Style::default().fg(DIM),
        )));
    }
    out
}

fn lattice_line(symbols: usize, files: usize, tokens: usize) -> TextLine<'static> {
    TextLine::from(vec![
        Span::styled("  ⌬ lattice ", Style::default().fg(TOOLCYAN).bold()),
        Span::styled(
            format!("→ injected {symbols} symbols · {files} files (~{tokens} tok)"),
            Style::default().fg(DIM),
        ),
    ])
}

/// A failed shell command + the model's likely-cause/fix, rendered as a header line plus one
/// dimmed line per diagnosis line (shell-error-interceptor.md).
fn shell_diagnosis_lines(
    command: &str,
    diagnosis: &str,
    fix: Option<&str>,
) -> Vec<TextLine<'static>> {
    let mut lines = vec![TextLine::from(vec![
        Span::styled("  ⚠ shell failed ", Style::default().fg(ERRRED).bold()),
        Span::styled(truncate(command, 56), Style::default().fg(DIM)),
    ])];
    for line in diagnosis.lines() {
        lines.push(TextLine::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(DIM),
        )));
    }
    if fix.is_some() {
        lines.push(TextLine::from(Span::styled(
            "    press F to populate fix command in input",
            Style::default().fg(TOOLCYAN),
        )));
    }
    lines
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
///
/// Layout (top → bottom, fixed total = LIVE_H):
///   [stream preview / picker / palette]  ← Min(1) after reserving panels
///   [running-subagents panel]             ← 0 when none running
///   [task list panel]                     ← 0 when empty
///   [permission bar]
///   [input box]
///   [statusline]
///
/// The inline viewport is never resized at runtime — recreating it would corrupt the scrollback.
/// The stream area shrinks as panels grow but always keeps ≥1 row (MIN_STREAM guarantee).
pub fn render_live(frame: &mut Frame, app: &App) {
    const MIN_STREAM: u16 = 1;
    let fixed = PERMISSION_H + INPUT_H + STATUS_H;
    let avail = frame.area().height.saturating_sub(fixed);
    let panel_avail = avail.saturating_sub(MIN_STREAM);

    // Subagent panel gets at most half; assay panel gets at most half of what remains; tasks get
    // the rest. Panels are typically mutually exclusive (assay and subagents don't run together).
    let sub_h = app.subagents_panel_height().min(panel_avail / 2);
    let assay_h = app
        .assay_panel_height()
        .min(panel_avail.saturating_sub(sub_h) / 2);
    let task_h = app
        .tasks_panel_height()
        .min(panel_avail.saturating_sub(sub_h + assay_h));
    let stream_h = avail.saturating_sub(sub_h + assay_h + task_h);

    let areas = Layout::vertical([
        Constraint::Length(stream_h),
        Constraint::Length(sub_h),
        Constraint::Length(assay_h),
        Constraint::Length(task_h),
        Constraint::Length(PERMISSION_H),
        Constraint::Length(INPUT_H),
        Constraint::Length(STATUS_H),
    ])
    .split(frame.area());

    // areas[0]: stream preview (or modal overlay when palette / picker / agent-picker is open).
    if app.palette.open {
        render_palette(frame, areas[0], app);
    } else if app.at_picker.open {
        render_at_path_picker(frame, areas[0], app);
    } else if app.picker.open {
        render_picker(frame, areas[0], app);
    } else if app.subagent_picking {
        render_subagent_picker(frame, areas[0], app);
    } else {
        render_preview(frame, areas[0], app);
    }
    if sub_h > 0 {
        render_subagents_panel(frame, areas[1], app);
    }
    if assay_h > 0 {
        render_assay_panel(frame, areas[2], app);
    }
    if task_h > 0 {
        frame.render_widget(
            Paragraph::new(tasks_panel_lines(&app.tasks, areas[3].height)),
            areas[3],
        );
    }
    if app.prompt.is_some() {
        render_permission(frame, areas[4], app);
    }
    render_input(frame, areas[5], app);
    render_statusline(frame, areas[6], app);
    // Usage overlay renders last so it appears on top of everything.
    render_usage_overlay(frame, app);
    render_mesh_overlay(frame, app);
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

/// The `@path` file-path picker: a scrolling, filter-narrowed list of files, revealed by the
/// same ease-in animation as the command palette.
fn render_at_path_picker(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let matches = app.at_picker.matches();
    if matches.is_empty() {
        frame.render_widget(
            Paragraph::new(TextLine::from(Span::styled(
                "  no files match",
                Style::default().fg(DIM),
            ))),
            area,
        );
        return;
    }
    let h = area.height as usize;
    let revealed = ((app.at_picker.anim * h as f32).ceil() as usize).clamp(1, h);
    let start = app.at_picker.selected.saturating_sub(h.saturating_sub(1));
    let lines: Vec<TextLine> = matches
        .iter()
        .enumerate()
        .skip(start)
        .take(revealed)
        .map(|(i, path)| {
            let selected = i == app.at_picker.selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default().fg(TOOLCYAN).bold()
            } else {
                Style::default().fg(USER)
            };
            TextLine::from(Span::styled(format!("  {marker}@{path}"), style))
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
    // Only the in-flight reply edge lives here now; the task + subagent panels are their own
    // always-visible regions (see `render_live`), so streaming no longer hides them.
    if app.streaming_active {
        let line = TextLine::from(vec![
            Span::raw(format!("  {}", app.streaming)),
            Span::styled("▌", Style::default().fg(ORANGE)),
        ]);
        let para = Paragraph::new(line).wrap(Wrap { trim: false });
        let count = para.line_count(area.width) as u16;
        let scroll = count.saturating_sub(area.height);
        frame.render_widget(para.scroll((scroll, 0)), area);
    }
}

/// The subagent picker overlay: shown in the stream area when Ctrl+O is pressed with multiple
/// agents. ↑↓ navigate, Enter opens that agent's full transcript, Esc closes.
fn render_subagent_picker(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let views = app.subagent_views();
    if views.is_empty() {
        return;
    }
    let h = area.height as usize;
    let mut lines: Vec<TextLine> = Vec::with_capacity(h);
    lines.push(TextLine::from(vec![
        Span::styled(
            format!("  ⚒ agents ({}) ", views.len()),
            Style::default().fg(ORANGE).bold(),
        ),
        Span::styled(
            "↑↓ select  ·  Enter open  ·  Esc close",
            Style::default().fg(DIM),
        ),
    ]));
    let list_h = h.saturating_sub(1);
    let start = app
        .subagent_pick_idx
        .saturating_sub(list_h.saturating_sub(1));
    for (i, v) in views.iter().enumerate().skip(start).take(list_h) {
        let selected = i == app.subagent_pick_idx;
        let marker = if selected { "▸ " } else { "  " };
        let status = if v.done { "done" } else { "…" };
        let name_style = if selected {
            Style::default().fg(ORANGE).bold()
        } else {
            Style::default().fg(TOOLCYAN)
        };
        lines.push(TextLine::from(vec![
            Span::styled(format!("  {marker}[{}] ", v.agent), name_style),
            Span::styled(
                format!("${:.4}  {status}  ", v.cost),
                Style::default().fg(DIM),
            ),
            Span::styled(truncate(&v.task, 44), Style::default().fg(DIM)),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The sticky running-subagents panel (its own live region): a header (with the Ctrl+O hint) then
/// one animated row per running child, sized to `area.height`, overflow summarized.
fn render_subagents_panel(frame: &mut Frame, area: Rect, app: &App) {
    if app.subagents.is_empty() {
        return;
    }
    let running: Vec<&SubRow> = app.subagents.iter().filter(|r| !r.done).collect();
    let h = area.height as usize;
    let mut lines: Vec<TextLine> = Vec::with_capacity(h);

    if running.is_empty() {
        // All agents in the batch finished — show a "done" summary until on_turn_start clears it.
        let n = app.subagents.len();
        lines.push(TextLine::from(vec![
            Span::styled(
                format!("  ⚒ subagents ({n} done)"),
                Style::default().fg(TOOLCYAN).bold(),
            ),
            Span::styled("  ^O transcript", Style::default().fg(DIM)),
        ]));
        let body_h = h.saturating_sub(1);
        for r in app.subagents.iter().take(body_h) {
            lines.push(TextLine::from(Span::styled(
                format!("    ✓ {}  {}", r.agent, r.task),
                Style::default().fg(DIM),
            )));
        }
    } else {
        let spin = SPINNER[app.tick % SPINNER.len()];
        lines.push(TextLine::from(vec![
            Span::styled(
                format!("  ⚒ subagents ({} running)", running.len()),
                Style::default().fg(TOOLCYAN).bold(),
            ),
            Span::styled("  ^O transcript", Style::default().fg(DIM)),
        ]));
        let body_h = h.saturating_sub(1);
        for r in running.iter().take(body_h) {
            lines.push(subagent_running_line(spin, &r.agent, &r.task, &r.last));
        }
        if running.len() > body_h {
            lines.pop();
            lines.push(TextLine::from(Span::styled(
                format!("  … +{} more running", running.len() - body_h + 1),
                Style::default().fg(DIM),
            )));
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The sticky live-assay panel: header showing total critic count, then one row per critic with
/// its current status glyph (queued / spinner / done / skipped). Cleared when AssayReport arrives.
fn render_assay_panel(frame: &mut Frame, area: Rect, app: &App) {
    use forge_types::AssayCriticStatus;
    if app.assay_critics.is_empty() {
        return;
    }
    let spin = SPINNER[app.tick % SPINNER.len()];
    let total = app.assay_critics.len();
    let done = app
        .assay_critics
        .iter()
        .filter(|r| !matches!(r.status, AssayCriticStatus::Queued))
        .count();
    let h = area.height as usize;
    let mut lines: Vec<TextLine> = Vec::with_capacity(h);
    lines.push(TextLine::from(Span::styled(
        format!("  ⚒ assay critics ({done}/{total})"),
        Style::default().fg(ORANGE).bold(),
    )));
    let body_h = h.saturating_sub(1);
    for r in app.assay_critics.iter().take(body_h) {
        let (glyph, style) = match &r.status {
            AssayCriticStatus::Queued => (format!("{spin} {}", r.lens), Style::default().fg(DIM)),
            AssayCriticStatus::Done { candidates } => (
                format!("✓ {} ({candidates})", r.lens),
                Style::default().fg(OKGREEN),
            ),
            AssayCriticStatus::Skipped { reason } => (
                format!("⏭ {} — {}", r.lens, truncate(reason, 40)),
                Style::default().fg(DIM),
            ),
        };
        lines.push(TextLine::from(Span::styled(format!("  {glyph}"), style)));
    }
    if app.assay_critics.len() > body_h {
        lines.pop();
        lines.push(TextLine::from(Span::styled(
            format!("  … +{} more", app.assay_critics.len() - body_h + 1),
            Style::default().fg(DIM),
        )));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// The sticky tasks panel (Task list always-visible): a header with the done/total count, then
/// the items with their status glyph, sized to the fixed live region. When the list is longer
/// than the region, the in-progress item is prioritized and the overflow is summarized.
fn tasks_panel_lines(tasks: &[forge_types::TodoItem], height: u16) -> Vec<TextLine<'static>> {
    use forge_types::TodoStatus;
    let h = height as usize;
    let done = tasks
        .iter()
        .filter(|t| t.status == TodoStatus::Done)
        .count();
    let mut lines = vec![TextLine::from(Span::styled(
        format!("  ⚒ tasks ({done}/{} done)", tasks.len()),
        Style::default().fg(ORANGE).bold(),
    ))];
    let body_h = h.saturating_sub(1);
    // Prioritize showing in-progress + pending items; if everything won't fit, lead with the
    // active item so the user always sees what's happening now.
    let mut idxs: Vec<usize> = (0..tasks.len()).collect();
    if tasks.len() > body_h {
        idxs.sort_by_key(|&i| match tasks[i].status {
            TodoStatus::InProgress => 0,
            TodoStatus::Pending => 1,
            TodoStatus::Done => 2,
        });
    }
    let shown = idxs
        .len()
        .min(body_h.saturating_sub(usize::from(tasks.len() > body_h)));
    for &i in idxs.iter().take(shown) {
        let t = &tasks[i];
        let style = match t.status {
            TodoStatus::Done => Style::default().fg(DIM),
            TodoStatus::InProgress => Style::default().fg(ORANGE).bold(),
            TodoStatus::Pending => Style::default().fg(Color::Rgb(205, 205, 215)),
        };
        lines.push(TextLine::from(Span::styled(
            format!("    {} {}", t.status.marker(), truncate(&t.title, 60)),
            style,
        )));
    }
    if tasks.len() > shown {
        lines.push(TextLine::from(Span::styled(
            format!("    … +{} more", tasks.len() - shown),
            Style::default().fg(DIM),
        )));
    }
    lines
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
    let mut spans = vec![Span::styled("› ", Style::default().fg(ORANGE).bold())];
    spans.extend(input_spans(&app.input));
    spans.push(Span::styled("▌", Style::default().fg(ORANGE)));
    frame.render_widget(Paragraph::new(TextLine::from(spans)).block(block), area);
}

/// Build the styled spans for the input buffer, highlighting a `/command` token wherever it
/// appears on the line (not only as the first word) so e.g. `please run /orchestrate` shows
/// `/orchestrate` in the command accent. The cursor sits at the end of the buffer, so the token
/// being edited is selected from there. A `//literal` escape stays plain.
fn input_spans(input: &str) -> Vec<Span<'static>> {
    if input.is_empty() {
        return vec![];
    }
    match crate::commands::slash_token_at(input, input.len()) {
        Some(tok) => {
            let mut out = Vec::with_capacity(3);
            if tok.start > 0 {
                out.push(Span::raw(input[..tok.start].to_string()));
            }
            out.push(Span::styled(
                input[tok.start..tok.end].to_string(),
                Style::default().fg(ORANGE).bold(),
            ));
            if tok.end < input.len() {
                out.push(Span::raw(input[tok.end..].to_string()));
            }
            out
        }
        None => vec![Span::raw(input.to_string())],
    }
}

/// A real status bar: working state · mesh tier+model · cost, with right-aligned key
/// hints. Lower-priority segments drop out on narrow terminals; model+cost always show.
/// Humanize a token count: `< 1000` as-is, `< 1M` as `12.3k`, else `1.1M`.
fn human(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// The context-window gauge spans: `◷ used/limit N%` (N% colored dim<70 / yellow≥70 / red≥90),
/// or just `◷ used` when the model's limit is unknown (no fabricated denominator).
fn context_gauge_spans(used: u64, limit: Option<u32>) -> Vec<Span<'static>> {
    match limit {
        Some(limit) if limit > 0 => {
            let pct = ((used as f64 / limit as f64) * 100.0).round() as u64;
            let color = if pct >= 90 {
                ERRRED
            } else if pct >= 70 {
                WARNYEL
            } else {
                DIM
            };
            vec![
                Span::styled(
                    format!("◷ {}/{} ", human(used), human(limit as u64)),
                    Style::default().fg(DIM).bg(STATUSBG),
                ),
                Span::styled(
                    format!("{pct}%"),
                    Style::default().fg(color).bold().bg(STATUSBG),
                ),
            ]
        }
        _ => vec![Span::styled(
            format!("◷ {}", human(used)),
            Style::default().fg(DIM).bg(STATUSBG),
        )],
    }
}

/// Render the `/usage` overlay as a centered popup over the terminal.
pub fn render_usage_overlay(f: &mut Frame, app: &App) {
    if !app.usage_overlay.open {
        return;
    }
    let area = f.area();
    let w = (area.width as f32 * 0.82).ceil() as u16;
    let h = (area.height as f32 * 0.72).ceil() as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(ratatui::widgets::Clear, popup);

    let spinner = SPINNER[(app.usage_overlay.anim_tick as usize) % SPINNER.len()];
    let title = format!(" {spinner} Usage ");
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(TOOLCYAN));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let chunks = Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).split(inner);

    let o = &app.usage_overlay;

    // Derive totals from per-model breakdowns so subscription ($0) rows still show tokens.
    let (cost_5h, in_5h, out_5h) = UsageOverlay::totals(&o.by_model_5h);
    let (cost_today, in_today, out_today) = UsageOverlay::totals(&o.by_model);
    let (cost_week, in_week, out_week) = UsageOverlay::totals(&o.by_model_week);

    // Bridge-provider annotation for each period row.
    let bridge_5h = {
        let mut parts = Vec::new();
        if let Some(p) = o.codex_5h_pct {
            parts.push(format!("codex:{:.0}%", p));
        }
        if let Some(p) = o.claude_5h_pct {
            parts.push(format!("claude:{:.0}%", p));
        } else {
            let ctok = o.claude_5h_in + o.claude_5h_out;
            if ctok > 0 {
                parts.push(format!(
                    "claude:↑{} ↓{}",
                    format_tok(o.claude_5h_in),
                    format_tok(o.claude_5h_out)
                ));
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("  [{}]", parts.join("  "))
        }
    };
    let bridge_week = {
        let mut parts = Vec::new();
        if let Some(p) = o.codex_weekly_pct {
            parts.push(format!("codex:{:.0}%", p));
        }
        if let Some(p) = o.claude_weekly_pct {
            parts.push(format!("claude:{:.0}%", p));
        } else {
            let ctok = o.claude_weekly_in + o.claude_weekly_out;
            if ctok > 0 {
                parts.push(format!(
                    "claude:↑{} ↓{}",
                    format_tok(o.claude_weekly_in),
                    format_tok(o.claude_weekly_out)
                ));
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("  [{}]", parts.join("  "))
        }
    };

    let fmt_period =
        |label: &str, cost: f64, inp: u64, out: u64, cap: Option<f64>, bridge: &str| -> String {
            let tok_str = format!("↑{} ↓{}", format_tok(inp), format_tok(out));
            let cost_str = if cost > 0.0 {
                format!("${cost:.4}")
            } else {
                "sub".to_string()
            };
            if let Some(c) = cap {
                let pct = (cost / c * 100.0).min(100.0);
                format!("{label:<8}{tok_str}  {cost_str} / ${c:.2} ({pct:.0}%){bridge}")
            } else {
                format!("{label:<8}{tok_str}  {cost_str}{bridge}")
            }
        };

    let month_str = if let Some(cap) = o.monthly_cap {
        let pct = (o.month_usd / cap * 100.0).min(100.0);
        format!(
            "{:<8}${:.4} / ${:.2}  ({:.0}%)",
            "Month", o.month_usd, cap, pct
        )
    } else {
        format!("{:<8}${:.4}", "Month", o.month_usd)
    };
    let session_str = format!(
        "{:<8}↑{} ↓{}  ${:.4}",
        "Session",
        format_tok(o.session_in),
        format_tok(o.session_out),
        o.session_usd,
    );
    let summary_text = ratatui::text::Text::from(vec![
        ratatui::text::Line::from(fmt_period("5h", cost_5h, in_5h, out_5h, None, &bridge_5h)),
        ratatui::text::Line::from(fmt_period(
            "Today",
            cost_today,
            in_today,
            out_today,
            o.daily_cap,
            "",
        )),
        ratatui::text::Line::from(fmt_period(
            "Week",
            cost_week,
            in_week,
            out_week,
            o.weekly_cap,
            &bridge_week,
        )),
        ratatui::text::Line::from(month_str),
        ratatui::text::Line::from(session_str),
        ratatui::text::Line::from(""),
        ratatui::text::Line::from(Span::styled("  Esc to close", Style::default().fg(DIM))),
    ]);
    f.render_widget(Paragraph::new(summary_text), chunks[0]);

    use ratatui::style::Modifier;
    use ratatui::widgets::{Cell, Row, Table};
    let header = Row::new(vec![
        Cell::from("Model").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Cost (today)").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("↑ In").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("↓ Out").style(Style::default().add_modifier(Modifier::BOLD)),
    ]);
    let rows: Vec<Row> = o
        .by_model
        .iter()
        .map(|(model, cost, inp, out)| {
            let display = if model.is_empty() {
                "side calls".to_string()
            } else {
                model.clone()
            };
            let style = if display.starts_with("claude-cli") || display.starts_with("codex-cli") {
                Style::default().fg(TOOLCYAN)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(display).style(style),
                Cell::from(if *cost > 0.0 {
                    format!("${:.5}", cost)
                } else {
                    "subscription".to_string()
                })
                .style(style),
                Cell::from(format_tok(*inp)).style(style),
                Cell::from(format_tok(*out)).style(style),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(50),
            Constraint::Percentage(20),
            Constraint::Percentage(15),
            Constraint::Percentage(15),
        ],
    )
    .header(header)
    .block(Block::default());
    f.render_widget(table, chunks[1]);
}

/// A 14-cell colour-coded meter for a fraction, eased by `ease` (animation grow-in).
fn mesh_meter(frac: f64, ease: f32, status: &str) -> Vec<Span<'static>> {
    use ratatui::style::Color;
    let shown = (frac as f32 * ease).clamp(0.0, 1.0);
    let filled = (shown * 14.0).round() as usize;
    let col = match status {
        "Exhausted" => Color::Red,
        "Warning" => Color::Yellow,
        _ if frac >= 0.6 => Color::Yellow,
        _ => Color::Green,
    };
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(col)),
        Span::styled("░".repeat(14 - filled), Style::default().fg(DIM)),
    ]
}

fn mesh_truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
    }
}

/// The animated `/mesh` routing inspector overlay.
pub fn render_mesh_overlay(f: &mut Frame, app: &App) {
    if !app.mesh_overlay.open {
        return;
    }
    use ratatui::style::{Color, Modifier};
    use ratatui::text::{Line, Text};

    let o = &app.mesh_overlay;
    let area = f.area();
    let w = (area.width as f32 * 0.84).ceil() as u16;
    let h = (area.height as f32 * 0.80).ceil() as u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(ratatui::widgets::Clear, popup);

    let spinner = SPINNER[(o.anim_tick as usize) % SPINNER.len()];
    let title = format!(" {spinner} mesh inspector ");
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(TOOLCYAN));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let ease = ((o.anim_tick as f32) / 6.0).min(1.0);

    // --- header + quota gauges + conservation verdict ---
    let mut top: Vec<Line> = Vec::new();
    if o.prompt.is_empty() {
        top.push(Line::from(Span::styled(
            "overview · complex-tier ranking — type `/mesh <task>` to trace a specific prompt",
            Style::default().fg(DIM),
        )));
    } else {
        top.push(Line::from(vec![
            Span::styled("task  ", Style::default().fg(DIM)),
            Span::raw(mesh_truncate(&o.prompt, inner.width.saturating_sub(8) as usize)),
        ]));
        let tier = if !o.routed.is_empty() && o.routed != o.classified {
            format!("tier  {} → {}   ({})", o.classified, o.routed, o.reasons)
        } else {
            format!("tier  {}   ({})", o.classified, o.reasons)
        };
        top.push(Line::from(Span::styled(tier, Style::default().fg(Color::Cyan))));
    }
    top.push(Line::from(""));
    for q in &o.quota {
        let mut spans = vec![Span::styled(format!("  {:<11} ", q.provider), Style::default())];
        spans.extend(mesh_meter(q.fraction, ease, &q.status));
        let plan = if q.plan.is_empty() { "?" } else { &q.plan };
        spans.push(Span::styled(
            format!(
                " {:>3.0}% · {plan} · {} · spread {:.0}%",
                q.fraction * 100.0 * ease as f64,
                q.status,
                q.spread_complex * 100.0
            ),
            Style::default().fg(DIM),
        ));
        top.push(Line::from(spans));
    }
    if !o.conserve_line.is_empty() {
        let col = if o.conserve_fired { Color::Yellow } else { Color::Gray };
        top.push(Line::from(Span::styled(
            format!("  conserve  {}", o.conserve_line),
            Style::default().fg(col),
        )));
    }
    top.push(Line::from(""));

    let top_h = (top.len() as u16).min(inner.height.saturating_sub(1));
    let chunks =
        Layout::vertical([Constraint::Length(top_h), Constraint::Min(0)]).split(inner);
    f.render_widget(Paragraph::new(Text::from(top)), chunks[0]);

    // --- candidate table (revealed row-by-row) + final pick ---
    let revealed = ((o.anim_tick as usize) / 2).min(o.candidates.len());
    let model_w = inner.width.saturating_sub(40).clamp(16, 48) as usize;
    let mut rows: Vec<Line> = Vec::new();
    for c in o.candidates.iter().take(revealed.max(1)) {
        let marker = if c.selected { "▶" } else { " " };
        let pen = if c.penalty > 0.0 {
            format!(" −{:.0}", c.penalty)
        } else {
            String::new()
        };
        let tag = format!(
            "{}{}{}{}",
            c.cost_tag,
            pen,
            if c.frontier { " · frontier" } else { "" },
            if c.usable { "" } else { " · unusable" },
        );
        let base = if c.selected {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else if !c.usable {
            Style::default().fg(DIM)
        } else {
            Style::default()
        };
        rows.push(Line::from(vec![
            Span::styled(format!("{marker} #{:<2} ", c.rank), base),
            Span::styled(format!("{:<width$}", mesh_truncate(&c.model, model_w), width = model_w), base),
            Span::styled(format!("  {:>6.2}  ", c.score), base),
            Span::styled(tag, if c.selected { base } else { Style::default().fg(DIM) }),
        ]));
    }
    rows.push(Line::from(""));
    rows.push(Line::from(vec![
        Span::styled("pick  ", Style::default().fg(DIM)),
        Span::styled(
            o.pick.clone(),
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        ),
    ]));
    if !o.rationale.is_empty() {
        rows.push(Line::from(Span::styled(
            mesh_truncate(&format!("why   {}", o.rationale), inner.width as usize),
            Style::default().fg(DIM),
        )));
    }
    rows.push(Line::from(Span::styled(
        "Esc to close",
        Style::default().fg(DIM),
    )));
    f.render_widget(Paragraph::new(Text::from(rows)), chunks[1]);
}

fn format_tok(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

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
    // Live token counter + context-window gauge (tui-token-counter.md). Width-gated so on a
    // shrinking terminal the gauge drops first, then the token segment, before tier/model/cost.
    if (app.session_in > 0 || app.session_out > 0) && w >= 76 {
        left.push(sep());
        left.push(Span::styled(
            format!("↑{} ↓{}", human(app.session_in), human(app.session_out)),
            Style::default().fg(DIM).bg(STATUSBG),
        ));
    }
    if app.context_tokens > 0 && w >= 96 {
        left.push(sep());
        left.extend(context_gauge_spans(app.context_tokens, app.context_limit));
    }
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

    /// Render the pinned live region at its natural (dynamic) live height — so the sticky task +
    /// subagent panels get their own rows, exactly as the real I/O shell sizes the viewport.
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

        // Once all done: the live panel switches to "done" state (still shows agent names) and the
        // group box also lands in scrollback. on_turn_start collapses the panel for the next turn.
        let done_screen = screen(&app);
        assert!(
            done_screen.contains("subagents (2 done)"),
            "panel shows done count: {done_screen}"
        );
        assert!(
            done_screen.contains("reviewer"),
            "done panel shows agent names: {done_screen}"
        );
        let sb = flush_text(&mut app);
        assert!(sb.contains("subagents"), "group header: {sb}");
        assert!(
            sb.contains("reviewer") && sb.contains("2 issues"),
            "branch: {sb}"
        );
        assert!(sb.contains("2 agents"), "footer with count: {sb}");
        // Panel collapses at next turn start, not on completion.
        app.on_turn_start();
        assert!(
            !screen(&app).contains("reviewer"),
            "panel gone after on_turn_start"
        );
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
            session_in: 0,
            session_out: 0,
            context_tokens: 0,
            context_limit: None,
        });
        assert!(screen(&app).contains("$0.0033"));
    }

    #[test]
    fn humanizes_token_counts() {
        assert_eq!(human(0), "0");
        assert_eq!(human(999), "999");
        assert_eq!(human(12_345), "12.3k");
        assert_eq!(human(1_100_000), "1.1M");
    }

    #[test]
    fn statusline_shows_token_counter_and_context_gauge() {
        let mut app = App::default();
        app.apply(PresenterEvent::Cost {
            session_total_usd: 0.01,
            session_in: 12_300,
            session_out: 4_100,
            context_tokens: 18_200,
            context_limit: Some(200_000),
        });
        let wide = screen_wh(&app, 120, LIVE_H);
        assert!(wide.contains("↑12.3k"), "token counter shown: {wide}");
        assert!(wide.contains("↓4.1k"));
        assert!(wide.contains("18.2k/200.0k"), "context gauge shown: {wide}");
        assert!(wide.contains("9%"), "context percentage: {wide}");
    }

    #[test]
    fn context_gauge_omits_denominator_when_limit_unknown() {
        let mut app = App::default();
        app.apply(PresenterEvent::Cost {
            session_total_usd: 0.01,
            session_in: 5_000,
            session_out: 1_000,
            context_tokens: 6_000,
            context_limit: None,
        });
        let wide = screen_wh(&app, 120, LIVE_H);
        assert!(wide.contains("◷ 6.0k"), "used tokens shown: {wide}");
        assert!(!wide.contains('%'), "no fabricated percentage: {wide}");
    }

    #[test]
    fn token_segments_drop_on_narrow_terminals_before_cost() {
        let mut app = App::default();
        app.apply(PresenterEvent::Cost {
            session_total_usd: 0.0033,
            session_in: 12_300,
            session_out: 4_100,
            context_tokens: 18_200,
            context_limit: Some(200_000),
        });
        // Narrow: gauge + token segment gone, but model+cost stay.
        let narrow = screen_wh(&app, 60, LIVE_H);
        assert!(!narrow.contains("18.2k/200.0k"), "gauge dropped: {narrow}");
        assert!(
            !narrow.contains("↑12.3k"),
            "token segment dropped: {narrow}"
        );
        assert!(narrow.contains("$0.0033"), "cost stays: {narrow}");
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

    fn todo(title: &str, status: forge_types::TodoStatus) -> forge_types::TodoItem {
        forge_types::TodoItem {
            title: title.into(),
            status,
        }
    }

    #[test]
    fn tasks_panel_visible_while_active_and_absent_when_empty() {
        use forge_types::TodoStatus;
        let mut app = App::default();
        // No tasks → no sticky panel in the idle preview region.
        assert!(!screen(&app).contains("tasks ("), "panel hidden when empty");

        app.apply(PresenterEvent::Tasks(vec![
            todo("scan repo", TodoStatus::Done),
            todo("write tests", TodoStatus::InProgress),
            todo("ship it", TodoStatus::Pending),
        ]));
        let s = screen(&app);
        assert!(s.contains("tasks (1/3 done)"), "panel header + count: {s}");
        // The in-progress item is shown with its glyph (prioritized into the small region).
        assert!(s.contains('◐'), "in-progress glyph shown: {s}");

        // Emptying the list collapses the panel.
        app.apply(PresenterEvent::Tasks(vec![]));
        assert!(
            !screen(&app).contains("tasks ("),
            "panel collapses when the list empties"
        );
    }

    #[test]
    fn tasks_panel_stays_visible_while_streaming() {
        use forge_types::TodoStatus;
        let mut app = App::default();
        app.apply(PresenterEvent::Tasks(vec![todo("a", TodoStatus::Pending)]));
        // The task panel has its OWN region now, so a live reply does NOT mask it — both show.
        app.apply(PresenterEvent::AssistantDelta("streaming now".into()));
        let s = screen(&app);
        assert!(s.contains("streaming now"), "stream shown: {s}");
        assert!(
            s.contains("tasks ("),
            "tasks panel stays visible while streaming: {s}"
        );
    }

    #[test]
    fn subagent_panel_stays_visible_while_streaming() {
        let mut app = App::default();
        app.apply(PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "reviewer".into(),
            task: "review the diff".into(),
        });
        app.apply(PresenterEvent::AssistantDelta("thinking out loud".into()));
        let s = screen(&app);
        assert!(s.contains("thinking out loud"), "stream shown: {s}");
        assert!(
            s.contains("subagents (1 running)"),
            "subagent panel stays visible while streaming: {s}"
        );
    }

    #[test]
    fn subagent_panel_present_while_running_and_after_done_collapses_on_next_turn() {
        let mut app = App::default();
        assert_eq!(app.running_subagents(), 0);
        app.apply(PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "reviewer".into(),
            task: "review the diff".into(),
        });
        assert_eq!(app.running_subagents(), 1);
        let s = screen(&app);
        assert!(
            s.contains("subagents (1 running)"),
            "panel header while running: {s}"
        );
        assert!(s.contains("reviewer"), "agent label shown: {s}");

        // After SubagentResult the panel stays visible (shows "done") so it's never invisible
        // when Start+Result arrive in the same render drain (the common bridge case).
        app.apply(PresenterEvent::SubagentResult {
            id: "a".into(),
            agent: "reviewer".into(),
            ok: true,
            summary: "ok".into(),
            cost_usd: 0.001,
        });
        assert_eq!(
            app.running_subagents(),
            0,
            "no running children after result"
        );
        let s = screen(&app);
        assert!(
            s.contains("subagents (1 done)"),
            "panel stays visible showing done state: {s}"
        );

        // The panel collapses at the START of the next user turn, not immediately on result.
        app.on_turn_start();
        assert!(
            !screen(&app).contains("subagents ("),
            "panel collapses after on_turn_start: {s}"
        );
    }

    #[test]
    fn mesh_overlay_renders_without_panic() {
        let mut app = App::default();
        app.mesh_overlay = MeshOverlay {
            open: true,
            prompt: "design a lock-free queue".into(),
            classified: "complex".into(),
            routed: "complex".into(),
            code_heavy: false,
            reasons: "reasoning term".into(),
            conserve_fired: true,
            conserve_line: "FIRED (roll 0.05 < P 0.53) → spread to free frontier".into(),
            quota: vec![MeshQuotaRow {
                provider: "claude-cli".into(),
                fraction: 0.78,
                plan: "max-20x".into(),
                status: "Ok".into(),
                spread_complex: 0.5,
            }],
            candidates: vec![
                MeshCandRow {
                    rank: 1,
                    model: "groq::llama-3.3-70b-versatile".into(),
                    score: 6.65,
                    cost_tag: "free".into(),
                    frontier: true,
                    usable: true,
                    selected: true,
                    penalty: 0.0,
                },
                MeshCandRow {
                    rank: 2,
                    model: "codex-cli::gpt-5.5".into(),
                    score: 3.05,
                    cost_tag: "subscription".into(),
                    frontier: true,
                    usable: true,
                    selected: false,
                    penalty: 4.0,
                },
            ],
            pick: "groq::llama-3.3-70b-versatile".into(),
            fallbacks: vec!["codex-cli::gpt-5.5".into()],
            rationale: "auto-selected best".into(),
            anim_tick: 50, // fully revealed
        };
        let s = screen_wh(&app, 100, 30);
        assert!(s.contains("mesh inspector"), "title rendered");
        assert!(s.contains("groq::llama-3.3-70b-versatile"), "pick shown");
        // A tiny terminal must not panic on the layout math.
        let _ = screen_wh(&app, 30, 6);
    }

    #[test]
    fn subagent_views_capture_full_transcript_and_result() {
        let mut app = App::default();
        app.apply(PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "general".into(),
            task: "find call sites".into(),
        });
        // More progress than the old 200-snippet cap — the full transcript must be kept.
        for i in 0..250 {
            app.apply(PresenterEvent::SubagentProgress {
                id: "a".into(),
                snippet: format!("step {i}"),
            });
        }
        app.apply(PresenterEvent::SubagentResult {
            id: "a".into(),
            agent: "general".into(),
            ok: true,
            summary: "found 3 call sites".into(),
            cost_usd: 0.01,
        });
        // Views are retained after the batch finishes (so Ctrl+O can still open them).
        let views = app.subagent_views();
        assert_eq!(views.len(), 1);
        let v = &views[0];
        assert!(
            v.done && v.log.len() > 200,
            "full log kept: {}",
            v.log.len()
        );
        assert!(v.log.iter().any(|l| l == "step 0"), "oldest line kept");
        assert!(v.log.iter().any(|l| l == "step 249"), "newest line kept");
        assert!(
            v.log.iter().any(|l| l.contains("found 3 call sites")),
            "result appended to transcript"
        );

        // A new batch drops the previous (finished) rows.
        app.apply(PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "next".into(),
        });
        assert_eq!(app.subagent_views().len(), 1);
        assert_eq!(app.subagent_views()[0].task, "next");
    }

    #[test]
    fn input_highlights_slash_command_mid_line() {
        let app = App {
            input: "please run /orchestrate scan".to_string(),
            ..Default::default()
        };
        let spans = input_spans(&app.input);
        // The `/orchestrate` token is its own bold-orange span.
        let hi = spans
            .iter()
            .find(|s| s.content.contains("/orchestrate"))
            .expect("slash token has its own span");
        assert!(
            hi.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "command token is highlighted bold"
        );
        // The preceding prose is a separate, unstyled span.
        assert!(
            spans.iter().any(|s| s.content == "please run "),
            "prose kept verbatim before the token"
        );
    }

    #[test]
    fn input_does_not_highlight_double_slash_escape() {
        let spans = input_spans("//literal text");
        // No bold command span — the whole thing is plain (escape preserved).
        assert!(
            !spans.iter().any(|s| s
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)),
            "// escape is not highlighted as a command"
        );
    }
}
