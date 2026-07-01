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

// ── Palette ──────────────────────────────────────────────────────────────────
// Forge identity
const ORANGE: Color = Color::Rgb(255, 138, 48); // forge brand — warm ember
                                                // Electric blue primary accent (active states, model display, busy indicator)
const ACCENT: Color = Color::Rgb(82, 162, 255); // electric blue
                                                // Text
const USER: Color = Color::Rgb(122, 183, 255); // user message headers
const DIM: Color = Color::Rgb(82, 87, 108); // muted / secondary
const TEXT: Color = Color::Rgb(208, 213, 224); // primary body text
                                               // Semantic
const OKGREEN: Color = Color::Rgb(92, 208, 122); // success / ok
const ERRRED: Color = Color::Rgb(243, 92, 92); // error
const WARNYEL: Color = Color::Rgb(238, 188, 82); // warning
const TOOLCYAN: Color = Color::Rgb(75, 212, 218); // tools / lattice
                                                  // Surfaces
const SELECT_BG: Color = Color::Rgb(40, 70, 132); // mouse text-selection
const STATUSBG: Color = Color::Rgb(14, 15, 21); // deep status-bar bg
const SEPCOL: Color = Color::Rgb(38, 42, 62); // status-bar separator tint

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
    /// Index into `candidates` of the row ↑/↓ highlights (browsing, independent of `selected` —
    /// the routed pick — which never moves). Clamped to `candidates.len() - 1` at render time.
    /// The viewport scroll offset needed to keep this visible is derived at render time (render
    /// takes `&App`, not `&mut App`, so it can't persist a scroll field across frames itself).
    pub cursor: usize,
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
    /// While `Some`, the mesh is failing over between models — drives the animated "finding a
    /// model" status indicator. Set on a `ModelSearch` event, cleared the moment real output
    /// (assistant text / a tool call) arrives, so it shows only during the search, never after.
    pub model_search: Option<String>,
    pub cost_usd: f64,
    /// Live token counter (tui-token-counter.md): session totals + current context fill.
    pub session_in: u64,
    pub session_out: u64,
    pub context_tokens: u64,
    pub context_limit: Option<u32>,
    /// Wall-clock seconds the current turn has been running; updated live by the I/O shell each
    /// frame while busy and left frozen at the final value once the turn ends (reset on the next
    /// turn). Drives the `⧖ Ns` turn timer in the statusline.
    pub turn_elapsed_secs: u64,
    /// Input/output tokens attributed to the current/last turn (session totals minus the snapshot
    /// taken at turn start). Shown alongside the timer; the session totals stay on their own.
    pub turn_in: u64,
    pub turn_out: u64,
    /// Session in/out totals captured at turn start, so `turn_in/out` are deltas from here.
    turn_base_in: u64,
    turn_base_out: u64,
    /// True once at least one turn has started this session — so the turn timer/token segment shows
    /// the last turn's frozen stats even for a sub-second turn (where `turn_elapsed_secs` is 0).
    turn_ran: bool,
    /// Set while compaction is running, driving the animated progress band. `None` otherwise.
    pub compaction: Option<CompactionState>,
    pub done: bool,
    /// Why the last turn ended; `None` before any turn completes or for lifecycle-only Done events.
    pub last_stop_reason: Option<forge_types::StopReason>,
    /// The active operating temper label (e.g. "Guarded"), shown in the statusline.
    pub temper: String,
    /// The active reasoning-effort pin, when set by config or `/effort`.
    pub effort: Option<forge_types::EffortLevel>,
    /// True while the effort slider popup is visible above the input bar (Ctrl+R).
    pub effort_slider: bool,
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
    /// Candidates for the `/copy` block picker (`PickerKind::CopyBlocks`) as `(lang, text)`: index 0
    /// is the full response (lang `""`), 1.. are the fenced code blocks (lang = the fence info). A
    /// picker row's `id` is the index here; Enter copies the text to the clipboard, `w` writes it to
    /// a file (extension derived from `lang`) — both handled in the render loop.
    pub copy_candidates: Vec<(String, String)>,
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
    /// The dynamic `/config` settings editor overlay (modal while `open`).
    pub config_editor: crate::config_editor::ConfigEditor,
    /// The `/usage` overlay state.
    pub usage_overlay: UsageOverlay,
    /// The `/mesh` routing-inspector overlay state.
    pub mesh_overlay: MeshOverlay,
    /// True while remote control is active (a browser can drive the session via `/remote`). Shown
    /// as a `◉ remote` segment in the statusline so it's visible at a glance that the session is
    /// remotely controllable.
    pub remote_active: bool,
    /// Cached git branch name (set at startup, not polled). Shown by the `GitBranch` widget.
    pub git_branch: Option<String>,
    /// Cached project/repo directory name (set at startup, not polled). Shown by `RepoName`.
    pub repo_name: Option<String>,
    /// Number of connected MCP servers (from the last `McpStatus` event). Shown by `McpStatus` widget.
    pub mcp_count: usize,
    /// Statusline layout loaded from config at startup. Drives `render_statusline`.
    pub statusline_config: forge_config::StatuslineConfig,
    /// Latest stdout from each shell-backed `Custom` widget, keyed by its `shell` command string
    /// (the command IS the id — two widgets configured with the same command share a cache entry,
    /// which is harmless). Populated by a periodic background refresh in the render loop, never
    /// blocking it: rendering only ever reads this cache.
    pub custom_widget_cache: std::collections::HashMap<String, String>,
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
    /// Monotonic revision of [`main_log`], bumped on every append/clear. Keys the wrap cache so a
    /// fold that *replaces* lines at the [`MAIN_LOG_MAX`] cap (len unchanged) still invalidates it.
    main_log_rev: u64,
    /// Memoized wrap of [`main_log`] for the full-screen transcript. Re-wrapping the whole log
    /// char-by-char every frame is O(transcript) and was the full-screen input/scroll lag on long
    /// conversations; this caches it so a streaming frame only re-wraps the one in-flight edge line.
    wrap_cache: std::cell::RefCell<WrapCache>,
    /// Screen geometry of the transcript area (col0, row0, width, height) captured each render, plus
    /// the scroll offset used — so a mouse event can be mapped to a wrapped-row/col in the log. `Cell`
    /// because it's written from the (immutable-`&self`) render path.
    transcript_geom: std::cell::Cell<Option<TranscriptGeom>>,
    /// Screen geometry of the floating "jump to bottom" bar (row, col0, width) when shown, for
    /// click hit-testing. `None` when at the bottom (bar hidden).
    jump_bar_geom: std::cell::Cell<Option<(u16, u16, u16)>>,
    /// Full-screen viewer scroll geometry `(wrapped_len, body_h)` of the selected entry, written by
    /// the (immutable-`&self`) render path so `viewer_key` can re-enable follow when the user scrolls
    /// back to the bottom (it has no other way to know the wrapped length at keypress time).
    viewer_geom: std::cell::Cell<Option<(usize, u16)>>,
    /// Active text selection in transcript coords: (wrapped_row, col) anchor + cursor. `None` when
    /// nothing is selected. Highlighted in the transcript and copied to the clipboard on release.
    selection: Option<(TextPos, TextPos)>,
    /// Memoized markdown render of the in-flight streaming reply, keyed on `(len, width)`. The live
    /// preview renders the partial reply as markdown (so it matches the finalized block instead of a
    /// raw unwrapped blob); re-parsing every frame would be O(reply) lag, so it's only rebuilt when
    /// new tokens arrive or the width changes.
    stream_cache: std::cell::RefCell<StreamCache>,
    /// Configurable keybinds loaded from config at startup. Synced into `Tui.keybinds` for
    /// `poll_event` matching; modified by the keybind configurator overlay.
    pub keybinds: forge_config::KeybindsConfig,
    /// Last terminal width seen at render time, so scrollback-line builders (in `apply`, which has
    /// no `Rect`) can size width-aware truncation. `Cell` because it's written from `&self` render.
    last_width: std::cell::Cell<u16>,
}

/// A position in the wrapped transcript: `row` is the wrapped-row index in the cache, `col` the
/// 0-based column within that row.
type TextPos = (usize, u16);

/// Transcript area geometry captured at render time, used to map mouse cells → transcript text.
#[derive(Debug, Clone, Copy)]
struct TranscriptGeom {
    col0: u16,
    row0: u16,
    width: u16,
    height: u16,
    /// The scroll offset (top wrapped-row index) the last frame rendered with.
    scroll: usize,
}

/// Cached result of wrapping [`App::main_log`] to a width. Valid while `(width, rev)` are unchanged.
#[derive(Debug, Clone, Default)]
struct WrapCache {
    width: u16,
    rev: u64,
    rows: Vec<TextLine<'static>>,
}

/// Cached markdown render of the streaming reply edge. Valid while `(len, width)` are unchanged;
/// `len` is the byte length of [`App::streaming`], which only grows as tokens arrive.
#[derive(Debug, Clone, Default)]
struct StreamCache {
    len: usize,
    width: u16,
    rows: Vec<TextLine<'static>>,
}

/// How many recent scrollback lines the remote snapshot keeps (a phone screen shows ~6–8).
const REMOTE_TRANSCRIPT_MAX: usize = 12;

