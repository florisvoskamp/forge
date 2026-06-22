//! Pure, testable TUI state and rendering for the inline-scrollback model.
//!
//! `App` folds [`PresenterEvent`]s into two kinds of state: *transient* state rendered
//! every frame in the small pinned live region (input, statusline, the in-flight reply,
//! the permission bar), and a *flush* outbox of finalized scrollback lines that the I/O
//! shell drains and pushes into the terminal's native scrollback (`insert_before`). The
//! line builders and `render_live` are free of terminal I/O so they stay TestBackend-able.

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Wrap};
use ratatui::Frame;
use serde::{Deserialize, Serialize};

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
/// Minimum input box height (1 text row + 2 border rows). The box grows up to [`INPUT_MAX_H`] as
/// the (wrapped/multiline) input gets taller, then scrolls internally.
pub const INPUT_H: u16 = 3;
/// Max input box height: up to [`INPUT_MAX_ROWS`] visible text rows + 2 border rows.
pub const INPUT_MAX_ROWS: u16 = 6;
pub const INPUT_MAX_H: u16 = INPUT_MAX_ROWS + 2;
pub const STATUS_H: u16 = 1;
/// Fixed inline-viewport height. Large enough to give the task + subagent panels their own rows
/// (each sized dynamically within `render_live`) while keeping a small idle footprint.
/// Cannot be resized at runtime — recreating the inline viewport pollutes the terminal scrollback.
pub const LIVE_H: u16 = 14;
/// Max task / running-subagent rows shown in their sticky panels before summarizing the overflow.
const TASKS_PANEL_MAX: usize = 6;
/// Max entry rows shown in the sticky activity panel before summarizing the overflow.
const ACTIVITY_PANEL_MAX: usize = 8;
/// Max styled lines retained for the "main chat" full-screen view (older lines drop off the front;
/// the terminal's native scrollback still holds the complete history).
const MAIN_LOG_MAX: usize = 5000;

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
    /// True while bridge stats are still loading in the background (subscription %s absent).
    pub loading: bool,
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
    /// Age (seconds) of the Claude rate-limit cache, if present — drives a "Xh ago" staleness
    /// note so the overlay never presents an old percentage as if it were live.
    pub claude_rl_age_secs: Option<i64>,
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
    /// True while bridge stats + routing explanation are loading in the background.
    pub loading: bool,
    /// The explained prompt ("" = overview mode).
    pub prompt: String,
    pub classified: String,
    /// Human-readable classifier label: "heuristic" / "llm (model)" / "hybrid — …".
    pub classifier: String,
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
    /// Animation tick — drives the bar-fill ease and the row-by-row candidate reveal. Stops
    /// advancing once the reveal settles (so the spinner doesn't spin forever).
    pub anim_tick: u32,
    /// Vertical scroll offset into the candidate list (↑/↓ while the overlay is open).
    pub scroll: u16,
}

impl MeshOverlay {
    /// The tick at which the open animation is fully settled (bars eased + every candidate row
    /// revealed). Past this the inspector is static — no more redraws, no infinite spinner.
    pub fn settle_tick(&self) -> u32 {
        self.candidates.len() as u32 * 2 + 12
    }
}

/// What a paste-block placeholder stands in for: a chunk of pasted text (substituted back into the
/// prompt on submit) or an attached image (sent out-of-band as vision input, the placeholder
/// stripped from the text).
#[derive(Debug, Clone)]
enum PasteKind {
    Text(String),
    Image(forge_types::ImageAttachment),
}

/// An attachment shown inline in the input as a one-line placeholder (e.g. `[pasted text (3 lines)]`
/// or `[image (PNG 800x600)]`). The placeholder is deletable as a single unit and is resolved on
/// submit: text is substituted back in, images are pulled out as vision input.
#[derive(Debug, Clone)]
struct PasteBlock {
    /// The exact placeholder string inserted into `input`.
    placeholder: String,
    kind: PasteKind,
}

/// Live state for the compaction progress band. Progress is indeterminate (a single summarizer
/// call with no measurable fraction), so it's eased toward ~95% over an expected duration and
/// snapped to 100% when [`CompactionFinished`](crate::PresenterEvent::CompactionFinished) clears
/// it — the standard "honest indeterminate" progress UX.
#[derive(Debug, Clone, Default)]
pub struct CompactionState {
    /// `App::tick` value when compaction started; elapsed ≈ (tick - start_tick) frames.
    pub start_tick: usize,
    /// Whether this was a silent auto-compact (vs an explicit `/compact`).
    pub auto: bool,
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
    /// Set while compaction is running, driving the animated progress band. `None` otherwise.
    pub compaction: Option<CompactionState>,
    pub done: bool,
    /// The active operating temper label (e.g. "Guarded"), shown in the statusline.
    pub temper: String,
    /// An in-flight AskUserQuestion: the choices + whether free text is allowed. The question
    /// text + options are already in scrollback; the input line collects the answer.
    question: Option<(Vec<QChoice>, bool)>,
    /// A pending permission question shown while the loop blocks on the user's y/n.
    pub prompt: Option<String>,
    /// The text of a pending AskUserQuestion (set by `set_question`, cleared on resolve), so a
    /// remote-control snapshot can tell the phone what's being asked without the private options.
    pub question_prompt: Option<String>,
    /// The current input-line buffer (shown in the input box).
    pub input: String,
    /// Byte offset of the text cursor within `input`. Always on a char boundary.
    pub input_cursor: usize,
    /// The current *partial* (un-flushed, newline-free) line of the streaming reply.
    pub streaming: String,
    /// Accumulated reasoning/thinking text, flushed as a dim block before the answer.
    reasoning: String,
    /// True once the `⚒ forge` header for the in-flight reply has been flushed.
    streaming_active: bool,
    /// True while a turn is running (drives the thinking spinner).
    pub busy: bool,
    /// Prompts the user typed while busy, queued to run after the current turn (shown in the
    /// statusline as "⏳ N queued"). Maintained by the I/O shell.
    pub queued: Vec<String>,
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
    /// Number of candidates being adversarially verified — shown in the panel header; `None`
    /// before the verifier phase starts or after the report arrives.
    assay_verifying: Option<usize>,
    /// The inline slash-command palette (RFC session-management-and-commands). Open while the
    /// input line starts with `/`.
    pub palette: crate::commands::Palette,
    /// The interactive session/checkpoint picker (RFC session-management-and-commands). Modal
    /// while open; reused for `/sessions`, `/resume`, and `/checkpoints`.
    pub picker: crate::commands::Picker,
    /// For the `/models` browser only: `Some(provider)` when drilled into a provider's models,
    /// `None` at the top-level provider list. Lets Esc step back a level instead of closing.
    pub models_drilled: Option<String>,
    /// When true the models picker was opened via bare `/model` (pin-mode): selecting a leaf model
    /// row pins it instead of just browsing. Reset to false when the picker closes.
    pub models_pin_mode: bool,
    /// The live task list (`update_tasks`). Kept so the sticky tasks panel stays visible during
    /// the turn (the inline scrollback copy scrolls away); cleared when the model empties the list.
    tasks: Vec<forge_types::TodoItem>,
    /// File-path picker for `@path` inline completion. Opens when the input contains `@…` at cursor.
    pub at_picker: crate::commands::AtPathPicker,
    /// A shell fix command from the last shell diagnosis. Pressing F (idle only) populates
    /// the input with this command for the user to review before submitting.
    pub pending_shell_fix: Option<String>,
    /// When true, the sticky activity panel has keyboard focus: ↑↓ move the selection, Enter opens
    /// the selected entry's full-screen transcript, Esc unfocuses. Toggled by Ctrl+O.
    pub activity_focused: bool,
    /// The highlighted row in the activity panel (0 = main chat, then subagents, then critics).
    pub activity_idx: usize,
    /// Full styled transcript of the main conversation, mirrored from the scrollback outbox so the
    /// activity viewer can show "main chat" full-screen. Bounded to [`MAIN_LOG_MAX`] lines.
    main_log: Vec<TextLine<'static>>,
    /// True when the chat renders on the alternate screen (full-screen): the transcript is drawn
    /// from [`main_log`] into a scrollable region and the panels are pinned at the bottom. When
    /// false (inline mode), finalized lines flow into the terminal's native scrollback instead and
    /// only the small pinned live region is drawn. Set once at startup.
    pub fullscreen: bool,
    /// Full-screen transcript scroll offset, in wrapped rows from the top. Only meaningful when
    /// [`fullscreen`] is set.
    pub transcript_scroll: usize,
    /// While true, the full-screen transcript auto-scrolls to the tail as new lines arrive (a
    /// normal terminal). Paging up pauses it; paging to the bottom (or End) resumes it.
    pub transcript_follow: bool,
    /// The in-loop activity transcript viewer, open while `Some` (full-screen mode only). It renders
    /// over the whole frame using the MAIN terminal — no nested alternate screen — so it can never
    /// collide with the chat. Inline mode uses the separate-alt-screen `run_transcript_viewer`.
    pub viewer: Option<ViewerState>,
    /// The `/usage` overlay state.
    pub usage_overlay: UsageOverlay,
    /// The `/mesh` routing-inspector overlay state.
    pub mesh_overlay: MeshOverlay,
    /// True while remote control is active (a browser can drive the session via `/remote`). Shown
    /// as a `◉ remote` segment in the statusline so it's visible at a glance that the session is
    /// remotely controllable.
    pub remote_active: bool,
    /// A bounded plain-text ring buffer of the most recent finalized scrollback lines, so a
    /// remote-control snapshot can show the phone the tail of the conversation. Kept small (the
    /// full transcript lives in the terminal's native scrollback); newest is last.
    pub recent_transcript: std::collections::VecDeque<String>,
    /// When true, model reasoning/thinking blocks are shown in scrollback. Default false (hidden).
    /// Toggled by `/thinking`.
    pub show_thinking: bool,
    /// Attachment blocks shown inline as placeholders (pasted text or images): the placeholder
    /// lives in `input`, the backing content here. On submit, `resolve_paste_blocks()` substitutes
    /// text back inline and pulls images out as vision input.
    paste_blocks: Vec<PasteBlock>,
    /// Images resolved from the last submitted prompt, stashed so the user-turn echo can show a
    /// marker line per image (the placeholder was stripped from the text). Cleared by `submit_user`.
    last_submit_images: Vec<forge_types::ImageAttachment>,
    /// True when the terminal window has lost focus (FocusLost). Inverted sense so the derived
    /// `Default` (false) means *focused* — terminals don't always emit an initial FocusGained.
    /// Drives a hollow/dim input cursor while another window is in front.
    pub unfocused: bool,
    /// Blink phase for the input cursor: when true the block is suppressed this frame (the "off"
    /// half of the blink). Inverted sense so `Default` (false) shows the solid block. Toggled by
    /// the render loop ~every 530ms while focused.
    pub cursor_hidden: bool,
}

/// How many recent scrollback lines the remote snapshot keeps (a phone screen shows ~6–8).
const REMOTE_TRANSCRIPT_MAX: usize = 12;

impl App {
    /// Build a [`remote::Snapshot`]-shaped view of the live state, for the remote-control WS to
    /// broadcast. Plain fields only (no ratatui types), so `forge-tui` needn't depend on the
    /// remote module — the caller maps this into the snapshot type.
    pub fn remote_snapshot(&self) -> RemoteSnapshot {
        RemoteSnapshot {
            busy: self.busy,
            done: self.done,
            temper: self.temper.clone(),
            tier: self.routing.as_ref().map(|r| r.tier.clone()),
            model: self
                .routing
                .as_ref()
                .map(|r| r.model.clone())
                .unwrap_or_else(|| "—".to_string()),
            cost_usd: self.cost_usd,
            context_tokens: self.context_tokens,
            context_limit: self.context_limit,
            streaming: self.streaming.clone(),
            transcript: self.recent_transcript.iter().cloned().collect(),
            permission_prompt: self.prompt.clone(),
            question: self.question_prompt.clone(),
        }
    }
}

/// A plain-data view of the live state, produced by [`App::remote_snapshot`] and mapped into the
/// `remote::Snapshot` JSON by the render loop. Defined here (in forge-tui) so the pure render
/// crate owns the projection without depending on the server module.
#[derive(Debug, Clone, Default)]
pub struct RemoteSnapshot {
    pub busy: bool,
    pub done: bool,
    pub temper: String,
    pub tier: Option<String>,
    pub model: String,
    pub cost_usd: f64,
    pub context_tokens: u64,
    pub context_limit: Option<u32>,
    pub streaming: String,
    pub transcript: Vec<String>,
    pub permission_prompt: Option<String>,
    pub question: Option<String>,
}

/// One subagent's live row in the TUI.
#[derive(Debug, Clone)]
struct SubRow {
    id: String,
    agent: String,
    task: String,
    /// The model this child routed to (shown in the activity panel). `None` until known.
    model: Option<String>,
    /// Trailing edge of the child's streamed activity (RFC subagent-orchestration Phase 3b).
    last: String,
    /// Recent progress snippets, newest last, for the expandable detail view. Bounded so a chatty
    /// child can't grow the buffer without limit.
    log: Vec<String>,
    done: bool,
    cost: f64,
}

/// What an [`ActivityRow`] / [`TranscriptView`] represents in the unified activity list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// The main conversation (top-level transcript).
    MainChat,
    /// A spawned child agent.
    Subagent,
    /// An assay critic.
    AssayCritic,
}

/// Run state of one activity entry, kind-agnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Done,
    Skipped,
}

/// A cheap, allocation-light row for the sticky activity panel: metadata only, no transcript lines.
/// Built every frame, so it must NOT clone the (potentially large) transcript — that's deferred to
/// [`App::activity_views`], which only runs when the full-screen viewer is open.
#[derive(Debug, Clone)]
pub struct ActivitySummary {
    pub kind: ActivityKind,
    pub title: String,
    pub subtitle: String,
    pub model: Option<String>,
    pub status: ActivityStatus,
    pub cost: f64,
    pub line_count: usize,
}

/// One owned snapshot in the unified activity viewer: main chat, a subagent, or an assay critic.
/// Carries pre-styled transcript lines so the full-screen viewer renders them exactly like the
/// main chat (markdown + role coloring), wrapped to the terminal width.
#[derive(Debug, Clone)]
pub struct TranscriptView {
    pub kind: ActivityKind,
    /// Display title — "main chat", the agent name, or the critic lens.
    pub title: String,
    /// One-line subtitle: the task (subagent), the focus (critic), or empty (main chat).
    pub subtitle: String,
    /// The model this entry routed to, if known.
    pub model: Option<String>,
    pub status: ActivityStatus,
    pub cost: f64,
    /// The full transcript, pre-styled and ready to wrap+render in the viewer.
    pub lines: Vec<TextLine<'static>>,
    /// Plain-text count of source entries (for the panel's "N lines" hint).
    pub line_count: usize,
}

/// State of the in-loop activity transcript viewer (full-screen mode). Selection + scroll position;
/// `scroll == usize::MAX / 2` is the "tail" sentinel, clamped to the real max at render time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerState {
    /// Index into the activity list (0 = main chat, then subagents, then critics).
    pub selected: usize,
    /// Wrapped-row offset from the top; the tail sentinel while `follow`.
    pub scroll: usize,
    /// Auto-scroll to the tail as the selected entry grows.
    pub follow: bool,
}

impl Default for ViewerState {
    fn default() -> Self {
        Self {
            selected: 0,
            scroll: usize::MAX / 2,
            follow: true,
        }
    }
}

/// A serializable snapshot of the live TUI view (activity panel + viewer + scroll), persisted to the
/// session at the end of each turn so a resume restores the exact on-screen state. The main
/// conversation transcript is NOT included — it's already rehydrated from the message history on
/// resume; this captures only the ephemeral view that history doesn't carry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ViewSnapshot {
    pub subagents: Vec<SubagentSnapshot>,
    pub assay_critics: Vec<forge_types::AssayCriticRow>,
    pub assay_verifying: Option<usize>,
    pub tasks: Vec<forge_types::TodoItem>,
    pub activity_focused: bool,
    pub activity_idx: usize,
    pub viewer: Option<ViewerState>,
    pub transcript_scroll: usize,
    pub transcript_follow: bool,
}

/// Serializable form of a subagent row (the live `SubRow` isn't serde and is private).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentSnapshot {
    pub id: String,
    pub agent: String,
    pub task: String,
    pub model: Option<String>,
    pub last: String,
    pub log: Vec<String>,
    pub done: bool,
    pub cost: f64,
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
    Left,
    Right,
    Home,
    End,
    /// Ctrl+J — insert a newline into the input without submitting.
    InsertNewline,
    /// Delete key — delete the character forward of the cursor.
    DeleteForward,
    /// Ctrl+W — delete word backward (to the previous word boundary).
    DeleteWordBack,
    /// Ctrl+U — kill from cursor to start of the current line.
    KillLineBack,
    /// Ctrl+K — kill from cursor to end of the current line.
    KillLineForward,
    /// Ctrl+Left — move cursor one word left.
    WordLeft,
    /// Ctrl+Right — move cursor one word right.
    WordRight,
    /// TAB — complete the palette selection (ignored by the input line).
    Tab,
    /// SHIFT+TAB — cycle the operating temper (handled by the shell, not the input line).
    CycleTemper,
    /// CTRL+O — toggle the expanded detail view for the active subagents (shell-handled).
    ToggleSubagentDetail,
    /// PageUp — scroll the full-screen transcript up (shell-handled; ignored by the input line).
    PageUp,
    /// PageDown — scroll the full-screen transcript down (shell-handled).
    PageDown,
}

/// The result of feeding a keystroke to the input line.
#[derive(Debug, PartialEq, Eq)]
pub enum InputOutcome {
    Editing,
    Submit(String),
    Quit,
}

fn prev_char_boundary(s: &str, pos: usize) -> usize {
    let mut p = pos;
    loop {
        if p == 0 {
            return 0;
        }
        p -= 1;
        if s.is_char_boundary(p) {
            return p;
        }
    }
}

fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut p = pos + 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
    p
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Ctrl+Left / Ctrl+W: find the start of the previous word from `pos`.
fn prev_word_start(s: &str, mut pos: usize) -> usize {
    // Skip non-word chars backward
    while pos > 0 {
        let p = prev_char_boundary(s, pos);
        if is_word_char(s[p..pos].chars().next().unwrap_or(' ')) {
            break;
        }
        pos = p;
    }
    // Skip word chars backward
    while pos > 0 {
        let p = prev_char_boundary(s, pos);
        if !is_word_char(s[p..pos].chars().next().unwrap_or(' ')) {
            break;
        }
        pos = p;
    }
    pos
}

/// Ctrl+Right: find the end of the next word from `pos`.
fn next_word_end(s: &str, mut pos: usize) -> usize {
    // Skip non-word chars forward
    while pos < s.len() {
        let next = next_char_boundary(s, pos);
        if is_word_char(s[pos..next].chars().next().unwrap_or(' ')) {
            break;
        }
        pos = next;
    }
    // Skip word chars forward
    while pos < s.len() {
        let next = next_char_boundary(s, pos);
        if !is_word_char(s[pos..next].chars().next().unwrap_or(' ')) {
            break;
        }
        pos = next;
    }
    pos
}