impl App {
    /// Build a [`remote::Snapshot`]-shaped view of the live state, for the remote-control WS to
    /// broadcast. Plain fields only (no ratatui types), so `forge-tui` needn't depend on the
    /// remote module — the caller maps this into the snapshot type.
    pub fn remote_snapshot(&self) -> RemoteSnapshot {
        let (question_options, question_allow_other) = match &self.question {
            Some((opts, allow_other)) => (opts.clone(), *allow_other),
            None => (Vec::new(), false),
        };
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
            tasks: self.tasks.clone(),
            // `id`/`log` are left empty — the remote wire type (`remote::SnapSubagent`) never
            // reads either, so cloning the real (unbounded-growing) log buffer here on every
            // dirty/busy frame would be pure waste. `ViewSnapshot` (the OTHER consumer of this
            // same `SubagentSnapshot` type, for session-resume persistence) still gets the real
            // values via its own construction path below.
            subagents: self
                .subagents
                .iter()
                .map(|r| SubagentSnapshot {
                    id: String::new(),
                    agent: r.agent.clone(),
                    task: r.task.clone(),
                    model: r.model.clone(),
                    phase: r.phase.clone(),
                    last: r.last.clone(),
                    log: Vec::new(),
                    done: r.done,
                    cost: r.cost,
                })
                .collect(),
            queued: self.queued.clone(),
            permission_prompt: self.prompt.clone(),
            question: self.question_prompt.clone(),
            question_options,
            question_allow_other,
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
    pub tasks: Vec<forge_types::TodoItem>,
    pub subagents: Vec<SubagentSnapshot>,
    pub queued: Vec<String>,
    pub permission_prompt: Option<String>,
    pub question: Option<String>,
    pub question_options: Vec<crate::QChoice>,
    pub question_allow_other: bool,
}

/// One subagent's live row in the TUI.
#[derive(Debug, Clone)]
struct SubRow {
    id: String,
    agent: String,
    task: String,
    /// The model this child routed to (shown in the activity panel). `None` until known.
    model: Option<String>,
    /// A workflow-script `phase()` label, if any (docs/rfcs/forge-workflow.md) — rows sharing a
    /// phase are grouped together under a header in the activity panel. `None` for a plain
    /// `spawn_agents` batch (unchanged, ungrouped rendering).
    phase: Option<String>,
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
    /// A workflow-script `phase()` label, if any (docs/rfcs/forge-workflow.md) — consecutive
    /// entries sharing a phase are grouped under one header in the activity panel.
    pub phase: Option<String>,
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
    pub phase: Option<String>,
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
    /// Ctrl+End — jump the full-screen transcript to the bottom and resume following (shell-handled).
    JumpBottom,
    /// Ctrl+R — toggle the effort-level slider popup above the input bar.
    ToggleEffortSlider,
    /// Mid-turn: abort + retry with next mesh model.
    SkipModel,
    /// Mid-turn or idle: escalate task to next tier (trivial→standard→complex).
    TierUp,
    /// Mid-turn or idle: de-escalate task to previous tier.
    TierDown,
    /// Toggle display of model reasoning/thinking blocks.
    ToggleReasoning,
    /// Open the keybind configurator overlay.
    OpenKeybindConfig,
    /// Open the model picker overlay.
    ModelPicker,
    /// Cycle effort level (low→medium→high→max).
    EffortCycle,
    /// Cycle operating temper/mode.
    TemperCycle,
    /// Copy last assistant response to clipboard.
    CopyLast,
    /// Show keybind help overlay (F1).
    ShowHelp,
    /// Save session checkpoint.
    SaveCheckpoint,
    /// Start a fresh session.
    NewSession,
    /// Undo last file write.
    UndoWrite,
    /// Compact/summarize the conversation.
    CompactSession,
    /// Hot-reload config without restarting.
    ReloadConfig,
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
        | KeyKind::ToggleEffortSlider
        | KeyKind::PageUp
        | KeyKind::PageDown
        | KeyKind::JumpBottom
        | KeyKind::SkipModel
        | KeyKind::TierUp
        | KeyKind::TierDown
        | KeyKind::ToggleReasoning
        | KeyKind::OpenKeybindConfig
        | KeyKind::ModelPicker
        | KeyKind::EffortCycle
        | KeyKind::TemperCycle
        | KeyKind::CopyLast
        | KeyKind::ShowHelp
        | KeyKind::SaveCheckpoint
        | KeyKind::NewSession
        | KeyKind::UndoWrite
        | KeyKind::CompactSession
        | KeyKind::ReloadConfig => InputOutcome::Editing,
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
            // Failover in progress: keep a single animated indicator instead of one warning per
            // hop. The model that just failed is recorded only for the (dim) hint; the status bar's
            // own routing line shows the model now being tried.
            PresenterEvent::ModelSearch { model } => {
                self.model_search = Some(model);
            }
            // A complete (non-streamed) assistant message: render markdown into scrollback.
            PresenterEvent::AssistantText(text) => {
                self.model_search = None;
                self.flush.push(header_line("⚒ forge", ORANGE));
                self.flush.extend(crate::render::markdown_to_lines(&text));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Reasoning(delta) => {
                self.model_search = None;
                self.reasoning.push_str(&delta)
            }
            PresenterEvent::AssistantDelta(delta) => {
                self.model_search = None;
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
            PresenterEvent::Error(msg) => self.flush.push(error_line(&msg)),
            PresenterEvent::ToolStart { name, args } => {
                self.model_search = None;
                self.flush
                    .push(tool_start_line(&name, &args, self.last_width.get()))
            }
            PresenterEvent::ToolResult { name, ok, summary } => {
                self.flush
                    .push(tool_result_line(&name, ok, &summary, self.last_width.get()))
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
                self.flush.extend(shell_diagnosis_lines(
                    &command,
                    &diagnosis,
                    fix.as_deref(),
                    self.last_width.get(),
                ));
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
                // Per-turn token deltas from the baseline snapshotted in `on_turn_start`.
                self.turn_in = session_in.saturating_sub(self.turn_base_in);
                self.turn_out = session_out.saturating_sub(self.turn_base_out);
            }
            PresenterEvent::SubagentStart {
                id,
                agent,
                task,
                model,
                phase,
            } => {
                // A new batch begins once every existing row has already finished (or there are
                // none yet) — open a fresh scrollback header for it. Rows themselves are NOT
                // cleared here: they're retained across batches within the same turn (so e.g. a
                // multi-phase workflow's earlier phases stay visible/viewable alongside a later
                // one) and are only reset by `on_turn_start` at the start of a genuinely new user
                // turn. (Previously this branch also cleared `self.subagents`, which wiped a
                // finished batch the instant the NEXT batch's first child started — invisible for
                // a single spawn_agents call, but silently dropped every earlier batch/phase the
                // moment a second one began within the same turn.)
                let starting_new_batch =
                    self.subagents.is_empty() || self.subagents.iter().all(|r| r.done);
                if starting_new_batch {
                    self.flush.push(subagent_header_line());
                }
                self.subagents.push(SubRow {
                    id,
                    agent,
                    task,
                    model,
                    phase,
                    last: String::new(),
                    log: Vec::new(),
                    done: false,
                    cost: 0.0,
                });
                // Bound total retention: drop the oldest already-finished rows first (never an
                // in-progress one) so a turn with many batches/phases can't grow this unboundedly.
                const MAX_SUBAGENT_ROWS: usize = 500;
                if self.subagents.len() > MAX_SUBAGENT_ROWS {
                    let mut excess = self.subagents.len() - MAX_SUBAGENT_ROWS;
                    self.subagents.retain(|r| {
                        if excess > 0 && r.done {
                            excess -= 1;
                            false
                        } else {
                            true
                        }
                    });
                }
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
                self.flush.push(subagent_branch_line(
                    &agent,
                    ok,
                    cost_usd,
                    &summary,
                    self.last_width.get(),
                ));
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
                self.mcp_count = servers.len();
                self.flush.extend(crate::render::mcp_status_lines(&servers));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Recap { text } => {
                self.flush.push(TextLine::from(vec![
                    Span::styled("  ※ recap  ", Style::default().fg(ACCENT).bold()),
                    Span::styled(text, Style::default().fg(TEXT)),
                ]));
                self.flush.push(TextLine::default());
            }
            PresenterEvent::Done { stop_reason, .. } => {
                self.model_search = None;
                self.done = true;
                self.last_stop_reason = Some(stop_reason);
            }
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
            PresenterEvent::CustomWidgetOutput { id, text } => {
                self.custom_widget_cache.insert(id, text);
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
            PresenterEvent::PlanProposed(plan) => {
                self.flush.extend(plan_card_lines(&plan));
            }
            PresenterEvent::Temper(label) => self.temper = label,
            PresenterEvent::Effort(effort) => self.effort = effort,
        }
    }

    /// The live task list (`update_tasks`), for the sticky tasks panel. Empty → panel hidden.
    pub fn tasks(&self) -> &[forge_types::TodoItem] {
        &self.tasks
    }

    /// Set the git branch label (call once at startup).
    pub fn set_git_branch(&mut self, branch: Option<String>) {
        self.git_branch = branch;
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
    /// [`ACTIVITY_PANEL_MAX`] entry rows + an overflow line + one line per workflow-script
    /// `phase()` transition among those rows (see `needs_phase_header`) — a plain `spawn_agents`
    /// batch has no phases, so this term is always 0 there, matching the old reservation exactly.
    pub fn activity_panel_height(&self) -> u16 {
        let n = self.activity_len();
        if n == 0 {
            return 0;
        }
        let shown = n.min(ACTIVITY_PANEL_MAX);
        let overflow = u16::from(n > ACTIVITY_PANEL_MAX);
        let views = self.activity_summaries();
        let mut last_phase: Option<&str> = None;
        let mut headers = 0u16;
        for v in views.iter().take(shown) {
            if needs_phase_header(last_phase, v) {
                headers += 1;
            }
            last_phase = v.phase.as_deref();
        }
        1 + shown as u16 + overflow + headers
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
        // Reset the per-turn timer + token counters and snapshot the session-token baseline so this
        // turn's in/out are measured as a delta from here.
        self.turn_elapsed_secs = 0;
        self.turn_in = 0;
        self.turn_out = 0;
        self.turn_base_in = self.session_in;
        self.turn_base_out = self.session_out;
        self.turn_ran = true;
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
            phase: None,
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
                phase: r.phase.clone(),
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
                phase: None,
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
                            Style::default().fg(TOOLCYAN)
                        } else {
                            Style::default().fg(TEXT)
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

    /// Toggle the effort slider popup.
    pub fn toggle_effort_slider(&mut self) {
        self.effort_slider = !self.effort_slider;
    }

    /// Move the effort slider one step left (lower).
    pub fn effort_slider_left(&mut self) {
        use forge_types::EffortLevel::*;
        let levels = [Low, Medium, High, XHigh];
        let cur = self.effort.unwrap_or(Medium);
        let i = levels.iter().position(|&l| l == cur).unwrap_or(1);
        if i > 0 {
            self.effort = Some(levels[i - 1]);
        }
    }

    /// Move the effort slider one step right (higher).
    pub fn effort_slider_right(&mut self) {
        use forge_types::EffortLevel::*;
        let levels = [Low, Medium, High, XHigh];
        let cur = self.effort.unwrap_or(Medium);
        let i = levels.iter().position(|&l| l == cur).unwrap_or(1);
        if i + 1 < levels.len() {
            self.effort = Some(levels[i + 1]);
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
    /// When `show_thinking` is false the block collapses to a single dim discoverability marker.
    fn flush_reasoning(&mut self) {
        if self.reasoning.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.reasoning);
        if !self.show_thinking {
            // Collapsed-by-default: a one-line marker so the user knows reasoning happened and is
            // toggleable, instead of silently discarding it (undiscoverable).
            self.flush.push(TextLine::from(Span::styled(
                "  💭 thinking… (/thinking to expand)",
                Style::default().fg(DIM),
            )));
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
                    self.flush
                        .push(tool_start_line(name, args, self.last_width.get()));
                }
                ReplayItem::ToolResult { name, ok, summary } => {
                    self.flush
                        .push(tool_result_line(name, *ok, summary, self.last_width.get()));
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
                    phase: r.phase.clone(),
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
                phase: r.phase,
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
        // Read render-recorded scroll geometry BEFORE the mutable borrow of `viewer` (Cell::get
        // needs `&self`). Lets a downward scroll that reaches the tail re-arm follow.
        let geom = self.viewer_geom.get();
        let Some(v) = self.viewer.as_mut() else {
            return false;
        };
        match key {
            KeyKind::Esc => self.viewer = None,
            KeyKind::Up => {
                v.follow = false;
                v.scroll = v.scroll.saturating_sub(1);
            }
            KeyKind::Down => {
                v.scroll = v.scroll.saturating_add(1);
                Self::viewer_refollow_at_tail(v, geom);
            }
            KeyKind::PageUp => {
                v.follow = false;
                v.scroll = v.scroll.saturating_sub(10);
            }
            KeyKind::PageDown => {
                v.scroll = v.scroll.saturating_add(10);
                Self::viewer_refollow_at_tail(v, geom);
            }
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
            KeyKind::Char('j') | KeyKind::Char(' ') => {
                v.scroll = v.scroll.saturating_add(1);
                Self::viewer_refollow_at_tail(v, geom);
            }
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

    /// When a downward scroll reaches the tail (last full page), clamp and re-arm follow so new
    /// activity auto-tails again — matching the full-screen browser (transcript.rs) and `End`/`G`.
    /// `geom` is `(wrapped_len, body_h)` recorded by the render path; absent → leave follow as-is.
    fn viewer_refollow_at_tail(v: &mut ViewerState, geom: Option<(usize, u16)>) {
        if let Some((wrapped_len, body_h)) = geom {
            let max_scroll = wrapped_len.saturating_sub(body_h as usize);
            if v.scroll >= max_scroll {
                v.scroll = max_scroll;
                v.follow = true;
            }
        }
    }

    /// Clear the full-screen transcript (`/clear`, `/new`): wipe the rendered log, the activity
    /// panel, and the viewer, re-anchoring at the tail. The session/transcript on disk are
    /// untouched — this only resets what's drawn (and what a snapshot would capture).
    pub fn clear_transcript(&mut self) {
        self.main_log.clear();
        self.main_log_rev += 1;
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
        let total = self.transcript_total_rows(width);
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

    /// Refresh the wrap cache for `width` if `main_log` or the width changed. Re-wrapping the whole
    /// log every frame was O(transcript) and showed up as input/scroll lag on long full-screen
    /// conversations; the cache makes a streaming frame re-wrap only the one in-flight edge line.
    fn ensure_wrapped_main(&self, width: u16) {
        let mut c = self.wrap_cache.borrow_mut();
        if c.width != width || c.rev != self.main_log_rev {
            c.rows =
                crate::transcript::wrap_lines(&self.main_log, width.saturating_sub(1) as usize);
            c.width = width;
            c.rev = self.main_log_rev;
        }
    }

    /// Rebuild the memoized markdown render of the in-flight reply if the content or width changed.
    /// Rendering the partial reply as markdown (rather than dumping `self.streaming` as one raw,
    /// unwrapped span) makes the live preview match the finalized block; memoizing on `(len, width)`
    /// keeps it from re-parsing O(reply) every frame.
    fn ensure_stream_cache(&self, width: u16) {
        let mut c = self.stream_cache.borrow_mut();
        if c.len != self.streaming.len() || c.width != width {
            let lines = if self.streaming.is_empty() {
                Vec::new()
            } else {
                crate::render::markdown_to_lines(&self.streaming)
            };
            c.rows = crate::transcript::wrap_lines(&lines, width.saturating_sub(1) as usize);
            c.len = self.streaming.len();
            c.width = width;
        }
    }

    /// The in-flight reply edge, markdown-rendered and wrapped to `width` (empty when not streaming),
    /// with the orange cursor block appended to the last row. The markdown parse is memoized; only
    /// the cheap cursor append + clone happens per frame.
    fn streaming_edge(&self, width: u16) -> Vec<TextLine<'static>> {
        if !self.streaming_active {
            return Vec::new();
        }
        self.ensure_stream_cache(width);
        let mut rows = self.stream_cache.borrow().rows.clone();
        let cursor = Span::styled("▌", Style::default().fg(ACCENT));
        match rows.last_mut() {
            Some(l) => l.spans.push(cursor),
            None => rows.push(TextLine::from(cursor)),
        }
        rows
    }

    /// Wrapped-row count of the streaming edge without cloning it (for scroll math).
    fn streaming_edge_len(&self, width: u16) -> usize {
        if !self.streaming_active {
            return 0;
        }
        self.ensure_stream_cache(width);
        self.stream_cache.borrow().rows.len().max(1)
    }

    /// Total wrapped rows of the full-screen transcript (memoized log + the streaming edge).
    fn transcript_total_rows(&self, width: u16) -> usize {
        self.ensure_wrapped_main(width);
        self.wrap_cache.borrow().rows.len() + self.streaming_edge_len(width)
    }

    /// Map a screen cell to a transcript position (wrapped-row, col) if it's inside the transcript
    /// area, using the geometry captured at the last render.
    fn pointer_to_text(&self, col: u16, row: u16) -> Option<TextPos> {
        let g = self.transcript_geom.get()?;
        if row < g.row0 || row >= g.row0 + g.height || col < g.col0 || col >= g.col0 + g.width {
            return None;
        }
        Some((g.scroll + (row - g.row0) as usize, col - g.col0))
    }

    /// True if a screen cell is on the floating jump-to-bottom bar.
    pub fn jump_bar_hit(&self, col: u16, row: u16) -> bool {
        match self.jump_bar_geom.get() {
            Some((br, c0, w)) => row == br && col >= c0 && col < c0 + w,
            None => false,
        }
    }

    /// Begin a text selection at a screen cell (left-button down in the transcript). Returns false
    /// if the cell isn't inside the transcript area (the caller may treat it as a click elsewhere).
    pub fn selection_begin(&mut self, col: u16, row: u16) -> bool {
        match self.pointer_to_text(col, row) {
            Some(p) => {
                self.selection = Some((p, p));
                true
            }
            None => false,
        }
    }

    /// Extend the active selection to a screen cell (left-button drag), clamped to the area.
    pub fn selection_extend(&mut self, col: u16, row: u16) {
        let Some((anchor, _)) = self.selection else {
            return;
        };
        let Some(g) = self.transcript_geom.get() else {
            return;
        };
        let cr = row.clamp(g.row0, g.row0 + g.height.saturating_sub(1));
        let cc = col.clamp(g.col0, g.col0 + g.width.saturating_sub(1));
        if let Some(p) = self.pointer_to_text(cc, cr) {
            self.selection = Some((anchor, p));
        }
    }

    /// The currently selected text (joined across wrapped rows), or `None` if nothing/empty is
    /// selected. The shell copies this to the clipboard on button release.
    /// Map a CELL column (a screen-x offset, what `pointer_to_text` records) to a CHAR index in
    /// `chars`, accounting for wide glyphs. A mouse column is in terminal CELLS, but a selection
    /// slices a `[char]` — and a CJK ideograph / emoji occupies 2 cells but 1 char, so using the cell
    /// offset as a char index drifts the boundary right by one per wide glyph before it. Walk the
    /// chars summing their display width until the target cell is reached. A `cell` past the end
    /// (e.g. `usize::MAX` for "to end of row") clamps to `chars.len()`.
    fn cell_to_char_index(chars: &[char], cell: usize) -> usize {
        use unicode_width::UnicodeWidthChar;
        let mut cells = 0usize;
        for (i, &c) in chars.iter().enumerate() {
            if cells >= cell {
                return i;
            }
            cells += UnicodeWidthChar::width(c).unwrap_or(1);
        }
        chars.len()
    }

    pub fn selection_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let ((r0, c0), (r1, c1)) = if a <= b { (a, b) } else { (b, a) };
        let width = self.transcript_geom.get()?.width;
        self.ensure_wrapped_main(width);
        self.ensure_stream_cache(width);
        let cache = self.wrap_cache.borrow();
        let stream = self.stream_cache.borrow();
        let committed = cache.rows.len();
        let mut out = String::new();
        for r in r0..=r1 {
            // The rendered transcript is the committed wrap-cache followed by the live streaming
            // edge; a selection can span that boundary, so pull each row from whichever side it
            // falls in. Without the streaming side, a copy ending in the in-flight reply was cut off.
            let line = if r < committed {
                cache.rows.get(r)
            } else if self.streaming_active {
                stream.rows.get(r - committed)
            } else {
                None
            };
            let Some(line) = line else { break };
            let chars: Vec<char> = line.spans.iter().flat_map(|s| s.content.chars()).collect();
            // c0/c1 are CELL columns; convert to char indices so wide glyphs don't drift the bounds.
            let start_cell = if r == r0 { c0 as usize } else { 0 };
            let end_cell = if r == r1 { c1 as usize } else { usize::MAX };
            let start = Self::cell_to_char_index(&chars, start_cell);
            let end = Self::cell_to_char_index(&chars, end_cell).max(start);
            if r > r0 {
                out.push('\n');
            }
            out.extend(&chars[start..end]);
        }
        let trimmed = out.trim_end_matches('\n').to_string();
        (!trimmed.trim().is_empty()).then_some(trimmed)
    }

    /// Clear any active selection (e.g. a fresh click or a new turn).
    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Whether a selection is currently held (for the shell to decide click vs select).
    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    /// Append flushed lines to the bounded main-chat log used by the activity viewer.
    fn fold_main_log(&mut self, lines: &[TextLine<'static>]) {
        if lines.is_empty() {
            return;
        }
        self.main_log.extend(lines.iter().cloned());
        if self.main_log.len() > MAIN_LOG_MAX {
            let drop = self.main_log.len() - MAIN_LOG_MAX;
            self.main_log.drain(0..drop);
        }
        self.main_log_rev += 1;
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

/// Greedy word-wrap to `width` display columns (approximated by char count, matching the rest of
/// this module). Words longer than `width` are hard-split so a single long token can't overflow.
/// Always returns at least one (possibly empty) line.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        // Measure in terminal CELLS (CJK/emoji = 2) so a row never overflows its column width.
        let ww = UnicodeWidthStr::width(word);
        if ww > width {
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            let mut chunk = String::new();
            let mut chunk_w = 0usize;
            for ch in word.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(1);
                if chunk_w + cw > width && chunk_w > 0 {
                    lines.push(std::mem::take(&mut chunk));
                    chunk_w = 0;
                }
                chunk.push(ch);
                chunk_w += cw;
            }
            cur = chunk;
            cur_w = chunk_w;
            continue;
        }
        let add = if cur.is_empty() { ww } else { ww + 1 };
        if cur_w + add > width {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_w = ww;
        } else {
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(word);
            cur_w += ww;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Render a proposed plan (the `present_plan` tool) as a styled card flushed to scrollback: an
/// orange frame + "⬡ PLAN" title, cyan step numbers with a green ❯ marker, dim per-step detail,
/// an optional yellow notes line, and a dim footer hinting the interactive approve prompt that
/// follows. The frame is sized to the widest row (clamped) so it stays tidy at any plan length.
fn plan_card_lines(plan: &forge_types::PlanProposal) -> Vec<TextLine<'static>> {
    let frame = Style::default().fg(ORANGE);
    let title = plan.title.trim().to_string();

    // Each logical row is a fixed prefix (number/marker/indent) plus a wrappable text payload.
    // The payload is word-wrapped to the inner width so long step titles/details stay inside the
    // frame instead of overflowing the right border.
    struct Row {
        prefix: Vec<Span<'static>>,
        prefix_w: usize,
        text: String,
        text_style: Style,
    }
    let mut rows: Vec<Row> = Vec::new();

    let head_tag = "⬡ PLAN  ";
    rows.push(Row {
        prefix: vec![Span::styled(head_tag, Style::default().fg(ORANGE).bold())],
        prefix_w: head_tag.chars().count(),
        text: title.clone(),
        text_style: Style::default().fg(ORANGE).bold(),
    });

    for (i, step) in plan.steps.iter().enumerate() {
        let n = format!("{:>2} ", i + 1);
        rows.push(Row {
            prefix: vec![
                Span::styled(n.clone(), Style::default().fg(TOOLCYAN).bold()),
                Span::styled("❯ ", Style::default().fg(OKGREEN)),
            ],
            prefix_w: n.chars().count() + 2,
            text: step.title.trim().to_string(),
            text_style: Style::default(),
        });
        let d = step.detail.trim();
        if !d.is_empty() {
            rows.push(Row {
                prefix: vec![Span::raw("     ")],
                prefix_w: 5,
                text: d.to_string(),
                text_style: Style::default().fg(DIM),
            });
        }
    }
    if let Some(notes) = plan
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        rows.push(Row {
            prefix: vec![Span::styled("⚠ ", Style::default().fg(WARNYEL))],
            prefix_w: 2,
            text: notes.to_string(),
            text_style: Style::default().fg(WARNYEL),
        });
    }

    let inner = rows
        .iter()
        .map(|r| r.prefix_w + r.text.chars().count())
        .max()
        .unwrap_or(20)
        .clamp(24, 72);
    let rule = |l: &str, r: &str| {
        TextLine::from(Span::styled(
            format!("  {l}{}{r}", "─".repeat(inner + 2)),
            frame,
        ))
    };

    let mut out = vec![rule("╭", "╮")];
    for (idx, row) in rows.iter().enumerate() {
        let avail = inner.saturating_sub(row.prefix_w).max(1);
        for (li, chunk) in wrap_words(&row.text, avail).iter().enumerate() {
            let mut spans = vec![Span::styled("  │ ", frame)];
            if li == 0 {
                spans.extend(row.prefix.iter().cloned());
            } else {
                spans.push(Span::raw(" ".repeat(row.prefix_w)));
            }
            let cw = chunk.chars().count();
            spans.push(Span::styled(chunk.clone(), row.text_style));
            let pad = inner.saturating_sub(row.prefix_w + cw);
            spans.push(Span::styled(format!("{} │", " ".repeat(pad)), frame));
            out.push(TextLine::from(spans));
        }
        if idx == 0 {
            out.push(rule("├", "┤")); // separate the title from the steps
        }
    }
    out.push(rule("╰", "╯"));
    out.push(TextLine::from(Span::styled(
        "    ▸ approve to build · or type your changes to revise",
        Style::default().fg(DIM),
    )));
    out.push(TextLine::default());
    out
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

fn error_line(msg: &str) -> TextLine<'static> {
    TextLine::from(Span::styled(
        format!("  ✖ {msg}"),
        Style::default().fg(ERRRED).bold(),
    ))
}

fn tool_start_line(name: &str, args: &str, width: u16) -> TextLine<'static> {
    let cap = width_cap(width, 6 + name.chars().count(), 48);
    TextLine::from(vec![
        Span::styled("  ↳ ", Style::default().fg(TOOLCYAN)),
        Span::styled(name.to_string(), Style::default().fg(TOOLCYAN).bold()),
        Span::styled(
            format!("  {}", truncate(args, cap)),
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
            Span::styled("    ● ", Style::default().fg(TOOLCYAN)),
            Span::styled(format!("{kind} "), Style::default().fg(DIM)),
            Span::styled(name.clone(), Style::default().fg(ACCENT).bold()),
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
    width: u16,
) -> Vec<TextLine<'static>> {
    let mut lines = vec![TextLine::from(vec![
        Span::styled("  ⚠ shell failed ", Style::default().fg(ERRRED).bold()),
        Span::styled(
            truncate(command, width_cap(width, 18, 56)),
            Style::default().fg(DIM),
        ),
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
fn subagent_branch_line(
    agent: &str,
    ok: bool,
    cost_usd: f64,
    summary: &str,
    width: u16,
) -> TextLine<'static> {
    let (mark, color) = if ok {
        ("✓", OKGREEN)
    } else {
        ("✗", ERRRED)
    };
    let cap = width_cap(width, 18 + agent.chars().count(), 44);
    TextLine::from(vec![
        Span::styled("  ├─ ", Style::default().fg(DIM)),
        Span::styled(format!("{mark} "), Style::default().fg(color)),
        Span::styled(format!("[{agent}] "), Style::default().fg(TOOLCYAN)),
        Span::styled(format!("${cost_usd:.4}  "), Style::default().fg(DIM)),
        Span::styled(truncate(summary, cap), Style::default().fg(DIM)),
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
fn tool_result_line(name: &str, ok: bool, summary: &str, width: u16) -> TextLine<'static> {
    let (mark, color) = if ok {
        ("  ✓ ", OKGREEN)
    } else {
        ("  ✗ ", ERRRED)
    };
    let cap = width_cap(width, 6 + name.chars().count(), 56);
    TextLine::from(vec![
        Span::styled(mark, Style::default().fg(color)),
        Span::styled(format!("{name}  "), Style::default().fg(color)),
        Span::styled(truncate(summary, cap), Style::default().fg(DIM)),
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

/// Print the startup banner directly to stdout using raw ANSI escape codes, bypassing ratatui's
/// `insert_before` / `draw_lines_over_cleared` path.
///
/// ratatui's `draw_lines_over_cleared` uses `old.diff(&new)` where old = `Buffer::empty` (all
/// `Cell::EMPTY` = space + Reset style). The banner logo is 42 chars wide; the remaining columns
/// on each logo row are also `Cell::EMPTY` in the new buffer, so the diff skips them and never
/// writes to those terminal cells. If the terminal had prior content at those positions (e.g.
/// Claude Code hook output from the same session), it bleeds through to the right of the logo.
///
/// By printing before ratatui creates the inline viewport, the terminal handles each `\n`
/// naturally, clearing the rest of each row implicitly.
pub fn print_banner_direct() {
    use std::io::Write;
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(80);
    let mut out = std::io::stdout();
    // ORANGE bold = ESC[1;38;2;255;138;48m  DIM = ESC[38;2;82;87;108m  reset = ESC[0m
    let orange = "\x1b[1;38;2;255;138;48m";
    let dim = "\x1b[38;2;82;87;108m";
    let reset = "\x1b[0m";
    if width < WORDMARK_WIDTH {
        let _ = writeln!(out, "\n{orange}⚒ FORGE{reset}");
        let _ = writeln!(out, "{dim}model-mesh coding agent{reset}\n");
    } else {
        let _ = writeln!(out);
        for row in FORGE_WORDMARK {
            let _ = writeln!(out, "{orange}{row}{reset}");
        }
        let _ = writeln!(out, "\n{dim}{TAGLINE}{reset}\n");
    }
    let _ = out.flush();
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

/// Color a `/model` pin-picker row: "mesh" cycles through accent rainbow; subscription=green,
/// frontier=orange, free=cyan, paid=yellow, benched=dim.
fn model_pin_row_color(row: &crate::commands::PickerRow, tick: usize) -> Color {
    if row.id == "mesh" {
        // Animate through a short palette so it catches the eye.
        const MESH_COLORS: [Color; 4] = [ACCENT, TOOLCYAN, OKGREEN, Color::Rgb(180, 80, 255)];
        return MESH_COLORS[(tick / 8) % MESH_COLORS.len()];
    }
    let s = row.subtitle.to_lowercase();
    if s.contains("subscription") {
        OKGREEN
    } else if s.contains("frontier") {
        ORANGE
    } else if s.contains("free") {
        TOOLCYAN
    } else if s.contains("benched") {
        DIM
    } else {
        WARNYEL // paid
    }
}

/// Flatten to one line, then delegate to the shared [`forge_types::truncate_ellipsis`] so the
/// truncation-length semantics stay in lockstep with the rest of the codebase.
fn truncate(s: &str, max: usize) -> String {
    forge_types::truncate_ellipsis(&s.replace('\n', " "), max)
}

/// Width-aware truncation budget: scales with the terminal width but never drops below `min` (the
/// old fixed cap, so narrow terminals are unchanged). `reserve` accounts for the columns already
/// taken by the line's glyph/label prefix. A width of 0 (pre-first-render) falls back to 80.
fn width_cap(width: u16, reserve: usize, min: usize) -> usize {
    let w = if width == 0 { 80 } else { width as usize };
    w.saturating_sub(reserve).max(min)
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
        // Record the scroll geometry so `viewer_key` can re-enable follow at the tail. `body_h`
        // mirrors `transcript_lines` (2 header + 1 footer rows reserved).
        let wrapped_len = views
            .get(v.selected)
            .map(|view| {
                crate::transcript::wrap_lines(&view.lines, a.width.saturating_sub(1) as usize).len()
            })
            .unwrap_or(0);
        let body_h = a.height.saturating_sub(3).max(1);
        app.viewer_geom.set(Some((wrapped_len, body_h)));
        frame.render_widget(
            Paragraph::new(crate::transcript::transcript_lines(
                &views, v.selected, scroll, a.height, a.width,
            )),
            a,
        );
        return;
    }
    app.viewer_geom.set(None);
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

    // areas[0]: the main region. The slash-command palette and @path picker are *completion popups*
    // — in full-screen they show as a small bottom-anchored list with the transcript still visible
    // above (not the whole screen). The session picker stays a full modal. Otherwise areas[0] is the
    // transcript (full-screen) or the in-flight reply edge (inline).
    const POPUP_MAX: u16 = 10;
    if app.palette.open || app.at_picker.open {
        let (top, popup) = if app.fullscreen && areas[0].height > POPUP_MAX + 1 {
            let popup_h = POPUP_MAX;
            let top = Rect {
                height: areas[0].height - popup_h,
                ..areas[0]
            };
            let popup = Rect {
                y: areas[0].y + areas[0].height - popup_h,
                height: popup_h,
                ..areas[0]
            };
            (Some(top), popup)
        } else {
            (None, areas[0])
        };
        if let Some(top) = top {
            render_transcript_area(frame, top, app);
        }
        if app.palette.open {
            render_palette(frame, popup, app);
        } else {
            render_at_path_picker(frame, popup, app);
        }
    } else if app.picker.open {
        render_picker(frame, areas[0], app);
    } else if app.fullscreen {
        render_transcript_area(frame, areas[0], app);
    } else {
        render_preview(frame, areas[0], app);
    }
    // Effort slider overlays the bottom of areas[0] when open (3 rows, anchored at its bottom).
    if app.effort_slider {
        render_effort_slider(frame, areas[0], app);
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
    crate::config_editor::render_config_overlay(frame, &app.config_editor);
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
                Style::default().fg(ACCENT).bold()
            } else {
                Style::default().fg(USER)
            };
            let mut spans = vec![
                Span::styled(format!("  {marker}/{}", c.name), name_style),
                Span::styled(format!("  {}", c.desc), Style::default().fg(DIM)),
            ];
            // Inline usage/arg hint on the highlighted row so non-obvious args (`/assay`,
            // `/replay`, `/model`, and fixed-enum args like `/effort [low|…]`) are discoverable.
            if selected && !c.usage.is_empty() {
                spans.push(Span::styled(
                    format!("   {}", c.usage),
                    Style::default().fg(TOOLCYAN),
                ));
                // Best-effort enum-value completion candidates for fixed-arg commands.
                let values = crate::commands::arg_values(&c.name);
                if !values.is_empty() {
                    spans.push(Span::styled(
                        format!("   ⇥ {}", values.join(" · ")),
                        Style::default().fg(DIM),
                    ));
                }
            }
            TextLine::from(spans)
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
        Style::default().fg(ACCENT).bold(),
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
    let model_pin = p.kind == Some(crate::commands::PickerKind::ModelPin);
    let tick = app.tick;
    for (i, row) in matches.iter().enumerate().skip(start).take(revealed) {
        let selected = i == p.selected;
        let marker = if selected { "▸ " } else { "  " };
        // Color rows by kind: tempers by posture, models browser by category, model-pin picker
        // by tier with the "mesh" row animated (cycles accent colors).
        let base = if tempers {
            temper_color(&row.title)
        } else if models {
            models_row_color(row)
        } else if model_pin {
            model_pin_row_color(row, tick)
        } else {
            USER
        };
        let title_style = if selected {
            Style::default().fg(base).bold()
        } else {
            Style::default().fg(base)
        };
        // ModelPin: add a tier badge between title and subtitle for at-a-glance scanning.
        let subtitle_str = if model_pin {
            truncate(&row.subtitle, 52)
        } else {
            truncate(&row.subtitle, 44)
        };
        lines.push(TextLine::from(vec![
            Span::styled(format!("  {marker}{}", row.title), title_style),
            Span::styled(format!("  {subtitle_str}"), Style::default().fg(DIM)),
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
    app.last_width.set(area.width);
    let body_h = area.height as usize;
    // Memoized: only re-wraps the bulk log when it changed; the streaming edge is the cheap part.
    app.ensure_wrapped_main(area.width);
    let cache = app.wrap_cache.borrow();
    let edge = app.streaming_edge(area.width);
    let total = cache.rows.len() + edge.len();
    let max_scroll = total.saturating_sub(body_h);
    let scroll = if app.transcript_follow {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    };
    // Clone only the visible window (~body_h rows), not the whole transcript, each frame.
    let mut lines: Vec<TextLine> = Vec::with_capacity(body_h.min(total.saturating_sub(scroll)));
    for i in scroll..(scroll + body_h).min(total) {
        if i < cache.rows.len() {
            lines.push(cache.rows[i].clone());
        } else {
            lines.push(edge[i - cache.rows.len()].clone());
        }
    }
    drop(cache);
    frame.render_widget(Paragraph::new(lines), area);

    // Record geometry so mouse events can map a cell → a wrapped-row/col in the log.
    app.transcript_geom.set(Some(TranscriptGeom {
        col0: area.x,
        row0: area.y,
        width: area.width,
        height: area.height,
        scroll,
    }));

    // Paint the selection highlight directly onto the rendered cells (preserves fg colors).
    if let Some((a, b)) = app.selection {
        let ((r0, c0), (r1, c1)) = if a <= b { (a, b) } else { (b, a) };
        let buf = frame.buffer_mut();
        for r in r0..=r1 {
            if r < scroll || r >= scroll + body_h {
                continue;
            }
            let y = area.y + (r - scroll) as u16;
            let start = if r == r0 { c0 } else { 0 };
            let end = if r == r1 { c1 } else { area.width };
            for c in start..end.min(area.width) {
                if let Some(cell) = buf.cell_mut(ratatui::layout::Position::new(area.x + c, y)) {
                    cell.set_bg(SELECT_BG);
                }
            }
        }
    }

    // Floating "jump to bottom" bar — only while scrolled up off the tail.
    if scroll < max_scroll && area.height > 0 {
        let label = " ↓ Jump to bottom · Ctrl+End ";
        let w = (label.chars().count() as u16).min(area.width);
        let x = area.x + (area.width.saturating_sub(w)) / 2;
        let y = area.y + area.height - 1;
        let bar = Paragraph::new(TextLine::from(Span::styled(
            label,
            Style::default().fg(STATUSBG).bg(ORANGE).bold(),
        )));
        frame.render_widget(bar, Rect::new(x, y, w, 1));
        app.jump_bar_geom.set(Some((y, x, w)));
    } else {
        app.jump_bar_geom.set(None);
    }
}

fn render_preview(frame: &mut Frame, area: Rect, app: &App) {
    app.last_width.set(area.width);
    // Only the in-flight reply edge lives here now; the task + subagent panels are their own
    // always-visible regions (see `render_live`), so streaming no longer hides them.
    if app.streaming_active {
        // Reuse the full-screen streaming edge so the inline reply reflows + markdown-highlights
        // exactly like the transcript, instead of a raw newline-collapsed blob.
        let rows = app.streaming_edge(area.width);
        let body_h = area.height as usize;
        let start = rows.len().saturating_sub(body_h);
        let visible: Vec<TextLine> = rows.into_iter().skip(start).collect();
        frame.render_widget(Paragraph::new(visible), area);
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
/// Whether rendering `v` (given the previous shown row's phase was `prev`) needs a phase-header
/// line first — a workflow-script `phase()` transition (docs/rfcs/forge-workflow.md). Never true
/// for `None` phases, so a plain `spawn_agents` batch (every row's phase is `None`) never groups.
fn needs_phase_header(prev: Option<&str>, v: &ActivitySummary) -> bool {
    matches!(v.phase.as_deref(), Some(p) if Some(p) != prev)
}

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
            format!("  ◈ activity ({})  ", views.len()),
            Style::default().fg(ACCENT).bold(),
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

    // Greedily take rows starting at `start` until `body_h` lines are used, accounting for the
    // extra line a `phase()` transition costs (NOT just a plain view count — a phase header is an
    // additional line the naive "1 view = 1 line" budget doesn't know about). Reserves 1 line for
    // a "+N more" hint unless the view being considered is the very last one overall (then
    // nothing will be hidden, so no reservation is needed).
    let mut end = start.min(views.len());
    let mut used = 0usize;
    let mut last_phase: Option<&str> = None;
    while end < views.len() {
        let v = &views[end];
        let cost = 1 + usize::from(needs_phase_header(last_phase, v));
        let reserve = usize::from(end + 1 < views.len());
        if used + cost + reserve > body_h {
            break;
        }
        used += cost;
        last_phase = v.phase.as_deref();
        end += 1;
    }
    let overflow = end < views.len();

    // Workflow-script `phase()` groups (docs/rfcs/forge-workflow.md): a header line is inserted
    // whenever the phase changes from the previous VISIBLE row — purely additive to `lines`, never
    // counted as its own `i`, so `activity_idx`/Ctrl+O/Enter-to-zoom indexing is untouched. `None`
    // phases (every row in a plain `spawn_agents` batch) never trigger a header, so this is a
    // no-op for anything that isn't a workflow script.
    let mut last_phase: Option<&str> = None;
    for (i, v) in views.iter().enumerate().skip(start).take(end - start) {
        if needs_phase_header(last_phase, v) {
            lines.push(TextLine::from(Span::styled(
                format!("  ▶ {}", v.phase.as_deref().unwrap_or_default()),
                Style::default().fg(WARNYEL).bold(),
            )));
        }
        last_phase = v.phase.as_deref();
        let selected = focused && i == app.activity_idx;
        let marker = if selected { "▸" } else { " " };
        let (kind_glyph, kind_color) = match v.kind {
            ActivityKind::MainChat => ("●", TOOLCYAN),
            ActivityKind::Subagent => ("◈", ACCENT),
            ActivityKind::AssayCritic => ("⚖", WARNYEL),
        };
        let status_span = match v.status {
            ActivityStatus::Running => Span::styled(format!("{spin} "), Style::default().fg(DIM)),
            ActivityStatus::Done => Span::styled("✓ ", Style::default().fg(OKGREEN)),
            ActivityStatus::Skipped => Span::styled("⏭ ", Style::default().fg(DIM)),
        };
        let title_style = if selected {
            Style::default().fg(ACCENT).bold()
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
                Style::default().fg(if selected { ACCENT } else { DIM }),
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
        let hidden = views.len() - end;
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
        "  ◈ {total} tasks ({done_count} done, {in_progress_count} in progress, {open_count} open)"
    );
    let mut lines = vec![TextLine::from(Span::styled(
        header,
        Style::default().fg(ACCENT).bold(),
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
            TodoStatus::InProgress => ("◼", Style::default().fg(ACCENT).bold()),
            TodoStatus::Pending => ("○", Style::default().fg(TEXT)),
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
        let line = TextLine::from(vec![
            Span::styled(
                " ◉ RESPOND ",
                Style::default().fg(STATUSBG).bg(ORANGE).bold(),
            ),
            Span::styled(format!("  {p}  "), Style::default().fg(WARNYEL)),
            Span::styled("[y]es", Style::default().fg(OKGREEN).bold()),
            Span::styled(" / ", Style::default().fg(DIM)),
            Span::styled("[a]lways", Style::default().fg(OKGREEN).bold()),
            Span::styled(" / ", Style::default().fg(DIM)),
            Span::styled("[N]o ", Style::default().fg(ERRRED).bold()),
        ]);
        frame.render_widget(Paragraph::new(line), area);
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
        // Cell WIDTH, not char count — ratatui wraps on terminal columns, so a CJK/emoji glyph
        // (2 cells) counted as 1 char under-counted rows and hid the cursor below the input box.
        let cols = unicode_width::UnicodeWidthStr::width(line) + if i == 0 { 2 } else { 0 }; // prompt on row 0
        rows += cols.saturating_sub(1) / inner + 1; // ≥1 row per logical line
    }
    rows.max(1) as u16
}

/// Dynamic input-box height: grows from [`INPUT_H`] to [`INPUT_MAX_H`] with the wrapped content.
pub fn input_box_height(input: &str, box_width: u16) -> u16 {
    (input_text_rows(input, box_width) + 2).clamp(INPUT_H, INPUT_MAX_H)
}

/// For a multiline input, the cursor position one logical line up (same column, snapped to a UTF-8
/// boundary), or `None` when the cursor is on the first row — in which case the caller recalls
/// prompt history instead of clobbering a multiline draft.
pub fn input_cursor_up(input: &str, cursor: usize) -> Option<usize> {
    let cursor = cursor.min(input.len());
    if cursor == 0 || !input[..cursor].contains('\n') {
        return None;
    }
    let line_start = input[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = cursor - line_start;
    let prev_nl = line_start - 1; // the '\n' that ends the previous line
    let prev_start = input[..prev_nl].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let prev_line = &input[prev_start..prev_nl];
    let mut target = col.min(prev_line.len());
    while target > 0 && !prev_line.is_char_boundary(target) {
        target -= 1;
    }
    Some(prev_start + target)
}

fn render_input(frame: &mut Frame, area: Rect, app: &App) {
    let (border_col, title_text) = if app.busy {
        (ACCENT, " ▸ working… ")
    } else if app.prompt.is_some() {
        (WARNYEL, " ◉ respond ")
    } else {
        (ORANGE, " ✦ message ")
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_col))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            title_text,
            Style::default().fg(border_col).bold(),
        ));

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

    // Ghost placeholder on an empty, idle prompt — advertises the discoverability cues a new user
    // would otherwise never find (`/` commands, `@` files, `?` keybind help).
    if app.input.is_empty() && !app.busy && app.prompt.is_none() {
        if let Some(first) = text_lines.first_mut() {
            first.spans.push(Span::styled(
                "Message…   / commands  ·  @ files  ·  ? keys",
                Style::default().fg(DIM),
            ));
        }
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
                Style::default().fg(ACCENT).bold(),
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

            let tok_span = |s: &str| -> Span<'static> {
                Span::styled(s.to_string(), Style::default().fg(ACCENT).bold())
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
    let target_w = (area.width as f32 * 0.44).ceil() as u16;
    let frac = ((app.usage_overlay.anim_tick as f32) / 8.0).min(1.0);
    let w = ((target_w as f32 * frac).ceil() as u16).max(2);
    let drawer = Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y,
        width: w,
        height: area.height,
    };
    f.render_widget(ratatui::widgets::Clear, drawer);

    let spinner = SPINNER[(app.usage_overlay.anim_tick as usize) % SPINNER.len()];
    let title = if app.usage_overlay.loading {
        format!(" {spinner} usage  loading… ")
    } else {
        " ◈ usage ".to_string()
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(title, Style::default().fg(ACCENT).bold()))
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(drawer);
    f.render_widget(block, drawer);

    if w < 20 {
        return;
    }
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
            let style = if display.starts_with("claude-cli")
                || display.starts_with("codex-cli")
                || display.starts_with("agy-cli")
            {
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
/// cost. At $0 with no bridge, `forge_mesh::catalog::is_free` tells apart a model we KNOW charges
/// nothing (ollama, groq, a `:free` gateway variant, …) from one we simply have no price for
/// (unpriced OpenRouter/OpenCode Zen models, which may still burn real gateway credit) — the
/// former reads "free", the latter "untracked" rather than lying that it's costless.
fn cost_cell(model: &str, cost: f64) -> String {
    let subscription = forge_mesh::catalog::is_subscription(model);
    if subscription {
        "subscription".to_string()
    } else if cost > 0.0 {
        format!("${cost:.5}")
    } else if forge_mesh::catalog::is_free(model, cost, subscription) {
        "free".to_string()
    } else {
        "untracked".to_string()
    }
}

/// A 14-cell colour-coded meter for a fraction, eased by `ease` (animation grow-in).
fn mesh_meter(frac: f64, ease: f32, status: &str) -> Vec<Span<'static>> {
    let shown = (frac as f32 * ease).clamp(0.0, 1.0);
    let filled = (shown * 14.0).round() as usize;
    let col = match status {
        "Exhausted" => ERRRED,
        "Warning" => WARNYEL,
        _ if frac >= 0.6 => WARNYEL,
        _ => OKGREEN,
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
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Text};

    let o = &app.mesh_overlay;
    let area = f.area();
    let target_w = (area.width as f32 * 0.48).ceil() as u16;
    let frac = ((o.anim_tick as f32) / 8.0).min(1.0);
    let w = ((target_w as f32 * frac).ceil() as u16).max(2);
    let drawer = Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y,
        width: w,
        height: area.height,
    };
    f.render_widget(ratatui::widgets::Clear, drawer);

    let settled = o.anim_tick >= o.settle_tick();
    let glyph = if settled {
        "◈"
    } else {
        SPINNER[(o.anim_tick as usize) % SPINNER.len()]
    };
    let title = format!(" {glyph} mesh inspector ");
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(title, Style::default().fg(ACCENT).bold()))
        .border_style(Style::default().fg(ACCENT));
    let inner = block.inner(drawer);
    f.render_widget(block, drawer);

    if w < 20 {
        return;
    }
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
        top.push(Line::from(Span::styled(tier, Style::default().fg(ACCENT))));
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
        let col = if o.conserve_fired { WARNYEL } else { DIM };
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
    let cursor = o.cursor.min(o.candidates.len().saturating_sub(1));
    let mut cursor_line = 0u16;
    let mut rows: Vec<Line> = Vec::new();
    for (i, c) in o.candidates.iter().take(revealed.max(1)).enumerate() {
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
        let mut base = if c.selected {
            Style::default().fg(OKGREEN).add_modifier(Modifier::BOLD)
        } else if !c.usable {
            Style::default().fg(DIM)
        } else {
            Style::default()
        };
        // The browsing cursor (↑/↓) is independent of the routed pick (▶) — reverse video marks
        // whichever row is currently highlighted, on top of whatever color the pick/usability
        // already applied.
        if i == cursor {
            base = base.add_modifier(Modifier::REVERSED);
            cursor_line = rows.len() as u16;
        }
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
                if i == cursor || c.selected {
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
            Style::default().fg(OKGREEN).add_modifier(Modifier::BOLD),
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
    // Auto-scroll to keep the cursor row on-screen: stay at the top until the cursor scrolls past
    // the last visible row, then follow it exactly to the bottom edge. Purely a function of
    // `cursor_line` + the viewport height — no state to persist across frames.
    let body_h = chunks[1].height;
    let max_scroll = (rows.len() as u16).saturating_sub(body_h);
    let scroll = if cursor_line < body_h {
        0
    } else {
        (cursor_line + 1).saturating_sub(body_h)
    }
    .min(max_scroll);
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
                Style::default().fg(ACCENT).bold().bg(STATUSBG),
            ),
            Span::styled(bar, Style::default().fg(ACCENT).bg(STATUSBG)),
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

/// Whether row 2 (turn timer / context gauge / session totals) has anything to show. Shared by
/// `statusline_height` (to size the reserved area) and `render_statusline` (to decide whether to
/// render it) so the two can never disagree about which row extra_rows starts on.
fn statusline_wants_row2(app: &App) -> bool {
    app.context_tokens > 0
        || app.context_limit.is_some()
        || app.session_in > 0
        || app.session_out > 0
        || app.busy
        || app.turn_ran
}

/// Returns 1 when idle (no session data), 2 once context / token data is available, plus one row
/// per `statusline_config.extra_rows` entry (a static, config-driven count — see `extra_rows`'s
/// doc comment). Used by [`render_live`] to allocate the right number of rows for the status area.
pub fn statusline_height(app: &App) -> u16 {
    let base = if statusline_wants_row2(app) { 2 } else { 1 };
    base + app.statusline_config.extra_rows.len() as u16
}

/// Compact wall-clock duration for the turn timer: `Ns` under a minute, `MmSSs` under an hour,
/// `HhMMm` beyond. No leading zeros on the largest unit so it stays short in the statusline.
fn fmt_dur(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn effort_status(effort: forge_types::EffortLevel) -> (&'static str, Style) {
    match effort {
        forge_types::EffortLevel::Low => ("effort low", Style::default().fg(TOOLCYAN).bg(STATUSBG)),
        forge_types::EffortLevel::Medium => (
            "effort medium",
            Style::default().fg(WARNYEL).bold().bg(STATUSBG),
        ),
        forge_types::EffortLevel::High => (
            "▲ effort high",
            Style::default().fg(WARNYEL).bold().bg(STATUSBG),
        ),
        forge_types::EffortLevel::XHigh => (
            "▲▲ effort xhigh",
            Style::default().fg(ERRRED).bold().bg(STATUSBG),
        ),
    }
}

// ── Effort Slider ─────────────────────────────────────────────────────────────

const EFFORT_SLIDER_H: u16 = 3;
const EFFORT_LEVELS: [forge_types::EffortLevel; 4] = [
    forge_types::EffortLevel::Low,
    forge_types::EffortLevel::Medium,
    forge_types::EffortLevel::High,
    forge_types::EffortLevel::XHigh,
];
const EFFORT_LABELS: [&str; 4] = ["LOW", "MEDIUM", "HIGH", "XHIGH"];

/// Sparkle chars that cycle at XHigh stop positions and handle.
const SPARKLES: [char; 6] = ['✦', '✧', '⋆', '✺', '✼', '❋'];

/// 12-color rainbow for XHigh — each track char gets a phase-shifted hue.
const XHIGH_COLORS: [Color; 12] = [
    Color::Rgb(255, 75, 110), // rose
    Color::Rgb(255, 110, 55), // coral
    Color::Rgb(255, 155, 30), // amber
    Color::Rgb(255, 215, 45), // gold
    Color::Rgb(190, 255, 55), // lime
    Color::Rgb(75, 230, 125), // neon-green
    Color::Rgb(35, 215, 215), // teal
    Color::Rgb(82, 162, 255), // electric-blue
    Color::Rgb(110, 95, 255), // indigo
    Color::Rgb(185, 75, 255), // violet
    Color::Rgb(255, 55, 255), // magenta
    Color::Rgb(255, 75, 160), // hot-pink
];

/// Three-phase pulse for HIGH: orange → gold → hot-red.
const HIGH_PULSE: [Color; 3] = [
    Color::Rgb(255, 138, 48),
    Color::Rgb(255, 210, 40),
    Color::Rgb(245, 55, 35),
];

fn slider_idx(app: &App) -> usize {
    let cur = app.effort.unwrap_or(forge_types::EffortLevel::Medium);
    EFFORT_LEVELS.iter().position(|&l| l == cur).unwrap_or(1)
}

fn slider_border_color(idx: usize, tick: usize) -> Color {
    match idx {
        0 => Color::Rgb(55, 60, 88),
        1 => TOOLCYAN,
        2 => HIGH_PULSE[(tick / 5) % HIGH_PULSE.len()],
        _ => XHIGH_COLORS[tick % XHIGH_COLORS.len()],
    }
}

fn slider_fill_color(idx: usize, tick: usize, pos: usize) -> Color {
    match idx {
        0 => Color::Rgb(85, 92, 118),
        1 => TOOLCYAN,
        2 => HIGH_PULSE[(tick / 5) % HIGH_PULSE.len()],
        _ => XHIGH_COLORS[(tick + pos) % XHIGH_COLORS.len()],
    }
}

fn slider_handle_color(idx: usize, tick: usize) -> Color {
    match idx {
        0 => Color::Rgb(175, 180, 208),
        1 => Color::Rgb(115, 242, 248),
        2 => {
            let t = (tick % 12) as f32;
            let pulse = (std::f32::consts::PI * t / 12.0).sin();
            Color::Rgb(255, (120.0 + 110.0 * pulse) as u8, (30.0 * pulse) as u8)
        }
        _ => XHIGH_COLORS[(tick * 3) % XHIGH_COLORS.len()],
    }
}

fn slider_label_style(idx: usize, tick: usize) -> Style {
    match idx {
        0 => Style::default().fg(DIM),
        1 => Style::default().fg(Color::Rgb(115, 242, 248)).bold(),
        2 => Style::default()
            .fg(HIGH_PULSE[(tick / 5) % HIGH_PULSE.len()])
            .bold(),
        _ => Style::default()
            .fg(XHIGH_COLORS[(tick * 2) % XHIGH_COLORS.len()])
            .bold(),
    }
}

/// Draw the effort slider popup: 3 rows anchored at the bottom of `area`.
/// Uses ratatui Block for the border — alignment is exact regardless of width.
fn render_effort_slider(frame: &mut Frame, area: Rect, app: &App) {
    if area.height < EFFORT_SLIDER_H || area.width < 24 {
        return;
    }
    let idx = slider_idx(app);
    let tick = app.tick;
    let border_col = slider_border_color(idx, tick);

    let box_area = Rect {
        x: area.x,
        y: area.y + area.height - EFFORT_SLIDER_H,
        width: area.width,
        height: EFFORT_SLIDER_H,
    };

    let title_text = if idx == 3 {
        let sp = SPARKLES[(tick / 2) % SPARKLES.len()];
        format!(" {sp} effort {sp} ")
    } else {
        " ⚡ effort ".to_string()
    };

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_col))
        .title(Span::styled(
            title_text,
            Style::default().fg(border_col).bold(),
        ))
        .title_bottom(Span::styled(
            " ←/→ adjust  Esc close ",
            Style::default().fg(DIM),
        ));

    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    // ── Track line in the 1-row inner area ───────────────────────────────────
    let label_text = EFFORT_LABELS[idx];
    let label_len = label_text.chars().count() as u16;
    // " " pad + track + "  " + label + " " pad = inner.width
    let track_w = (inner.width.saturating_sub(1 + 2 + label_len + 1)) as usize;
    if track_w < 4 {
        return;
    }
    let stops: [usize; 4] = std::array::from_fn(|i| i * track_w.saturating_sub(1) / 3);

    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    for pos in 0..track_w {
        let at_stop = stops.iter().position(|&s| s == pos);
        let filled = stops.get(idx).is_some_and(|&s| pos <= s);
        let (ch, style) = match at_stop {
            Some(si) if si == idx => {
                let hcol = slider_handle_color(idx, tick);
                let ch = if idx == 3 {
                    SPARKLES[(tick * 2) % SPARKLES.len()]
                } else {
                    '●'
                };
                (ch, Style::default().fg(hcol).bold())
            }
            Some(si) if si < idx => {
                let fcol = slider_fill_color(idx, tick, pos);
                let ch = if idx == 3 {
                    SPARKLES[(tick + si * 4) % SPARKLES.len()]
                } else {
                    '●'
                };
                (ch, Style::default().fg(fcol).bold())
            }
            Some(_) => ('○', Style::default().fg(DIM)),
            None if filled => ('━', Style::default().fg(slider_fill_color(idx, tick, pos))),
            None => ('─', Style::default().fg(DIM)),
        };
        spans.push(Span::styled(ch.to_string(), style));
    }
    spans.push(Span::raw("  "));
    spans.push(Span::styled(label_text, slider_label_style(idx, tick)));

    frame.render_widget(Paragraph::new(TextLine::from(spans)), inner);
}

/// Render one statusline widget into a span list, or `None` if the widget has no data to show.
fn render_statusline_widget<'a>(
    widget: &forge_config::StatuslineWidget,
    app: &App,
    w: u16,
) -> Option<Vec<Span<'a>>> {
    use forge_config::StatuslineWidget as W;
    match widget {
        W::Model => {
            let model = app
                .routing
                .as_ref()
                .map(|r| r.model.as_str())
                .unwrap_or("—");
            let tier = app.routing.as_ref().map(|r| r.tier.as_str());
            let mut spans: Vec<Span> = Vec::new();
            if app.model_search.is_some() && w >= 40 {
                let f = SPINNER[app.tick % SPINNER.len()];
                spans.push(Span::styled(
                    format!("{f} finding a model"),
                    Style::default().fg(WARNYEL).bg(STATUSBG),
                ));
                spans.push(Span::styled(
                    "  │  ",
                    Style::default().fg(SEPCOL).bg(STATUSBG),
                ));
            } else if app.busy && w >= 40 {
                let f = SPINNER[app.tick % SPINNER.len()];
                spans.push(Span::styled(
                    format!("{f} working"),
                    Style::default().fg(ACCENT).bold().bg(STATUSBG),
                ));
                spans.push(Span::styled(
                    "  │  ",
                    Style::default().fg(SEPCOL).bg(STATUSBG),
                ));
            }
            if let (Some(t), true) = (tier, w >= 52) {
                spans.push(Span::styled(
                    format!("[{t}] "),
                    Style::default().fg(DIM).bg(STATUSBG),
                ));
            }
            spans.push(Span::styled(
                // In-band display uses the short tail (e.g. `claude-opus-4-8`), matching the
                // activity panel + transcript; the full `provider::model` id is only kept where
                // disambiguation matters (cost lookup, routing rationale).
                model_short(Some(model)),
                Style::default().fg(ACCENT).bold().bg(STATUSBG),
            ));
            Some(spans)
        }
        W::Tier => {
            let tier = app.routing.as_ref().map(|r| r.tier.clone())?;
            Some(vec![Span::styled(
                format!("[{tier}]"),
                Style::default().fg(DIM).bg(STATUSBG),
            )])
        }
        W::SessionCost => {
            let model_id = app.routing.as_ref().map(|r| r.model.as_str()).unwrap_or("");
            let text = cost_cell(model_id, app.cost_usd);
            let style = if text.starts_with('$') {
                Style::default().fg(OKGREEN).bold().bg(STATUSBG)
            } else {
                Style::default().fg(DIM).bg(STATUSBG)
            };
            Some(vec![Span::styled(format!("◈ {text}"), style)])
        }
        W::Effort => {
            let effort = app.effort?;
            let (label, style) = effort_status(effort);
            Some(vec![Span::styled(label, style)])
        }
        W::Mode => {
            if app.temper.is_empty() {
                return None;
            }
            if w < 46 {
                return None;
            }
            Some(vec![Span::styled(
                format!("◆ {}", app.temper),
                Style::default()
                    .fg(temper_color(&app.temper))
                    .bold()
                    .bg(STATUSBG),
            )])
        }
        W::TurnElapsed => {
            if !app.busy && !app.turn_ran {
                return None;
            }
            Some(vec![Span::styled(
                format!("⧖ {}", fmt_dur(app.turn_elapsed_secs)),
                Style::default()
                    .fg(if app.busy { ACCENT } else { DIM })
                    .bg(STATUSBG),
            )])
        }
        W::TokensIn => {
            if !app.busy && !app.turn_ran {
                return None;
            }
            if app.turn_in == 0 {
                return None;
            }
            Some(vec![Span::styled(
                format!("↑{}", human(app.turn_in)),
                Style::default()
                    .fg(if app.busy { ACCENT } else { DIM })
                    .bg(STATUSBG),
            )])
        }
        W::TokensOut => {
            if !app.busy && !app.turn_ran {
                return None;
            }
            if app.turn_out == 0 {
                return None;
            }
            Some(vec![Span::styled(
                format!("↓{}", human(app.turn_out)),
                Style::default()
                    .fg(if app.busy { ACCENT } else { DIM })
                    .bg(STATUSBG),
            )])
        }
        W::SessionTokens => {
            if app.session_in == 0 && app.session_out == 0 {
                return None;
            }
            Some(vec![Span::styled(
                format!("Σ ↑{} ↓{}", human(app.session_in), human(app.session_out)),
                Style::default().fg(DIM).bg(STATUSBG),
            )])
        }
        W::GitBranch => {
            let branch = app.git_branch.as_deref()?;
            Some(vec![Span::styled(
                format!("⎇ {branch}"),
                Style::default().fg(DIM).bg(STATUSBG),
            )])
        }
        W::RepoName => {
            let repo = app.repo_name.as_deref()?;
            Some(vec![Span::styled(
                format!("⚑ {repo}"),
                Style::default().fg(DIM).bg(STATUSBG),
            )])
        }
        W::QuotaClaude => {
            let pct = app.usage_overlay.claude_5h_pct?;
            let color = if pct >= 90.0 {
                ERRRED
            } else if pct >= 70.0 {
                WARNYEL
            } else {
                DIM
            };
            Some(vec![Span::styled(
                format!("claude {pct:.0}%"),
                Style::default().fg(color).bg(STATUSBG),
            )])
        }
        W::QuotaCodex => {
            let pct = app.usage_overlay.codex_5h_pct?;
            let color = if pct >= 90.0 {
                ERRRED
            } else if pct >= 70.0 {
                WARNYEL
            } else {
                DIM
            };
            Some(vec![Span::styled(
                format!("codex {pct:.0}%"),
                Style::default().fg(color).bg(STATUSBG),
            )])
        }
        W::McpStatus => {
            if app.mcp_count == 0 {
                return None;
            }
            Some(vec![Span::styled(
                format!("⌬ {} mcp", app.mcp_count),
                Style::default().fg(DIM).bg(STATUSBG),
            )])
        }
        W::Custom {
            text,
            shell: Some(cmd),
            ..
        } => {
            let out = app
                .custom_widget_cache
                .get(cmd)
                .map(String::as_str)
                .unwrap_or(text);
            if out.is_empty() {
                return None;
            }
            Some(vec![Span::styled(
                out.to_string(),
                Style::default().fg(DIM).bg(STATUSBG),
            )])
        }
        W::Custom {
            text, shell: None, ..
        } => {
            if text.is_empty() {
                return None;
            }
            Some(vec![Span::raw(text.clone())])
        }
    }
}

fn render_statusline(frame: &mut Frame, area: Rect, app: &App) {
    let bg = Style::default().bg(STATUSBG);
    let w = area.width;
    let sep = |s: &str| Span::styled(s.to_string(), Style::default().fg(SEPCOL).bg(STATUSBG));
    let widget_sep = || sep(&app.statusline_config.separator);

    // ── Row 1 ─────────────────────────────────────────────────────────────────
    // Build the configurable LEFT segment from the widget list.
    let mut left_spans: Vec<Span> = vec![Span::styled(" ", bg)];
    let mut first_widget = true;

    for widget in &app.statusline_config.left {
        if let Some(spans) = render_statusline_widget(widget, app, w) {
            if !first_widget {
                left_spans.push(widget_sep());
            }
            first_widget = false;
            left_spans.extend(spans);
        }
    }

    // Always-shown burst indicators appended after the configured widgets.
    // These are situational and not worth making configurable.
    if app.remote_active && w >= 52 {
        if !first_widget {
            left_spans.push(widget_sep());
        }
        first_widget = false;
        left_spans.push(Span::styled(
            "◉ remote",
            Style::default().fg(OKGREEN).bold().bg(STATUSBG),
        ));
    }
    if !app.queued.is_empty() {
        if !first_widget {
            left_spans.push(widget_sep());
        }
        first_widget = false;
        left_spans.push(Span::styled(
            format!("⏳ {} queued", app.queued.len()),
            Style::default().fg(WARNYEL).bold().bg(STATUSBG),
        ));
    }
    if app.done && w >= 50 {
        match app.last_stop_reason {
            Some(forge_types::StopReason::MaxSteps) => {
                if !first_widget {
                    left_spans.push(widget_sep());
                }
                first_widget = false;
                left_spans.push(Span::styled(
                    "⚠ step limit — send `continue`",
                    Style::default().fg(WARNYEL).bold().bg(STATUSBG),
                ));
            }
            Some(forge_types::StopReason::BudgetExhausted) => {
                if !first_widget {
                    left_spans.push(widget_sep());
                }
                first_widget = false;
                left_spans.push(Span::styled(
                    "✕ budget cap",
                    Style::default().fg(ERRRED).bold().bg(STATUSBG),
                ));
            }
            _ => {}
        }
    }
    let _ = first_widget; // suppress unused warning

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
        let right_text = format!("{version}  {hint}");
        let right_len = right_text.chars().count() as u16;
        let cols =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(right_len)]).split(row1);
        frame.render_widget(
            Paragraph::new(TextLine::from(left_spans)).style(bg),
            cols[0],
        );
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
        frame.render_widget(Paragraph::new(TextLine::from(left_spans)).style(bg), row1);
    }

    // ── Row 2 ─────────────────────────────────────────────────────────────────
    // Row 2: token timer, context gauge, session totals — unchanged from the original. Gated on
    // the same signal `statusline_height` used to decide whether to reserve this row at all (NOT
    // on raw `area.height`, which now also grows for unrelated `extra_rows` — conflating the two
    // would make row 2 swallow the row space actually meant for the first extra row whenever the
    // app is otherwise idle).
    let mut next_y = area.y + 1;
    if statusline_wants_row2(app) {
        let row2 = Rect {
            y: next_y,
            height: 1,
            ..area
        };
        next_y += 1;
        let mut line2: Vec<Span> = vec![Span::styled(" ", bg)];
        // Per-turn timer + this-turn token deltas: live (orange) while the turn runs, frozen (dim)
        // once it ends — like the per-response readout in Claude Code / Codex.
        let show_turn = app.busy || app.turn_ran;
        if show_turn {
            // CLI bridge models (agy-cli, codex-cli, claude-cli) don't report API token usage;
            // suppress the ↑/↓ counts when both are zero to avoid showing stale "↑0 ↓0".
            let has_token_data = app.turn_in > 0 || app.turn_out > 0;
            let turn_label = if has_token_data {
                format!(
                    "⧖ {} ↑{} ↓{}",
                    fmt_dur(app.turn_elapsed_secs),
                    human(app.turn_in),
                    human(app.turn_out)
                )
            } else {
                format!("⧖ {}", fmt_dur(app.turn_elapsed_secs))
            };
            line2.push(Span::styled(
                turn_label,
                Style::default()
                    .fg(if app.busy { ACCENT } else { DIM })
                    .bg(STATUSBG),
            ));
        }
        // Context gauge next — it's the most important readout, so it comes before the session
        // totals and survives right-truncation on a narrow terminal.
        if app.context_tokens > 0 || app.context_limit.is_some() {
            if line2.len() > 1 {
                line2.push(sep("  │  "));
            }
            line2.extend(context_gauge_spans(app.context_tokens, app.context_limit));
        }
        // Session running totals last (least critical — the per-turn figures are above): if the row
        // is too narrow this is what gets clipped, not the gauge.
        // Only show when session differs from turn delta: on the first turn turn_base_in=0 so
        // turn_in == session_in — showing both would be identical, useless duplication.
        let session_differs = app.session_in != app.turn_in || app.session_out != app.turn_out;
        if (app.session_in > 0 || app.session_out > 0) && (!show_turn || session_differs) {
            if line2.len() > 1 {
                line2.push(sep("  │  "));
            }
            line2.push(Span::styled(
                format!("Σ ↑{} ↓{}", human(app.session_in), human(app.session_out)),
                Style::default().fg(DIM).bg(STATUSBG),
            ));
        }
        frame.render_widget(Paragraph::new(TextLine::from(line2)).style(bg), row2);
    }

    // ── Extra rows (user-configured) ────────────────────────────────────────────
    // Each `extra_rows` entry is one more left-aligned row below row 1 (and row 2, if shown),
    // using the same widget rendering + separator as row 1. `statusline_height` already reserved
    // the space; `next_y` picks up wherever row 2 left off (or right after row 1 if row 2 didn't
    // render this frame).
    for row_widgets in &app.statusline_config.extra_rows {
        let y = next_y;
        if y >= area.y + area.height {
            break;
        }
        next_y += 1;
        let mut spans: Vec<Span> = vec![Span::styled(" ", bg)];
        let mut first = true;
        for widget in row_widgets {
            if let Some(w_spans) = render_statusline_widget(widget, app, w) {
                if !first {
                    spans.push(widget_sep());
                }
                first = false;
                spans.extend(w_spans);
            }
        }
        let row = Rect {
            y,
            height: 1,
            ..area
        };
        frame.render_widget(Paragraph::new(TextLine::from(spans)).style(bg), row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn model_short_strips_provider_prefix() {
        assert_eq!(
            model_short(Some("anthropic::claude-opus-4-8")),
            "claude-opus-4-8"
        );
        assert_eq!(
            model_short(Some("groq::llama-3.1-8b-instant")),
            "llama-3.1-8b-instant"
        );
        // No prefix → unchanged; empty/None → placeholder.
        assert_eq!(model_short(Some("opus")), "opus");
        assert_eq!(model_short(Some("")), "…");
        assert_eq!(model_short(None), "…");
    }

    #[test]
    fn input_cursor_up_recalls_on_first_row_moves_otherwise() {
        // Single-line / first-row cursor → None (caller recalls history).
        assert_eq!(input_cursor_up("hello", 3), None);
        assert_eq!(input_cursor_up("", 0), None);
        // Multiline, cursor on the second line → moves up to the same column on line 1.
        // "abc\nxy|z" : cursor at byte 6 (col 2 on line 2) → line 1 col 2 = byte 2.
        assert_eq!(input_cursor_up("abc\nxyz", 6), Some(2));
        // Column clamped when the previous line is shorter.
        // "ab\nlongline|" cursor at end (col 8) → clamp to line-1 len (2) = byte 2.
        assert_eq!(input_cursor_up("ab\nlongline", 11), Some(2));
    }

    #[test]
    fn width_cap_scales_but_floors_at_min() {
        // Wide terminal scales the budget above the old fixed min.
        assert!(width_cap(200, 6, 48) > 48);
        // Narrow terminal never drops below the min (old fixed cap preserved).
        assert_eq!(width_cap(40, 6, 48), 48);
        // Width 0 (pre-first-render) falls back to 80, not 0.
        assert!(width_cap(0, 6, 48) >= 48);
    }

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
    fn cost_cell_distinguishes_subscription_priced_free_and_untracked() {
        assert_eq!(cost_cell("claude-cli::", 0.0), "subscription");
        assert_eq!(cost_cell("codex-cli::gpt-5.5", 0.0), "subscription");
        assert_eq!(cost_cell("openai::gpt-4o-mini", 0.0123), "$0.01230");
        // Genuinely free: known-$0 provider, positive evidence.
        assert_eq!(cost_cell("ollama::llama3", 0.0), "free");
        assert_eq!(cost_cell("groq::llama-3.1-8b-instant", 0.0), "free");
        assert_eq!(
            cost_cell("openrouter::cohere/north-mini-code:free", 0.0),
            "free"
        );
        // Unpriced gateway/credit model: not a bridge, $0 only because we lack a price.
        assert_eq!(cost_cell("opencode_go::glm-5.2", 0.0), "untracked");
        assert_eq!(
            cost_cell("openrouter::anthropic/claude-opus", 0.0),
            "untracked"
        );
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
    fn error_event_renders_distinct_red_glyph_from_warning() {
        let mut app = App::default();
        app.apply(PresenterEvent::Error("provider hard-fail".into()));
        let lines = app.drain_flush();
        let line = lines.first().expect("error line flushed");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('✖'), "distinct error glyph: {text}");
        assert!(text.contains("provider hard-fail"));
        // Rendered in the error red, not the benign warning yellow.
        assert!(
            line.spans.iter().any(|s| s.style.fg == Some(ERRRED)),
            "error styled red"
        );

        // A Warning still uses the benign yellow ⚠ — the two are visually distinct.
        let mut app2 = App::default();
        app2.apply(PresenterEvent::Warning("heads up".into()));
        let wlines = app2.drain_flush();
        let wtext: String = wlines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(wtext.contains('⚠') && !wtext.contains('✖'));
    }

    #[test]
    fn reasoning_collapses_to_discoverable_marker_when_hidden() {
        let mut app = App::default();
        assert!(!app.show_thinking, "hidden by default");
        app.apply(PresenterEvent::Reasoning("deep thoughts".into()));
        app.apply(PresenterEvent::AssistantDone);
        let text = flush_text(&mut app);
        // Collapsed: a discoverability marker, not the raw reasoning text, not silence.
        assert!(text.contains("thinking"), "collapsed marker shown: {text}");
        assert!(
            text.contains("/thinking"),
            "tells the user how to expand: {text}"
        );
        assert!(
            !text.contains("deep thoughts"),
            "raw reasoning hidden when collapsed"
        );
    }

    #[test]
    fn subagent_batch_animates_live_then_folds_into_a_scrollback_box() {
        let mut app = App::default();
        app.apply(PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "reviewer".into(),
            task: "review the diff".into(),
            model: Some("anthropic::opus".into()),
            phase: None,
        });
        app.apply(PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "find call sites".into(),
            model: Some("groq::llama".into()),
            phase: None,
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
    fn model_search_shows_one_indicator_and_clears_on_output() {
        // Failover must drive a SINGLE animated status indicator, not one scrollback warning per
        // hop. The indicator clears the moment real output begins.
        let mut app = App::default();
        app.apply(PresenterEvent::ModelSearch {
            model: "groq::llama-3.3-70b-versatile".into(),
        });
        assert!(app.model_search.is_some());
        assert!(
            screen(&app).contains("finding a model"),
            "animated search indicator shown in the status bar"
        );
        // The failed model id is NOT spammed into scrollback.
        assert!(
            !flush_text(&mut app).contains("groq::llama-3.3-70b-versatile"),
            "no per-hop failover line flushed to scrollback"
        );
        // Output settles it.
        app.apply(PresenterEvent::AssistantDelta("hello".into()));
        assert!(
            app.model_search.is_none(),
            "indicator cleared once output begins"
        );
        assert!(!screen(&app).contains("finding a model"));
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
        // In-band display uses the short tail (consistent with the panels/transcript), not the
        // full `provider::model` id.
        assert!(
            text.contains("gpt-4o-mini"),
            "short model in statusline: {text:?}"
        );
        assert!(
            !text.contains("openai::gpt-4o-mini"),
            "full id not shown in-band"
        );
        assert!(text.contains("$0.0042"), "cost in statusline");
        assert!(text.contains("standard"), "tier in statusline");
    }

    #[test]
    fn statusline_shows_pinned_effort() {
        let mut app = App::default();
        app.apply(PresenterEvent::Effort(Some(
            forge_types::EffortLevel::XHigh,
        )));
        let text = screen_wh(&app, 100, LIVE_H);
        assert!(text.contains("effort xhigh"), "effort in statusline");

        app.apply(PresenterEvent::Effort(None));
        let text = screen_wh(&app, 100, LIVE_H);
        assert!(!text.contains("effort"), "cleared effort is hidden");
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
    fn statusline_shows_live_turn_timer_and_per_turn_tokens() {
        let mut app = App {
            session_in: 1_000,
            session_out: 200,
            ..Default::default()
        };
        // A turn starts: baseline snapshot taken, the I/O shell ticks the timer, usage reported.
        app.on_turn_start();
        app.busy = true;
        app.turn_elapsed_secs = 73; // 1m13s
        app.apply(PresenterEvent::Cost {
            session_total_usd: 0.02,
            session_in: 2_200, // +1.2k this turn
            session_out: 540,  // +340 this turn
            context_tokens: 0,
            context_limit: None,
        });
        let s = screen_wh(&app, 120, LIVE_H);
        // The spinner says only "working" (no duration); the timer lives once in the ⧖ segment so
        // there's no double clock.
        assert!(s.contains("working"), "spinner shows working: {s}");
        assert!(
            !s.contains("working 1m13s"),
            "no duplicate timer on the spinner: {s}"
        );
        assert!(
            s.contains("⧖ 1m13s"),
            "single turn-timer segment on row 2: {s}"
        );
        assert!(
            s.contains("↑1.2k") && s.contains("↓340"),
            "per-turn token deltas: {s}"
        );
        assert!(
            s.contains("Σ ↑2.2k ↓540"),
            "session totals relabeled Σ: {s}"
        );
    }

    #[test]
    fn turn_timer_does_not_hide_the_compaction_band() {
        // The turn timer/token segment lives in the statusline; the compaction band is its own row.
        // Both must show at once (the regression report was "the timer hides compaction").
        let mut app = App {
            session_in: 100,
            session_out: 50,
            ..Default::default()
        };
        app.on_turn_start();
        app.busy = true;
        app.turn_elapsed_secs = 5;
        app.compaction = Some(CompactionState {
            start_tick: 0,
            auto: false,
        });
        let s = screen_wh(&app, 120, LIVE_H);
        assert!(
            s.contains("compacting"),
            "compaction band still visible: {s}"
        );
        assert!(s.contains("⧖ 5s"), "turn timer also visible: {s}");
    }

    #[test]
    fn fmt_dur_is_compact_across_scales() {
        assert_eq!(fmt_dur(0), "0s");
        assert_eq!(fmt_dur(45), "45s");
        assert_eq!(fmt_dur(73), "1m13s");
        assert_eq!(fmt_dur(3_661), "1h01m");
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
    fn input_text_rows_counts_cell_width_not_chars() {
        // 40 CJK glyphs are 80 terminal cells, so they wrap to more rows than 40 ascii chars (40
        // cells) at the same width — counting `chars()` would tie them and hide the cursor.
        let wide: String = "世".repeat(40);
        let narrow = "x".repeat(40);
        assert!(
            input_text_rows(&wide, 80) > input_text_rows(&narrow, 80),
            "wide glyphs must occupy more wrapped rows than the same count of ascii"
        );
    }

    #[test]
    fn viewer_down_at_tail_reenables_follow() {
        // Bug-hunt 6 (deferred): the inline viewer never re-armed follow on a downward scroll.
        let mut app = App {
            viewer: Some(ViewerState {
                selected: 0,
                scroll: 79,
                follow: false,
            }),
            ..Default::default()
        };
        app.viewer_geom.set(Some((100, 20))); // wrapped_len=100, body_h=20 → max_scroll=80
        app.viewer_key(KeyKind::Down); // 79 → 80 reaches the tail
        let v = app.viewer.as_ref().unwrap();
        assert!(v.follow, "reaching the tail must re-arm follow");
        assert_eq!(v.scroll, 80, "scroll clamps to the tail");
        app.viewer_key(KeyKind::Up);
        assert!(
            !app.viewer.as_ref().unwrap().follow,
            "scrolling up pauses follow again"
        );
    }

    #[test]
    fn selection_spans_committed_and_streaming_rows() {
        // Bug-hunt 6 (deferred): a copy that ran into the live streaming reply was cut at the
        // committed/stream boundary because `selection_text` only read the wrap cache.
        let app = App {
            main_log: vec![TextLine::from("AAA")],
            streaming: "BBB".to_string(),
            streaming_active: true,
            // Select committed row 0 ("AAA") through the whole streaming row 1.
            selection: Some(((0, 0), (1, 100))),
            ..Default::default()
        };
        app.transcript_geom.set(Some(TranscriptGeom {
            col0: 0,
            row0: 0,
            width: 80,
            height: 24,
            scroll: 0,
        }));
        let sel = app
            .selection_text()
            .expect("a selection spanning the boundary yields text");
        assert!(
            sel.contains("AAA") && sel.contains("BBB"),
            "selection must include the streaming tail, got: {sel:?}"
        );
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
    fn plan_card_renders_title_steps_and_frame() {
        let plan = forge_types::PlanProposal {
            title: "Refactor main.rs".into(),
            steps: vec![
                forge_types::PlanStep {
                    title: "Extract clap defs".into(),
                    detail: "into cli/args.rs".into(),
                },
                forge_types::PlanStep {
                    title: "Split dispatch".into(),
                    detail: String::new(),
                },
            ],
            notes: Some("keep the CLI surface identical".into()),
        };
        let text = plan_card_lines(&plan)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("⬡ PLAN"), "has the plan tag: {text}");
        assert!(text.contains("Refactor main.rs"), "has the title");
        assert!(text.contains("Extract clap defs") && text.contains("Split dispatch"));
        assert!(text.contains("into cli/args.rs"), "shows step detail");
        assert!(
            text.contains("keep the CLI surface identical"),
            "shows notes"
        );
        assert!(text.contains('╭') && text.contains('╰'), "has a frame");
        assert!(text.contains("approve to build"), "footer hint present");
    }

    #[test]
    fn streaming_edge_renders_markdown_not_a_raw_blob() {
        let app = App {
            streaming_active: true,
            streaming: "# Heading\n\n- first point\n- second point\n\nA closing paragraph.".into(),
            ..Default::default()
        };
        let rows = app.streaming_edge(80);
        let text: Vec<String> = rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        // The old code dumped the whole reply (newlines and all) as ONE wrapped span. Markdown
        // rendering must split it across rows for the heading, each bullet, and the paragraph.
        assert!(
            rows.len() >= 4,
            "partial markdown split into rows: {text:?}"
        );
        assert!(
            text.iter().any(|l| l.contains("first point"))
                && text.iter().any(|l| l.contains("second point")),
            "bullets on their own rows: {text:?}"
        );
        // The blinking cursor block rides the last row.
        assert!(
            text.last().unwrap().contains('▌'),
            "cursor on last row: {text:?}"
        );
        // streaming_edge_len agrees with the rendered row count (no cursor-only extra row here).
        assert_eq!(app.streaming_edge_len(80), rows.len());
    }

    #[test]
    fn plan_card_wraps_long_text_within_the_frame() {
        let long = "Move every clap struct and enum out of main.rs into cli/args.rs without \
                    changing any field, variant, attribute, or doc comment so the parsed CLI \
                    surface stays byte-for-byte identical across the whole refactor";
        let plan = forge_types::PlanProposal {
            title: "Refactor".into(),
            steps: vec![forge_types::PlanStep {
                title: long.into(),
                detail: long.into(),
            }],
            notes: Some(long.into()),
        };
        let lines: Vec<String> = plan_card_lines(&plan)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        // Every framed row (starts with the left border) must end at the right border and all
        // share the same visible width — i.e. nothing overflowed past the box.
        let framed: Vec<&String> = lines.iter().filter(|l| l.starts_with("  │")).collect();
        assert!(framed.len() > 3, "long text wrapped to multiple rows");
        let w = framed[0].chars().count();
        for l in &framed {
            assert!(l.ends_with('│'), "row stays inside the right border: {l:?}");
            assert_eq!(l.chars().count(), w, "all framed rows equal width: {l:?}");
        }
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
            phase: None,
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
            phase: None,
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
    fn a_second_batch_in_the_same_turn_does_not_wipe_the_first() {
        let mut app = App::default();
        app.apply(PresenterEvent::SubagentStart {
            id: "phase1-a".into(),
            agent: "researcher".into(),
            task: "survey the codebase".into(),
            model: Some("anthropic::opus".into()),
            phase: None,
        });
        app.apply(PresenterEvent::SubagentResult {
            id: "phase1-a".into(),
            agent: "researcher".into(),
            ok: true,
            summary: "found 3 relevant files".into(),
            cost_usd: 0.001,
        });
        assert_eq!(app.subagents.len(), 1, "phase 1's row exists");
        assert!(app.subagents[0].done);

        // A second batch starts IN THE SAME TURN (no `on_turn_start` in between) — e.g. a
        // workflow script's second phase. Before the fix, `SubagentStart` itself cleared every
        // prior finished row here, silently deleting phase 1's row (and its Ctrl+O-viewable
        // transcript) the instant phase 2 began.
        app.apply(PresenterEvent::SubagentStart {
            id: "phase2-a".into(),
            agent: "implementer".into(),
            task: "apply the fix".into(),
            model: Some("anthropic::sonnet".into()),
            phase: None,
        });
        assert_eq!(
            app.subagents.len(),
            2,
            "phase 1's row must survive phase 2 starting"
        );
        assert!(
            app.subagents.iter().any(|r| r.id == "phase1-a"),
            "phase 1's row specifically must still be present"
        );
        let s = screen(&app);
        assert!(
            s.contains("researcher") && s.contains("implementer"),
            "both phases' agents visible together: {s}"
        );

        // Only a genuinely new user turn clears the retained history.
        app.apply(PresenterEvent::SubagentResult {
            id: "phase2-a".into(),
            agent: "implementer".into(),
            ok: true,
            summary: "applied".into(),
            cost_usd: 0.002,
        });
        app.on_turn_start();
        assert!(
            app.subagents.is_empty(),
            "on_turn_start still clears at the real turn boundary"
        );
    }

    #[test]
    fn activity_panel_groups_workflow_phases_with_header_lines() {
        let mut app = App::default();
        app.apply(PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "researcher".into(),
            task: "survey the codebase".into(),
            model: Some("anthropic::opus".into()),
            phase: Some("research".into()),
        });
        app.apply(PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "researcher2".into(),
            task: "survey more".into(),
            model: Some("anthropic::opus".into()),
            phase: Some("research".into()),
        });
        app.apply(PresenterEvent::SubagentStart {
            id: "c".into(),
            agent: "implementer".into(),
            task: "apply the fix".into(),
            model: Some("anthropic::sonnet".into()),
            phase: Some("implement".into()),
        });
        // The default test screen's activity panel is only a few rows tall — too short to fit 3
        // rows + 2 phase headers alongside the input box/statusline. Use a taller one.
        let s = screen_wh(&app, 100, 30);
        assert!(s.contains("▶ research"), "research phase header shown: {s}");
        assert!(
            s.contains("▶ implement"),
            "implement phase header shown: {s}"
        );
        // Exactly one header per phase — the second `research` row must NOT repeat the header.
        assert_eq!(
            s.matches("▶ research").count(),
            1,
            "header shown once per phase, not once per row: {s}"
        );
    }

    #[test]
    fn activity_panel_shows_no_phase_headers_for_a_plain_spawn_agents_batch() {
        let mut app = App::default();
        app.apply(PresenterEvent::SubagentStart {
            id: "a".into(),
            agent: "reviewer".into(),
            task: "review the diff".into(),
            model: Some("anthropic::opus".into()),
            phase: None,
        });
        let s = screen(&app);
        assert!(
            !s.contains('▶'),
            "no phase header glyph for a plain batch: {s}"
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
            cursor: 0,
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
            phase: None,
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

        // A new batch starting in the same turn RETAINS the previous (finished) batch's rows —
        // both stay viewable, e.g. for a multi-phase workflow's earlier phases.
        app.apply(PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "next".into(),
            model: None,
            phase: None,
        });
        let views = app.activity_views();
        assert_eq!(views.len(), 3, "main chat + both batches' subagents");
        assert_eq!(
            views[1].subtitle, "find call sites",
            "batch a's row survives"
        );
        assert_eq!(views[2].subtitle, "next");
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
        assert_eq!(app.transcript_total_rows(40), 0);
    }

    #[test]
    fn wrap_cache_invalidates_on_append_and_at_the_cap() {
        let mut app = App {
            fullscreen: true,
            ..Default::default()
        };
        app.push_scrollback(vec![TextLine::from("one")]);
        assert_eq!(app.transcript_total_rows(40), 1);
        // Appending more must invalidate the memoized wrap (not serve a stale 1-row count).
        app.push_scrollback(vec![TextLine::from("two"), TextLine::from("three")]);
        assert_eq!(app.transcript_total_rows(40), 3);
        // At the MAIN_LOG_MAX cap the len stays constant while lines are replaced — the rev-based
        // key (not len) must still invalidate, so the newest line is reflected.
        for i in 0..MAIN_LOG_MAX + 50 {
            app.push_scrollback(vec![TextLine::from(format!("fill {i}"))]);
        }
        let _ = app.transcript_total_rows(40); // prime the cache at the cap
        app.push_scrollback(vec![TextLine::from("NEWEST")]);
        app.ensure_wrapped_main(40);
        let cache = app.wrap_cache.borrow();
        let last: String = cache
            .rows
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(last.contains("NEWEST"), "cap append reflected: {last:?}");
    }

    #[test]
    fn mouse_selection_extracts_text_within_and_across_wrapped_rows() {
        let mut app = App {
            fullscreen: true,
            ..Default::default()
        };
        app.push_scrollback(vec![TextLine::from("alpha"), TextLine::from("bravo")]);
        app.ensure_wrapped_main(80);
        app.transcript_geom.set(Some(TranscriptGeom {
            col0: 0,
            row0: 0,
            width: 80,
            height: 10,
            scroll: 0,
        }));
        // A cell outside the area is not a selection start.
        assert!(
            !app.selection_begin(0, 50),
            "row below the area → no selection"
        );

        // Single-row: select all of "alpha".
        assert!(app.selection_begin(0, 0));
        app.selection_extend(5, 0);
        assert_eq!(app.selection_text().as_deref(), Some("alpha"));

        // Across two rows: "pha" + newline + "bra".
        assert!(app.selection_begin(2, 0));
        app.selection_extend(3, 1);
        assert_eq!(app.selection_text().as_deref(), Some("pha\nbra"));

        // Clearing drops the selection (no text to copy).
        app.clear_selection();
        assert!(!app.has_selection());
        assert!(app.selection_text().is_none());
    }

    #[test]
    fn jump_bar_hit_tests_only_its_own_row_and_span() {
        let app = App {
            fullscreen: true,
            ..Default::default()
        };
        app.jump_bar_geom.set(Some((20, 10, 8))); // row 20, cols [10, 18)
        assert!(app.jump_bar_hit(10, 20));
        assert!(app.jump_bar_hit(17, 20));
        assert!(!app.jump_bar_hit(18, 20), "past the right edge");
        assert!(!app.jump_bar_hit(12, 19), "wrong row");
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
            phase: None,
        });
        app.apply(crate::PresenterEvent::SubagentStart {
            id: "b".into(),
            agent: "general".into(),
            task: "y".into(),
            model: Some("m".into()),
            phase: None,
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
            phase: None,
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

    #[test]
    fn cell_to_char_index_accounts_for_wide_glyphs() {
        // ASCII: cell offset == char index.
        let ascii: Vec<char> = "hello".chars().collect();
        assert_eq!(App::cell_to_char_index(&ascii, 0), 0);
        assert_eq!(App::cell_to_char_index(&ascii, 3), 3);
        assert_eq!(
            App::cell_to_char_index(&ascii, 99),
            5,
            "past end clamps to len"
        );

        // "日本語x" — each ideograph is 2 cells, so cells run 0,2,4 then 'x' at cell 6.
        let wide: Vec<char> = "日本語x".chars().collect();
        assert_eq!(App::cell_to_char_index(&wide, 0), 0, "日 at cell 0");
        assert_eq!(
            App::cell_to_char_index(&wide, 2),
            1,
            "本 starts at cell 2 → char 1"
        );
        assert_eq!(
            App::cell_to_char_index(&wide, 4),
            2,
            "語 starts at cell 4 → char 2"
        );
        assert_eq!(
            App::cell_to_char_index(&wide, 6),
            3,
            "x at cell 6 → char 3 (the bug: was 6)"
        );
        assert_eq!(
            App::cell_to_char_index(&wide, usize::MAX),
            4,
            "to end → len"
        );

        // The actual selection bug: selecting the trailing 'x' by its on-screen column. Cell 6 must
        // map to char 3, not 6 (which would be out of bounds / drift past the string).
        assert_eq!(&wide[App::cell_to_char_index(&wide, 6)..], &['x']);
    }

    #[test]
    fn wrap_words_measures_wide_glyphs_in_cells() {
        use unicode_width::UnicodeWidthStr;
        // A long unbreakable run of CJK (each 2 cells) must hard-split so no row exceeds the width in
        // CELLS — a char-count wrapper would double-fill each row.
        let rows = super::wrap_words(&"語".repeat(20), 8);
        for r in &rows {
            assert!(
                UnicodeWidthStr::width(r.as_str()) <= 8,
                "row '{r}' is {} cells > 8",
                UnicodeWidthStr::width(r.as_str())
            );
        }
    }

    #[test]
    fn statusline_config_default_shows_model_and_cost() {
        let app = App::default();
        let out = screen(&app);
        // Default config has Model + SessionCost widgets on the left.
        // Model widget shows "—" when no routing; SessionCost shows "untracked" for no-model.
        assert!(
            out.contains('—') || out.contains("untracked"),
            "default statusline renders: {out:?}"
        );
    }

    #[test]
    fn statusline_repo_name_widget_renders_when_set() {
        let mut app = App {
            repo_name: Some("forge".to_string()),
            ..Default::default()
        };
        app.statusline_config
            .left
            .push(forge_config::StatuslineWidget::RepoName);
        let out = screen(&app);
        assert!(out.contains("forge"), "repo name not shown: {out:?}");

        // Absent entirely when unset (widget returns None, doesn't render an empty tag).
        app.repo_name = None;
        let out = screen(&app);
        assert!(
            !out.contains('⚑'),
            "repo glyph shown with no repo name: {out:?}"
        );
    }

    #[test]
    fn statusline_extra_rows_render_below_the_built_in_two() {
        let mut app = App {
            repo_name: Some("myrepo".to_string()),
            ..Default::default()
        };
        app.statusline_config.extra_rows = vec![vec![forge_config::StatuslineWidget::RepoName]];
        let out = screen_wh(&app, 80, LIVE_H);
        assert!(
            out.contains("myrepo"),
            "extra row's widget not rendered: {out:?}"
        );
    }

    #[test]
    fn statusline_custom_shell_widget_shows_fallback_then_cached_output() {
        let mut app = App::default();
        app.statusline_config.left = vec![forge_config::StatuslineWidget::Custom {
            text: "loading…".to_string(),
            shell: Some("git rev-parse --short HEAD".to_string()),
            refresh_secs: 5,
        }];
        let out = screen(&app);
        assert!(out.contains("loading…"), "fallback text not shown: {out:?}");

        app.custom_widget_cache.insert(
            "git rev-parse --short HEAD".to_string(),
            "a1b2c3d".to_string(),
        );
        let out = screen(&app);
        assert!(out.contains("a1b2c3d"), "cached output not shown: {out:?}");
        assert!(
            !out.contains("loading…"),
            "stale fallback still shown once cached: {out:?}"
        );
    }

    #[test]
    fn custom_widget_output_event_populates_cache() {
        let mut app = App::default();
        app.apply(PresenterEvent::CustomWidgetOutput {
            id: "echo hi".to_string(),
            text: "hi".to_string(),
        });
        assert_eq!(
            app.custom_widget_cache.get("echo hi").map(String::as_str),
            Some("hi")
        );
    }
}