/// Apply one keystroke to the input buffer (pure; no terminal I/O). `cursor` is the byte
/// offset of the text cursor within `input`; updated in place, always kept on a char boundary.
pub fn handle_key(input: &mut String, cursor: &mut usize, key: KeyKind) -> InputOutcome {
    *cursor = (*cursor).min(input.len());
    match key {
        KeyKind::Char(c) => {
            input.insert(*cursor, c);
            *cursor += c.len_utf8();
            InputOutcome::Editing
        }
        KeyKind::InsertNewline => {
            input.insert(*cursor, '\n');
            *cursor += 1;
            InputOutcome::Editing
        }
        KeyKind::Backspace => {
            if *cursor > 0 {
                let prev = prev_char_boundary(input, *cursor);
                input.remove(prev);
                *cursor = prev;
            }
            InputOutcome::Editing
        }
        KeyKind::Left => {
            if *cursor > 0 {
                *cursor = prev_char_boundary(input, *cursor);
            }
            InputOutcome::Editing
        }
        KeyKind::Right => {
            if *cursor < input.len() {
                *cursor = next_char_boundary(input, *cursor);
            }
            InputOutcome::Editing
        }
        KeyKind::Home => {
            let before = &input[..*cursor];
            *cursor = before.rfind('\n').map(|p| p + 1).unwrap_or(0);
            InputOutcome::Editing
        }
        KeyKind::End => {
            let after = &input[*cursor..];
            *cursor += after.find('\n').unwrap_or(after.len());
            InputOutcome::Editing
        }
        KeyKind::Enter => {
            if input.trim().is_empty() {
                InputOutcome::Editing
            } else {
                *cursor = 0;
                InputOutcome::Submit(std::mem::take(input))
            }
        }
        KeyKind::DeleteForward => {
            if *cursor < input.len() {
                let next = next_char_boundary(input, *cursor);
                input.drain(*cursor..next);
            }
            InputOutcome::Editing
        }
        KeyKind::DeleteWordBack => {
            let start = prev_word_start(input, *cursor);
            input.drain(start..*cursor);
            *cursor = start;
            InputOutcome::Editing
        }
        KeyKind::KillLineBack => {
            let line_start = input[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
            input.drain(line_start..*cursor);
            *cursor = line_start;
            InputOutcome::Editing
        }
        KeyKind::KillLineForward => {
            let line_end = input[*cursor..]
                .find('\n')
                .map(|p| *cursor + p)
                .unwrap_or(input.len());
            input.drain(*cursor..line_end);
            InputOutcome::Editing
        }
        KeyKind::WordLeft => {
            *cursor = prev_word_start(input, *cursor);
            InputOutcome::Editing
        }
        KeyKind::WordRight => {
            *cursor = next_word_end(input, *cursor);
            InputOutcome::Editing
        }
        KeyKind::Esc => InputOutcome::Quit,
        KeyKind::Up
        | KeyKind::Down
        | KeyKind::Tab
        | KeyKind::CycleTemper
        | KeyKind::ToggleSubagentDetail
        | KeyKind::PageUp
        | KeyKind::PageDown => InputOutcome::Editing,
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
            PresenterEvent::SubagentStart {
                id,
                agent,
                task,
                model,
            } => {
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
                    model,
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
                    // Assemble the streamed token-fragments into coherent transcript lines: append
                    // to the open last line, breaking only on real newlines. (Pushing each fragment
                    // as its own line fragmented identifiers like `count_text` across many rows.)
                    if row.log.is_empty() {
                        row.log.push(String::new());
                    }
                    for ch in snippet.chars() {
                        if ch == '\n' {
                            row.log.push(String::new());
                        } else {
                            row.log.last_mut().unwrap().push(ch);
                        }
                    }
                    // Cap the transcript by a high safety bound so a pathological child can't
                    // exhaust memory; keep the newest lines.
                    const MAX_LOG: usize = 10_000;
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
                    // Drop a trailing empty/partial line, then record the outcome at the tail of the
                    // transcript so the browser shows it.
                    if row.log.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
                        row.log.pop();
                    }
                    row.log.push(String::new());
                    row.log.push(format!(
                        "── result ({}) ──",
                        if ok { "ok" } else { "failed" }
                    ));
                    for piece in summary.split('\n') {
                        row.log.push(piece.to_string());
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
                // Update the live panel: insert on Queued, merge status+model+cost+output on Done/Skipped.
                if let Some(existing) = self.assay_critics.iter_mut().find(|r| r.lens == row.lens) {
                    existing.status = row.status;
                    if row.model.is_some() {
                        existing.model = row.model;
                        existing.cost_usd = row.cost_usd;
                    }
                    if !row.output.is_empty() {
                        existing.output = row.output;
                    }
                } else {
                    self.assay_critics.push(row);
                }
            }
            PresenterEvent::AssayVerifying { candidates } => {
                self.assay_verifying = Some(candidates);
            }
            PresenterEvent::AssayReport(report) => {
                self.assay_critics.clear();
                self.assay_verifying = None;
                self.clamp_activity_selection();
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
            PresenterEvent::Recap { text } => {
                self.flush.push(TextLine::from(vec![
                    Span::styled("  ※ recap  ", Style::default().fg(ORANGE).bold()),
                    Span::styled(text, Style::default().fg(Color::Rgb(205, 205, 215))),
                ]));
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
            PresenterEvent::CompactionStarted { auto } => {
                self.compaction = Some(CompactionState {
                    start_tick: self.tick,
                    auto,
                });
            }
            PresenterEvent::CompactionFinished { .. } => {
                // The band clears; the core also emits a "compacted N → M" warning into scrollback.
                self.compaction = None;
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

    /// True when there is subagent or assay activity to show in the sticky activity panel. The
    /// panel (and its "main chat" entry) only appears while there's something to drill into.
    pub fn has_activity(&self) -> bool {
        !self.subagents.is_empty() || !self.assay_critics.is_empty()
    }

    /// Number of rows in the activity list: main chat + each subagent + each assay critic.
    pub fn activity_len(&self) -> usize {
        if !self.has_activity() {
            return 0;
        }
        1 + self.subagents.len() + self.assay_critics.len()
    }

    /// Rows the sticky activity panel wants in the live region (0 = hidden). Header + up to
    /// [`ACTIVITY_PANEL_MAX`] entry rows + an overflow line.
    pub fn activity_panel_height(&self) -> u16 {
        let n = self.activity_len();
        if n == 0 {
            return 0;
        }
        let shown = n.min(ACTIVITY_PANEL_MAX);
        let overflow = u16::from(n > ACTIVITY_PANEL_MAX);
        1 + shown as u16 + overflow
    }

    /// Keep `activity_idx` within range as the list grows/shrinks; drop focus when empty.
    fn clamp_activity_selection(&mut self) {
        let n = self.activity_len();
        if n == 0 {
            self.activity_focused = false;
            self.activity_idx = 0;
        } else if self.activity_idx >= n {
            self.activity_idx = n - 1;
        }
    }

    /// Called at the start of each new user turn. Collapses the "done" subagent batch that the
    /// panel was holding so it doesn't bleed into the new turn's live region.
    pub fn on_turn_start(&mut self) {
        if !self.subagents.is_empty() && self.subagents.iter().all(|r| r.done) {
            self.subagents.clear();
        }
        self.activity_focused = false;
        self.activity_idx = 0;
        self.pending_shell_fix = None;
    }

    /// Cheap per-frame metadata for the sticky activity panel (no transcript cloning). Order:
    /// main chat, then subagents, then assay critics. Empty when there's no activity.
    pub fn activity_summaries(&self) -> Vec<ActivitySummary> {
        if !self.has_activity() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.activity_len());
        out.push(ActivitySummary {
            kind: ActivityKind::MainChat,
            title: "main chat".to_string(),
            subtitle: String::new(),
            model: self.routing.as_ref().map(|r| r.model.clone()),
            status: if self.busy {
                ActivityStatus::Running
            } else {
                ActivityStatus::Done
            },
            cost: self.cost_usd,
            line_count: self.main_log.len(),
        });
        for r in &self.subagents {
            out.push(ActivitySummary {
                kind: ActivityKind::Subagent,
                title: r.agent.clone(),
                subtitle: r.task.clone(),
                model: r.model.clone(),
                status: if r.done {
                    ActivityStatus::Done
                } else {
                    ActivityStatus::Running
                },
                cost: r.cost,
                line_count: r.log.len(),
            });
        }
        for c in &self.assay_critics {
            use forge_types::AssayCriticStatus;
            let (status, subtitle) = match &c.status {
                AssayCriticStatus::Queued => (ActivityStatus::Running, c.focus.clone()),
                AssayCriticStatus::Done { candidates } => {
                    (ActivityStatus::Done, format!("{candidates} found"))
                }
                AssayCriticStatus::Skipped { reason } => {
                    (ActivityStatus::Skipped, format!("skipped ({reason})"))
                }
            };
            out.push(ActivitySummary {
                kind: ActivityKind::AssayCritic,
                title: c.lens.clone(),
                subtitle,
                model: c.model.clone(),
                status,
                cost: c.cost_usd,
                line_count: c.output.lines().count(),
            });
        }
        out
    }

    /// Build the unified activity views (main chat + subagents + assay critics), in panel order, so
    /// the full-screen viewer can render any of them. Heavier (clones transcripts + renders
    /// markdown) — call ONLY when the full-screen viewer is open, never per render frame.
    /// Empty when there's no activity (see [`has_activity`]).
    pub fn activity_views(&self) -> Vec<TranscriptView> {
        if !self.has_activity() {
            return Vec::new();
        }
        let mut views = Vec::with_capacity(self.activity_len());

        // 0: main chat — the real, already-styled transcript lines plus anything still pending in
        // the flush outbox (so the view updates live even while the full-screen viewer is open).
        let mut main_lines = self.main_log.clone();
        main_lines.extend(self.flush.iter().cloned());
        let main_count = main_lines.len();
        views.push(TranscriptView {
            kind: ActivityKind::MainChat,
            title: "main chat".to_string(),
            subtitle: String::new(),
            model: self.routing.as_ref().map(|r| r.model.clone()),
            status: if self.busy {
                ActivityStatus::Running
            } else {
                ActivityStatus::Done
            },
            cost: self.cost_usd,
            lines: main_lines,
            line_count: main_count,
        });

        // Subagents, in spawn order. The transcript is streamed token-fragments assembled into
        // lines — render as plain styled text (markdown would mangle the partial streaming).
        for r in &self.subagents {
            let lines: Vec<TextLine<'static>> = if r.log.iter().all(|l| l.trim().is_empty()) {
                vec![TextLine::from(Span::styled(
                    "(no activity captured yet)",
                    Style::default().fg(DIM),
                ))]
            } else {
                r.log
                    .iter()
                    .map(|l| {
                        let style = if l.starts_with("── result") {
                            Style::default().fg(ORANGE)
                        } else {
                            Style::default().fg(Color::Rgb(205, 205, 215))
                        };
                        TextLine::from(Span::styled(l.clone(), style))
                    })
                    .collect()
            };
            views.push(TranscriptView {
                kind: ActivityKind::Subagent,
                title: r.agent.clone(),
                subtitle: r.task.clone(),
                model: r.model.clone(),
                status: if r.done {
                    ActivityStatus::Done
                } else {
                    ActivityStatus::Running
                },
                cost: r.cost,
                lines,
                line_count: r.log.len(),
            });
        }

        // Assay critics, in panel order.
        for c in &self.assay_critics {
            use forge_types::AssayCriticStatus;
            let (status, subtitle) = match &c.status {
                AssayCriticStatus::Queued => (ActivityStatus::Running, c.focus.clone()),
                AssayCriticStatus::Done { candidates } => (
                    ActivityStatus::Done,
                    format!("{candidates} found · {}", c.focus),
                ),
                AssayCriticStatus::Skipped { reason } => {
                    (ActivityStatus::Skipped, format!("skipped ({reason})"))
                }
            };
            let lines = if c.output.trim().is_empty() {
                vec![TextLine::from(Span::styled(
                    "(no output yet)",
                    Style::default().fg(DIM),
                ))]
            } else {
                crate::render::markdown_to_lines(&c.output)
            };
            let line_count = c.output.lines().count();
            views.push(TranscriptView {
                kind: ActivityKind::AssayCritic,
                title: c.lens.clone(),
                subtitle,
                model: c.model.clone(),
                status,
                cost: c.cost_usd,
                lines,
                line_count,
            });
        }

        views
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
        self.question_prompt = Some(question.to_string());
    }

    /// True while a question is awaiting an answer.
    pub fn awaiting_question(&self) -> bool {
        self.question.is_some()
    }

    /// Drop any in-flight question/permission prompt (e.g. when the turn is interrupted).
    pub fn clear_question(&mut self) {
        self.question = None;
        self.prompt = None;
        self.question_prompt = None;
    }

    /// Try to resolve a submitted line against the active question. `Some(answer)` clears the
    /// question; `None` means invalid input — keep the question open and re-prompt.
    pub fn resolve_question(&mut self, line: &str) -> Option<String> {
        let (opts, allow_other) = self.question.as_ref()?;
        let ans = crate::resolve_answer(line, opts, *allow_other)?;
        self.question = None;
        self.prompt = None;
        self.question_prompt = None;
        self.flush.push(TextLine::from(vec![
            Span::styled("  ↳ ", Style::default().fg(DIM)),
            Span::styled(ans.clone(), Style::default().fg(OKGREEN)),
        ]));
        Some(ans)
    }

    /// Flush accumulated reasoning into scrollback as a dim "thinking" block (once), if any.
    /// When `show_thinking` is false the buffer is discarded silently (no scrollback line).
    fn flush_reasoning(&mut self) {
        if self.reasoning.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.reasoning);
        if !self.show_thinking {
            return;
        }
        let dim = Style::default().fg(DIM);
        self.flush
            .push(TextLine::from(Span::styled("✱ thinking", dim)));
        for l in text.lines() {
            self.flush
                .push(TextLine::from(Span::styled(l.to_string(), dim)));
        }
        self.flush.push(TextLine::default());
    }

    /// Clamp `input_cursor` into a valid byte index: never past the end, and always on a UTF-8
    /// char boundary. The cursor can drift out of sync (e.g. a cleared input left it > 0, or it
    /// landed inside a multibyte char) — slicing `input` at such a position panics, which is the
    /// class of crash this guards. Cheap to call before any slice into `input`.
    fn sanitize_cursor(&mut self) {
        self.input_cursor = self.input_cursor.min(self.input.len());
        if !self.input.is_char_boundary(self.input_cursor) {
            self.input_cursor = prev_char_boundary(&self.input, self.input_cursor);
        }
    }

    /// Insert a bracketed paste at the cursor. Single-line pastes are inserted as plain text;
    /// multiline pastes show a `[pasted text (N lines)]` placeholder so the input stays on one
    /// line and won't accidentally auto-submit when the pasted text contains newlines.
    pub fn handle_paste(&mut self, content: String) {
        self.sanitize_cursor();
        if !content.contains('\n') {
            // Single-line: insert directly as if the user typed it.
            self.input.insert_str(self.input_cursor, &content);
            self.input_cursor += content.len();
            return;
        }
        let n = content.lines().count();
        let placeholder = format!("[pasted text ({n} lines)]");
        self.insert_block(placeholder, PasteKind::Text(content));
    }

    /// Attach an image as an input block: a `[image (<label>)]` placeholder is inserted at the
    /// cursor (deletable as a unit, like a text paste), and the image is sent as vision input when
    /// the prompt is submitted. `label` is a short human descriptor, e.g. "PNG 800x600".
    pub fn attach_image(&mut self, image: forge_types::ImageAttachment, label: &str) {
        self.sanitize_cursor();
        let placeholder = format!("[image ({label})]");
        self.insert_block(placeholder, PasteKind::Image(image));
    }

    /// Insert a placeholder at the cursor and register its backing paste-block.
    fn insert_block(&mut self, placeholder: String, kind: PasteKind) {
        self.input.insert_str(self.input_cursor, &placeholder);
        self.input_cursor += placeholder.len();
        self.paste_blocks.push(PasteBlock { placeholder, kind });
    }

    /// If cursor is immediately after (Backspace) or at (DeleteForward) a paste-block placeholder,
    /// delete the entire placeholder in one action. Returns `true` if consumed.
    pub fn try_delete_paste_block(&mut self, key: KeyKind) -> bool {
        self.sanitize_cursor();
        match key {
            KeyKind::Backspace => {
                let found = {
                    let before = &self.input[..self.input_cursor];
                    self.paste_blocks
                        .iter()
                        .position(|b| before.ends_with(&b.placeholder))
                        .map(|i| (i, self.paste_blocks[i].placeholder.len()))
                };
                if let Some((idx, ph_len)) = found {
                    let start = self.input_cursor - ph_len;
                    self.input.drain(start..self.input_cursor);
                    self.input_cursor = start;
                    self.paste_blocks.remove(idx);
                    true
                } else {
                    false
                }
            }
            KeyKind::DeleteForward => {
                let found = {
                    let after = &self.input[self.input_cursor..];
                    self.paste_blocks
                        .iter()
                        .position(|b| after.starts_with(&b.placeholder))
                        .map(|i| (i, self.paste_blocks[i].placeholder.len()))
                };
                if let Some((idx, ph_len)) = found {
                    let end = self.input_cursor + ph_len;
                    self.input.drain(self.input_cursor..end);
                    self.paste_blocks.remove(idx);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Resolve every paste-block placeholder in `text`: text blocks are substituted back inline,
    /// image blocks are stripped from the text and their attachments returned (for vision input).
    /// Call with the line from `handle_key`'s Submit. Drains `paste_blocks`.
    pub fn resolve_paste_blocks(
        &mut self,
        text: String,
    ) -> (String, Vec<forge_types::ImageAttachment>) {
        let mut result = text;
        let mut images = Vec::new();
        for block in self.paste_blocks.drain(..) {
            let Some(pos) = result.find(&block.placeholder) else {
                // Placeholder was deleted from the input — drop the block (and its image) too.
                continue;
            };
            let span = pos..pos + block.placeholder.len();
            match block.kind {
                PasteKind::Text(content) => result.replace_range(span, &content),
                PasteKind::Image(img) => {
                    result.replace_range(span, "");
                    images.push(img);
                }
            }
        }
        // Stash a copy so the user-turn echo (submit_user) can show a marker per image, since the
        // placeholders were just stripped from the text.
        self.last_submit_images = images.clone();
        (result, images)
    }

    /// Echo a just-submitted user message into scrollback. Any images attached to this prompt
    /// (stashed by `resolve_paste_blocks`) are shown as a marker line each, so the conversation
    /// history reflects that an image was sent (terminals can't render the pixels inline here).
    pub fn submit_user(&mut self, line: &str) {
        self.flush.push(header_line("you", USER));
        for l in line.lines() {
            self.flush.push(body_line(l));
        }
        for img in std::mem::take(&mut self.last_submit_images) {
            let kb = (img.data_base64.len() * 3 / 4).div_ceil(1024);
            self.flush.push(body_line(&format!(
                "🖼 image attached ({}, ~{kb} KB)",
                img.media_type
            )));
        }
        self.flush.push(TextLine::default());
    }

    /// Render a resumed session's prior transcript into scrollback (after a `/resume` swap), so the
    /// FULL conversation reappears — user turns, assistant text, AND the tool calls/results between
    /// them — instead of a sparse user-only echo. User turns echo like live input; assistant turns
    /// render markdown under the `⚒ forge` header; tool activity renders exactly like it did live.
    pub fn replay_history(&mut self, items: &[ReplayItem]) {
        for item in items {
            match item {
                ReplayItem::User(content) => self.submit_user(content),
                ReplayItem::Assistant(content) => {
                    self.flush.push(header_line("⚒ forge", ORANGE));
                    self.flush.extend(crate::render::markdown_to_lines(content));
                    self.flush.push(TextLine::default());
                }
                ReplayItem::Tool { name, args } => {
                    self.flush.push(tool_start_line(name, args));
                }
                ReplayItem::ToolResult { name, ok, summary } => {
                    self.flush.push(tool_result_line(name, *ok, summary));
                }
                ReplayItem::Note(text) => self.flush.push(warning_line(text)),
            }
        }
    }

    /// Update the visible queued-prompts list (prompts typed while a turn was running).
    pub fn set_queued(&mut self, queued: &[String]) {
        self.queued = queued.to_vec();
    }

    /// Push a dim separator line after replaying a resumed session's transcript, so the user
    /// can tell where the prior history ends and new input begins.
    pub fn push_resume_separator(&mut self, text: &str) {
        self.flush.push(TextLine::default());
        self.flush.push(TextLine::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(DIM),
        )));
        self.flush.push(TextLine::default());
    }

    /// Push a dim informational line into scrollback (command feedback, session lists, etc).
    pub fn note(&mut self, text: &str) {
        self.flush.push(TextLine::from(Span::styled(
            format!("  {text}"),
            Style::default().fg(DIM),
        )));
    }

    /// Push plain (unstyled) multi-line text into the scrollback outbox — for verbatim blocks
    /// like a QR code whose alignment must not be restyled. Drained with the next flush.
    pub fn print_lines(&mut self, lines: Vec<String>) {
        for l in lines {
            self.flush.push(TextLine::from(l));
        }
    }

    /// Take the finalized scrollback lines queued since the last call. Each line is also mirrored
    /// into [`main_log`] so the activity viewer can show the full "main chat" transcript.
    pub fn drain_flush(&mut self) -> Vec<TextLine<'static>> {
        let lines = std::mem::take(&mut self.flush);
        self.fold_main_log(&lines);
        lines
    }

    /// Like [`drain_flush`], but also folds each line's plain text into the remote transcript ring
    /// buffer so the remote-control snapshot mirrors the conversation tail. Use this when remote
    /// control is active; otherwise [`drain_flush`] is cheaper.
    pub fn drain_flush_remote(&mut self) -> Vec<TextLine<'static>> {
        let lines = std::mem::take(&mut self.flush);
        for l in &lines {
            self.push_remote_transcript_line(l);
        }
        self.fold_main_log(&lines);
        lines
    }

    /// Append out-of-band scrollback (banner, command output, `/clear` marker, …) directly into the
    /// full-screen transcript log. In inline mode these lines go to the terminal's native scrollback
    /// instead (the I/O shell calls `Tui::insert_lines`); in full-screen mode the transcript IS the
    /// log, so they must land here. Auto-tails the view (`transcript_follow`).
    pub fn push_scrollback(&mut self, lines: Vec<TextLine<'static>>) {
        self.fold_main_log(&lines);
    }

    /// Like [`push_scrollback`] but for plain multi-line text (e.g. command output that isn't
    /// pre-styled). Full-screen counterpart to `Tui::print_text`.
    pub fn push_scrollback_text(&mut self, text: &str) {
        let lines: Vec<TextLine<'static>> =
            text.lines().map(|s| TextLine::from(s.to_owned())).collect();
        self.fold_main_log(&lines);
    }

    /// Serialize the live view (activity panel, viewer, scroll, tasks) to JSON for session storage.
    /// Returns `None` when there's nothing worth persisting (no activity, no viewer, no tasks) so a
    /// plain chat session doesn't write an empty blob every turn.
    pub fn view_snapshot_json(&self) -> Option<String> {
        if self.subagents.is_empty()
            && self.assay_critics.is_empty()
            && self.tasks.is_empty()
            && self.viewer.is_none()
        {
            return None;
        }
        serde_json::to_string(&self.view_snapshot()).ok()
    }

    /// Restore a view previously captured by [`view_snapshot_json`] (best-effort; malformed or
    /// stale JSON is ignored). Called on resume after the transcript has been replayed.
    pub fn restore_view_json(&mut self, json: &str) {
        if let Ok(snap) = serde_json::from_str::<ViewSnapshot>(json) {
            self.restore_view(snap);
        }
    }

    fn view_snapshot(&self) -> ViewSnapshot {
        ViewSnapshot {
            subagents: self
                .subagents
                .iter()
                .map(|r| SubagentSnapshot {
                    id: r.id.clone(),
                    agent: r.agent.clone(),
                    task: r.task.clone(),
                    model: r.model.clone(),
                    last: r.last.clone(),
                    log: r.log.clone(),
                    done: r.done,
                    cost: r.cost,
                })
                .collect(),
            assay_critics: self.assay_critics.clone(),
            assay_verifying: self.assay_verifying,
            tasks: self.tasks.clone(),
            activity_focused: self.activity_focused,
            activity_idx: self.activity_idx,
            viewer: self.viewer.clone(),
            transcript_scroll: self.transcript_scroll,
            transcript_follow: self.transcript_follow,
        }
    }

    fn restore_view(&mut self, s: ViewSnapshot) {
        self.subagents = s
            .subagents
            .into_iter()
            .map(|r| SubRow {
                id: r.id,
                agent: r.agent,
                task: r.task,
                model: r.model,
                last: r.last,
                log: r.log,
                done: r.done,
                cost: r.cost,
            })
            .collect();
        self.assay_critics = s.assay_critics;
        self.assay_verifying = s.assay_verifying;
        if !s.tasks.is_empty() {
            self.tasks = s.tasks;
        }
        self.activity_focused = s.activity_focused;
        self.activity_idx = s.activity_idx;
        self.viewer = s.viewer;
        self.transcript_scroll = s.transcript_scroll;
        self.transcript_follow = s.transcript_follow;
        self.clamp_activity_selection();
    }

    /// Open the in-loop activity viewer at `selected` (full-screen mode). Renders over the whole
    /// frame using the main terminal — no nested alternate screen.
    pub fn open_viewer(&mut self, selected: usize) {
        let n = self.activity_len();
        if n == 0 {
            return;
        }
        self.viewer = Some(ViewerState {
            selected: selected.min(n - 1),
            ..ViewerState::default()
        });
    }

    /// Feed a keystroke to the in-loop activity viewer. Returns true if the viewer consumed it (it
    /// is open). Esc closes it. ↑↓/PgUp/PgDn scroll, ←→/Tab switch entry, g/G top/tail.
    pub fn viewer_key(&mut self, key: KeyKind) -> bool {
        let n = self.activity_len().max(1);
        let Some(v) = self.viewer.as_mut() else {
            return false;
        };
        match key {
            KeyKind::Esc => self.viewer = None,
            KeyKind::Up => {
                v.follow = false;
                v.scroll = v.scroll.saturating_sub(1);
            }
            KeyKind::Down => v.scroll = v.scroll.saturating_add(1),
            KeyKind::PageUp => {
                v.follow = false;
                v.scroll = v.scroll.saturating_sub(10);
            }
            KeyKind::PageDown => v.scroll = v.scroll.saturating_add(10),
            KeyKind::Home => {
                v.follow = false;
                v.scroll = 0;
            }
            KeyKind::End => {
                v.follow = true;
                v.scroll = usize::MAX / 2;
            }
            KeyKind::Right | KeyKind::Tab => {
                v.selected = (v.selected + 1) % n;
                v.scroll = usize::MAX / 2;
                v.follow = true;
            }
            KeyKind::Left => {
                v.selected = (v.selected + n - 1) % n;
                v.scroll = usize::MAX / 2;
                v.follow = true;
            }
            KeyKind::Char('q') => self.viewer = None,
            KeyKind::Char('k') => {
                v.follow = false;
                v.scroll = v.scroll.saturating_sub(1);
            }
            KeyKind::Char('j') | KeyKind::Char(' ') => v.scroll = v.scroll.saturating_add(1),
            KeyKind::Char('g') => {
                v.follow = false;
                v.scroll = 0;
            }
            KeyKind::Char('G') => {
                v.follow = true;
                v.scroll = usize::MAX / 2;
            }
            _ => {}
        }
        true
    }

    /// Clear the full-screen transcript (`/clear`, `/new`): wipe the rendered log, the activity
    /// panel, and the viewer, re-anchoring at the tail. The session/transcript on disk are
    /// untouched — this only resets what's drawn (and what a snapshot would capture).
    pub fn clear_transcript(&mut self) {
        self.main_log.clear();
        self.subagents.clear();
        self.assay_critics.clear();
        self.assay_verifying = None;
        self.viewer = None;
        self.activity_focused = false;
        self.activity_idx = 0;
        self.transcript_scroll = 0;
        self.transcript_follow = true;
    }

    /// Wrapped-row metrics for the full-screen transcript: total rows at the given width, and the
    /// max scroll offset given a visible body height. Used by the I/O shell to clamp paging.
    pub fn transcript_metrics(&self, width: u16, body_h: u16) -> (usize, usize) {
        let total = self.wrapped_transcript(width).len();
        let max_scroll = total.saturating_sub(body_h.max(1) as usize);
        (total, max_scroll)
    }

    /// Scroll the full-screen transcript up by `rows` (toward the top); pauses auto-follow.
    pub fn transcript_scroll_up(&mut self, rows: usize) {
        self.transcript_follow = false;
        self.transcript_scroll = self.transcript_scroll.saturating_sub(rows);
    }

    /// Scroll the full-screen transcript down by `rows`; resumes follow at the bottom.
    pub fn transcript_scroll_down(&mut self, rows: usize, max_scroll: usize) {
        self.transcript_scroll = (self.transcript_scroll + rows).min(max_scroll);
        if self.transcript_scroll >= max_scroll {
            self.transcript_follow = true;
        }
    }

    /// Jump to the top of the full-screen transcript; pauses follow.
    pub fn transcript_to_top(&mut self) {
        self.transcript_follow = false;
        self.transcript_scroll = 0;
    }

    /// Jump to the tail of the full-screen transcript; resumes follow.
    pub fn transcript_to_bottom(&mut self) {
        self.transcript_follow = true;
        self.transcript_scroll = usize::MAX / 2;
    }

    /// The full-screen transcript body: finalized `main_log` plus the in-flight reply edge, each
    /// wrapped to `width`. Pure; the wrap mirrors the activity viewer's so scroll math is exact.
    fn wrapped_transcript(&self, width: u16) -> Vec<TextLine<'static>> {
        let mut lines = self.main_log.clone();
        if self.streaming_active {
            lines.push(TextLine::from(vec![
                Span::raw(format!("  {}", self.streaming)),
                Span::styled("▌", Style::default().fg(ORANGE)),
            ]));
        }
        crate::transcript::wrap_lines(&lines, width.saturating_sub(1) as usize)
    }

    /// Append flushed lines to the bounded main-chat log used by the activity viewer.
    fn fold_main_log(&mut self, lines: &[TextLine<'static>]) {
        self.main_log.extend(lines.iter().cloned());
        if self.main_log.len() > MAIN_LOG_MAX {
            let drop = self.main_log.len() - MAIN_LOG_MAX;
            self.main_log.drain(0..drop);
        }
    }

    fn push_remote_transcript_line(&mut self, line: &TextLine<'static>) {
        let plain: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if plain.trim().is_empty() {
            return;
        }
        self.recent_transcript.push_back(plain);
        while self.recent_transcript.len() > REMOTE_TRANSCRIPT_MAX {
            self.recent_transcript.pop_front();
        }
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

/// One renderable item of a resumed session's transcript. Built by the core from the rehydrated
/// messages and replayed by [`App::replay_history`] so a resumed session shows the full
/// conversation — text *and* tool activity — exactly as it looked live (not a user-only echo).
#[derive(Debug, Clone)]
pub enum ReplayItem {
    /// A user prompt.
    User(String),
    /// Assistant answer text (markdown).
    Assistant(String),
    /// A tool the assistant invoked, with its (compacted) arguments.
    Tool { name: String, args: String },
    /// A tool's result line.
    ToolResult {
        name: String,
        ok: bool,
        summary: String,
    },
    /// A dim advisory (e.g. the "earlier conversation summarized" compaction marker).
    Note(String),
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

/// The welcome banner, printed once into scrollback. Left-aligned.
pub fn banner_lines(width: u16) -> Vec<TextLine<'static>> {
    let mut lines = vec![TextLine::default()];
    if width < WORDMARK_WIDTH {
        lines.push(TextLine::from(Span::styled(
            "⚒ FORGE",
            Style::default().fg(ORANGE).bold(),
        )));
        lines.push(TextLine::from(Span::styled(
            "model-mesh coding agent",
            Style::default().fg(DIM),
        )));
    } else {
        for row in FORGE_WORDMARK {
            lines.push(TextLine::from(Span::styled(
                row.to_string(),
                Style::default().fg(ORANGE).bold(),
            )));
        }
        lines.push(TextLine::default());
        lines.push(TextLine::from(Span::styled(
            TAGLINE.to_string(),
            Style::default().fg(DIM),
        )));
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
    // The in-loop activity viewer (full-screen mode) takes over the whole frame, rendered through
    // the SAME terminal as the chat — no nested alternate screen, so it can't collide with it.
    if let Some(v) = &app.viewer {
        let views = app.activity_views();
        let scroll = if v.follow { usize::MAX / 2 } else { v.scroll };
        let a = frame.area();
        frame.render_widget(
            Paragraph::new(crate::transcript::transcript_lines(
                &views, v.selected, scroll, a.height, a.width,
            )),
            a,
        );
        return;
    }
    const MIN_STREAM: u16 = 1;
    // The input box grows with wrapped/multiline content (capped); the stream area absorbs the
    // change, so the inline viewport's total height is untouched (never resized at runtime).
    let input_h = input_box_height(&app.input, frame.area().width);
    let status_h = statusline_height(app);
    // A one-row band between the input and the statusline: shows the animated compaction bar while
    // compacting, otherwise an "approaching auto-compact" hint when the context fills up.
    let band_h = compact_band_height(app);
    let fixed = PERMISSION_H + input_h + band_h + status_h;
    let avail = frame.area().height.saturating_sub(fixed);
    let panel_avail = avail.saturating_sub(MIN_STREAM);

    // The activity panel (main chat + subagents + critics) and the tasks panel each want their
    // full height. When both fit in the panel budget (always true in full-screen mode, where the
    // live region spans the terminal) they each get it — so the activity list shows every entry up
    // to its cap, like the tasks list. Only when the inline budget is contended do we split it,
    // giving each panel a fair half but letting the smaller one keep its full size.
    let (activity_h, task_h) = split_panel_budget(
        app.activity_panel_height(),
        app.tasks_panel_height(),
        panel_avail,
    );
    let stream_h = avail.saturating_sub(activity_h + task_h);

    let areas = Layout::vertical([
        Constraint::Length(stream_h),
        Constraint::Length(activity_h),
        Constraint::Length(task_h),
        Constraint::Length(PERMISSION_H),
        Constraint::Length(input_h),
        Constraint::Length(band_h),
        Constraint::Length(status_h),
    ])
    .split(frame.area());

    // areas[0]: the main region. A modal overlay (palette / picker) takes it when open; otherwise
    // it's the scrollable transcript in full-screen mode, or just the in-flight reply edge inline.
    if app.palette.open {
        render_palette(frame, areas[0], app);
    } else if app.at_picker.open {
        render_at_path_picker(frame, areas[0], app);
    } else if app.picker.open {
        render_picker(frame, areas[0], app);
    } else if app.fullscreen {
        render_transcript_area(frame, areas[0], app);
    } else {
        render_preview(frame, areas[0], app);
    }
    if activity_h > 0 {
        render_activity_panel(frame, areas[1], app);
    }
    if task_h > 0 {
        frame.render_widget(
            Paragraph::new(tasks_panel_lines(&app.tasks, areas[2].height)),
            areas[2],
        );
    }
    if app.prompt.is_some() {
        render_permission(frame, areas[3], app);
    }
    render_input(frame, areas[4], app);
    if band_h > 0 {
        render_compact_band(frame, areas[5], app);
    }
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
/// Divide the panel budget between the activity panel (`want_a`) and the tasks panel (`want_t`).
/// If both fit, each gets its full desired height. Otherwise split fairly: each keeps up to half,
/// and any slack the smaller panel doesn't use is handed to the larger one.
fn split_panel_budget(want_a: u16, want_t: u16, budget: u16) -> (u16, u16) {
    if want_a + want_t <= budget {
        return (want_a, want_t);
    }
    let half = budget / 2;
    if want_a <= half {
        (want_a, budget.saturating_sub(want_a).min(want_t))
    } else if want_t <= half {
        (budget.saturating_sub(want_t).min(want_a), want_t)
    } else {
        (half, budget.saturating_sub(half).min(want_t))
    }
}

/// Render the full-screen transcript: the finalized conversation (`main_log`) plus the in-flight
/// reply edge, wrapped to the area width and scrolled to `transcript_scroll` (or the tail while
/// following). This is the full-screen counterpart to the inline scrollback + [`render_preview`].
fn render_transcript_area(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let body_h = area.height as usize;
    let wrapped = app.wrapped_transcript(area.width);
    let total = wrapped.len();
    let max_scroll = total.saturating_sub(body_h);
    let scroll = if app.transcript_follow {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    };
    let lines: Vec<TextLine> = wrapped.into_iter().skip(scroll).take(body_h).collect();
    frame.render_widget(Paragraph::new(lines), area);
}

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

/// Short model label: strip the `provider::` prefix so the panel shows e.g. `opus` not
/// `anthropic::claude-opus-4-8`-style fully-qualified ids.
fn model_short(model: Option<&str>) -> String {
    match model {
        Some(m) if !m.is_empty() => m.split("::").last().unwrap_or(m).to_string(),
        _ => "…".to_string(),
    }
}

/// The unified sticky activity panel: lists the main chat plus every subagent and assay critic in
/// one navigable list. When focused (Ctrl+O) the selected row is highlighted and ↑↓ move it; Enter
/// opens that entry's full-screen transcript. Themed per kind: ● main chat, ⚒ subagent, ⚖ critic.
fn render_activity_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    // Cheap per-frame metadata only — building full transcripts here would clone the whole main
    // log + re-render markdown every frame (jank/ghosting). Full views are built lazily on Enter.
    let views = app.activity_summaries();
    if views.is_empty() {
        return;
    }
    let h = area.height as usize;
    let w = area.width as usize;
    let spin = SPINNER[app.tick % SPINNER.len()];
    let focused = app.activity_focused;

    let mut lines: Vec<TextLine> = Vec::with_capacity(h);
    let hint = if focused {
        "↑↓ select · ⏎ open · esc"
    } else {
        "^O focus"
    };
    lines.push(TextLine::from(vec![
        Span::styled(
            format!("  ⚒ activity ({})  ", views.len()),
            Style::default().fg(ORANGE).bold(),
        ),
        Span::styled(hint, Style::default().fg(DIM)),
    ]));

    let body_h = h.saturating_sub(1);
    // Scroll so the selected row stays visible when the list overflows the panel.
    let start = if focused {
        app.activity_idx.saturating_sub(body_h.saturating_sub(1))
    } else {
        0
    };
    let overflow = views.len() > start + body_h;
    let row_budget = if overflow {
        body_h.saturating_sub(1)
    } else {
        body_h
    };

    for (i, v) in views.iter().enumerate().skip(start).take(row_budget) {
        let selected = focused && i == app.activity_idx;
        let marker = if selected { "▸" } else { " " };
        let (kind_glyph, kind_color) = match v.kind {
            ActivityKind::MainChat => ("●", TOOLCYAN),
            ActivityKind::Subagent => ("⚒", ORANGE),
            ActivityKind::AssayCritic => ("⚖", WARNYEL),
        };
        let status_span = match v.status {
            ActivityStatus::Running => Span::styled(format!("{spin} "), Style::default().fg(DIM)),
            ActivityStatus::Done => Span::styled("✓ ", Style::default().fg(OKGREEN)),
            ActivityStatus::Skipped => Span::styled("⏭ ", Style::default().fg(DIM)),
        };
        let title_style = if selected {
            Style::default().fg(ORANGE).bold()
        } else {
            Style::default().fg(kind_color).bold()
        };
        let model = model_short(v.model.as_deref());
        // Trailing detail: line count for chats, the subtitle (findings/focus) for critics.
        let detail = match v.kind {
            ActivityKind::AssayCritic => v.subtitle.clone(),
            _ => format!("{} ln", v.line_count),
        };
        let cost = if v.cost > 0.0 {
            format!("  ${:.4}", v.cost)
        } else {
            String::new()
        };
        let head = format!("  {marker} {kind_glyph} ");
        let used = head.chars().count() + v.title.chars().count() + model.len() + 8;
        let detail_max = w.saturating_sub(used).max(8);
        lines.push(TextLine::from(vec![
            Span::styled(
                head,
                Style::default().fg(if selected { ORANGE } else { DIM }),
            ),
            status_span,
            Span::styled(format!("{} ", v.title), title_style),
            Span::styled(format!("[{model}]  "), Style::default().fg(DIM)),
            Span::styled(
                format!("{}{cost}", truncate(&detail, detail_max)),
                Style::default().fg(DIM),
            ),
        ]));
    }
    if overflow {
        let hidden = views.len() - (start + row_budget);
        lines.push(TextLine::from(Span::styled(
            format!("    … +{hidden} more"),
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
    let total = tasks.len();
    let done_count = tasks
        .iter()
        .filter(|t| t.status == TodoStatus::Done)
        .count();
    let in_progress_count = tasks
        .iter()
        .filter(|t| t.status == TodoStatus::InProgress)
        .count();
    let open_count = tasks
        .iter()
        .filter(|t| t.status == TodoStatus::Pending)
        .count();
    let header = format!(
        "  ⚒ {total} tasks ({done_count} done, {in_progress_count} in progress, {open_count} open)"
    );
    let mut lines = vec![TextLine::from(Span::styled(
        header,
        Style::default().fg(ORANGE).bold(),
    ))];
    let body_h = h.saturating_sub(1);
    // Prioritize: in-progress first, then pending, then done.
    let mut idxs: Vec<usize> = (0..total).collect();
    idxs.sort_by_key(|&i| match tasks[i].status {
        TodoStatus::InProgress => 0,
        TodoStatus::Pending => 1,
        TodoStatus::Done => 2,
    });
    // Count non-done (always show) vs done (may be truncated).
    let non_done: Vec<usize> = idxs
        .iter()
        .copied()
        .filter(|&i| tasks[i].status != TodoStatus::Done)
        .collect();
    let done_idxs: Vec<usize> = idxs
        .iter()
        .copied()
        .filter(|&i| tasks[i].status == TodoStatus::Done)
        .collect();
    // Always show all non-done; fill remaining rows with done items.
    let rows_for_done = body_h
        .saturating_sub(non_done.len())
        .saturating_sub(usize::from(!done_idxs.is_empty()));
    let show_done = rows_for_done.min(done_idxs.len());
    let overflow_done = done_idxs.len().saturating_sub(show_done);
    let shown_idxs: Vec<usize> = non_done
        .iter()
        .chain(done_idxs.iter().take(show_done))
        .copied()
        .collect();
    for &i in &shown_idxs {
        let t = &tasks[i];
        let (glyph, style) = match t.status {
            TodoStatus::Done => ("✔", Style::default().fg(DIM)),
            TodoStatus::InProgress => ("◼", Style::default().fg(ORANGE).bold()),
            TodoStatus::Pending => ("○", Style::default().fg(Color::Rgb(205, 205, 215))),
        };
        lines.push(TextLine::from(Span::styled(
            format!("  {glyph} {}", truncate(&t.title, 62)),
            style,
        )));
    }
    if overflow_done > 0 {
        lines.push(TextLine::from(Span::styled(
            format!("   … +{overflow_done} completed"),
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

/// Inner text width available to the input: box width minus the two borders, the 1-col horizontal
/// padding each side. The leading `› ` prompt (2 cols) eats into the first row, handled by callers.
fn input_inner_width(box_width: u16) -> usize {
    (box_width as usize).saturating_sub(4).max(1)
}

/// How many visual text rows the input occupies once wrapped at `box_width` (accounting for the
/// `› ` prompt on the first row and any explicit newlines), before clamping. Drives both the box
/// height and the scroll-to-cursor offset, so wrapping never hides what's being typed.
fn input_text_rows(input: &str, box_width: u16) -> u16 {
    let inner = input_inner_width(box_width);
    let mut rows = 0usize;
    for (i, line) in input.split('\n').enumerate() {
        let cols = line.chars().count() + if i == 0 { 2 } else { 0 }; // prompt on row 0
        rows += cols.saturating_sub(1) / inner + 1; // ≥1 row per logical line
    }
    rows.max(1) as u16
}

/// Dynamic input-box height: grows from [`INPUT_H`] to [`INPUT_MAX_H`] with the wrapped content.
pub fn input_box_height(input: &str, box_width: u16) -> u16 {
    (input_text_rows(input, box_width) + 2).clamp(INPUT_H, INPUT_MAX_H)
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ORANGE))
        .padding(Padding::horizontal(1))
        .title(Span::styled(" message ", Style::default().fg(ORANGE)));

    // Build one ratatui Line per explicit input line so pasted newlines render as separate rows;
    // long lines are then soft-wrapped by `Wrap`. Slash-command highlighting + block cursor apply
    // to the line that contains the cursor; later lines render plain.
    let cursor = app.input_cursor.min(app.input.len());
    // Cursor appearance: a solid orange block when focused, suppressed on the blink "off" frame,
    // and a dim hollow (underline) when the terminal window has lost focus.
    let cursor_style = if app.unfocused {
        Style::default().fg(DIM).add_modifier(Modifier::UNDERLINED)
    } else if app.cursor_hidden {
        Style::default()
    } else {
        Style::default().fg(STATUSBG).bg(ORANGE)
    };
    let input_lines: Vec<&str> = app.input.split('\n').collect();
    let mut byte_off = 0usize;
    let mut text_lines: Vec<TextLine> = Vec::with_capacity(input_lines.len().max(1));
    for (i, line) in input_lines.iter().enumerate() {
        let line_end = byte_off + line.len();
        let cursor_col = if cursor >= byte_off && cursor <= line_end {
            Some(cursor - byte_off)
        } else {
            None
        };
        byte_off = line_end + 1; // skip the \n separator

        let mut spans = Vec::new();
        if i == 0 {
            spans.push(Span::styled("› ", Style::default().fg(ORANGE).bold()));
        }
        if let Some(col) = cursor_col {
            spans.extend(line_spans_with_cursor(line, col, i == 0, cursor_style));
        } else if i == 0 {
            spans.extend(input_spans(line));
        } else {
            spans.push(Span::raw(line.to_string()));
        }
        text_lines.push(TextLine::from(spans));
    }

    // Scroll so the cursor row (bottom) stays visible once content exceeds the visible rows.
    let visible_rows = area.height.saturating_sub(2).max(1);
    let total_rows = input_text_rows(&app.input, area.width);
    let scroll = total_rows.saturating_sub(visible_rows);
    frame.render_widget(
        Paragraph::new(ratatui::text::Text::from(text_lines))
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
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

/// Render one input line that contains the cursor, producing spans with a block cursor
/// (the character under the cursor shown with inverted fg/bg). For the first input line
/// (`first_line = true`) a slash-command token anywhere on the line is highlighted in orange;
/// the highlight continues correctly even when the cursor is inside the command name.
fn line_spans_with_cursor(
    line: &str,
    col: usize,
    first_line: bool,
    cursor_style: Style,
) -> Vec<Span<'static>> {
    let tok = if first_line {
        crate::commands::slash_token_at(line, line.len())
    } else {
        None
    };

    // The character at `col` (or a space if at end) becomes the cursor cell, styled by the caller
    // (solid block / blink-off / hollow-when-unfocused).
    let at_bytes = &line[col..];
    let (cursor_ch, cursor_len) = at_bytes
        .chars()
        .next()
        .map(|c| (c, c.len_utf8()))
        .unwrap_or((' ', 0));
    let cursor_span = Span::styled(cursor_ch.to_string(), cursor_style);

    match tok {
        Some(ref tok) => {
            let tok_start = tok.start;
            let tok_end = tok.end;

            // Helper: emit a styled tok-segment (orange bold).
            let tok_span = |s: &str| -> Span<'static> {
                Span::styled(s.to_string(), Style::default().fg(ORANGE).bold())
            };

            if col < tok_start {
                // cursor is before the token
                let mut out = vec![];
                if col > 0 {
                    out.push(Span::raw(line[..col].to_string()));
                }
                out.push(cursor_span);
                let between = &line[col + cursor_len..tok_start];
                if !between.is_empty() {
                    out.push(Span::raw(between.to_string()));
                }
                out.push(tok_span(&line[tok_start..tok_end]));
                if tok_end < line.len() {
                    out.push(Span::raw(line[tok_end..].to_string()));
                }
                out
            } else if col >= tok_end {
                // cursor is after the token
                let mut out = vec![];
                if tok_start > 0 {
                    out.push(Span::raw(line[..tok_start].to_string()));
                }
                out.push(tok_span(&line[tok_start..tok_end]));
                let between = &line[tok_end..col];
                if !between.is_empty() {
                    out.push(Span::raw(between.to_string()));
                }
                out.push(cursor_span);
                let rest = &line[col + cursor_len..];
                if !rest.is_empty() {
                    out.push(Span::raw(rest.to_string()));
                }
                out
            } else {
                // cursor is inside the token
                let mut out = vec![];
                if tok_start > 0 {
                    out.push(Span::raw(line[..tok_start].to_string()));
                }
                let pre_in_tok = &line[tok_start..col];
                if !pre_in_tok.is_empty() {
                    out.push(tok_span(pre_in_tok));
                }
                out.push(cursor_span);
                let post_in_tok = &line[col + cursor_len..tok_end];
                if !post_in_tok.is_empty() {
                    out.push(tok_span(post_in_tok));
                }
                if tok_end < line.len() {
                    out.push(Span::raw(line[tok_end..].to_string()));
                }
                out
            }
        }
        None => {
            // No slash token — just render with block cursor.
            let mut out = vec![];
            if col > 0 {
                out.push(Span::raw(line[..col].to_string()));
            }
            out.push(cursor_span);
            let rest = &line[col + cursor_len..];
            if !rest.is_empty() {
                out.push(Span::raw(rest.to_string()));
            }
            out
        }
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
/// Assumed context window when the model's real limit is unknown (not in the pricing table), so a
/// percentage + bar can still be shown. Marked approximate (`~`) in the UI. 128k is a common
/// mid-size window — conservative enough to warn before most models actually overflow.
const CONTEXT_FALLBACK_LIMIT: u64 = 128_000;

/// Render the context gauge: a small bar + `used/limit` + `pct%`, colored by fill. When the model's
/// real window is unknown, a conservative fallback is assumed and the reading is marked `~approx`.
fn context_gauge_spans(used: u64, limit: Option<u32>) -> Vec<Span<'static>> {
    let (limit, approx) = match limit {
        Some(l) if l > 0 => (l as u64, false),
        _ => (CONTEXT_FALLBACK_LIMIT, true),
    };
    let frac = (used as f64 / limit as f64).clamp(0.0, 1.0);
    let pct = (frac * 100.0).round() as u64;
    let color = if pct >= 90 {
        ERRRED
    } else if pct >= 70 {
        WARNYEL
    } else {
        DIM
    };
    // A compact 10-cell bar: filled cells scale with the fill fraction.
    const CELLS: usize = 10;
    let filled = (frac * CELLS as f64).round() as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(CELLS - filled);
    let tail = if approx { " ~approx" } else { "" };
    vec![
        Span::styled("◷ ", Style::default().fg(DIM).bg(STATUSBG)),
        Span::styled(bar, Style::default().fg(color).bg(STATUSBG)),
        Span::styled(
            format!(" {}/{} ", human(used), human(limit)),
            Style::default().fg(DIM).bg(STATUSBG),
        ),
        Span::styled(
            format!("{pct}%{tail}"),
            Style::default().fg(color).bold().bg(STATUSBG),
        ),
    ]
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
    let title = if app.usage_overlay.loading {
        format!(" {spinner} Usage  loading… ")
    } else {
        format!(" {spinner} Usage ")
    };
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
    // A staleness note for the Claude rate-limit %, so an old reading is never shown as live.
    let claude_age = rl_age_note(o.claude_rl_age_secs);
    let bridge_5h = {
        let mut parts = Vec::new();
        if let Some(p) = o.codex_5h_pct {
            parts.push(format!("codex:{:.0}%", p));
        }
        if let Some(p) = o.claude_5h_pct {
            parts.push(format!("claude:{:.0}%{}", p, claude_age));
        } else if o.claude_rl_age_secs.is_some() {
            // Cache exists but the 5h reading is too old to trust (5h window) — say so plainly
            // rather than falling back to a confusing multi-million raw-token sum.
            parts.push(format!("claude:5h stale{claude_age}"));
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
            parts.push(format!("claude:{:.0}%{}", p, claude_age));
        } else if o.claude_rl_age_secs.is_some() {
            parts.push(format!("claude:wk stale{claude_age}"));
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
                Cell::from(display.clone()).style(style),
                Cell::from(cost_cell(&display, *cost)).style(style),
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

/// Honest cost label for a per-model row. A flat-rate subscription bridge (Claude Code / Codex
/// CLI) genuinely costs $0 per call, so it reads "subscription". A priced model shows its dollar
/// cost. Anything else at $0 is a model we have NO price for (gateway/credit providers like
/// OpenCode Zen, OpenRouter pass-through) — it may still burn real credit, so we say "untracked"
/// rather than lie that it's a subscription.
fn cost_cell(model: &str, cost: f64) -> String {
    if model.starts_with("claude-cli") || model.starts_with("codex-cli") {
        "subscription".to_string()
    } else if cost > 0.0 {
        format!("${cost:.5}")
    } else {
        "untracked".to_string()
    }
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
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
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

    let settled = o.anim_tick >= o.settle_tick();
    let glyph = if settled {
        "⚒"
    } else {
        SPINNER[(o.anim_tick as usize) % SPINNER.len()]
    };
    let title = format!(" {glyph} mesh inspector ");
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(TOOLCYAN));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Show loading spinner while bridge stats + routing explanation are fetched in background.
    if o.loading {
        let spinner = SPINNER[(o.anim_tick as usize) % SPINNER.len()];
        f.render_widget(
            ratatui::widgets::Paragraph::new(format!(" {spinner} analyzing routing…"))
                .style(Style::default().fg(DIM)),
            inner,
        );
        return;
    }

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
            Span::raw(mesh_truncate(
                &o.prompt,
                inner.width.saturating_sub(8) as usize,
            )),
        ]));
        let tier = if !o.routed.is_empty() && o.routed != o.classified {
            format!("tier  {} → {}   ({})", o.classified, o.routed, o.reasons)
        } else {
            format!("tier  {}   ({})", o.classified, o.reasons)
        };
        top.push(Line::from(Span::styled(
            tier,
            Style::default().fg(Color::Cyan),
        )));
        if !o.classifier.is_empty() {
            top.push(Line::from(vec![
                Span::styled("cls   ", Style::default().fg(DIM)),
                Span::styled(o.classifier.clone(), Style::default().fg(DIM)),
            ]));
        }
    }
    top.push(Line::from(""));
    for q in &o.quota {
        let mut spans = vec![Span::styled(
            format!("  {:<11} ", q.provider),
            Style::default(),
        )];
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
        let col = if o.conserve_fired {
            Color::Yellow
        } else {
            Color::Gray
        };
        top.push(Line::from(Span::styled(
            format!("  conserve  {}", o.conserve_line),
            Style::default().fg(col),
        )));
    }
    top.push(Line::from(""));

    let top_h = (top.len() as u16).min(inner.height.saturating_sub(1));
    let chunks = Layout::vertical([Constraint::Length(top_h), Constraint::Min(0)]).split(inner);
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
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else if !c.usable {
            Style::default().fg(DIM)
        } else {
            Style::default()
        };
        rows.push(Line::from(vec![
            Span::styled(format!("{marker} #{:<2} ", c.rank), base),
            Span::styled(
                format!(
                    "{:<width$}",
                    mesh_truncate(&c.model, model_w),
                    width = model_w
                ),
                base,
            ),
            Span::styled(format!("  {:>6.2}  ", c.score), base),
            Span::styled(
                tag,
                if c.selected {
                    base
                } else {
                    Style::default().fg(DIM)
                },
            ),
        ]));
    }
    rows.push(Line::from(""));
    rows.push(Line::from(vec![
        Span::styled("pick  ", Style::default().fg(DIM)),
        Span::styled(
            o.pick.clone(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if !o.rationale.is_empty() {
        rows.push(Line::from(Span::styled(
            mesh_truncate(&format!("why   {}", o.rationale), inner.width as usize),
            Style::default().fg(DIM),
        )));
    }
    rows.push(Line::from(Span::styled(
        "↑/↓ scroll · Esc to close",
        Style::default().fg(DIM),
    )));
    // Clamp the scroll so it can't run past the content.
    let body_h = chunks[1].height;
    let max_scroll = (rows.len() as u16).saturating_sub(body_h);
    let scroll = o.scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(Text::from(rows)).scroll((scroll, 0)),
        chunks[1],
    );
}

/// A compact " (Xm/Xh ago)" suffix for rate-limit data older than ~10 min; empty when fresh or
/// unknown. Keeps the overlay honest about staleness instead of presenting old % as live.
fn rl_age_note(age_secs: Option<i64>) -> String {
    match age_secs {
        Some(a) if a >= 3600 => format!(" ({}h ago)", a / 3600),
        Some(a) if a >= 600 => format!(" ({}m ago)", a / 60),
        _ => String::new(),
    }
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

/// Context fill at/above which the "approaching auto-compact" hint appears below the input.
const COMPACT_HINT_FRACTION: f64 = 0.65;
/// The context fill at which the core auto-compacts (~80% of the usable window). Shown as the
/// target in the hint and used to color it red once reached.
const AUTO_COMPACT_FRACTION: f64 = 0.80;
/// Time constant (seconds) for easing the indeterminate compaction bar toward its ceiling.
const COMPACT_EASE_TAU_SECS: f64 = 2.5;

/// Current context fill as a fraction (0..=1), using the same fallback window as the gauge.
/// `None` when there's no token/limit signal yet (so no band is shown on a fresh session).
fn context_fraction(app: &App) -> Option<f64> {
    if app.context_tokens == 0 && app.context_limit.is_none() {
        return None;
    }
    let limit = match app.context_limit {
        Some(l) if l > 0 => l as u64,
        _ => CONTEXT_FALLBACK_LIMIT,
    };
    Some((app.context_tokens as f64 / limit as f64).clamp(0.0, 1.0))
}

/// One row while compaction runs (animated bar) or while the context is approaching the
/// auto-compact threshold (hint); zero otherwise.
pub fn compact_band_height(app: &App) -> u16 {
    if app.compaction.is_some() {
        return 1;
    }
    match context_fraction(app) {
        Some(f) if f >= COMPACT_HINT_FRACTION => 1,
        _ => 0,
    }
}

/// Render the compaction band: an animated, eased progress bar with elapsed time while compacting,
/// else a colored "approaching auto-compact" hint with the tokens remaining until the trigger.
fn render_compact_band(frame: &mut Frame, area: Rect, app: &App) {
    let bg = Style::default().bg(STATUSBG);
    let spans: Vec<Span> = if let Some(c) = &app.compaction {
        let elapsed = app.tick.saturating_sub(c.start_tick) as f64 * 0.06;
        // Indeterminate work (one summarizer call): ease toward a ceiling instead of faking a real
        // fraction; CompactionFinished clears the band (the "snap to done").
        let frac = 1.0 - (-elapsed / COMPACT_EASE_TAU_SECS).exp();
        let pct = (frac * 95.0).round() as u64;
        const CELLS: usize = 16;
        let filled = ((frac * 0.95) * CELLS as f64).round() as usize;
        let filled = filled.min(CELLS);
        let spin = SPINNER[app.tick % SPINNER.len()];
        let bar: String = "█".repeat(filled) + &"░".repeat(CELLS - filled);
        let label = if c.auto {
            "auto-compacting"
        } else {
            "compacting"
        };
        vec![
            Span::styled(
                format!(" {spin} {label} "),
                Style::default().fg(ORANGE).bold().bg(STATUSBG),
            ),
            Span::styled(bar, Style::default().fg(ORANGE).bg(STATUSBG)),
            Span::styled(
                format!(" {pct}%  {elapsed:.1}s"),
                Style::default().fg(DIM).bg(STATUSBG),
            ),
        ]
    } else {
        let frac = context_fraction(app).unwrap_or(0.0);
        let pct = (frac * 100.0).round() as u64;
        let limit = match app.context_limit {
            Some(l) if l > 0 => l as u64,
            _ => CONTEXT_FALLBACK_LIMIT,
        };
        let trigger = (AUTO_COMPACT_FRACTION * limit as f64) as u64;
        let left = trigger.saturating_sub(app.context_tokens);
        let color = if frac >= AUTO_COMPACT_FRACTION {
            ERRRED
        } else if frac >= 0.72 {
            WARNYEL
        } else {
            DIM
        };
        let msg = if frac >= AUTO_COMPACT_FRACTION {
            format!(" ⚠ context {pct}% — auto-compact imminent")
        } else {
            format!(
                " ⚠ context {pct}% — auto-compact at {:.0}% (~{} left)",
                AUTO_COMPACT_FRACTION * 100.0,
                human(left)
            )
        };
        vec![Span::styled(msg, Style::default().fg(color).bg(STATUSBG))]
    };
    frame.render_widget(Paragraph::new(TextLine::from(spans)).style(bg), area);
}

/// Returns 1 when idle (no session data), 2 once context / token data is available.
/// Used by [`render_live`] to allocate the right number of rows for the status area.
pub fn statusline_height(app: &App) -> u16 {
    if app.context_tokens > 0
        || app.context_limit.is_some()
        || app.session_in > 0
        || app.session_out > 0
    {
        2
    } else {
        1
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

    // Line 1: spinner · [tier] model · $cost · ◆ temper · ◉ remote   (hint right-aligned)
    let mut line1: Vec<Span> = vec![Span::styled(" ", bg)];
    if app.busy && w >= 40 {
        let f = SPINNER[app.tick % SPINNER.len()];
        line1.push(Span::styled(
            format!("{f} working"),
            Style::default().fg(ORANGE).bg(STATUSBG),
        ));
        line1.push(sep());
    }
    if let (Some(t), true) = (tier, w >= 52) {
        line1.push(Span::styled(
            format!("[{t}] "),
            Style::default().fg(ORANGE).bold().bg(STATUSBG),
        ));
    }
    line1.push(Span::styled(
        model.to_string(),
        Style::default().fg(Color::White).bg(STATUSBG),
    ));
    line1.push(sep());
    line1.push(Span::styled(
        format!("${:.4}", app.cost_usd),
        Style::default().fg(OKGREEN).bold().bg(STATUSBG),
    ));
    // The active temper (operating mode), color-coded by how permissive it is.
    if !app.temper.is_empty() && w >= 46 {
        line1.push(sep());
        line1.push(Span::styled(
            format!("◆ {}", app.temper),
            Style::default()
                .fg(temper_color(&app.temper))
                .bold()
                .bg(STATUSBG),
        ));
    }
    if app.remote_active && w >= 52 {
        line1.push(sep());
        line1.push(Span::styled(
            "◉ remote",
            Style::default().fg(OKGREEN).bold().bg(STATUSBG),
        ));
    }
    if !app.queued.is_empty() {
        line1.push(sep());
        line1.push(Span::styled(
            format!("⏳ {} queued", app.queued.len()),
            Style::default().fg(WARNYEL).bold().bg(STATUSBG),
        ));
    }

    let version = concat!("v", env!("CARGO_PKG_VERSION"));
    let hint = if app.busy {
        "esc stop "
    } else if app.done {
        "done · esc quit "
    } else {
        "⇧⇥ temper · esc quit "
    };
    let row1 = Rect { height: 1, ..area };
    if w >= 70 {
        // Right side: version + hint
        let right_text = format!("{version}  {hint}");
        let right_len = right_text.chars().count() as u16;
        let cols =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right_len)]).split(row1);
        frame.render_widget(Paragraph::new(TextLine::from(line1)).style(bg), cols[0]);
        frame.render_widget(
            Paragraph::new(TextLine::from(vec![
                Span::styled(
                    format!("{version}  "),
                    Style::default().fg(DIM).bg(STATUSBG),
                ),
                Span::styled(hint, Style::default().fg(DIM).bg(STATUSBG)),
            ]))
            .alignment(Alignment::Right)
            .style(bg),
            cols[1],
        );
    } else {
        frame.render_widget(Paragraph::new(TextLine::from(line1)).style(bg), row1);
    }

    // Line 2: ↑in ↓out · ◷ bar used/limit pct% — always untruncated on its own row.
    if area.height >= 2 {
        let row2 = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };
        let mut line2: Vec<Span> = vec![Span::styled(" ", bg)];
        if app.session_in > 0 || app.session_out > 0 {
            line2.push(Span::styled(
                format!("↑{} ↓{}", human(app.session_in), human(app.session_out)),
                Style::default().fg(DIM).bg(STATUSBG),
            ));
        }
        if app.context_tokens > 0 || app.context_limit.is_some() {
            if app.session_in > 0 || app.session_out > 0 {
                line2.push(sep());
            }
            line2.extend(context_gauge_spans(app.context_tokens, app.context_limit));
        }
        frame.render_widget(Paragraph::new(TextLine::from(line2)).style(bg), row2);
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

    #[test]
    fn compact_band_hidden_until_context_fills_then_shows_hint() {
        assert_eq!(
            compact_band_height(&App::default()),
            0,
            "no signal → no band"
        );
        let low = App {
            context_tokens: 10_000,
            context_limit: Some(100_000), // 10%
            ..Default::default()
        };
        assert_eq!(compact_band_height(&low), 0, "plenty of room → no band");
        let near = App {
            context_tokens: 70_000, // 70% ≥ 65% hint threshold
            context_limit: Some(100_000),
            ..Default::default()
        };
        assert_eq!(compact_band_height(&near), 1, "approaching → hint shows");
        let out = screen_wh(&near, 80, LIVE_H);
        assert!(
            out.contains("auto-compact"),
            "hint names the trigger: {out:?}"
        );
    }

    #[test]
    fn compact_band_shows_animated_bar_while_compacting() {
        let app = App {
            compaction: Some(CompactionState {
                start_tick: 0,
                auto: true,
            }),
            tick: 40, // ~2.4s elapsed → eased bar well underway
            ..Default::default()
        };
        assert_eq!(compact_band_height(&app), 1);
        let out = screen_wh(&app, 80, LIVE_H);
        assert!(out.contains("auto-compacting"), "shows the label: {out:?}");
        assert!(out.contains('%'), "shows a percentage: {out:?}");
        assert!(out.contains('█'), "shows a filled bar: {out:?}");
    }

    #[test]
    fn cost_cell_distinguishes_subscription_priced_and_untracked() {
        assert_eq!(cost_cell("claude-cli::", 0.0), "subscription");
        assert_eq!(cost_cell("codex-cli::gpt-5.5", 0.0), "subscription");
        assert_eq!(cost_cell("openai::gpt-4o-mini", 0.0123), "$0.01230");
        // Unpriced gateway/credit model: not a bridge, $0 only because we lack a price.
        assert_eq!(cost_cell("opencode_go::glm-5.2", 0.0), "untracked");
    }

    #[test]
    fn resolve_paste_blocks_substitutes_text_and_extracts_images() {
        let mut app = App::default();
        app.input.clear();
        app.input_cursor = 0;
        // Type "see ", paste a multiline text block, type " and ", attach an image.
        for c in "see ".chars() {
            handle_key(&mut app.input, &mut app.input_cursor, KeyKind::Char(c));
        }
        app.handle_paste("line1\nline2\nline3".to_string());
        for c in " and ".chars() {
            handle_key(&mut app.input, &mut app.input_cursor, KeyKind::Char(c));
        }
        app.attach_image(
            forge_types::ImageAttachment {
                media_type: "image/png".to_string(),
                data_base64: "Zm9v".to_string(),
            },
            "PNG 2x2",
        );
        let raw = std::mem::take(&mut app.input);
        let (resolved, images) = app.resolve_paste_blocks(raw);
        // Text block restored inline; image placeholder stripped; image returned out-of-band.
        assert_eq!(resolved, "see line1\nline2\nline3 and ");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data_base64, "Zm9v");
        // Blocks are drained.
        let (again, imgs2) = app.resolve_paste_blocks("nothing".to_string());
        assert_eq!(again, "nothing");
        assert!(imgs2.is_empty());
    }

    #[test]
    fn submitted_image_shows_a_marker_in_history() {
        let mut app = App::default();
        app.attach_image(
            forge_types::ImageAttachment {
                media_type: "image/png".to_string(),
                data_base64: "Zm9vYmFy".to_string(),
            },
            "PNG 4x4",
        );
        let raw = std::mem::take(&mut app.input);
        let (_text, images) = app.resolve_paste_blocks(raw);
        assert_eq!(images.len(), 1);
        app.submit_user("look at this");
        let echoed = flush_text(&mut app);
        assert!(echoed.contains("look at this"));
        assert!(
            echoed.contains("🖼 image attached"),
            "history should mark the attached image, got: {echoed}"
        );
        // Marker is one-shot: a later plain turn doesn't repeat it.
        app.submit_user("next");
        let again = flush_text(&mut app);
        assert!(!again.contains("🖼"));
    }

    #[test]
    fn paste_into_empty_input_with_stale_cursor_does_not_panic() {
        // Regression: input_cursor could outlive the input contents (e.g. after a submit clears
        // input but a key path left the cursor > 0), making a `&input[..cursor]` slice panic.
        let mut app = App::default();
        app.input.clear();
        app.input_cursor = 5; // stale: past end of empty input
        app.handle_paste("x".to_string());
        app.input_cursor = 5;
        app.handle_paste("a\nb\nc".to_string());
        let _ = app.try_delete_paste_block(KeyKind::Backspace);
        let _ = app.try_delete_paste_block(KeyKind::DeleteForward);
    }

    #[test]
    fn input_handling_never_panics_and_keeps_cursor_valid() {
        // Deterministic fuzz: drive the input line with a long pseudo-random sequence of edits,
        // pastes (single- and multi-line, multibyte) and paste-block deletes, asserting after
        // every op that the cursor stays in-bounds and on a char boundary. No rand crate — a
        // small LCG keeps it reproducible. Guards the class of panic the paste crash belonged to.
        let mut app = App::default();
        let chars = ['a', 'é', '你', '🦀', ' ', '\n', '/'];
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = |n: u64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) % n
        };
        for _ in 0..20_000 {
            match next(10) {
                0..=4 => {
                    let c = chars[next(chars.len() as u64) as usize];
                    let key = if c == '\n' {
                        KeyKind::InsertNewline
                    } else {
                        KeyKind::Char(c)
                    };
                    let _ = handle_key(&mut app.input, &mut app.input_cursor, key);
                }
                5 => {
                    let key = match next(9) {
                        0 => KeyKind::Backspace,
                        1 => KeyKind::DeleteForward,
                        2 => KeyKind::Left,
                        3 => KeyKind::Right,
                        4 => KeyKind::Home,
                        5 => KeyKind::End,
                        6 => KeyKind::DeleteWordBack,
                        7 => KeyKind::KillLineBack,
                        _ => KeyKind::KillLineForward,
                    };
                    let _ = handle_key(&mut app.input, &mut app.input_cursor, key);
                }
                6 => app.handle_paste("single line paste".to_string()),
                7 => app.handle_paste("multi\nline\né你🦀\npaste".to_string()),
                8 => {
                    let _ = app.try_delete_paste_block(KeyKind::Backspace);
                }
                _ => {
                    // Occasionally desync the cursor to simulate the bug's precondition.
                    app.input_cursor = app.input_cursor.wrapping_add(next(4) as usize);
                    let _ = app.try_delete_paste_block(KeyKind::DeleteForward);
                }
            }
            assert!(
                app.input_cursor <= app.input.len(),
                "cursor {} > len {}",
                app.input_cursor,
                app.input.len()
            );
            assert!(
                app.input.is_char_boundary(app.input_cursor),
                "cursor {} not on a char boundary in {:?}",
                app.input_cursor,
                app.input
            );
        }
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
            model: Some("anthropic::opus".into()),
        });
        app.apply(PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "find call sites".into(),
            model: Some("groq::llama".into()),
        });

        // Both children appear in the unified activity list while running.
        let live = screen(&app);
        assert!(
            live.contains("reviewer"),
            "running child shown in activity list: {live}"
        );
        // The activity panel shows each child's model (stripped to the short name).
        assert!(live.contains("[opus]"), "child model shown: {live}");

        app.apply(PresenterEvent::SubagentProgress {
            id: "a".into(),
            snippet: "inspecting auth".into(),
        });

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
            done_screen.contains("activity ("),
            "activity panel visible: {done_screen}"
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
        let mut cur = 0usize;
        assert_eq!(
            handle_key(&mut input, &mut cur, KeyKind::CycleTemper),
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
    fn remote_active_shows_indicator_in_statusline() {
        // Remote control off → no indicator.
        let app = App::default();
        assert!(
            !screen_wh(&app, 90, LIVE_H).contains("◉ remote"),
            "no indicator when remote control is off"
        );
        // On → the green `◉ remote` segment appears alongside the statusline.
        let app = App {
            remote_active: true,
            ..Default::default()
        };
        assert!(
            screen_wh(&app, 90, LIVE_H).contains("◉ remote"),
            "indicator shown when remote control is on"
        );
        // On a narrow terminal the segment drops out (like the temper).
        let narrow = screen_wh(&app, 48, LIVE_H);
        assert!(
            !narrow.contains("◉ remote"),
            "indicator dropped on narrow terminal: {narrow}"
        );
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
    fn context_gauge_uses_fallback_limit_when_unknown() {
        let mut app = App::default();
        app.apply(PresenterEvent::Cost {
            session_total_usd: 0.01,
            session_in: 5_000,
            session_out: 1_000,
            context_tokens: 6_000,
            context_limit: None,
        });
        let wide = screen_wh(&app, 120, LIVE_H);
        // Unknown limit → conservative fallback so a % + bar still show, marked approximate.
        assert!(
            wide.contains("6.0k/128.0k"),
            "fallback denominator shown: {wide}"
        );
        assert!(
            wide.contains('%'),
            "percentage shown against fallback: {wide}"
        );
        assert!(
            wide.contains("approx"),
            "fallback marked approximate: {wide}"
        );
    }

    #[test]
    fn input_box_grows_with_wrapped_content_then_caps() {
        // Empty input → minimum height. A very long single line → wraps and grows, capped.
        assert_eq!(input_box_height("", 80), INPUT_H);
        let long = "x".repeat(80 * 10); // far exceeds the cap
        assert_eq!(input_box_height(&long, 80), INPUT_MAX_H);
        // A short line stays at the minimum.
        assert_eq!(input_box_height("hello", 80), INPUT_H);
    }

    #[test]
    fn token_and_gauge_always_on_second_statusline_row() {
        let mut app = App::default();
        app.apply(PresenterEvent::Cost {
            session_total_usd: 0.0033,
            session_in: 12_300,
            session_out: 4_100,
            context_tokens: 18_200,
            context_limit: Some(200_000),
        });
        // Gauge + token counter live on line 2 — visible regardless of terminal width.
        let narrow = screen_wh(&app, 60, LIVE_H);
        assert!(narrow.contains("18.2k/200.0k"), "gauge on line 2: {narrow}");
        assert!(narrow.contains("↑12.3k"), "tokens on line 2: {narrow}");
        assert!(narrow.contains("$0.0033"), "cost on line 1: {narrow}");
    }

    #[test]
    fn input_bar_renders_when_present() {
        let input = "fix the bug".to_string();
        let app = App {
            input_cursor: input.len(),
            input,
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
        let mut cur = 0usize;
        assert_eq!(
            handle_key(&mut buf, &mut cur, KeyKind::Char('h')),
            InputOutcome::Editing
        );
        assert_eq!(
            handle_key(&mut buf, &mut cur, KeyKind::Char('i')),
            InputOutcome::Editing
        );
        assert_eq!(buf, "hi");
        assert_eq!(cur, 2);
    }

    #[test]
    fn backspace_removes_last_char() {
        let mut buf = "abc".to_string();
        let mut cur = 3usize;
        assert_eq!(
            handle_key(&mut buf, &mut cur, KeyKind::Backspace),
            InputOutcome::Editing
        );
        assert_eq!(buf, "ab");
        assert_eq!(cur, 2);
    }

    #[test]
    fn enter_submits_and_clears_buffer() {
        let mut buf = "do it".to_string();
        let mut cur = buf.len();
        assert_eq!(
            handle_key(&mut buf, &mut cur, KeyKind::Enter),
            InputOutcome::Submit("do it".into())
        );
        assert_eq!(buf, "", "buffer cleared after submit");
        assert_eq!(cur, 0);
    }

    #[test]
    fn enter_on_empty_buffer_keeps_editing() {
        let mut buf = "   ".to_string();
        let mut cur = buf.len();
        assert_eq!(
            handle_key(&mut buf, &mut cur, KeyKind::Enter),
            InputOutcome::Editing
        );
    }

    #[test]
    fn esc_quits() {
        let mut buf = "whatever".to_string();
        let mut cur = buf.len();
        assert_eq!(
            handle_key(&mut buf, &mut cur, KeyKind::Esc),
            InputOutcome::Quit
        );
    }

    #[test]
    fn left_right_move_cursor() {
        let mut buf = "abc".to_string();
        let mut cur = 3usize;
        handle_key(&mut buf, &mut cur, KeyKind::Left);
        assert_eq!(cur, 2);
        handle_key(&mut buf, &mut cur, KeyKind::Left);
        assert_eq!(cur, 1);
        handle_key(&mut buf, &mut cur, KeyKind::Right);
        assert_eq!(cur, 2);
    }

    #[test]
    fn insert_at_cursor_mid_string() {
        let mut buf = "ac".to_string();
        let mut cur = 1usize; // between 'a' and 'c'
        handle_key(&mut buf, &mut cur, KeyKind::Char('b'));
        assert_eq!(buf, "abc");
        assert_eq!(cur, 2);
    }

    #[test]
    fn ctrl_j_inserts_newline_without_submit() {
        let mut buf = "hello".to_string();
        let mut cur = buf.len();
        assert_eq!(
            handle_key(&mut buf, &mut cur, KeyKind::InsertNewline),
            InputOutcome::Editing
        );
        assert_eq!(buf, "hello\n");
        assert_eq!(cur, 6);
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
        assert!(
            s.contains("3 tasks (1 done, 1 in progress, 1 open)"),
            "panel header + breakdown: {s}"
        );
        // The in-progress item is shown with its glyph (prioritized into the small region).
        assert!(s.contains('◼'), "in-progress glyph shown: {s}");

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
            model: Some("anthropic::opus".into()),
        });
        app.apply(PresenterEvent::AssistantDelta("thinking out loud".into()));
        let s = screen(&app);
        assert!(s.contains("thinking out loud"), "stream shown: {s}");
        assert!(
            s.contains("activity ("),
            "activity panel stays visible while streaming: {s}"
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
            model: Some("anthropic::opus".into()),
        });
        assert_eq!(app.running_subagents(), 1);
        let s = screen(&app);
        assert!(s.contains("activity ("), "panel header while running: {s}");
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
            s.contains("activity ("),
            "panel stays visible showing done state: {s}"
        );

        // The panel collapses at the START of the next user turn, not immediately on result.
        app.on_turn_start();
        assert!(
            !screen(&app).contains("activity ("),
            "panel collapses after on_turn_start: {s}"
        );
    }

    #[test]
    fn mesh_overlay_renders_without_panic() {
        let mesh_overlay = MeshOverlay {
            open: true,
            loading: false,
            prompt: "design a lock-free queue".into(),
            classified: "complex".into(),
            classifier: "heuristic".into(),
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
            scroll: 0,
        };
        let app = App {
            mesh_overlay,
            ..Default::default()
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
            model: Some("groq::llama".into()),
        });
        // More progress than the old 200-snippet cap — the full transcript must be kept. Each
        // snippet ends in a newline so the line-assembler keeps them as distinct lines.
        for i in 0..250 {
            app.apply(PresenterEvent::SubagentProgress {
                id: "a".into(),
                snippet: format!("step {i}\n"),
            });
        }
        app.apply(PresenterEvent::SubagentResult {
            id: "a".into(),
            agent: "general".into(),
            ok: true,
            summary: "found 3 call sites".into(),
            cost_usd: 0.01,
        });
        // activity_views = main chat (index 0) + the subagent (index 1), retained after the batch
        // finishes so the full-screen viewer can still open them.
        let views = app.activity_views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].kind, ActivityKind::MainChat);
        let v = &views[1];
        assert_eq!(v.kind, ActivityKind::Subagent);
        assert_eq!(v.status, ActivityStatus::Done);
        assert!(v.line_count > 200, "full log kept: {}", v.line_count);
        let body: String = v
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(body.contains("step 0"), "oldest line kept: {body}");
        assert!(body.contains("step 249"), "newest line kept");
        assert!(
            body.contains("found 3 call sites"),
            "result appended to transcript"
        );

        // A new batch drops the previous (finished) rows.
        app.apply(PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "next".into(),
            model: None,
        });
        let views = app.activity_views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[1].subtitle, "next");
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

    #[test]
    fn panel_budget_gives_both_full_height_when_they_fit() {
        // Full-screen mode: a generous budget → each panel keeps its full desired height.
        assert_eq!(split_panel_budget(9, 7, 40), (9, 7));
    }

    #[test]
    fn panel_budget_splits_fairly_when_contended() {
        // Both want more than half of a tight inline budget → fair split, neither starved.
        let (a, t) = split_panel_budget(9, 7, 10);
        assert_eq!(a + t, 10, "uses the whole budget");
        assert!(a >= 4 && t >= 4, "neither panel is starved: {a},{t}");
        // A small panel keeps its full size; the big one takes the slack.
        assert_eq!(split_panel_budget(2, 9, 10), (2, 8));
    }

    #[test]
    fn fullscreen_transcript_renders_log_tail_and_clears() {
        let mut app = App {
            fullscreen: true,
            transcript_follow: true,
            ..Default::default()
        };
        app.push_scrollback(
            (0..30)
                .map(|i| TextLine::from(format!("line {i}")))
                .collect(),
        );
        // Following → the tail is visible, the head scrolled off, in a 5-row body.
        let area = Rect::new(0, 0, 40, 5);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 5)).unwrap();
        term.draw(|f| render_transcript_area(f, area, &app))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("line 29"), "tail visible: {text:?}");
        assert!(!text.contains("line 0 "), "head scrolled off");
        // /clear empties the rendered transcript.
        app.clear_transcript();
        assert_eq!(app.wrapped_transcript(40).len(), 0);
    }

    #[test]
    fn in_loop_viewer_opens_navigates_and_closes() {
        let mut app = App {
            fullscreen: true,
            ..Default::default()
        };
        // No activity → open is a no-op (nothing to view).
        app.open_viewer(0);
        assert!(app.viewer.is_none());

        // Two subagents → activity list is [main, sub, sub] (len 3).
        app.apply(crate::PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "general".into(),
            task: "x".into(),
            model: Some("m".into()),
        });
        app.apply(crate::PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "y".into(),
            model: Some("m".into()),
        });
        app.open_viewer(1);
        assert_eq!(app.viewer.as_ref().unwrap().selected, 1);

        // Right/Left switch entries (wrapping); a modal key is always consumed.
        assert!(app.viewer_key(KeyKind::Right));
        assert_eq!(app.viewer.as_ref().unwrap().selected, 2);
        assert!(app.viewer_key(KeyKind::Right)); // wraps 2 → 0
        assert_eq!(app.viewer.as_ref().unwrap().selected, 0);

        // Up pauses follow; Esc closes.
        app.viewer_key(KeyKind::Up);
        assert!(!app.viewer.as_ref().unwrap().follow);
        app.viewer_key(KeyKind::Esc);
        assert!(app.viewer.is_none());
        // Closed viewer ignores keys (not consumed).
        assert!(!app.viewer_key(KeyKind::Down));
    }

    #[test]
    fn view_snapshot_round_trips_activity_and_viewer() {
        let mut app = App {
            fullscreen: true,
            ..Default::default()
        };
        app.apply(crate::PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "general".into(),
            task: "scan".into(),
            model: Some("opus".into()),
        });
        app.apply(crate::PresenterEvent::SubagentProgress {
            id: "a".into(),
            snippet: "working\n".into(),
        });
        app.open_viewer(1);
        let json = app.view_snapshot_json().expect("activity → snapshot");

        // A fresh app restores the same activity list + open viewer.
        let mut restored = App::default();
        restored.restore_view_json(&json);
        assert_eq!(restored.activity_len(), 2, "main + 1 subagent");
        assert_eq!(restored.viewer.as_ref().unwrap().selected, 1);
        let views = restored.activity_views();
        assert!(views[1]
            .lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains("working"))));

        // A plain session (no activity / viewer / tasks) writes nothing.
        assert!(App::default().view_snapshot_json().is_none());
    }
}
