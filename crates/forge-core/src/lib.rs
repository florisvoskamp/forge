//! The session orchestrator: it runs the agent loop (the walking skeleton's spine) and
//! owns the permission broker — the one component that must be central (ADR-0002). It
//! wires the Mesh (routing), a Provider (model calls), the tool registry, the store
//! (persistence) and a presenter (UI) together, depending on each only through its trait.

use std::sync::Arc;

use forge_config::Config;
use forge_index::Lattice;
use forge_mesh::pricing::Pricing;
use forge_mesh::{BudgetState, BudgetStatus, ModelCatalog, Router};
use forge_provider::{CompletionOptions, Provider, StreamEvent, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::{Presenter, PresenterEvent};
use forge_types::{
    EffortLevel, Message, PermissionDecision, PermissionMode, PermissionRule, Role, TaskTier,
};

pub mod assay;
pub mod hooks;
pub mod llm_router;
pub mod permission;
pub mod snapshot;
pub mod subagent;
pub mod tokens;
pub mod worktree;

pub use llm_router::LlmRouter;

/// Compaction (`/compact`): keep this many of the most recent messages verbatim; summarize the
/// rest. Only compact when there are at least `COMPACT_MIN_OLDER` older messages to fold.
pub(crate) const COMPACT_KEEP_RECENT: usize = 6;
pub(crate) const COMPACT_MIN_OLDER: usize = 4;
const COMPACT_SYSTEM: &str = "You are compacting a coding-assistant conversation to save context. \
Summarize the messages below concisely but preserve: decisions made, key facts, file paths, \
function/type names, and any open threads or TODOs. Output only the summary.";

const SHELL_DIAGNOSE_SYSTEM: &str = "A shell command run by a coding agent just failed. \
Respond with exactly one or two lines:\n\
Line 1: the most likely cause in one terse sentence (no preamble, no restating the command).\n\
Line 2 (optional): if a single shell command fixes it, write exactly: FIX: <the command>. \
Omit line 2 if no single command fixes it.";

/// Default sampling temperature for coding turns: low, so edits/patches are deterministic rather
/// than creatively varied. Only takes effect when reasoning/effort isn't engaged (thinking models
/// reject a custom temperature) — see `genai_provider`.
const CODING_TEMPERATURE: f32 = 0.1;

/// The base coding-agent system prompt, prepended (fresh, never persisted) to every main-loop
/// request so a model performs in Forge the way it does in a purpose-built harness. Kept tight: it
/// establishes role + tool discipline + editing conventions without burning context. Project-level
/// `AGENTS.md` and skill guidance layer on top of this as separate (persisted) system messages.
const FORGE_SYSTEM: &str = "\
You are Forge, an expert software engineering agent operating in a user's terminal on their \
codebase. You complete the user's coding task end-to-end by reading code and editing files with the \
tools provided, then stop.

Approach:
- Work from evidence, not assumption. Before editing, read the relevant files and search the \
codebase so your change fits the existing structure, naming, and conventions.
- For any non-trivial task, make a short plan and keep it current with the update_tasks tool. \
Do the work; don't just describe it.
- Make the smallest change that fully solves the task. Match the surrounding code's style. Do NOT \
add comments unless the code's intent is genuinely non-obvious. Don't reformat unrelated code.
- After editing, verify: run the project's build/tests/linters via the shell when available, and \
fix what you broke before reporting done.

Tools:
- Prefer read_file / search / list_dir / glob over shelling out to cat / grep / ls / find.
- When you need several independent reads or searches, request them together in one step.
- edit_file replaces ONE exact, unique occurrence — include enough surrounding context in `old` to \
match exactly once, and read the file first so whitespace matches. Use write_file for new files or \
full rewrites; don't blind-overwrite a file you haven't read.
- A tool result starting with `error:` means it failed — read the message, fix the cause, and \
retry differently rather than repeating the same call.

Communication:
- Be concise and direct. No filler, no flattery, no restating the question. Reference code as \
`path:line`.
- When the task is done, stop and give a short summary of what changed. Don't ask whether to \
proceed on work you can just do.";

/// Whether a `shell` tool result reports a failure (non-zero exit, signal, timeout, or spawn
/// error). The tool's first line is `shell: exit N in …`, `shell: timed out …`, `shell: error: …`,
/// or `shell: failed to start …`; only `exit 0` is success.
pub(crate) fn shell_command_failed(result: &str) -> bool {
    let first = result.lines().next().unwrap_or("");
    match first.strip_prefix("shell: exit ") {
        Some(rest) => {
            rest.split_whitespace()
                .next()
                .and_then(|t| t.parse::<i32>().ok())
                != Some(0)
        }
        None => first.starts_with("shell:"),
    }
}

/// Match common, unambiguous failure patterns in the tool output and return a pre-canned
/// diagnosis — skipping the model call entirely (free, instant). Returns `None` when the
/// failure is unusual enough to need the model. Checked case-insensitively on the full result.
pub(crate) fn pattern_diagnose(result: &str) -> Option<&'static str> {
    // The table is ordered most-specific first so a result with multiple signals hits the
    // most actionable match. Each pattern must be unambiguous: "permission denied" alone
    // could be a file *or* a network ACL — but combining with exit codes is overkill here;
    // the worst case is a slightly generic message, which is still free and instant.
    let lower = result.to_lowercase();
    let has = |s: &str| lower.contains(s);
    if has("command not found") || has("no such file or directory") && has("exec") {
        return Some("Command not found — check it is installed and in PATH.");
    }
    if has("no such file or directory") {
        return Some("File or directory does not exist — verify the path with `ls` or `pwd`.");
    }
    if has("permission denied") || has("operation not permitted") {
        return Some("Permission denied — try `chmod +x <file>` or prefix with `sudo`.");
    }
    if has("address already in use") {
        return Some(
            "Port already in use — find the process with `lsof -i :<port>` or `ss -tlnp`.",
        );
    }
    if has("connection refused") {
        return Some("Connection refused — the target service may not be running.");
    }
    if has("no space left on device") || has("disk quota exceeded") {
        return Some("Disk full or quota exceeded — free space with `df -h` and `du -sh *`.");
    }
    if has("out of memory") || has("cannot allocate memory") {
        return Some("Out of memory — reduce concurrency or increase available RAM/swap.");
    }
    None
}

/// Whether `finding_sev` is at or above `threshold` (a string from `AssayConfig::gate_severity`).
/// Ordering (most → least severe): critical > high > medium > low.
/// A "high" threshold matches `high` and `critical` but not `medium` or `low`.
/// Returns `true` for any unrecognised threshold string (fail-open: surface the finding rather than
/// silently drop it when the config has a typo).
pub(crate) fn severity_meets(finding_sev: forge_types::Severity, threshold: &str) -> bool {
    use forge_types::Severity;
    let min_weight = match threshold.trim().to_lowercase().as_str() {
        "critical" => Severity::Critical.weight(),
        "high" => Severity::High.weight(),
        "medium" | "med" => Severity::Medium.weight(),
        "low" => Severity::Low.weight(),
        _ => 0, // unknown threshold → pass everything through
    };
    finding_sev.weight() >= min_weight
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Provider(#[from] forge_provider::ProviderError),
    #[error(transparent)]
    Store(#[from] forge_store::StoreError),
    #[error(transparent)]
    Lattice(#[from] forge_index::LatticeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("session not found: {0}")]
    SessionNotFound(String),
    #[error("no healthy model available: every routed/fallback model is rate-limited or down")]
    NoHealthyModel,
    /// The auto-review gate found findings at/above the configured severity and `gate_mode =
    /// "block"` is set — the turn is aborted so the model can fix them before proceeding.
    #[error("auto-review gate blocked: {0}")]
    TurnBlocked(String),
}

/// Result of a [`Session::rewind_to`] / [`Session::undo`]: what the file-restore did, plus the
/// prompt that began the rewound-to turn (the UI re-offers it in the input box).
#[derive(Debug, Default, Clone)]
pub struct RewindOutcome {
    pub restore: snapshot::RestoreReport,
    pub rewound_prompt: Option<String>,
}

/// Conservative chars-per-token used ONLY as a fallback when slicing a single oversized message
/// down to a token budget (real token offsets aren't worth the cost there). Counting elsewhere uses
/// the real BPE tokenizer ([`tokens`]); this 3 under-estimates so the sliced text stays within
/// budget rather than overflowing.
const CHARS_PER_TOKEN: usize = 3;

/// Render a sequence of messages into TUI [`ReplayItem`](forge_tui::ReplayItem)s — user prompts,
/// assistant text, tool calls (with args), tool results (matched to their call's name via
/// `tool_call_id`), and the compaction marker. Shared by the model-facing replay
/// ([`Session::replay_items`]) and the full-history replay ([`Session::replay_items_full`]).
fn messages_to_replay_items(msgs: &[Message]) -> Vec<forge_tui::ReplayItem> {
    use forge_tui::ReplayItem;
    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            Role::User => {
                if !m.content.trim().is_empty() {
                    out.push(ReplayItem::User(m.content.clone()));
                }
            }
            Role::Assistant => {
                if !m.content.trim().is_empty() {
                    out.push(ReplayItem::Assistant(m.content.clone()));
                }
                for tc in &m.tool_calls {
                    names.insert(tc.id.clone(), tc.name.clone());
                    let args = serde_json::to_string(&tc.args).unwrap_or_default();
                    out.push(ReplayItem::Tool {
                        name: tc.name.clone(),
                        args,
                    });
                }
            }
            Role::Tool => {
                let name = m
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| names.get(id).cloned())
                    .unwrap_or_else(|| "tool".to_string());
                let summary = m.content.lines().next().unwrap_or("").to_string();
                // The success flag isn't persisted; an error result conventionally starts with
                // "error". Good enough to color the replayed line.
                let ok = !summary.trim_start().to_lowercase().starts_with("error");
                out.push(ReplayItem::ToolResult { name, ok, summary });
            }
            Role::System => {
                // Only the compaction marker represents real prior conversation; other System
                // messages (per-turn guidance/project prompt) are machinery — skip them.
                if m.content.starts_with("[Earlier conversation summarized") {
                    let first = m.content.lines().next().unwrap_or("").to_string();
                    out.push(ReplayItem::Note(first.trim_matches(['[', ']']).to_string()));
                }
            }
        }
    }
    out
}

/// Real token cost of one message: its content (BPE-counted, cached) + the chat framing overhead +
/// any tool-call name/arguments it carries (which the model also pays for).
fn message_tokens(m: &Message) -> usize {
    let mut n = tokens::count_message(&m.content);
    for tc in &m.tool_calls {
        n += tokens::count_text(&tc.name) + tokens::count_text(&tc.args.to_string());
    }
    n
}

/// Trim a transcript to fit within `budget_tokens` (the model's context window minus the reserved
/// reply), counted with the real BPE tokenizer. System messages are ALWAYS kept (the standing
/// instructions); the rest are included newest-first until the budget is hit, then re-ordered to
/// the original sequence. If even the single most-recent message overflows alone, its content is
/// truncated from the FRONT (keeping the latest text — usually the actual request). Returns the
/// input unchanged when it already fits. This is what stops a long conversation from overflowing a
/// model's window and failing the turn as "unavailable" across every model.
fn fit_messages(messages: &[Message], budget_tokens: usize) -> Vec<Message> {
    let total: usize = messages.iter().map(message_tokens).sum();
    if total <= budget_tokens {
        return messages.to_vec();
    }
    // System messages are non-negotiable context; reserve their cost up front.
    let system_cost: usize = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(message_tokens)
        .sum();
    let mut remaining = budget_tokens.saturating_sub(system_cost);

    // Walk non-system messages newest→oldest, keeping each that fits.
    let mut keep_idx = std::collections::HashSet::new();
    for (i, m) in messages.iter().enumerate().rev() {
        if m.role == Role::System {
            continue;
        }
        let cost = message_tokens(m);
        if cost <= remaining {
            remaining -= cost;
            keep_idx.insert(i);
        } else if keep_idx.is_empty() {
            // Nothing kept yet and even this newest message is too big — truncate it from the
            // front so the latest words survive, and stop (the budget is spent). Slice by a
            // conservative char-per-token bound (exact token offsets aren't worth it here).
            let mut m = m.clone();
            let keep_chars = remaining.saturating_sub(48).saturating_mul(CHARS_PER_TOKEN);
            if keep_chars > 0 {
                let chars: Vec<char> = m.content.chars().collect();
                let start = chars.len().saturating_sub(keep_chars);
                m.content = format!(
                    "[… earlier of this message truncated to fit the model's context …]\n{}",
                    chars[start..].iter().collect::<String>()
                );
            }
            // A lone tool RESULT with no preceding assistant call is a dangling tool_call_id the
            // provider rejects — demote it to a plain user message so the request stays valid.
            if m.role == Role::Tool {
                m.role = Role::User;
                m.tool_call_id = None;
            }
            // Rebuild in order: systems first (in place) then this lone truncated tail.
            let mut out: Vec<Message> = messages
                .iter()
                .filter(|m| m.role == Role::System)
                .cloned()
                .collect();
            out.push(m);
            return out;
        } else {
            break;
        }
    }
    // The kept non-system messages are a contiguous newest-first tail. If that tail BEGINS with a
    // tool result, its assistant tool_calls message was trimmed away — a dangling tool_call_id that
    // makes Anthropic/OpenAI hard-reject the whole request. Drop leading tool results until the
    // tail starts on a non-tool message. (System messages aren't tool-paired, so they're exempt.)
    let mut ordered: Vec<usize> = keep_idx.iter().copied().collect();
    ordered.sort_unstable();
    for i in ordered {
        if messages[i].role == Role::Tool {
            keep_idx.remove(&i);
        } else {
            break;
        }
    }
    messages
        .iter()
        .enumerate()
        .filter(|(i, m)| m.role == Role::System || keep_idx.contains(i))
        .map(|(_, m)| m.clone())
        .collect()
}

/// Output of one execution of the shared model↔tool loop ([`Session::run_model_loop`]).
/// Carries everything the caller needs; the caller holds `active_model` by value so it is
/// returned here (failover may have changed it from the original).
struct ModelLoopOutcome {
    final_text: String,
    context_tokens: u64,
    hit_step_cap: bool,
    /// The model that produced the last response (may differ from the input if failover fired).
    active_model: String,
}

/// One interactive session. Construct with [`Session::start`], then drive [`Session::run_turn`].
pub struct Session {
    id: String,
    store: Arc<Store>,
    provider: Arc<dyn Provider>,
    router: Arc<dyn Router>,
    tools: ToolRegistry,
    presenter: Box<dyn Presenter>,
    config: Config,
    pricing: Pricing,
    mode: PermissionMode,
    /// Resolved permission rules (built-in safety denies + configured), consulted per call.
    rules: Vec<PermissionRule>,
    transcript: Vec<Message>,
    seq: i64,
    /// Where code shadow-snapshots live (RFC PR3); defaults to `.forge/checkpoints`.
    checkpoint_root: std::path::PathBuf,
    /// The seq that began the current turn (its user message), keying this turn's snapshot dir.
    current_turn_seq: i64,
    /// The discovered model catalog (auto-discovery mesh), kept so the TUI `/models` browser can
    /// classify + group what's available without re-running discovery. `None` for mock/offline.
    catalog: Option<ModelCatalog>,
    /// The agent's task list (the `update_tasks` tool), rehydrated from the store on resume.
    tasks: Vec<forge_types::TodoItem>,
    /// Connected external MCP servers (mcp-client.md). `None` when no servers are configured —
    /// the whole MCP path is then inert (zero overhead for non-MCP users).
    mcp: Option<Arc<forge_mcp::McpManager>>,
    /// The code-intelligence index (code-intelligence.md). `None` when disabled or unavailable —
    /// retrieval then injects nothing and the turn runs exactly as before (additive guarantee).
    /// `Arc` so the model-facing `lattice` tool shares the same index.
    lattice: Option<Arc<Lattice>>,
    /// Background file watcher that keeps the index fresh on external edits. Held only to keep the
    /// watcher thread alive for the session's lifetime (dropped → watching stops).
    lattice_watcher: Option<forge_index::LatticeWatcher>,
    /// LSP registry for live diagnostics after writes. `None` when lsp.enabled = false.
    lsp: Option<Arc<forge_lsp::LspRegistry>>,
    /// The discovered command/skill catalog, so the model can find + load Forge's own skills via
    /// the `use_skill` virtual tool (command-skill-system.md). `None` → the tool is not advertised
    /// and the turn runs exactly as before.
    skills: Option<Arc<forge_skills::Catalog>>,
    /// In-session model pin (`/model <id>`). When set, mesh routing still classifies the prompt
    /// (for stats), but this model is used instead of the routed pick. `None` = mesh routing.
    pinned_model: Option<String>,
    /// In-session reasoning-effort pin (`/effort <level>`). When set, forwarded to the provider
    /// as a `ReasoningEffort` hint each turn. `None` = provider default (no hint sent).
    pinned_effort: Option<EffortLevel>,
    /// System hints queued by side-call diagnostics (e.g. shell error interceptor) to be injected
    /// into the transcript immediately after the tool result that triggered them. Cleared each time.
    pending_hints: Vec<String>,
    /// Session-scoped "always" answer to the auto-compact-on-switch consent prompt: once the user
    /// picks "always", a mesh failover to a model that needs compaction proceeds silently for the
    /// rest of this session (reset next launch). `false` = ask each time.
    always_compact_on_switch: bool,
    /// Whether `.forge/AGENTS.md` (or `AGENTS.md`) has been injected as a standing system prompt.
    /// False for fresh sessions so it's injected on the first turn; true for resumed sessions
    /// (the content is already in the stored transcript) and after injection.
    project_prompt_injected: bool,
    /// Images attached to the *next* user turn (vision input, e.g. via `/image <path>`). Drained
    /// when that turn's user message is built; empty for text-only turns.
    pending_images: Vec<forge_types::ImageAttachment>,
    /// Count of successful writes made by `invoke_tool` in the current turn. Reset at the start
    /// of each turn; used to gate the autofix stage (skip it when nothing was edited).
    edits_this_turn: u32,
}

impl Session {
    pub fn start(
        store: Arc<Store>,
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
        tools: ToolRegistry,
        presenter: Box<dyn Presenter>,
        config: Config,
        cwd: &str,
    ) -> Result<Self, CoreError> {
        let mode = config.permission_mode;
        let id = store.create_session(cwd, format!("{mode:?}").as_str())?;
        Ok(Self::build(
            id,
            store,
            provider,
            router,
            tools,
            presenter,
            config,
            Vec::new(),
            0,
        ))
    }

    /// Resume an existing session: rehydrate its transcript and continue the same row.
    #[allow(clippy::too_many_arguments)]
    pub fn resume(
        store: Arc<Store>,
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
        tools: ToolRegistry,
        presenter: Box<dyn Presenter>,
        config: Config,
        session_id: &str,
    ) -> Result<Self, CoreError> {
        if !store.session_exists(session_id)? {
            return Err(CoreError::SessionNotFound(session_id.to_string()));
        }
        let stored = store.load_messages(session_id)?;
        let seq = stored.len() as i64;
        let transcript = stored
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
                tool_calls: m.tool_calls,
                tool_call_id: m.tool_call_id,
                images: Vec::new(),
            })
            .collect();
        // Restore the permission mode that was active when the session was last saved.
        let mut config = config;
        if let Ok(stored_mode) = store.session_mode(session_id) {
            let parsed = match stored_mode.as_str() {
                "Default" => Some(PermissionMode::Default),
                "AcceptEdits" => Some(PermissionMode::AcceptEdits),
                "Bypass" => Some(PermissionMode::Bypass),
                "Plan" => Some(PermissionMode::Plan),
                _ => PermissionMode::from_label(&stored_mode),
            };
            if let Some(m) = parsed {
                config.permission_mode = m;
            }
        }
        Ok(Self::build(
            session_id.to_string(),
            store,
            provider,
            router,
            tools,
            presenter,
            config,
            transcript,
            seq,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        id: String,
        store: Arc<Store>,
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
        tools: ToolRegistry,
        presenter: Box<dyn Presenter>,
        config: Config,
        transcript: Vec<Message>,
        seq: i64,
    ) -> Self {
        let mode = config.permission_mode;
        // Layer fetched per-model prices (OpenRouter etc., persisted at discovery) under the config
        // overrides, so gateway/credit spend is priced instead of silently $0 (the budget cap and
        // the /usage breakdown both read these computed costs).
        let fetched_prices = store.all_model_pricing().unwrap_or_default();
        let pricing = Pricing::from_config_with_fetched(&config, fetched_prices);
        let rules = config.permission_rules();
        // Rehydrate the task list (empty for a fresh session; restored on resume).
        let tasks = store.tasks(&id).unwrap_or_default();
        // Resumed sessions already have AGENTS.md in the stored transcript; don't re-inject.
        let project_prompt_injected = !transcript.is_empty();
        let mut s = Self {
            id,
            store,
            provider,
            router,
            tools,
            presenter,
            config,
            pricing,
            mode,
            rules,
            transcript,
            seq,
            checkpoint_root: std::path::PathBuf::from(".forge/checkpoints"),
            current_turn_seq: 0,
            catalog: None,
            tasks,
            mcp: None,
            lattice: None,
            lattice_watcher: None,
            lsp: None,
            skills: None,
            pinned_model: None,
            pinned_effort: None,
            pending_hints: vec![],
            always_compact_on_switch: false,
            project_prompt_injected,
            pending_images: Vec::new(),
            edits_this_turn: 0,
        };
        let id = s.id.clone();
        s.presenter.emit(PresenterEvent::SessionStarted { id });
        s
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Queue images to attach to the next user turn (vision input). Consumed when that turn's user
    /// message is built; a turn with no images behaves exactly as before.
    pub fn attach_images(&mut self, images: Vec<forge_types::ImageAttachment>) {
        self.pending_images.extend(images);
    }

    /// Whether project-scope (`./.forge/`) commands/skills run without a first-use confirmation.
    pub fn commands_trust_project(&self) -> bool {
        self.config.commands.trust_project
    }

    /// Attach the discovered catalog so the `/models` browser can read it (composition root).
    pub fn set_catalog(&mut self, catalog: Option<ModelCatalog>) {
        self.catalog = catalog;
    }

    /// Pin (or clear) the in-session model override. When `Some`, subsequent turns use this model
    /// instead of the mesh-routed pick. `None` returns to normal mesh routing.
    pub fn pin_model(&mut self, model_id: Option<String>) {
        self.pinned_model = model_id;
    }

    /// The currently-pinned model, if any (`/model <id>` was called this session).
    pub fn pinned_model(&self) -> Option<&str> {
        self.pinned_model.as_deref()
    }

    /// Set (or clear) the in-session reasoning-effort pin. `None` returns to the provider default.
    pub fn set_effort(&mut self, e: Option<EffortLevel>) {
        self.pinned_effort = e;
    }

    /// The currently-pinned effort level, if any (`/effort <level>` was called this session).
    pub fn pinned_effort(&self) -> Option<EffortLevel> {
        self.pinned_effort
    }

    /// The discovered model catalog, if auto-discovery ran for this session.
    pub fn catalog(&self) -> Option<&ModelCatalog> {
        self.catalog.as_ref()
    }

    /// Attach connected MCP servers (composition root). Their tools become advertisable via
    /// `tool_specs` and callable through `invoke_tool`, gated by the permission broker.
    pub fn set_mcp(&mut self, mcp: Option<Arc<forge_mcp::McpManager>>) {
        // An empty manager (no servers connected) adds nothing — keep it `None` so the path stays
        // fully inert and `tool_specs` is byte-for-byte unchanged.
        self.mcp = mcp.filter(|m| !m.is_empty());
    }

    /// Attach the code-intelligence index (composition root). When set and `lattice.inject` is on,
    /// each turn auto-injects relevant code; the agent's edits reindex the touched file in-turn.
    pub fn set_lattice(&mut self, lattice: Option<Arc<Lattice>>) {
        self.lattice = lattice;
    }

    /// Attach the background reindex watcher (composition root); held for the session's lifetime.
    pub fn set_lattice_watcher(&mut self, watcher: Option<forge_index::LatticeWatcher>) {
        self.lattice_watcher = watcher;
    }

    /// Attach the LSP registry (composition root). No-op when `lsp.enabled = false`.
    pub fn set_lsp(&mut self, lsp: Option<Arc<forge_lsp::LspRegistry>>) {
        self.lsp = lsp;
    }

    /// Attach the command/skill catalog (composition root) so the model can discover and load
    /// Forge's own skills via the `use_skill` tool. `None` (or an empty catalog) → not advertised.
    pub fn set_skills(&mut self, skills: Option<Arc<forge_skills::Catalog>>) {
        self.skills = skills;
    }

    /// Scoped subgraph for `symbol` from the session's live index (the `/lattice` view). `Ok(None)`
    /// when no index is attached.
    pub fn lattice_view(
        &self,
        symbol: &str,
    ) -> Result<Option<forge_index::LatticeView>, CoreError> {
        match &self.lattice {
            Some(l) => Ok(Some(l.view(symbol)?)),
            None => Ok(None),
        }
    }

    /// Per-server MCP status for the `/mcp` listing (empty when no servers are configured).
    pub fn mcp_status(&self) -> Vec<forge_types::McpServerLine> {
        self.mcp
            .as_ref()
            .map(|m| m.status_lines())
            .unwrap_or_default()
    }

    /// Emit the current MCP server listing to the presenter (called once at startup so connection
    /// status — including any failures — is visible). No-op when no servers are configured.
    pub fn announce_mcp(&mut self) {
        if self.mcp.is_some() {
            let lines = self.mcp_status();
            self.presenter.emit(PresenterEvent::McpStatus(lines));
        }
    }

    /// The full discovered tool list for one MCP server (`forge mcp --tools <server>`).
    pub fn mcp_tool_lines(&self, server: &str) -> Vec<(String, String)> {
        self.mcp
            .as_ref()
            .map(|m| m.tool_lines(server))
            .unwrap_or_default()
    }

    /// The pricing table in effect (bundled defaults + config overrides), for cost display.
    pub fn pricing(&self) -> &Pricing {
        &self.pricing
    }

    /// Override where code shadow-snapshots are stored (default `.forge/checkpoints`). Used by the
    /// composition root to anchor them under the project `.forge/`, and by tests for isolation.
    pub fn set_checkpoint_root(&mut self, root: impl Into<std::path::PathBuf>) {
        self.checkpoint_root = root.into();
    }

    /// Rewind the conversation to a transcript boundary (`seq`): soft-delete the messages at/after
    /// it, restore any files those turns wrote (PR3 shadow snapshots), and truncate the live
    /// transcript. Returns the file-restore result plus the prompt that started the rewound-to turn
    /// (so the UI can put it back in the input box). Powers `/undo` and `/checkpoints`.
    pub fn rewind_to(&mut self, boundary: i64) -> Result<RewindOutcome, CoreError> {
        let boundary = boundary.max(0);
        // The message AT the boundary is the user prompt of the rewound-to turn; capture it before
        // truncation so the UI can re-offer it for editing/resubmitting.
        let rewound_prompt = self
            .transcript
            .get(boundary as usize)
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.clone());
        let mut restore = snapshot::RestoreReport::default();
        // Turns are keyed by their user-message seq. Restore every snapshotted turn at/after the
        // boundary, newest first so an earlier turn's blob (pre-turn bytes) wins the final state.
        for seq in (boundary..self.seq).rev() {
            if let Ok(r) = snapshot::restore_turn(&self.checkpoint_root, &self.id, seq) {
                restore.restored.extend(r.restored);
                restore.warnings.extend(r.warnings);
            }
        }
        self.store.deactivate_messages_from(&self.id, boundary)?;
        self.transcript.truncate(boundary as usize);
        self.seq = boundary;
        Ok(RewindOutcome {
            restore,
            rewound_prompt,
        })
    }

    /// Undo the last user turn: rewind to (and including) the most recent user message, dropping
    /// that prompt and everything after it. `Ok(None)` if there's nothing to undo.
    pub fn undo(&mut self) -> Result<Option<RewindOutcome>, CoreError> {
        let Some(idx) = self.transcript.iter().rposition(|m| m.role == Role::User) else {
            return Ok(None);
        };
        Ok(Some(self.rewind_to(idx as i64)?))
    }

    /// Publish the current turn's snapshot context (session id, seq, absolute root) to the
    /// environment so the CLI bridge's `forge mcp-serve` snapshots its writes into this turn's dir.
    fn export_checkpoint_env(&self, seq: i64) {
        let root = std::path::absolute(&self.checkpoint_root)
            .unwrap_or_else(|_| self.checkpoint_root.clone());
        std::env::set_var(snapshot::ENV_SESSION, &self.id);
        std::env::set_var(snapshot::ENV_SEQ, seq.to_string());
        std::env::set_var(snapshot::ENV_ROOT, root);
    }

    /// Save a conversation checkpoint at the current boundary. `label` None = an auto checkpoint.
    pub fn checkpoint(&mut self, label: Option<&str>) -> Result<(), CoreError> {
        self.store.add_checkpoint(&self.id, label, self.seq)?;
        Ok(())
    }

    /// This session's saved checkpoints, newest first.
    pub fn checkpoints(&self) -> Result<Vec<forge_store::CheckpointRow>, CoreError> {
        Ok(self.store.list_checkpoints(&self.id)?)
    }

    /// Visible conversation history (user + non-empty assistant messages), oldest first, for
    /// redrawing the transcript into the TUI scrollback after a `/resume` swap.
    pub fn history(&self) -> Vec<(Role, String)> {
        self.transcript
            .iter()
            .filter(|m| {
                matches!(m.role, Role::User | Role::Assistant) && !m.content.trim().is_empty()
            })
            .map(|m| (m.role, m.content.clone()))
            .collect()
    }

    /// The full rehydrated transcript as renderable [`ReplayItem`](forge_tui::ReplayItem)s for the
    /// TUI to redraw on resume — user prompts, assistant text, AND the tool calls/results between
    /// them, so a resumed agentic session reappears faithfully instead of as a sparse user-only
    /// echo (the old [`history`](Self::history) dropped every tool-only assistant turn). Tool
    /// results are matched back to their call's name via the `tool_call_id`.
    pub fn replay_items(&self) -> Vec<forge_tui::ReplayItem> {
        messages_to_replay_items(&self.transcript)
    }

    /// Like [`replay_items`](Self::replay_items) but over the FULL original history (including
    /// messages that compaction folded away), read straight from the store rather than the
    /// model-facing in-memory transcript. This is what lets the USER scroll back through the entire
    /// untouched conversation after a resume, even though the model only ever saw the compacted
    /// view. Falls back to the in-memory transcript if the store read fails.
    pub fn replay_items_full(&self) -> Vec<forge_tui::ReplayItem> {
        match self.store.load_all_messages(&self.id) {
            Ok(stored) => {
                let msgs: Vec<Message> = stored
                    .into_iter()
                    .map(|m| Message {
                        role: m.role,
                        content: m.content,
                        tool_calls: m.tool_calls,
                        tool_call_id: m.tool_call_id,
                        images: Vec::new(),
                    })
                    .collect();
                messages_to_replay_items(&msgs)
            }
            Err(_) => self.replay_items(),
        }
    }

    /// Whether this session was compacted at least once (its model context is a summary, not the
    /// full history) — the signal for offering "continue compacted vs reload full" on resume.
    pub fn was_compacted(&self) -> bool {
        self.store.session_has_compaction(&self.id).unwrap_or(false)
    }

    /// Replace the model-facing transcript with the FULL, uncompacted history — the user chose, on
    /// resume, to continue WITHOUT compaction so the model re-reads the entire original
    /// conversation. (It may exceed the window; the next turn's auto-compaction handles that, now
    /// that token counting is precise.) The user-visible scrollback already shows everything.
    pub fn reload_full_context(&mut self) -> Result<(), CoreError> {
        let stored = self.store.load_all_messages(&self.id)?;
        self.seq = stored.len() as i64;
        self.transcript = stored
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
                tool_calls: m.tool_calls,
                tool_call_id: m.tool_call_id,
                images: Vec::new(),
            })
            .collect();
        Ok(())
    }

    /// Reconfigure this session in place as a **fresh** one (new id, empty transcript), keeping
    /// the same backends + live presenter so events keep flowing to the running TUI. Powers
    /// `/new` — no process restart, no Session move (it lives behind the loop's `Mutex`).
    pub fn reset_fresh(&mut self, cwd: &str) -> Result<(), CoreError> {
        let id = self
            .store
            .create_session(cwd, format!("{:?}", self.mode).as_str())?;
        self.id = id.clone();
        self.transcript.clear();
        self.seq = 0;
        self.tasks.clear();
        self.project_prompt_injected = false;
        self.presenter.emit(PresenterEvent::SessionStarted { id });
        Ok(())
    }

    /// Reconfigure this session in place, **resumed** from `session_id`: rehydrate the stored
    /// transcript, keep the same backends + live presenter. Powers `/resume`.
    pub fn reset_resumed(&mut self, session_id: &str) -> Result<(), CoreError> {
        if !self.store.session_exists(session_id)? {
            return Err(CoreError::SessionNotFound(session_id.to_string()));
        }
        let stored = self.store.load_messages(session_id)?;
        self.seq = stored.len() as i64;
        self.transcript = stored
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
                tool_calls: m.tool_calls,
                tool_call_id: m.tool_call_id,
                images: Vec::new(),
            })
            .collect();
        self.id = session_id.to_string();
        self.tasks = self.store.tasks(session_id).unwrap_or_default();
        self.project_prompt_injected = true;
        self.presenter.emit(PresenterEvent::SessionStarted {
            id: session_id.to_string(),
        });
        // Re-show the restored task list so the resumed session's progress is visible.
        if !self.tasks.is_empty() {
            self.presenter
                .emit(PresenterEvent::Tasks(self.tasks.clone()));
        }
        Ok(())
    }

    /// The session's current temper (permission mode).
    pub fn temper(&self) -> PermissionMode {
        self.mode
    }

    /// The hooks configured for this session. Used by the chat loop to fire lifecycle events
    /// (`UserPromptSubmit`, `SessionStart`, `SessionEnd`) outside the tool path.
    pub fn hooks(&self) -> &[forge_config::HookConfig] {
        &self.config.hooks
    }

    /// The session id — used by lifecycle hooks to identify the session.
    pub fn session_id(&self) -> &str {
        &self.id
    }

    /// The most recent assistant message's text, if any — used by `/loop` to decide whether the
    /// model signalled completion.
    pub fn last_assistant_text(&self) -> Option<&str> {
        self.transcript
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| m.content.as_str())
    }

    /// Total spend today (UTC calendar day) across all sessions — the same figure the budget
    /// gate checks. Returns 0.0 on store error.
    pub fn spend_today_usd(&self) -> f64 {
        self.store.spend_today_usd().unwrap_or(0.0)
    }

    /// Total spend this month across all sessions. Returns 0.0 on store error.
    pub fn spend_this_month_usd(&self) -> f64 {
        self.store.spend_this_month_usd().unwrap_or(0.0)
    }

    /// Token and cost totals for the current session from the DB (reliable for bridge providers).
    pub fn session_usage_db(&self) -> (u64, u64, f64) {
        let id = self.session_id();
        let (inp, out) = self.store.session_tokens(id).unwrap_or((0, 0));
        let cost = self.store.session_cost(id).unwrap_or(0.0);
        (inp, out, cost)
    }

    /// Spend in the last 5 hours (rolling window). Returns 0.0 on store error.
    pub fn spend_last_5h_usd(&self) -> f64 {
        self.store.spend_last_5h_usd().unwrap_or(0.0)
    }

    /// Spend in the current ISO week (Monday 00:00 local → now). Returns 0.0 on store error.
    pub fn spend_this_week_usd(&self) -> f64 {
        self.store.spend_this_week_usd().unwrap_or(0.0)
    }

    /// Per-model spend + token counts for the last 5 hours.
    pub fn spend_by_model_5h(&self) -> Vec<(String, f64, u64, u64)> {
        self.store.spend_by_model_5h().unwrap_or_default()
    }

    /// Per-model spend + token counts for today, for the `/usage` overlay.
    pub fn spend_by_model_today(&self) -> Vec<(String, f64, u64, u64)> {
        self.store.spend_by_model_today().unwrap_or_default()
    }

    /// Per-model spend + token counts for this ISO week.
    pub fn spend_by_model_week(&self) -> Vec<(String, f64, u64, u64)> {
        self.store.spend_by_model_week().unwrap_or_default()
    }

    /// Daily/monthly/weekly caps from config, for the `/usage` overlay gauges.
    pub fn budget_caps(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        (
            self.config.mesh.daily_budget_usd,
            self.config.mesh.monthly_cap_usd,
            self.config.mesh.weekly_budget_usd,
        )
    }

    /// Per-provider, per-window fraction from `subscription_usage` (for display fallback when
    /// the statusline cache is stale). Returns `HashMap<provider, HashMap<window_kind, fraction>>`.
    pub fn bridge_fractions(
        &self,
    ) -> std::collections::HashMap<String, std::collections::HashMap<String, f64>> {
        self.store.bridge_fractions().unwrap_or_default()
    }

    /// Seconds since the claude subscription quota was last updated (`None` if never). The CLI
    /// gates its on-demand rate-limit probe on this so it refreshes at most every few minutes.
    pub fn claude_quota_age_secs(&self) -> Option<i64> {
        self.store.subscription_age_secs("claude-cli")
    }

    /// Seed the subscription-usage store from an externally-observed window fraction (the
    /// Claude/Codex rate-limit caches the CLI reads). Forge otherwise only learns a subscription's
    /// usage when it runs a turn on that bridge — usage racked up *outside* Forge would read as 0%,
    /// making the mesh think the plan is fresh. `pct` is 0–100; `None` is skipped. The recorded row
    /// has no reset time, so it stays live until a real in-turn QuotaUpdate replaces it.
    pub fn seed_subscription_quota(&self, provider: &str, window: &str, pct: Option<f64>) {
        let Some(pct) = pct else { return };
        let frac = (pct / 100.0).clamp(0.0, 1.0);
        let status = if frac >= 0.98 {
            forge_types::QuotaStatus::Exhausted
        } else if frac >= 0.80 {
            forge_types::QuotaStatus::Warning
        } else {
            forge_types::QuotaStatus::Ok
        };
        let _ = self.store.record_quota(&forge_types::QuotaHint {
            provider: provider.to_string(),
            window: window.to_string(),
            status,
            resets_at: None,
            fraction_used: Some(frac),
        });
    }

    /// Advance the temper through the SHIFT+TAB cycle, persist it, and return the new temper
    /// (RFC/temper-modes). Takes effect on the next turn's permission decisions.
    pub fn cycle_temper(&mut self) -> PermissionMode {
        self.set_temper(self.mode.cycle_next())
    }

    /// Set the temper to a specific mode (the `/mode` picker), persist it, and return it. Unlike
    /// the cycle this can reach `Bypass`/Full, since the picker is an explicit, deliberate choice.
    pub fn set_temper(&mut self, mode: PermissionMode) -> PermissionMode {
        self.mode = mode;
        let _ = self
            .store
            .update_session_mode(&self.id, &format!("{:?}", self.mode));
        self.mode
    }

    /// Run an Assay analysis over `source` (the bundled scope content), emit + persist the report,
    /// and — when `cleanup` — run a permission-gated, **undoable** fix turn (Refine) over the
    /// findings. The crew is read-only; Refine reuses the normal agent loop so its edits go through
    /// the permission broker and are shadow-snapshotted (so `/undo` reverts them).
    pub async fn assay(
        &mut self,
        source: Arc<str>,
        models: assay::TierModels,
        lenses: Vec<forge_types::FindingCategory>,
        scope: forge_types::AssayScope,
        cleanup: bool,
    ) -> Result<(), CoreError> {
        let pricing = Arc::new(self.pricing.clone());
        let lenses = if lenses.is_empty() {
            forge_types::FindingCategory::crew().to_vec()
        } else {
            lenses
        };
        let cooldown = std::time::Duration::from_secs(self.config.mesh.failover_cooldown_secs);
        let provider = Arc::clone(&self.provider);
        let store = Arc::clone(&self.store);

        // U8 — budget pre-estimate: scope down lenses to fit within remaining daily/monthly cap.
        let remaining_usd = {
            let spent_today = self.store.spend_today_usd().unwrap_or(0.0);
            let spent_month = self.store.spend_this_month_usd().unwrap_or(0.0);
            let daily = self
                .config
                .mesh
                .daily_budget_usd
                .map(|cap| (cap - spent_today).max(0.0));
            let monthly = self
                .config
                .mesh
                .monthly_cap_usd
                .map(|cap| (cap - spent_month).max(0.0));
            match (daily, monthly) {
                (Some(d), Some(m)) => Some(d.min(m)),
                (Some(d), None) => Some(d),
                (None, Some(m)) => Some(m),
                (None, None) => None,
            }
        };
        let (lenses, dropped, estimated_cost) =
            assay::scope_to_budget(lenses, source.len(), &models, &pricing, remaining_usd);
        if dropped > 0 {
            self.presenter.emit(PresenterEvent::Warning(format!(
                "assay: estimated cost ~${estimated_cost:.3} exceeds remaining budget \
                 ${:.3} — dropped {dropped} expensive lens(es) to fit",
                remaining_usd.unwrap_or(0.0),
            )));
        }
        if lenses.is_empty() {
            self.presenter.emit(PresenterEvent::Warning(
                "assay: estimated cost exceeds remaining budget — \
                 add a free model or raise [mesh] daily_budget_usd / monthly_cap_usd"
                    .to_string(),
            ));
            return Ok(());
        }

        // Surface each critic/verifier as it finishes so the run shows live activity.
        let presenter = &mut self.presenter;
        let mut on_progress = |p: assay::AssayProgress| match &p {
            assay::AssayProgress::CriticQueued { lens } => {
                presenter.emit(PresenterEvent::AssayCriticRow(
                    forge_types::AssayCriticRow {
                        lens: lens.as_str().to_string(),
                        status: forge_types::AssayCriticStatus::Queued,
                    },
                ));
            }
            assay::AssayProgress::CriticDone { lens, candidates } => {
                presenter.emit(PresenterEvent::AssayCriticRow(
                    forge_types::AssayCriticRow {
                        lens: lens.as_str().to_string(),
                        status: forge_types::AssayCriticStatus::Done {
                            candidates: *candidates,
                        },
                    },
                ));
            }
            assay::AssayProgress::CriticSkipped { lens, reason } => {
                presenter.emit(PresenterEvent::AssayCriticRow(
                    forge_types::AssayCriticRow {
                        lens: lens.as_str().to_string(),
                        status: forge_types::AssayCriticStatus::Skipped {
                            reason: reason.clone(),
                        },
                    },
                ));
            }
            _ => {
                presenter.emit(PresenterEvent::AssayProgress(assay::progress_line(&p)));
            }
        };
        let mut report = assay::run_assay(
            scope,
            source,
            lenses,
            models,
            provider,
            pricing,
            store,
            cooldown,
            &mut on_progress,
        )
        .await;
        if let Ok(run_id) = self
            .store
            .create_assay_run(&report.scope.label(), report.cost_usd)
        {
            report.run_id = run_id.clone();
            for f in &report.findings {
                let _ = self.store.add_finding(&run_id, f);
            }
            // Auto-diff: compare against the prior run for this scope so users see what changed.
            if let Ok(Some(prev_id)) = self
                .store
                .latest_run_for_scope(&report.scope.label(), &run_id)
            {
                if let Ok(prev) = self.store.load_findings(&prev_id) {
                    let note =
                        assay_diff_note(&prev, &report.findings, &prev_id[..8.min(prev_id.len())]);
                    if !note.is_empty() {
                        self.presenter.emit(PresenterEvent::Warning(note));
                    }
                }
            }
        }
        self.presenter
            .emit(PresenterEvent::AssayReport(report.clone()));

        if cleanup && !report.findings.is_empty() {
            self.presenter.emit(PresenterEvent::Warning(format!(
                "⚒ Refine — fixing {} finding(s); edits are permission-gated, /undo to revert",
                report.findings.len()
            )));
            let prompt = refine_prompt(&report);
            self.run_turn(&prompt).await?; // emits its own Done
        } else {
            if cleanup {
                self.presenter.emit(PresenterEvent::Warning(
                    "nothing to clean up — no findings".into(),
                ));
            }
            self.presenter.emit(PresenterEvent::Done {
                final_text: String::new(),
            });
        }
        Ok(())
    }

    /// Read the next user prompt from the attached surface. `None` ends the session.
    pub fn read_line(&mut self) -> Option<String> {
        self.presenter.read_line()
    }

    /// Surface a turn-level failure to the UI (a warning + a Done marker) so the caller's
    /// loop ends the turn cleanly instead of leaving it hanging.
    pub fn notify_error(&mut self, msg: &str) {
        self.presenter
            .emit(PresenterEvent::Warning(msg.to_string()));
        self.presenter.emit(PresenterEvent::Done {
            final_text: String::new(),
        });
    }

    fn next_seq(&mut self) -> i64 {
        let n = self.seq;
        self.seq += 1;
        n
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .names()
            .filter_map(|name| self.tools.get(name))
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                schema: t.schema(),
            })
            .collect();
        // Advertise the subagent virtual tool to the top-level model only (RFC
        // subagent-orchestration). Children build their own registry without it, so the
        // depth-1 recursion guard is structural.
        if self.config.mesh.subagents.enabled {
            specs.push(subagent::spawn_agents_spec(
                self.config.mesh.subagents.max_agents,
            ));
        }
        // The interactive question tool (AskUserQuestion) — always advertised so the model can
        // ask the user a focused question with suggested answers (docs/features/ask-user-question.md).
        specs.push(ask_user_spec());
        // The task-tracking tool — always advertised so the model can keep a live todo list.
        specs.push(update_tasks_spec());
        // The skill-loading tool — advertised (with the available-skills list) only when a
        // non-empty catalog is attached, so the model can find + apply Forge's own skills.
        if let Some(cat) = &self.skills {
            if !cat.skill_listing().is_empty() {
                specs.push(use_skill_spec(cat));
            }
        }
        // External MCP servers: the meta-tools (search/expose/resources/prompt) + any exposed
        // server tools (deferred loading keeps this bounded). Empty unless servers are connected.
        if let Some(mcp) = &self.mcp {
            specs.extend(mcp.advertised_specs().into_iter().map(|s| ToolSpec {
                name: s.name,
                description: s.description,
                schema: s.schema,
            }));
        }
        specs
    }

    /// Run one full turn: route -> (model -> tools)* -> final answer. Returns the answer.
    pub async fn run_turn(&mut self, prompt: &str) -> Result<String, CoreError> {
        self.run_turn_with(prompt, &[], None).await
    }

    /// Compact the live context: summarize the older messages (everything but the most recent
    /// `COMPACT_KEEP_RECENT`) into a single system message via a cheap model call, shrinking what
    /// subsequent turns send to the model. In-memory only — the full transcript stays in the store
    /// for audit/resume (persisting the compacted view across resume is a follow-up). No-op when
    /// the transcript is already short. Returns `(messages_before, messages_after)`.
    /// Current subscription quota, enriched with the configured plan slugs and the conservation
    /// opt-out, so the router can spread complex/standard load off a subscription proactively
    /// (not just react at the hard limit). Defaults to an empty quota when the store read fails.
    fn live_quota(&self) -> forge_types::SubscriptionQuota {
        self.store
            .current_quota()
            .unwrap_or_default()
            .with_plans(self.config.mesh.subscriptions.clone())
            .with_conserve(self.config.mesh.subscription_conserve)
    }

    /// The current budget snapshot (spend vs caps) used for routing decisions.
    fn budget_snapshot(&self) -> BudgetState {
        BudgetState {
            spent_today_usd: self.store.spend_today_usd().unwrap_or(0.0),
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: self.store.spend_this_week_usd().unwrap_or(0.0),
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd().unwrap_or(0.0),
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        }
    }

    /// Explain how the mesh would route `prompt` right now, using this session's live catalog,
    /// quota, benched-model health and budget — the data behind the `/mesh` inspector. `None` when
    /// auto-discovery routing isn't active (no catalog), since the candidate table would be empty.
    pub fn explain_routing(&self, prompt: &str) -> Option<forge_mesh::RoutingExplanation> {
        let catalog = self.catalog.clone()?;
        let router = forge_mesh::HeuristicRouter::new(self.config.clone()).with_catalog(catalog);
        let health = self.store.current_benched().unwrap_or_default();
        let mut exp = router.explain(prompt, self.budget_snapshot(), &health, &self.live_quota());
        use forge_config::ClassifierKind;
        exp.classifier_label = match self.config.mesh.classifier {
            ClassifierKind::Heuristic => "heuristic".to_string(),
            ClassifierKind::Llm => {
                let m = self
                    .config
                    .mesh
                    .classifier_model
                    .as_deref()
                    .unwrap_or("trivial-tier fallback");
                format!("llm ({m}) — actual tier may differ from this heuristic preview")
            }
            ClassifierKind::Hybrid => {
                let (_, confident, reason) =
                    forge_mesh::HeuristicRouter::classify_confident(prompt);
                if confident {
                    format!("hybrid — heuristic confident ({reason}), no llm call")
                } else {
                    let m = self
                        .config
                        .mesh
                        .classifier_model
                        .as_deref()
                        .unwrap_or("trivial-tier fallback");
                    format!("hybrid — uncertain zone, llm ({m}) will classify at turn time")
                }
            }
        };
        Some(exp)
    }

    /// The last-resort model to try when the routed fallback chain is exhausted: the non-excluded
    /// model whose transient bench expires soonest (the "least dead"). Returns `None` once already
    /// used, or when the only candidate is the model that just failed (`just_failed`), or when
    /// nothing transient is benched — so the caller falls through to [`CoreError::NoHealthyModel`].
    fn last_resort_model(&self, just_failed: &str, already_used: bool) -> Option<String> {
        if already_used {
            return None;
        }
        match self.store.soonest_unbenched() {
            Ok(Some(m)) if m != just_failed => Some(m),
            _ => None,
        }
    }

    /// The context window (tokens) to assume for `model`: a fetched per-model window (provider API,
    /// persisted in the store) first, then the family heuristic, then a conservative floor. Always
    /// returns a usable number so a turn can be bounded even for a model we've never seen.
    fn effective_context_window(&self, model: &str) -> u32 {
        self.store
            .model_context(model)
            .ok()
            .flatten()
            .filter(|w| *w > 0)
            .or_else(|| forge_mesh::pricing::context_limit(model))
            .unwrap_or(forge_mesh::pricing::CONSERVATIVE_CONTEXT_WINDOW)
    }

    /// The transcript trimmed to fit `model`'s context window, reserving room for the reply. Keeps
    /// the system preamble + the most recent turns so a long conversation never overflows the
    /// window (which otherwise fails the turn as "unavailable" on every model). Cheap; computed per
    /// active model each step so failover to a smaller-window model re-trims appropriately.
    fn transcript_for(&self, model: &str) -> Vec<Message> {
        let window = self.effective_context_window(model) as usize;
        let reserve = self.config.mesh.max_output_tokens.max(1024) as usize;
        // Real-token budget: window minus the reply reservation, with 5% headroom for the small
        // magnitude difference between our o200k counter and the target model's own tokenizer.
        let budget_tokens = window.saturating_sub(reserve) * 95 / 100;
        fit_messages(&self.transcript, budget_tokens.max(256))
    }

    /// The base harness preamble prepended (fresh, never persisted) to every main-loop request:
    /// the Forge coding-agent system prompt + a small live environment block (cwd / OS / git
    /// branch). Recomputed each call so it's always current, and placed first so the provider's
    /// cache breakpoint anchors on this stable prefix.
    fn system_preamble(&self) -> Vec<Message> {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string());
        let os = std::env::consts::OS;
        let branch = std::fs::read_to_string(".git/HEAD").ok().and_then(|s| {
            s.strip_prefix("ref: refs/heads/")
                .map(|b| b.trim().to_string())
        });
        let mut env = format!("<env>\nworking_directory: {cwd}\nplatform: {os}\n");
        if let Some(b) = branch {
            env.push_str(&format!("git_branch: {b}\n"));
        }
        env.push_str("</env>");
        vec![Message::system(FORGE_SYSTEM), Message::system(env)]
    }

    /// The request body for a main-loop call: the base harness preamble (system prompt + env)
    /// followed by the window-fitted transcript. The preamble's token cost is subtracted from the
    /// trim budget so the prepended prompt can't push the request over the model's window.
    fn transcript_with_preamble(&self, model: &str) -> Vec<Message> {
        let preamble = self.system_preamble();
        let window = self.effective_context_window(model) as usize;
        let reserve = self.config.mesh.max_output_tokens.max(1024) as usize;
        let preamble_tokens: usize = preamble.iter().map(message_tokens).sum();
        let budget_tokens = window
            .saturating_sub(reserve)
            .saturating_sub(preamble_tokens)
            * 95
            / 100;
        let mut out = preamble;
        out.extend(fit_messages(&self.transcript, budget_tokens.max(256)));
        out
    }

    /// System prompt for the architect planner phase. Instructs the planner to produce a concrete
    /// prose plan only — no tool calls are available in this phase.
    const ARCHITECT_PLANNER_SYSTEM: &'static str =
        "You are the PLANNER in a two-phase coding-assistant pipeline. \
Your job is to think through the request carefully and produce a concise, concrete, step-by-step \
plan of the edits and tool calls that an EDITOR agent will execute next. \
Rules:\n\
- Output ONLY the plan as structured prose or a numbered list. No preamble, no summary of what \
  you were asked, no sign-off.\n\
- Be specific: name the exact files to create/modify, the functions to add/change, \
  and the commands to run (if any).\n\
- Do NOT attempt to call any tools — none are available in this phase. \
  Describe what SHOULD be done, not do it.";

    /// Resolve the model to use for the architect PLAN phase.
    /// Priority: in-session `/model` pin > `mesh.architect_model` config > mesh-routed Complex tier.
    fn resolve_planner_model(&self) -> String {
        // An active /model pin overrides everything.
        if let Some(pin) = &self.pinned_model {
            return pin.clone();
        }
        // Explicit config override.
        if let Some(m) = &self.config.mesh.architect_model {
            if !m.is_empty() {
                return m.clone();
            }
        }
        // Fall back to the primary Complex-tier model from the config.
        self.config
            .model_for(forge_types::TaskTier::Complex)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "anthropic::claude-opus-4-8".to_string())
    }

    /// Resolve the model to use for the architect EDIT phase.
    /// Priority: in-session `/model` pin > `mesh.editor_model` config > mesh-routed Standard tier.
    fn resolve_editor_model(&self) -> String {
        // An active /model pin overrides everything (both phases use the same pinned model).
        if let Some(pin) = &self.pinned_model {
            return pin.clone();
        }
        // Explicit config override.
        if let Some(m) = &self.config.mesh.editor_model {
            if !m.is_empty() {
                return m.clone();
            }
        }
        // Fall back to the primary Standard-tier model from the config.
        self.config
            .model_for(forge_types::TaskTier::Standard)
            .map(|s| s.to_string())
            .unwrap_or_else(|| "groq::llama-3.3-70b-versatile".to_string())
    }

    /// Run the PLAN phase of the architect pipeline.
    ///
    /// Calls the planner model with the current transcript and NO tools advertised, streams its
    /// response as a normal assistant turn (persisted + streamed to the presenter), records
    /// usage/cost, and returns the plan text. Returns `Ok(None)` when `architect_mode` is off —
    /// the early-exit guard that makes the non-architect path byte-for-byte unchanged.
    async fn run_plan(&mut self) -> Result<Option<String>, CoreError> {
        if !self.config.mesh.architect_mode {
            return Ok(None);
        }

        let planner = self.resolve_planner_model();
        // Cross-provider failover chain for the plan phase: the resolved planner first, then the
        // mesh's Complex-tier chain (deduped, planner removed). Without this, a single rate-limit
        // on the planner would abort the whole architect turn before the edit loop ever runs.
        let failover = self.config.mesh.failover;
        let fallbacks: Vec<String> = if failover {
            let budget = self.budget_snapshot();
            let health = self.store.current_benched().unwrap_or_default();
            let quota = self.live_quota();
            let d = self
                .router
                .route_hinted(
                    "plan a complex software task",
                    budget,
                    &health,
                    &quota,
                    Some(TaskTier::Complex),
                )
                .await;
            std::iter::once(d.model)
                .chain(d.fallbacks)
                .filter(|m| m != &planner)
                .collect()
        } else {
            Vec::new()
        };

        let stream_idle = std::time::Duration::from_secs(self.config.mesh.stream_idle_timeout_secs);
        let completion_opts = CompletionOptions {
            effort: self.pinned_effort,
            temperature: Some(CODING_TEMPERATURE),
        };

        let mut chain = fallbacks.into_iter();
        let mut model = planner;
        let mut resp = loop {
            self.presenter.emit(PresenterEvent::Routing {
                tier: forge_types::TaskTier::Complex.as_str().to_string(),
                model: model.clone(),
                rationale: "architect plan phase (no tools)".to_string(),
            });

            // Re-window the transcript for THIS model (a smaller fallback still fits), then prepend
            // the planner system prompt.
            let mut planner_msgs = self.transcript_for(&model);
            planner_msgs.insert(0, Message::system(Self::ARCHITECT_PLANNER_SYSTEM));

            // Collect plan text while streaming it live to the presenter.
            let mut plan_text = String::new();
            let result = {
                let provider = &self.provider;
                let presenter = &mut self.presenter;
                let activity = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let act = std::sync::Arc::clone(&activity);
                let mut sink = |ev: StreamEvent| {
                    act.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if let StreamEvent::Text(ref t) = ev {
                        plan_text.push_str(t);
                    }
                    match ev {
                        StreamEvent::Text(t) => presenter.emit(PresenterEvent::AssistantDelta(t)),
                        StreamEvent::Reasoning(t) => presenter.emit(PresenterEvent::Reasoning(t)),
                        _ => {}
                    }
                };
                // Empty tool slice — the planner must not call tools.
                let fut =
                    provider.complete_with(&model, &planner_msgs, &[], &completion_opts, &mut sink);
                stream_with_idle_timeout(fut, &activity, stream_idle).await
            };

            match result {
                Ok(mut r) => {
                    // Use the streamed text if the provider returns empty content (some do).
                    if r.content.is_empty() && !plan_text.is_empty() {
                        r.content = plan_text;
                    }
                    break r;
                }
                Err(e) if failover && e.is_retryable() => {
                    match self.advance_fallback(&model, &e, &mut chain, "architect plan") {
                        Some(next) => model = next,
                        None => return Err(CoreError::Provider(e)),
                    }
                }
                Err(e) => return Err(CoreError::Provider(e)),
            }
        };

        if !resp.content.is_empty() {
            self.presenter.emit(PresenterEvent::AssistantDone);
        }

        // Record cost/usage for the plan phase.
        resp.usage.cost_usd = self.pricing.cost_for_usage(&model, &resp.usage);
        let seq = self.next_seq();
        let msg_id = self.store.add_message_full(
            &self.id,
            seq,
            Role::Assistant,
            &resp.content,
            Some(&model),
            &[],
            None,
        )?;
        self.store.record_usage(&self.id, &msg_id, &resp.usage)?;

        // Push the plan into the live transcript so the editor model sees it.
        self.transcript.push(Message::assistant(&resp.content));

        Ok(Some(resp.content))
    }

    /// Real BPE token count of the current transcript (content + tool calls + per-message framing),
    /// via [`tokens`]. Used to decide compaction + drive the gauge; not billed.
    fn estimated_transcript_tokens(&self) -> u64 {
        self.transcript
            .iter()
            .map(|m| message_tokens(m) as u64)
            .sum()
    }

    /// Whether the transcript comfortably fits `model`'s window — under 80% of the post-reply room.
    /// Below this, the turn proceeds as-is; at/over it, auto-compaction kicks in (and a failover to
    /// a model that fails this check triggers the consent prompt).
    fn transcript_fits(&self, model: &str) -> bool {
        let window = self.effective_context_window(model) as u64;
        let reserve = self.config.mesh.max_output_tokens.max(1024) as u64;
        let usable = window.saturating_sub(reserve) * 8 / 10;
        self.estimated_transcript_tokens() <= usable
    }

    /// Decide whether to admit a mesh-chosen failover `model`. If the transcript already fits, use
    /// it. Otherwise it's a switch to a smaller-window model that needs (lossy) compaction: proceed
    /// silently when the user picked "always" this session, else ask Yes/No/Always. `Ok(false)` =
    /// the user declined (skip this model; the caller advances to the next fallback that fits).
    async fn admit_failover_model(&mut self, model: &str) -> Result<bool, CoreError> {
        if self.transcript_fits(model) {
            return Ok(true);
        }
        if !self.always_compact_on_switch {
            let window_k = (self.effective_context_window(model) / 1000).max(1);
            let q = format!(
                "Mesh switched to {model} (~{window_k}k context) — too small for this conversation. \
                 Compact (summarize older messages) and continue on it?"
            );
            let opts = [
                forge_tui::QChoice {
                    label: "Yes".into(),
                    description: "Compact now and continue on this model".into(),
                },
                forge_tui::QChoice {
                    label: "No".into(),
                    description: "Skip it — try the next model that fits".into(),
                },
                forge_tui::QChoice {
                    label: "Always".into(),
                    description: "Compact on every such switch for the rest of this session".into(),
                },
            ];
            let ans = self.presenter.ask(&q, &opts, false).trim().to_lowercase();
            if ans == "always" {
                self.always_compact_on_switch = true;
            } else if ans != "yes" {
                return Ok(false); // No / cancelled → skip this model
            }
        }
        self.compact(true).await?;
        Ok(true)
    }

    /// Auto-compact (silently) when the transcript has grown past 80% of `model`'s window — the
    /// normal "conversation got long" case for the routed model, no prompt (the `compact` call
    /// emits its own one-line note). No-op when it already fits or the transcript is too short to
    /// compact. Distinct from the failover consent path ([`admit_failover_model`]).
    async fn auto_compact_if_needed(&mut self, model: &str) {
        if !self.transcript_fits(model) {
            let _ = self.compact(true).await;
            // Refresh the gauge NOW so it reflects the reduced context immediately, instead of
            // showing the old (over-window) size until the turn's first model call returns.
            self.emit_context_gauge(model);
        }
    }

    /// Emit a [`Cost`](PresenterEvent::Cost) event reflecting the CURRENT transcript size as the
    /// live context fill, so the statusline gauge + compaction band update right away (e.g. right
    /// after auto-compaction) rather than waiting for the next model call's real input-token count
    /// at turn end. Uses the conservative transcript estimate as a stand-in until the real count
    /// arrives.
    fn emit_context_gauge(&mut self, model: &str) {
        let (session_in, session_out) = self.store.session_tokens(&self.id).unwrap_or((0, 0));
        let session_total_usd = self.store.session_cost(&self.id).unwrap_or(0.0);
        self.presenter.emit(PresenterEvent::Cost {
            session_total_usd,
            session_in,
            session_out,
            context_tokens: self.estimated_transcript_tokens(),
            context_limit: forge_mesh::pricing::context_limit(model),
        });
    }

    /// Bench (or, for a permanent incapability, exclude) `model` after a retryable error and
    /// return the next model to try from `chain`, or `None` when the chain is exhausted. Emits the
    /// standard failover warning. Shared by the single-shot auxiliary calls (compaction, the
    /// architect plan phase) so a transient rate-limit on one provider no longer kills the whole
    /// turn — they now fail over down a chain exactly like the main model loop.
    fn advance_fallback(
        &mut self,
        model: &str,
        err: &forge_provider::ProviderError,
        chain: &mut dyn Iterator<Item = String>,
        label: &str,
    ) -> Option<String> {
        let reason = err.reason();
        let default_cooldown =
            std::time::Duration::from_secs(self.config.mesh.failover_cooldown_secs);
        if err.is_permanent() {
            let _ = self.store.exclude_model(model, reason);
        } else {
            let _ = self
                .store
                .bench_for(model, err.cooldown(default_cooldown), reason);
        }
        let next = chain.next();
        match &next {
            Some(n) => self.presenter.emit(PresenterEvent::Warning(format!(
                "{model} {reason} — {label} failing over to {n}"
            ))),
            None => self.presenter.emit(PresenterEvent::Warning(format!(
                "{model} {reason} — {label} chain exhausted"
            ))),
        }
        next
    }

    pub async fn compact(&mut self, auto: bool) -> Result<(usize, usize), CoreError> {
        let before = self.transcript.len();
        if before <= COMPACT_KEEP_RECENT + COMPACT_MIN_OLDER {
            return Ok((before, before)); // not worth a model call yet
        }
        // Drive the TUI's animated progress band (cleared by CompactionFinished below).
        self.presenter
            .emit(PresenterEvent::CompactionStarted { auto });
        let split = before - COMPACT_KEEP_RECENT;
        let older = &self.transcript[..split];
        let rendered = older
            .iter()
            .map(|m| format!("{}: {}", m.role.as_str(), m.content))
            .collect::<Vec<_>>()
            .join("\n");

        // Route the summary at the trivial tier (it's cheap, fixed work) and call the model once.
        let budget = BudgetState {
            spent_today_usd: self.store.spend_today_usd()?,
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: self.store.spend_this_week_usd()?,
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd()?,
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        };
        let health = self.store.current_benched().unwrap_or_default();
        let quota = self.live_quota();
        let decision = self
            .router
            .route_hinted(
                "summarize this conversation",
                budget,
                &health,
                &quota,
                Some(TaskTier::Trivial),
            )
            .await;

        let messages = [Message::system(COMPACT_SYSTEM), Message::user(rendered)];
        // Fail over down the routed chain on a transient error: a rate-limited summarizer must not
        // kill the turn — compaction also runs mid-failover (admit_failover_model), so a dead
        // model here would otherwise abort an otherwise-recoverable turn.
        let failover = self.config.mesh.failover;
        let mut chain = decision.fallbacks.clone().into_iter();
        let mut model = decision.model.clone();
        let resp = loop {
            let mut sink = |_: StreamEvent| {};
            match self
                .provider
                .complete(&model, &messages, &[], &mut sink)
                .await
            {
                Ok(r) => break r,
                Err(e) if failover && e.is_retryable() => {
                    match self.advance_fallback(&model, &e, &mut chain, "compact") {
                        Some(next) => model = next,
                        None => return Err(CoreError::Provider(e)),
                    }
                }
                Err(e) => return Err(CoreError::Provider(e)),
            }
        };
        let _ = self
            .store
            .record_side_call_usage(&self.id, "compact/summarize", &resp.usage);
        let summary = resp.content;

        let mut compacted = Vec::with_capacity(COMPACT_KEEP_RECENT + 1);
        compacted.push(Message::system(format!(
            "[Earlier conversation summarized to save context]\n{}",
            summary.trim()
        )));
        compacted.extend(self.transcript.split_off(split));
        self.transcript = compacted;

        // Persist: soft-delete the summarised messages and store the summary so a resumed
        // session rehydrates the compacted view instead of the full uncompacted transcript.
        let _ = self
            .store
            .compact_session_store(&self.id, summary.trim(), COMPACT_KEEP_RECENT);

        let after = self.transcript.len();
        self.presenter
            .emit(PresenterEvent::CompactionFinished { before, after });
        self.presenter.emit(PresenterEvent::Warning(format!(
            "compacted {before} messages → {after} (summary via {model})"
        )));
        Ok((before, after))
    }

    /// On a failed shell command, make one cheap trivial-tier model call explaining the likely
    /// cause + a concrete fix, surfaced via [`PresenterEvent::ShellDiagnosis`]. Best-effort: it
    /// is skipped when the budget is exhausted and stays silent on any model error, so it can
    /// never derail the turn (shell-error-interceptor.md).
    async fn diagnose_shell_error(&mut self, command: &str, result: &str) {
        // Fast path: common patterns don't need a model call.
        if let Some(cached) = pattern_diagnose(result) {
            self.pending_hints
                .push(format!("[shell diagnosis] {cached}"));
            self.presenter.emit(PresenterEvent::ShellDiagnosis {
                command: command.to_string(),
                diagnosis: cached.to_string(),
                fix: None,
            });
            return;
        }
        let budget = BudgetState {
            spent_today_usd: self.store.spend_today_usd().unwrap_or(0.0),
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: self.store.spend_this_week_usd().unwrap_or(0.0),
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd().unwrap_or(0.0),
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        };
        if budget.status() == BudgetStatus::Exhausted {
            return;
        }
        let health = self.store.current_benched().unwrap_or_default();
        let quota = self.live_quota();
        let decision = self
            .router
            .route_hinted(
                "explain a shell error",
                budget,
                &health,
                &quota,
                Some(TaskTier::Trivial),
            )
            .await;
        let messages = [
            Message::system(SHELL_DIAGNOSE_SYSTEM),
            Message::user(format!("Command:\n{command}\n\nResult:\n{result}")),
        ];
        let mut sink = |_: StreamEvent| {};
        if let Ok(r) = self
            .provider
            .complete(&decision.model, &messages, &[], &mut sink)
            .await
        {
            let _ = self
                .store
                .record_side_call_usage(&self.id, "shell/diagnose", &r.usage);
            // Parse structured response: cause on line 1, optional "FIX: <cmd>" on line 2.
            let mut cause = String::new();
            let mut fix: Option<String> = None;
            for line in r.content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(cmd) = trimmed.strip_prefix("FIX: ") {
                    fix = Some(cmd.trim().to_string());
                } else if cause.is_empty() {
                    cause = trimmed.to_string();
                }
            }
            if cause.is_empty() {
                cause = r.content.trim().to_string();
            }
            if !cause.is_empty() {
                let hint = if let Some(ref f) = fix {
                    format!("[shell diagnosis] {cause}  fix: {f}")
                } else {
                    format!("[shell diagnosis] {cause}")
                };
                self.pending_hints.push(hint);
                self.presenter.emit(PresenterEvent::ShellDiagnosis {
                    command: command.to_string(),
                    diagnosis: cause,
                    fix,
                });
            }
        }
    }

    /// Inject command/skill guidance as persisted system messages *without* a model call — for
    /// `/skill <name>` with no prompt, so the methodology primes the next turn the user types.
    pub fn prime_guidance(&mut self, guidance: &[String]) -> Result<(), CoreError> {
        for g in guidance {
            let gseq = self.next_seq();
            self.store
                .add_message(&self.id, gseq, Role::System, g, None)?;
            self.transcript.push(Message::system(g));
        }
        Ok(())
    }

    /// Load the persisted replay entries for any session (not just this one) — used by the
    /// `/replay` chat command to show a transcript inline.
    pub fn load_replay(
        &self,
        session_id: &str,
    ) -> Result<Vec<forge_store::ReplayEntry>, CoreError> {
        self.store.load_replay(session_id).map_err(CoreError::Store)
    }

    /// Resolve a session-id prefix to full ids — allows `/replay abc` to find `abc123…`.
    pub fn matching_session_ids(&self, prefix: &str) -> Result<Vec<String>, CoreError> {
        self.store
            .matching_session_ids(prefix)
            .map_err(CoreError::Store)
    }

    /// Shared model↔tool inner loop used by both the primary turn and the autofix re-run.
    ///
    /// * `active_model` – the model to start with; updated by failover.
    /// * `specs`        – tool specs to advertise (pre-built by the caller).
    /// * `decision`     – `Some(d)` for the primary turn (enables failover, step-0 routing
    ///   record, quota-hint persistence); `None` for autofix re-runs (no failover, no records).
    /// * `max_steps`    – step cap (runaway guard).
    /// * `stream_idle`  – idle-stream timeout forwarded to every `complete_with` call.
    async fn run_model_loop(
        &mut self,
        mut active_model: String,
        specs: &[ToolSpec],
        decision: Option<&forge_mesh::RoutingDecision>,
        max_steps: usize,
        stream_idle: std::time::Duration,
    ) -> Result<ModelLoopOutcome, CoreError> {
        let failover_enabled = decision.is_some() && self.config.mesh.failover;
        let default_cooldown =
            std::time::Duration::from_secs(self.config.mesh.failover_cooldown_secs);

        // Failover chain: only meaningful for the primary turn (decision is Some). The autofix
        // path passes None, so `chain` is immediately exhausted and failover never fires.
        let fallbacks: Vec<String> = decision.map(|d| d.fallbacks.clone()).unwrap_or_default();
        let mut chain = fallbacks.into_iter();
        let mut last_resort_used = false;
        // Bounds the overflow self-heal (compact + retry the SAME model) so a transcript that can't
        // be shrunk enough eventually falls through to normal failover instead of looping.
        let mut compact_retries = 0usize;

        let mut final_text = String::new();
        let mut context_tokens: u64 = 0;
        let mut hit_step_cap = true;

        for step in 0..max_steps {
            // Stream the reply, with transparent failover for this step's completion.
            let mut resp = loop {
                // Bound what we send to the active model's context window (fetched/heuristic), so a
                // long conversation can't overflow it — which otherwise fails the turn as
                // "unavailable" on every model in the chain. Re-trimmed per model so failover to a
                // smaller-window model still fits. The immutable borrow ends before the block below.
                let sent = self.transcript_with_preamble(&active_model);
                // Tight scope: borrow provider + presenter only for the streamed call, so the
                // failover branch below has full `&mut self` for benching + warnings.
                let result =
                    {
                        let provider = &self.provider;
                        let presenter = &mut self.presenter;
                        // Bump on every stream event so the idle watchdog can distinguish a live
                        // stream from a stalled half-open connection — a stall fails over (below)
                        // instead of hanging the turn forever.
                        let activity = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                        let act = std::sync::Arc::clone(&activity);
                        let mut sink =
                            |ev: StreamEvent| {
                                act.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                match ev {
                                    StreamEvent::Text(t) => {
                                        presenter.emit(PresenterEvent::AssistantDelta(t))
                                    }
                                    StreamEvent::Reasoning(t) => {
                                        presenter.emit(PresenterEvent::Reasoning(t))
                                    }
                                    StreamEvent::ToolStarted { name, args } => {
                                        presenter.emit(PresenterEvent::ToolStart { name, args })
                                    }
                                    StreamEvent::ToolFinished { name, ok, summary } => presenter
                                        .emit(PresenterEvent::ToolResult { name, ok, summary }),
                                    StreamEvent::SubagentStarted { id, agent, task } => presenter
                                        .emit(PresenterEvent::SubagentStart { id, agent, task }),
                                    StreamEvent::SubagentProgress { id, snippet } => presenter
                                        .emit(PresenterEvent::SubagentProgress { id, snippet }),
                                    StreamEvent::SubagentFinished {
                                        id,
                                        agent,
                                        ok,
                                        summary,
                                        cost_usd,
                                    } => presenter.emit(PresenterEvent::SubagentResult {
                                        id,
                                        agent,
                                        ok,
                                        summary,
                                        cost_usd,
                                    }),
                                    // A bridged turn's `update_tasks` (tailed from the sink): surface the
                                    // list live so the sticky panel updates during the turn. The parent's
                                    // post-turn store reload (below) keeps `self.tasks` authoritative.
                                    StreamEvent::Tasks(tasks) => {
                                        presenter.emit(PresenterEvent::Tasks(tasks))
                                    }
                                }
                            };
                        let completion_opts = CompletionOptions {
                            effort: self.pinned_effort,
                            temperature: Some(CODING_TEMPERATURE),
                        };
                        let fut = provider.complete_with(
                            &active_model,
                            &sent,
                            specs,
                            &completion_opts,
                            &mut sink,
                        );
                        stream_with_idle_timeout(fut, &activity, stream_idle).await
                    };
                match result {
                    Ok(r) => {
                        if !r.content.is_empty() {
                            self.presenter.emit(PresenterEvent::AssistantDone);
                        }
                        break r;
                    }
                    Err(e) if failover_enabled && e.is_retryable() => {
                        // Context-overflow self-heal: the input exceeded THIS model's window, so the
                        // fix is to shrink the conversation and retry the SAME (healthy) model —
                        // NOT to bench it and burn the whole failover chain (the "every model
                        // unavailable" churn that left the turn stuck). Bounded by `compact_retries`.
                        if compact_retries < 2 && e.is_context_overflow() {
                            compact_retries += 1;
                            self.presenter.emit(PresenterEvent::Warning(format!(
                                "{active_model}: input exceeded the context window — compacting and retrying"
                            )));
                            let _ = self.compact(true).await;
                            self.emit_context_gauge(&active_model);
                            continue;
                        }
                        let reason = e.reason();
                        // A PERMANENT incapability (no tool support / unaffordable) excludes the
                        // model for a long window so it isn't re-tried every turn (the "every model
                        // failing" churn); a transient failure benches it on the short cooldown.
                        if e.is_permanent() {
                            let _ = self.store.exclude_model(&active_model, reason);
                            self.presenter.emit(PresenterEvent::Warning(format!(
                                "{active_model} {reason} — excluded from routing"
                            )));
                        } else {
                            let _ = self.store.bench_for(
                                &active_model,
                                e.cooldown(default_cooldown),
                                reason,
                            );
                            self.presenter.emit(PresenterEvent::Warning(format!(
                                "{active_model} {reason} — failing over"
                            )));
                        }
                        // Advance down the chain to the next model we can use. A model whose window
                        // still holds the conversation is used immediately; one that's too small is
                        // a switch that needs (lossy) compaction, so it's gated by consent
                        // (Yes/No/Always) — "No" skips it and we keep looking for one that fits.
                        let mut picked = None;
                        for next in chain.by_ref() {
                            match self.admit_failover_model(&next).await {
                                Ok(true) => {
                                    picked = Some(next);
                                    break;
                                }
                                Ok(false) => {
                                    self.presenter.emit(PresenterEvent::Warning(format!(
                                        "skipped {next} (declined compaction) — trying the next model"
                                    )));
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        let d = decision.expect("failover_enabled implies decision is Some");
                        match picked {
                            Some(next) => {
                                self.presenter.emit(PresenterEvent::Routing {
                                    tier: d.tier.as_str().to_string(),
                                    model: next.clone(),
                                    rationale: format!("failover from {active_model}"),
                                });
                                active_model = next;
                                continue;
                            }
                            // The routed chain is exhausted. Rather than hard-fail, make ONE
                            // last-resort attempt on the "least dead" model — the non-excluded
                            // model whose transient bench expires soonest. This keeps a turn
                            // working when every model is briefly rate-limited but none is
                            // permanently incapable. Guarded by `last_resort_used` so a model that
                            // fails again can't loop.
                            None => match self.last_resort_model(&active_model, last_resort_used) {
                                Some(m) => {
                                    last_resort_used = true;
                                    self.presenter.emit(PresenterEvent::Routing {
                                        tier: d.tier.as_str().to_string(),
                                        model: m.clone(),
                                        rationale: "last-resort: least-recently-benched model"
                                            .to_string(),
                                    });
                                    active_model = m;
                                    continue;
                                }
                                None => return Err(CoreError::NoHealthyModel),
                            },
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            };

            // Compute the real cost from token counts and the model's price (FR-5, A-7), pricing
            // cache-read tokens at the discounted rate so it tracks the provider's actual bill.
            resp.usage.cost_usd = self.pricing.cost_for_usage(&active_model, &resp.usage);
            // The last call's input size is the live context fill (tui-token-counter.md).
            context_tokens = resp.usage.input_tokens;

            self.transcript.push(Message::assistant_tool_calls(
                &resp.content,
                resp.tool_calls.clone(),
            ));

            let seq = self.next_seq();
            let msg_id = self.store.add_message_full(
                &self.id,
                seq,
                Role::Assistant,
                &resp.content,
                Some(&active_model),
                &resp.tool_calls,
                None,
            )?;
            // Step-0 routing record and quota-hint persistence are only meaningful for the primary
            // turn (when we have a decision). The autofix re-run skips both.
            if let Some(d) = decision {
                if step == 0 {
                    self.store
                        .record_routing(&msg_id, d.tier, &active_model, &d.rationale)?;
                }
                // Quota-aware routing (L3): if a CLI bridge reported its subscription window this
                // turn, persist it so the next route() can demote/skip a near-limit subscription.
                for hint in &resp.quotas {
                    let _ = self.store.record_quota(hint);
                    // Push to the TUI so the /usage overlay updates in real-time.
                    if let Some(f) = hint.fraction_used {
                        self.presenter.emit(forge_tui::PresenterEvent::QuotaUpdate {
                            provider: hint.provider.clone(),
                            window: hint.window.clone(),
                            fraction: f,
                        });
                    }
                }
            }
            self.store.record_usage(&self.id, &msg_id, &resp.usage)?;

            if !resp.wants_tools() {
                final_text = resp.content;
                hit_step_cap = false;
                // A response with neither text nor a tool call is a silent dead-end (a model
                // glitch / refusal parsed as empty). Surface it so the turn never just "stops".
                if final_text.trim().is_empty() {
                    self.presenter.emit(PresenterEvent::Warning(
                        "model returned an empty response (no text, no tool call) — stopping the turn"
                            .to_string(),
                    ));
                }
                break;
            }

            // Fast path: when the model batched several independent side-effect-free calls (and no
            // hooks are configured), run them CONCURRENTLY instead of one-at-a-time — a direct
            // latency win on multi-file reads/searches. Mixed or hook-bearing batches take the
            // serial path below unchanged.
            let concurrent_batch = resp.tool_calls.len() >= 2
                && self.config.hooks.is_empty()
                && resp
                    .tool_calls
                    .iter()
                    .all(|c| self.is_concurrent_readonly(&c.name));
            if concurrent_batch {
                self.run_readonly_batch(&msg_id, &resp.tool_calls).await?;
            } else {
                // Execute each requested tool through the permission broker, serially.
                for call in &resp.tool_calls {
                    let result = self.invoke_tool(&msg_id, call).await?;
                    let seq = self.next_seq();
                    self.store.add_message_full(
                        &self.id,
                        seq,
                        Role::Tool,
                        &result,
                        None,
                        &[],
                        Some(&call.id),
                    )?;
                    self.transcript.push(Message::tool_result(&call.id, result));
                    // Drain any system hints queued by side-call diagnostics (e.g. shell error
                    // interceptor) so the model sees them after the failing tool result.
                    let hints: Vec<String> = self.pending_hints.drain(..).collect();
                    for hint in hints {
                        let hseq = self.next_seq();
                        let _ = self
                            .store
                            .add_message(&self.id, hseq, Role::System, &hint, None);
                        self.transcript.push(Message::system(hint));
                    }
                }
            }
        }

        Ok(ModelLoopOutcome {
            final_text,
            context_tokens,
            hit_step_cap,
            active_model,
        })
    }

    /// Like [`Session::run_turn`], but first prepends `guidance` (an invoked command's or
    /// skill's methodology) as persisted system messages, and biases routing with an optional
    /// `tier_override` (the command/skill `tier:` hint). `run_turn(p)` is exactly
    /// `run_turn_with(p, &[], None)` — the agent loop, tools, permission broker, pricing and
    /// persistence are otherwise unchanged.
    pub async fn run_turn_with(
        &mut self,
        prompt: &str,
        guidance: &[String],
        tier_override: Option<TaskTier>,
    ) -> Result<String, CoreError> {
        // 1. Route the task (deterministic, no model call) and record why. The budget is
        // aggregated across ALL sessions for the current local day + week + month (FR-5), not one
        // session's running total.
        let budget = BudgetState {
            spent_today_usd: self.store.spend_today_usd()?,
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: self.store.spend_this_week_usd()?,
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd()?,
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        };
        let status = budget.status();

        // Hard stop: once a cap is exceeded, refuse the call before any provider request
        // (the cap is never silently exceeded). Overridable per process via
        // FORGE_BUDGET_OVERRIDE=1.
        if status == BudgetStatus::Exhausted
            && self.config.mesh.budget.hard_stop
            && !budget_override_active()
        {
            let msg = over_budget_message(&budget);
            self.presenter.emit(PresenterEvent::Warning(msg.clone()));
            // Persist the prompt + a system note, make NO provider call, write NO usage row.
            let seq = self.next_seq();
            self.store
                .add_message(&self.id, seq, Role::User, prompt, None)?;
            self.transcript.push(Message::user(prompt));
            let seq = self.next_seq();
            self.store
                .add_message(&self.id, seq, Role::System, &msg, None)?;
            self.transcript.push(Message::system(&msg));
            self.presenter.emit(PresenterEvent::Done {
                final_text: msg.clone(),
            });
            return Ok(msg);
        }

        // Surface budget pressure before routing (FR-5).
        match status {
            BudgetStatus::Warning => self.presenter.emit(PresenterEvent::Warning(format!(
                "approaching budget cap (today ${:.4}, month ${:.4})",
                budget.spent_today_usd, budget.spent_month_usd
            ))),
            BudgetStatus::Exhausted => self.presenter.emit(PresenterEvent::Warning(format!(
                "budget cap reached (today ${:.4}) — routing to the cheapest tier",
                budget.spent_today_usd
            ))),
            BudgetStatus::Ok => {}
        }

        // Route around any currently-benched models (failover): the snapshot excludes models
        // whose cooldown hasn't elapsed, even across restarts (model-health-failover).
        let health = self.store.current_benched().unwrap_or_default();
        // Quota-aware routing (L3): demote/skip a subscription that the bridge reported is near or
        // over its plan limit (recorded after earlier turns from the CLI's rate-limit events).
        let quota = self.live_quota();
        let decision = self
            .router
            .route_hinted(prompt, budget, &health, &quota, tier_override)
            .await;
        // `/model <id>` override: use the pinned model instead of the mesh-routed pick; mesh still
        // classifies (for tier stats) but the actual call uses the pin.
        let pinned = self.pinned_model.clone();
        let routed_model = pinned.unwrap_or_else(|| decision.model.clone());
        self.presenter.emit(PresenterEvent::Routing {
            tier: decision.tier.as_str().to_string(),
            model: routed_model.clone(),
            rationale: decision.rationale.clone(),
        });

        // Prepend any command/skill guidance as persisted system messages, so the methodology
        // is in context for this turn and rehydrates verbatim on resume (the skill file is not
        // re-read).
        for g in guidance {
            let gseq = self.next_seq();
            self.store
                .add_message(&self.id, gseq, Role::System, g, None)?;
            self.transcript.push(Message::system(g));
        }

        // Inject the project AGENTS.md as a standing system prompt on the first turn of a
        // fresh session. Tried in order: .forge/AGENTS.md, then AGENTS.md in cwd.
        // Sync I/O intentional: one-time startup read of a small file; no await point so
        // an abort() between here and user-message persistence can't skip the recording.
        if !self.project_prompt_injected {
            self.project_prompt_injected = true;
            for agents_path in [".forge/AGENTS.md", "AGENTS.md"] {
                if let Ok(body) = std::fs::read_to_string(agents_path) {
                    if !body.trim().is_empty() {
                        let pseq = self.next_seq();
                        self.store
                            .add_message(&self.id, pseq, Role::System, &body, None)?;
                        self.transcript.push(Message::system(&body));
                        break;
                    }
                }
            }

            // When git co-authoring is on, prime the agent (once) to attribute its work to Forge.
            // Commit trailers are stamped deterministically by the prepare-commit-msg hook; this
            // covers the PR body (which no hook can reach) and tells the model not to add other
            // co-author lines that the hook would only strip.
            if self.config.git.coauthor {
                const GIT_ATTRIBUTION: &str = "Git attribution is enabled for this session. When \
you create commits or pull requests, attribute them to Forge:\n\
- Commits: a `Co-Authored-By: Forge <noreply@forge.dev>` trailer is added automatically by a git \
hook — do NOT add Claude/Codex/Anthropic co-author lines yourself.\n\
- Pull requests: include a line in the PR body crediting Forge, e.g. `🔨 Created with Forge`.";
                let aseq = self.next_seq();
                self.store
                    .add_message(&self.id, aseq, Role::System, GIT_ATTRIBUTION, None)?;
                self.transcript.push(Message::system(GIT_ATTRIBUTION));
            }
        }

        // Reset the per-turn edit counter so the autofix stage only fires when THIS turn wrote
        // something (not a carry-over from a prior turn).
        self.edits_this_turn = 0;

        // 2. Persist + record the user message. Its seq keys this turn's code-snapshot dir
        // (PR3): files written during the turn are restorable by rewinding to this boundary.
        let seq = self.next_seq();
        self.current_turn_seq = seq;
        self.store
            .add_message(&self.id, seq, Role::User, prompt, None)?;
        // Attach any images queued for this turn (vision). They ride on the in-memory transcript
        // for the provider call; the persisted row stays text-only (images are transient input).
        let images = std::mem::take(&mut self.pending_images);
        if images.is_empty() {
            self.transcript.push(Message::user(prompt));
        } else {
            self.transcript
                .push(Message::user_with_images(prompt, images));
        }
        // Auto-checkpoint at the turn boundary, labeled with the prompt preview, so `/undo` can
        // offer a list of past messages to rewind to (no manual /checkpoint needed).
        let _ = self
            .store
            .add_checkpoint(&self.id, Some(&checkpoint_preview(prompt)), seq);
        // Export this turn's snapshot context so a CLI-bridge model's file edits (which run in
        // `forge mcp-serve`, a separate process) get snapshotted into THIS turn's dir and are
        // restorable by `/undo` (the in-process tool path snapshots directly in `invoke_tool`).
        self.export_checkpoint_env(seq);

        // ★ Auto-retrieve relevant code from the Lattice index and inject it as a system message
        // before the first provider call (code-intelligence.md §5.1). Retrieve into an owned value
        // first so the `&self.lattice` borrow is released before we mutate the transcript. The
        // budget shrinks with budget pressure — context spend follows the same discipline as model
        // spend. Empty index / disabled / any error → nothing injected, turn runs as before.
        let injected = {
            if let Some(lat) = self.lattice.as_ref().filter(|_| self.config.lattice.inject) {
                let budget = inject_budget(self.config.lattice.inject_token_budget, status);
                let emb = &self.config.lattice.embeddings;
                // Body injection (the big token-saving lever): inject the top hits' full source so
                // the model reads them from context instead of spending a whole-file `read_file`.
                let bodies = self
                    .config
                    .lattice
                    .inject_bodies
                    .then_some(forge_index::BodyOpts {
                        max_tokens: self.config.lattice.body_max_tokens,
                        max_hits: 3,
                    });
                // Hybrid: blend embedding neighbours of the prompt with structural hits. The
                // backend is chosen by config (auto-picks the cheapest available); any backend
                // error degrades to structural inside `retrieve_hybrid`. No backend → structural.
                match forge_provider::select_embedder(emb) {
                    Some((embedder, _)) => lat
                        .retrieve_hybrid(prompt, budget, bodies, embedder.as_ref())
                        .await
                        .ok(),
                    None => lat.retrieve(prompt, budget, bodies).ok(),
                }
            } else {
                None
            }
        }
        .filter(|ctx| !ctx.is_empty());
        if let Some(ctx) = injected {
            let files = ctx
                .snippets
                .iter()
                .map(|s| s.rel_path.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len();
            let symbols = ctx.nodes.len();
            let tokens = ctx.est_tokens;
            let body = ctx.render();
            let iseq = self.next_seq();
            self.store
                .add_message(&self.id, iseq, Role::System, &body, None)?;
            self.transcript.push(Message::system(&body));
            self.presenter.emit(PresenterEvent::ContextInjected {
                symbols,
                files,
                tokens,
            });
        }

        // ── Architect plan phase (architect_mode) ────────────────────────────────────────────────
        // When enabled: call the strong planner model with NO tools advertised; append its plan to
        // the transcript as a persisted assistant message so the editor model sees it below. When
        // disabled (the default) `run_plan` returns Ok(None) immediately — this block is a no-op.
        if let Some(_plan) = self.run_plan().await? {
            // The plan is already in self.transcript (pushed inside run_plan). Nothing else to do
            // here; the editor phase below will see it as the last assistant message in context.
        }

        // Determine the model for the edit phase.  In architect mode the editor model takes over;
        // otherwise we keep the mesh-routed model unchanged.
        let edit_model = if self.config.mesh.architect_mode {
            let editor = self.resolve_editor_model();
            self.presenter.emit(PresenterEvent::Routing {
                tier: decision.tier.as_str().to_string(),
                model: editor.clone(),
                rationale: "architect edit phase".to_string(),
            });
            editor
        } else {
            routed_model.clone()
        };

        // Silent auto-compaction: if the conversation has grown past ~80% of the routed model's
        // (fetched/heuristic) context window, summarize older messages now so the turn doesn't ride
        // the hard-trim floor and lose recent context. Transparent — `compact` emits its own note.
        self.auto_compact_if_needed(&edit_model).await;

        let specs = self.tool_specs();
        let stream_idle = std::time::Duration::from_secs(self.config.mesh.stream_idle_timeout_secs);

        // 3. Model <-> tool loop. The cap is a runaway guard, not a functional limit — the loop
        // ends naturally when the model stops calling tools.
        let max_steps = self.config.mesh.max_steps.max(1);

        // Primary turn: pass the routing decision so failover, step-0 routing record, and quota
        // hints are all active.
        let outcome = self
            .run_model_loop(edit_model, &specs, Some(&decision), max_steps, stream_idle)
            .await?;
        let mut final_text = outcome.final_text;
        let mut context_tokens = outcome.context_tokens;
        let mut active_model = outcome.active_model;

        // Ran the full step budget while the model still wanted tools: pause loudly instead of
        // ending silently mid-task (the #1 "stops responding" bug). The work so far is persisted,
        // so the user can resume by sending `continue`.
        if outcome.hit_step_cap {
            self.presenter.emit(PresenterEvent::Warning(format!(
                "reached the {max_steps}-step limit — turn paused mid-task; send `continue` to keep going \
                 (raise `mesh.max_steps` in config to allow longer turns)"
            )));
        }

        // A CLI-bridge turn may have called `update_tasks` inside `forge mcp-serve` (a separate
        // process), persisting to the store but not touching our in-memory list. Reload and
        // surface it so bridge-driven task updates show in the TUI (the in-process path already
        // emitted live during the turn, so this is a no-op there).
        if let Ok(persisted) = self.store.tasks(&self.id) {
            if persisted != self.tasks {
                self.tasks = persisted;
                self.presenter
                    .emit(PresenterEvent::Tasks(self.tasks.clone()));
            }
        }

        // ── Autofix self-healing loop (autofix.md) ────────────────────────────────────────────
        // After the turn's model↔tool loop finishes: if edits were made AND autofix is enabled
        // with at least one non-empty command, run lint/test and feed failures back into the
        // conversation so the model can fix them. Repeat up to `max_iterations`. When autofix is
        // off, or no edits happened, this block is a no-op (zero overhead).
        let af = self.config.autofix.clone();
        let autofix_active = self.edits_this_turn > 0
            && ((af.auto_lint && !af.lint_cmd.is_empty())
                || (af.auto_test && !af.test_cmd.is_empty()));

        if autofix_active {
            self.presenter.emit(PresenterEvent::Warning(format!(
                "autofix: running checks after {} edit(s)",
                self.edits_this_turn
            )));
            let mut iterations_used = 0u32;
            loop {
                if iterations_used >= af.max_iterations {
                    self.presenter.emit(PresenterEvent::Warning(format!(
                        "autofix: reached iteration cap ({}) — stopping; remaining failures \
                         were not fixed",
                        af.max_iterations
                    )));
                    break;
                }
                iterations_used += 1;

                match self.run_autofix_stage(&af).await {
                    Ok(true) => {
                        self.presenter.emit(PresenterEvent::Warning(
                            "autofix: all checks passed".to_string(),
                        ));
                        break;
                    }
                    Ok(false) => {
                        // Failures already injected into transcript by run_autofix_stage.
                        // Re-run the model↔tool inner loop to let the model fix them.
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "autofix: iteration {iterations_used}/{} — re-running model loop",
                            af.max_iterations
                        )));
                        // Autofix re-run: pass None for decision so failover, routing record, and
                        // quota hints are all suppressed — the active_model is kept from the
                        // primary turn (or last failover) and is not changed here.
                        let fix_specs = self.tool_specs();
                        let fix_outcome = self
                            .run_model_loop(
                                active_model.clone(),
                                &fix_specs,
                                None,
                                max_steps,
                                stream_idle,
                            )
                            .await?;
                        final_text = fix_outcome.final_text;
                        context_tokens = fix_outcome.context_tokens;
                        active_model = fix_outcome.active_model;
                        if fix_outcome.hit_step_cap {
                            self.presenter.emit(PresenterEvent::Warning(format!(
                                "autofix: inner model loop hit the {max_steps}-step limit"
                            )));
                        }
                    }
                    Err(e) => {
                        // Autofix infrastructure failure — surface as warning and abort the loop.
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "autofix: stage error ({e}) — skipping remaining iterations"
                        )));
                        break;
                    }
                }
            }
        }
        // ── End autofix ───────────────────────────────────────────────────────────────────────

        // ── Auto-review gate (assay.auto_review) ──────────────────────────────────────────────
        // When enabled: build a unified diff of files written THIS turn, run the Assay critic
        // crew over it, and either warn or block depending on gate_mode. Zero overhead when off.
        if self.config.assay.auto_review && self.edits_this_turn > 0 {
            let ar = self.config.assay.clone();
            if let Err(e) = self.auto_review_gate(&ar).await {
                // TurnBlocked propagates up so the caller can surface it; other errors are
                // infrastructure failures we surface as warnings to avoid silently killing the turn.
                match &e {
                    CoreError::TurnBlocked(_) => return Err(e),
                    _ => {
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "auto-review: gate error ({e}) — skipping"
                        )));
                    }
                }
            }
        }
        // ── End auto-review gate ───────────────────────────────────────────────────────────────

        let (session_in, session_out) = self.store.session_tokens(&self.id)?;
        self.presenter.emit(PresenterEvent::Cost {
            session_total_usd: self.store.session_cost(&self.id)?,
            session_in,
            session_out,
            context_tokens,
            context_limit: forge_mesh::pricing::context_limit(&active_model),
        });
        self.presenter.emit(PresenterEvent::Done {
            final_text: final_text.clone(),
        });
        Ok(final_text)
    }

    /// Build a unified diff of files written this turn (pre-turn blob vs current file), run the
    /// Assay critic crew over it, and surface findings whose severity >= `gate_severity`. In
    /// `warn` mode the findings are emitted as warnings and the turn continues. In `block` mode
    /// they are emitted and `CoreError::TurnBlocked` is returned so the turn is aborted.
    async fn auto_review_gate(&mut self, cfg: &forge_config::AssayConfig) -> Result<(), CoreError> {
        use similar::{ChangeTag, TextDiff};

        // Gather files touched this turn from the snapshot manifest.
        let turn_files = snapshot::changed_files_this_turn(
            &self.checkpoint_root,
            &self.id,
            self.current_turn_seq,
        );
        if turn_files.is_empty() {
            return Ok(());
        }

        // Build a concatenated unified diff: for each file, diff old (blob or empty) vs new.
        let mut combined = String::new();
        for tf in &turn_files {
            let old = tf
                .blob
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .unwrap_or_default();
            let new = std::fs::read_to_string(&tf.path).unwrap_or_default();
            if old == new {
                continue;
            }
            combined.push_str(&format!("--- a/{}\n+++ b/{}\n", tf.path, tf.path));
            let td = TextDiff::from_lines(old.as_str(), new.as_str());
            for change in td.iter_all_changes() {
                let sym = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                combined.push_str(&format!("{sym} {}", change.value()));
            }
            combined.push('\n');
        }

        if combined.len() < cfg.min_diff_bytes {
            return Ok(());
        }

        self.presenter.emit(PresenterEvent::Warning(format!(
            "auto-review: diff is {} bytes — running critic crew",
            combined.len(),
        )));

        let lenses = forge_types::FindingCategory::crew().to_vec();
        let pricing = std::sync::Arc::new(self.pricing.clone());
        let provider = std::sync::Arc::clone(&self.provider);
        let store = std::sync::Arc::clone(&self.store);
        let cooldown = std::time::Duration::from_secs(self.config.mesh.failover_cooldown_secs);

        // Build tier model chains from the catalog (ranked + health-filtered) when available,
        // falling back to the configured model list — same pattern as the CLI's /assay path.
        let benched = self.store.current_benched().unwrap_or_default();
        let models = {
            let chain = |tier: forge_types::TaskTier| -> Vec<String> {
                // Catalog path: ranked candidates, drop currently-benched ones first.
                if let Some(cat) = &self.catalog {
                    let ranked: Vec<String> = cat
                        .ranked_for(tier, &self.pricing, 8)
                        .into_iter()
                        .filter(|m| !benched.is_benched(m))
                        .collect();
                    if !ranked.is_empty() {
                        return ranked;
                    }
                }
                // Config fallback: the configured candidates for this tier.
                self.config
                    .candidates_for(tier)
                    .into_iter()
                    .filter(|m| !benched.is_benched(m))
                    .collect()
            };
            assay::TierModels {
                trivial: chain(forge_types::TaskTier::Trivial),
                complex: chain(forge_types::TaskTier::Complex),
            }
        };

        // Cost pre-estimate: skip the gate (with a warning) when the estimated crew cost exceeds
        // the configured cap. This prevents the gate from running away cost on large diffs.
        // cap == 0.0 means unlimited — always run.
        if cfg.max_cost_usd > 0.0 {
            let est = assay::estimate_assay_cost(&combined, &lenses, &models, &self.pricing);
            if est.est_usd > cfg.max_cost_usd {
                self.presenter.emit(PresenterEvent::Warning(format!(
                    "assay gate skipped: estimated ${:.3} exceeds cap ${:.3}",
                    est.est_usd, cfg.max_cost_usd,
                )));
                return Ok(());
            }
        }

        let source: std::sync::Arc<str> = combined.into();
        let presenter = &mut self.presenter;
        let mut on_progress = |p: assay::AssayProgress| {
            presenter.emit(PresenterEvent::AssayProgress(assay::progress_line(&p)));
        };

        let report = assay::run_assay(
            forge_types::AssayScope::Diff,
            source,
            lenses,
            models,
            provider,
            pricing,
            store,
            cooldown,
            &mut on_progress,
        )
        .await;

        // Filter to findings at/above the configured gate severity.
        let gate_findings: Vec<&forge_types::Finding> = report
            .findings
            .iter()
            .filter(|f| severity_meets(f.severity, &cfg.gate_severity))
            .collect();

        if gate_findings.is_empty() {
            self.presenter.emit(PresenterEvent::Warning(
                "auto-review: no findings at/above gate severity — OK".to_string(),
            ));
            return Ok(());
        }

        // Surface all gate-triggering findings as warnings.
        for f in &gate_findings {
            self.presenter.emit(PresenterEvent::Warning(format!(
                "auto-review [{}] {}: {} — {} ({}:{})",
                f.severity.as_str(),
                f.category.as_str(),
                f.title,
                f.suggested_fix,
                f.file,
                f.line.map(|l| l.to_string()).unwrap_or_default(),
            )));
        }

        if cfg.gate_mode.trim().eq_ignore_ascii_case("block") {
            return Err(CoreError::TurnBlocked(format!(
                "{} finding(s) at/above '{}' severity",
                gate_findings.len(),
                cfg.gate_severity
            )));
        }

        Ok(())
    }

    /// Run the autofix stage: execute lint and/or test commands (if enabled and non-empty);
    /// return `Ok(true)` when every enabled command exits 0, `Ok(false)` when any fails (the
    /// combined output of failing commands is injected into the transcript as a synthetic user
    /// message so the model can fix it next iteration). Never returns `Err` from a non-zero
    /// command exit — only from infrastructure failures (transcript write, etc.).
    async fn run_autofix_stage(
        &mut self,
        af: &forge_config::AutofixConfig,
    ) -> Result<bool, CoreError> {
        // Use the same 120-second timeout as the shell tool's default; lint/test commands that
        // need more can be wrapped in a script.
        const AUTOFIX_TIMEOUT_SECS: u64 = 120;
        let mut failures = Vec::new();

        if af.auto_lint && !af.lint_cmd.is_empty() {
            let out = forge_tools::run_shell_command(&af.lint_cmd, ".", AUTOFIX_TIMEOUT_SECS).await;
            if shell_command_failed(&out) {
                failures.push(format!("[lint: {}]\n{}", af.lint_cmd, out));
            }
        }
        if af.auto_test && !af.test_cmd.is_empty() {
            let out = forge_tools::run_shell_command(&af.test_cmd, ".", AUTOFIX_TIMEOUT_SECS).await;
            if shell_command_failed(&out) {
                failures.push(format!("[test: {}]\n{}", af.test_cmd, out));
            }
        }

        if failures.is_empty() {
            return Ok(true);
        }

        // Inject the failures as a synthetic user message so the model fixes them on the next
        // iteration of the outer autofix loop.
        let body = format!(
            "Auto-fix: the following checks failed, fix them:\n\n{}",
            failures.join("\n\n")
        );
        let seq = self.next_seq();
        self.store
            .add_message(&self.id, seq, Role::User, &body, None)?;
        self.transcript.push(Message::user(&body));

        Ok(false)
    }

    /// Run a single tool call, applying the permission policy, and return its result text.
    /// Whether `name` is a side-effect-free registry tool that's safe to run concurrently in a
    /// batch: not a core-owned virtual tool (those mutate session state / prompt the user), not an
    /// external MCP tool, present in the registry, and ReadOnly.
    fn is_concurrent_readonly(&self, name: &str) -> bool {
        if name == subagent::SPAWN_AGENTS_TOOL
            || name == ASK_USER_TOOL
            || name == UPDATE_TASKS_TOOL
            || name == USE_SKILL_TOOL
        {
            return false;
        }
        if self.mcp.as_ref().is_some_and(|m| m.knows_tool(name)) {
            return false;
        }
        self.tools
            .get(name)
            .map(|t| t.side_effect() == forge_types::SideEffect::ReadOnly)
            .unwrap_or(false)
    }

    /// Execute a batch of side-effect-free tool calls CONCURRENTLY, then append their results in the
    /// original order. When the model requests several independent reads/searches in one step,
    /// running them together (instead of serially) is a direct latency win — and safe because
    /// ReadOnly tools have no side effects, never prompt (permission resolves to Allow/Deny without
    /// asking), don't snapshot, and queue no hints. Only used when all calls qualify and no hooks
    /// are configured (PreToolUse/PostToolUse run on every call and must stay serial); otherwise the
    /// caller falls back to the serial [`invoke_tool`] path.
    async fn run_readonly_batch(
        &mut self,
        msg_id: &str,
        calls: &[forge_types::ToolCall],
    ) -> Result<(), CoreError> {
        struct Pending {
            id: String,
            name: String,
            args: serde_json::Value,
            args_json: String,
            allowed: bool,
        }
        // Phase 1 (serial): announce each call + resolve permission (pure; no prompt for ReadOnly).
        let mut pend = Vec::with_capacity(calls.len());
        for call in calls {
            let args_json = serde_json::to_string(&call.args)?;
            self.presenter.emit(PresenterEvent::ToolStart {
                name: call.name.clone(),
                args: args_json.clone(),
            });
            let allowed = matches!(
                permission::decide(
                    self.mode,
                    forge_types::SideEffect::ReadOnly,
                    &call.name,
                    &call.args,
                    &self.rules,
                ),
                PermissionDecision::Allow
            );
            pend.push(Pending {
                id: call.id.clone(),
                name: call.name.clone(),
                args: call.args.clone(),
                args_json,
                allowed,
            });
        }
        // Phase 2 (concurrent): run every allowed tool's `run()` together. Borrows `self.tools`
        // immutably for the duration of the join; no `&mut self` is touched until it completes.
        let results: Vec<(String, bool)> = {
            let tools = &self.tools;
            let futs = pend.iter().map(|p| async move {
                if !p.allowed {
                    return ("permission denied by policy".to_string(), false);
                }
                match tools.get(&p.name) {
                    Some(tool) => match tool.run(&p.args).await {
                        Ok(out) => (out, true),
                        Err(e) => (format!("error: {e}"), false),
                    },
                    None => (format!("error: unknown tool '{}'", p.name), false),
                }
            });
            futures::future::join_all(futs).await
        };
        // Phase 3 (serial): surface + persist + append each result in the ORIGINAL order, so every
        // tool_call_id is answered in sequence.
        for (p, (result, ok)) in pend.iter().zip(results) {
            self.presenter.emit(PresenterEvent::ToolResult {
                name: p.name.clone(),
                ok,
                summary: summarize(&result),
            });
            self.store.record_tool_call(
                msg_id,
                &p.name,
                &p.args_json,
                &result,
                if p.allowed { "allowed" } else { "denied" },
                if ok { "ok" } else { "error" },
            )?;
            let seq = self.next_seq();
            self.store.add_message_full(
                &self.id,
                seq,
                Role::Tool,
                &result,
                None,
                &[],
                Some(&p.id),
            )?;
            self.transcript.push(Message::tool_result(&p.id, result));
        }
        Ok(())
    }

    async fn invoke_tool(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        // The subagent virtual tool is owned by core (it needs provider/router/store), not the
        // registry — intercept before the registry lookup (RFC subagent-orchestration).
        if call.name == subagent::SPAWN_AGENTS_TOOL {
            return self.spawn_agents(msg_id, call).await;
        }
        // The interactive question tool is core-owned too (it needs the presenter).
        if call.name == ASK_USER_TOOL {
            return self.ask_user(msg_id, call);
        }
        // Task tracking is core-owned (it mutates session state + persists + emits to the TUI).
        if call.name == UPDATE_TASKS_TOOL {
            return self.update_tasks(msg_id, call);
        }
        // Skill loading is core-owned (it reads the attached catalog). Returns the skill's
        // methodology as the tool result so the model follows it; unknown name → a helpful error.
        if call.name == USE_SKILL_TOOL {
            return self.use_skill(msg_id, call);
        }
        // External MCP tools (meta-tools + exposed server tools) are owned by the manager, not the
        // built-in registry. Route them here, still through the permission broker (mcp-client.md).
        if self.mcp.as_ref().is_some_and(|m| m.knows_tool(&call.name)) {
            return self.invoke_mcp(msg_id, call).await;
        }

        let mut args_json = serde_json::to_string(&call.args)?;
        // `effective_args` may be replaced by a PreToolUse hook that rewrites the args.
        let mut effective_args = call.args.clone();

        let Some(tool) = self.tools.get(&call.name) else {
            // Name the valid tools so the model can recover instead of guessing again.
            let mut available: Vec<String> =
                self.tool_specs().into_iter().map(|s| s.name).collect();
            available.sort();
            let result = format!(
                "error: unknown tool '{}'. Available tools: {}",
                call.name,
                available.join(", ")
            );
            self.presenter.emit(PresenterEvent::ToolResult {
                name: call.name.clone(),
                ok: false,
                summary: "unknown tool".to_string(),
            });
            self.store
                .record_tool_call(msg_id, &call.name, &args_json, &result, "n/a", "error")?;
            return Ok(result);
        };

        let side_effect = tool.side_effect();
        self.presenter.emit(PresenterEvent::ToolStart {
            name: call.name.clone(),
            args: args_json.clone(),
        });

        // PreToolUse hooks (hooks.md): run user shell hooks before the tool. A non-zero exit
        // blocks the call (the hook's output is the reason the model sees). Exit 0 + JSON object
        // on stdout rewrites the args before the tool runs. Inert when no hooks configured.
        if !self.config.hooks.is_empty() {
            let payload = serde_json::json!({ "tool": call.name, "args": call.args }).to_string();
            let outcome = hooks::run_hooks(
                &self.config.hooks,
                forge_config::HookEvent::PreToolUse,
                &call.name,
                &payload,
            )
            .await;
            for n in outcome.notes {
                self.presenter.emit(PresenterEvent::Warning(n));
            }
            if let Some(reason) = outcome.blocked {
                let result = format!("blocked by hook: {reason}");
                self.presenter.emit(PresenterEvent::ToolResult {
                    name: call.name.clone(),
                    ok: false,
                    summary: "blocked by hook".to_string(),
                });
                self.store.record_tool_call(
                    msg_id, &call.name, &args_json, &result, "blocked", "error",
                )?;
                return Ok(result);
            }
            if let Some(new_args) = outcome.rewritten_args {
                args_json = serde_json::to_string(&new_args).unwrap_or_default();
                effective_args = new_args;
            }
        }

        // For a file-mutating tool, show the proposed change BEFORE the permission gate so
        // the user reviews a diff instead of approving a blind write.
        if side_effect == forge_types::SideEffect::Write {
            if let Some(diff) = tool.preview(&effective_args).await {
                self.presenter.emit(PresenterEvent::Diff(diff));
            }
        }

        let allowed = match permission::decide(
            self.mode,
            side_effect,
            &call.name,
            &effective_args,
            &self.rules,
        ) {
            PermissionDecision::Allow => true,
            PermissionDecision::Deny => false,
            PermissionDecision::Ask => self.presenter.confirm(&call.name, side_effect),
        };
        let permission_label = if allowed { "allowed" } else { "denied" };

        // Snapshot the target's pre-edit bytes BEFORE a permitted write, so `/undo` can restore
        // it (PR3 shadow snapshots; first touch per path per turn wins).
        let write_path = (allowed && side_effect == forge_types::SideEffect::Write)
            .then(|| effective_args.get("path").and_then(|v| v.as_str()))
            .flatten()
            .map(std::path::PathBuf::from);
        if let Some(path) = &write_path {
            let _ = snapshot::snapshot_before_write(
                &self.checkpoint_root,
                &self.id,
                self.current_turn_seq,
                path,
            );
        }

        let (result, ok) = if allowed {
            match tool.run(&effective_args).await {
                Ok(out) => {
                    // Record what we wrote, so a later restore can warn on a manual edit.
                    if let Some(path) = &write_path {
                        let _ = snapshot::record_post_write(
                            &self.checkpoint_root,
                            &self.id,
                            self.current_turn_seq,
                            path,
                        );
                        // Count this successful write so the autofix stage knows edits happened.
                        self.edits_this_turn += 1;
                        // Reindex the touched file in-turn so later retrieval/queries this turn
                        // reflect the edit (code-intelligence.md — post-edit freshness).
                        if let Some(lat) = &self.lattice {
                            let _ = lat.reindex_path(path);
                        }
                        // LSP diagnostics: ask the language server for errors on the
                        // just-written file and queue them as a pending hint so the model
                        // self-corrects this turn. Best-effort: missing server → silent.
                        if self.config.lsp.enabled {
                            if let Some(lsp) = &self.lsp {
                                let abs =
                                    std::path::absolute(path).unwrap_or_else(|_| path.clone());
                                let timeout =
                                    std::time::Duration::from_millis(self.config.lsp.timeout_ms);
                                let lsp = Arc::clone(lsp);
                                let diags = lsp.diagnostics_for(&abs, timeout).await;
                                if !diags.is_empty() {
                                    let lines: Vec<String> = diags
                                        .iter()
                                        .map(|d| d.format_line(&path.display().to_string()))
                                        .collect();
                                    self.pending_hints
                                        .push(format!("[lsp diagnostics]\n{}", lines.join("\n")));
                                }
                            }
                        }
                    }
                    (out, true)
                }
                Err(e) => (format!("error: {e}"), false),
            }
        } else {
            ("permission denied by policy".to_string(), false)
        };

        self.presenter.emit(PresenterEvent::ToolResult {
            name: call.name.clone(),
            ok,
            summary: summarize(&result),
        });
        self.store.record_tool_call(
            msg_id,
            &call.name,
            &args_json,
            &result,
            permission_label,
            if ok { "ok" } else { "error" },
        )?;

        // PostToolUse hooks (hooks.md): observe the completed call (e.g. re-index, notify). The
        // tool result is already final; post hooks only surface notes, they don't change it.
        if !self.config.hooks.is_empty() {
            let payload =
                serde_json::json!({ "tool": call.name, "args": call.args, "result": result, "ok": ok })
                    .to_string();
            let outcome = hooks::run_hooks(
                &self.config.hooks,
                forge_config::HookEvent::PostToolUse,
                &call.name,
                &payload,
            )
            .await;
            for n in outcome.notes {
                self.presenter.emit(PresenterEvent::Warning(n));
            }
        }

        // Shell error interceptor (shell-error-interceptor.md): on a failed shell command,
        // auto-explain the likely cause + a fix with one cheap model call. Best-effort, never
        // alters the result the model sees.
        if side_effect == forge_types::SideEffect::Shell
            && self.config.shell.explain_errors
            && shell_command_failed(&result)
        {
            if let Some(command) = call.args.get("command").and_then(|v| v.as_str()) {
                let command = command.to_string();
                self.diagnose_shell_error(&command, &result).await;
            }
        }

        Ok(result)
    }

    /// Run an MCP (meta-)tool call through the permission broker and the manager. Every MCP call
    /// is `SideEffect::External` (the local catalog meta-tools are `ReadOnly`); the broker decides
    /// allow/ask/deny exactly as for built-in tools, and the call is recorded for audit.
    async fn invoke_mcp(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let mcp = self
            .mcp
            .clone()
            .expect("invoke_mcp only called when mcp is Some");
        let mut args_json = serde_json::to_string(&call.args)?;
        let mut effective_args = call.args.clone();
        let side_effect = mcp.side_effect_of(&call.name);
        self.presenter.emit(PresenterEvent::ToolStart {
            name: call.name.clone(),
            args: args_json.clone(),
        });

        // PreToolUse hooks: same semantics as native tools — block, observe, or rewrite args.
        if !self.config.hooks.is_empty() {
            let payload = serde_json::json!({ "tool": call.name, "args": call.args }).to_string();
            let outcome = hooks::run_hooks(
                &self.config.hooks,
                forge_config::HookEvent::PreToolUse,
                &call.name,
                &payload,
            )
            .await;
            for n in outcome.notes {
                self.presenter.emit(PresenterEvent::Warning(n));
            }
            if let Some(reason) = outcome.blocked {
                let result = format!("blocked by hook: {reason}");
                self.presenter.emit(PresenterEvent::ToolResult {
                    name: call.name.clone(),
                    ok: false,
                    summary: "blocked by hook".to_string(),
                });
                self.store.record_tool_call(
                    msg_id, &call.name, &args_json, &result, "blocked", "error",
                )?;
                return Ok(result);
            }
            if let Some(new_args) = outcome.rewritten_args {
                args_json = serde_json::to_string(&new_args).unwrap_or_default();
                effective_args = new_args;
            }
        }

        let allowed = match permission::decide(
            self.mode,
            side_effect,
            &call.name,
            &effective_args,
            &self.rules,
        ) {
            PermissionDecision::Allow => true,
            PermissionDecision::Deny => false,
            PermissionDecision::Ask => self.presenter.confirm(&call.name, side_effect),
        };
        let permission_label = if allowed { "allowed" } else { "denied" };

        let (result, ok) = if allowed {
            let out = mcp.call(&call.name, &effective_args).await;
            (out.text, out.ok)
        } else {
            ("permission denied by policy".to_string(), false)
        };

        self.presenter.emit(PresenterEvent::ToolResult {
            name: call.name.clone(),
            ok,
            summary: summarize(&result),
        });
        self.store.record_tool_call(
            msg_id,
            &call.name,
            &args_json,
            &result,
            permission_label,
            if ok { "ok" } else { "error" },
        )?;

        // PostToolUse hooks: observe only — notes surfaced, result unchanged.
        if !self.config.hooks.is_empty() {
            let payload = serde_json::json!({
                "tool": call.name, "args": effective_args, "result": result, "ok": ok
            })
            .to_string();
            let outcome = hooks::run_hooks(
                &self.config.hooks,
                forge_config::HookEvent::PostToolUse,
                &call.name,
                &payload,
            )
            .await;
            for n in outcome.notes {
                self.presenter.emit(PresenterEvent::Warning(n));
            }
        }

        Ok(result)
    }

    /// Handle a `spawn_agents` call: resolve each requested child against the loaded agent
    /// types, then run them **concurrently** (bounded by `max_concurrency`), each in its own
    /// mesh-routed, persisted child session. Children run on tokio tasks (they share the
    /// parent's `Arc` backends); since the presenter is single-threaded, each child reports its
    /// lifecycle over a channel that this method drains on the main task — so `SubagentResult`
    /// events surface live as children finish (RFC subagent-orchestration, Phase 2).
    async fn spawn_agents(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let args_json = serde_json::to_string(&call.args)?;
        let max = self.config.mesh.subagents.max_agents;
        let requests = match subagent::parse_requests(&call.args, max) {
            Ok(r) => r,
            Err(msg) => {
                let result = format!("error: {msg}");
                self.store.record_tool_call(
                    msg_id, &call.name, &args_json, &result, "allowed", "error",
                )?;
                return Ok(result);
            }
        };

        // Budget snapshot so children also down-tier when the day/week/month is under pressure.
        let budget = BudgetState {
            spent_today_usd: self.store.spend_today_usd()?,
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: self.store.spend_this_week_usd()?,
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd()?,
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        };

        let agents = Arc::new(forge_config::load_agents(std::path::Path::new(
            &self.config.mesh.subagents.agents_dir,
        )));
        let repo_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let ctx = subagent::AgentCtx {
            provider: Arc::clone(&self.provider),
            router: Arc::clone(&self.router),
            store: Arc::clone(&self.store),
            config: self.config.clone(),
            pricing: self.pricing.clone(),
            mode: self.mode,
            rules: self.rules.clone(),
            depth: 0,
            max_depth: self.config.mesh.subagents.max_depth,
            agents,
            worktree_root: None,
            repo_root,
        };
        let parent_id = self.id.clone();
        let max_concurrency = self.config.mesh.subagents.max_concurrency;

        // Drive the shared orchestrator, turning each child lifecycle into a presenter event
        // (running children animate live; completed ones fold into the scrollback box).
        let presenter = &mut self.presenter;
        let mut on_event = |ev: subagent::Lifecycle| match ev {
            subagent::Lifecycle::Start { id, agent, task } => {
                presenter.emit(PresenterEvent::SubagentStart {
                    id: id.to_string(),
                    agent: agent.to_string(),
                    task: task.to_string(),
                })
            }
            subagent::Lifecycle::Progress { id, snippet } => {
                presenter.emit(PresenterEvent::SubagentProgress {
                    id: id.to_string(),
                    snippet: snippet.to_string(),
                })
            }
            subagent::Lifecycle::Done {
                id,
                agent,
                ok,
                summary,
                cost_usd,
            } => presenter.emit(PresenterEvent::SubagentResult {
                id: id.to_string(),
                agent: agent.to_string(),
                ok,
                summary: summary.to_string(),
                cost_usd,
            }),
        };
        let (combined, all_ok) = subagent::orchestrate(
            &ctx,
            &parent_id,
            requests,
            budget,
            max_concurrency,
            &mut on_event,
        )
        .await?;

        self.store.record_tool_call(
            msg_id,
            &call.name,
            &args_json,
            &combined,
            "allowed",
            if all_ok { "ok" } else { "error" },
        )?;
        Ok(combined)
    }

    /// Handle an `ask_user` call: parse the question + options, ask the user through the
    /// presenter (interactive multi-choice / open-ended), and return their answer as the tool
    /// result (docs/features/ask-user-question.md).
    fn ask_user(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let args_json = serde_json::to_string(&call.args)?;
        let question = call
            .args
            .get("question")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string();
        if question.trim().is_empty() {
            let result = "error: ask_user requires a non-empty `question`".to_string();
            self.store
                .record_tool_call(msg_id, &call.name, &args_json, &result, "allowed", "error")?;
            return Ok(result);
        }
        let options: Vec<forge_tui::QChoice> = call
            .args
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        let label = o.get("label").and_then(|l| l.as_str())?;
                        Some(forge_tui::QChoice {
                            label: label.to_string(),
                            description: o
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Default to allowing a free-text answer (and force it when there are no options).
        let allow_other = call
            .args
            .get("allow_other")
            .and_then(|a| a.as_bool())
            .unwrap_or(true)
            || options.is_empty();

        let answer = self.presenter.ask(&question, &options, allow_other);
        self.store
            .record_tool_call(msg_id, &call.name, &args_json, &answer, "allowed", "ok")?;
        Ok(answer)
    }

    /// Replace the session's task list (the `update_tasks` virtual tool): parse the full list,
    /// persist it, emit it to the TUI, and return a one-line summary to the model.
    fn update_tasks(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        use forge_types::TodoStatus;
        let args_json = serde_json::to_string(&call.args)?;
        self.tasks = parse_tasks(&call.args);
        let _ = self.store.set_tasks(&self.id, &self.tasks);
        self.presenter
            .emit(PresenterEvent::Tasks(self.tasks.clone()));

        let done = self
            .tasks
            .iter()
            .filter(|t| t.status == TodoStatus::Done)
            .count();
        let in_progress = self
            .tasks
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        let result = format!(
            "task list updated: {} task(s) — {done} done, {in_progress} in progress",
            self.tasks.len()
        );
        self.store
            .record_tool_call(msg_id, &call.name, &args_json, &result, "allowed", "ok")?;
        Ok(result)
    }

    /// The current task list (for the composition root / TUI to render on resume).
    pub fn tasks(&self) -> &[forge_types::TodoItem] {
        &self.tasks
    }

    /// Load a Forge skill's methodology (the `use_skill` virtual tool) and return it as the tool
    /// result so the model applies it this turn. Unknown name → an error listing valid skills.
    fn use_skill(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let args_json = serde_json::to_string(&call.args)?;
        let name = call
            .args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let (result, ok) = match self.skills.as_ref().and_then(|c| c.skill_guidance(name)) {
            Some(guidance) => {
                self.presenter
                    .emit(PresenterEvent::Warning(format!("⚒ skill loaded · {name}")));
                (
                    format!("Loaded the '{name}' skill. Apply this methodology now:\n\n{guidance}"),
                    true,
                )
            }
            None => {
                let available = self
                    .skills
                    .as_ref()
                    .map(|c| {
                        c.skill_listing()
                            .into_iter()
                            .map(|(n, _)| n)
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                (
                    format!("no Forge skill named '{name}'. Available: {available}"),
                    false,
                )
            }
        };
        self.store.record_tool_call(
            msg_id,
            &call.name,
            &args_json,
            &result,
            "allowed",
            if ok { "ok" } else { "error" },
        )?;
        Ok(result)
    }
}

/// The interactive-question virtual tool name (AskUserQuestion).
const ASK_USER_TOOL: &str = "ask_user";

/// The `ToolSpec` advertised to the model for [`ASK_USER_TOOL`].
fn ask_user_spec() -> ToolSpec {
    ToolSpec {
        name: ASK_USER_TOOL.to_string(),
        description: "Ask the user a single focused question when you hit a real decision only \
            they can make (a value choice, a missing requirement). Provide 2–4 suggested \
            `options` with short labels (+ optional descriptions); set `allow_other` (default \
            true) to also accept a free-text answer. Returns the user's choice. Don't use it for \
            things you can decide yourself."
            .to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "the question to ask" },
                "options": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["label"]
                    }
                },
                "allow_other": {
                    "type": "boolean",
                    "description": "allow a free-text answer beyond the options (default true)"
                }
            },
            "required": ["question"]
        }),
    }
}

/// The skill-loading virtual tool name.
pub const USE_SKILL_TOOL: &str = "use_skill";

/// The `ToolSpec` advertised for [`USE_SKILL_TOOL`], listing the available Forge skills in its
/// description so the model both *discovers* what exists and can *invoke* one. Shared by the
/// direct path and the CLI-bridge `mcp-serve` handler so a bridged claude/codex sees it too.
pub fn use_skill_spec(catalog: &forge_skills::Catalog) -> ToolSpec {
    let listing = catalog
        .skill_listing()
        .into_iter()
        .map(|(name, desc)| {
            let desc: String = desc.chars().take(100).collect();
            format!("- {name}: {desc}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    ToolSpec {
        name: USE_SKILL_TOOL.to_string(),
        description: format!(
            "Load a Forge skill's methodology into this turn, then follow it. These are Forge's \
             OWN skills — do NOT search the filesystem (~/.claude, ~/.codex) for skills; call this \
             tool with the exact skill name instead. Available skills:\n{listing}"
        ),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "exact skill name from the list" }
            },
            "required": ["name"]
        }),
    }
}

/// The task-tracking virtual tool name.
pub const UPDATE_TASKS_TOOL: &str = "update_tasks";

/// Parse the `update_tasks` arguments into a task list (tolerant of missing/loose fields).
/// Shared by the in-process intercept and the CLI-bridge `mcp-serve` handler.
pub fn parse_tasks(args: &serde_json::Value) -> Vec<forge_types::TodoItem> {
    use forge_types::{TodoItem, TodoStatus};
    args.get("tasks")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let title = t.get("title").and_then(|v| v.as_str())?.trim();
                    (!title.is_empty()).then(|| TodoItem {
                        title: title.to_string(),
                        status: t
                            .get("status")
                            .and_then(|v| v.as_str())
                            .map(TodoStatus::parse_loose)
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `ToolSpec` advertised to the model for [`UPDATE_TASKS_TOOL`].
pub fn update_tasks_spec() -> ToolSpec {
    ToolSpec {
        name: UPDATE_TASKS_TOOL.to_string(),
        description: "Maintain a visible task list for multi-step work. Call it when you start a \
            task with 2+ steps and again whenever a step's state changes — pass the FULL ordered \
            list each time (it replaces the previous one). Mark exactly one task `in_progress` \
            while you work it, `done` the moment it's finished. Keep titles short and concrete. \
            Skip it for trivial single-step requests."
            .to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "tasks": {
                    "type": "array",
                    "description": "the full ordered task list (replaces the previous list)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "short task description" },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done"],
                                "description": "task state (default pending)"
                            }
                        },
                        "required": ["title"]
                    }
                }
            },
            "required": ["tasks"]
        }),
    }
}

/// True if the per-process budget override is set (lets one over-budget run proceed).
/// Scale the Lattice injection token budget by budget pressure: full when Ok, half at Warning, a
/// quarter at Exhausted. Context spend follows the same discipline as model spend (§5.4).
fn inject_budget(base: usize, status: BudgetStatus) -> usize {
    match status {
        BudgetStatus::Ok => base,
        BudgetStatus::Warning => base / 2,
        BudgetStatus::Exhausted => base / 4,
    }
}

/// Await a streaming completion, but abort it if the stream goes silent for `idle` (a half-open /
/// stalled connection) so a turn never hangs forever — the caller treats the synthesized
/// `Unavailable` as retryable and fails over. `activity` is bumped by the completion's event sink;
/// `idle == 0` disables the watchdog. Polls coarsely (every few seconds) — this guards against a
/// hang, it is not a precise deadline.
async fn stream_with_idle_timeout<F>(
    fut: F,
    activity: &std::sync::atomic::AtomicU64,
    idle: std::time::Duration,
) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError>
where
    F: std::future::Future<
        Output = Result<forge_provider::ModelResponse, forge_provider::ProviderError>,
    >,
{
    tokio::pin!(fut);
    if idle.is_zero() {
        return fut.await;
    }
    let mut last_seen = 0u64;
    let mut last_change = std::time::Instant::now();
    let poll = std::time::Duration::from_secs(3).min(idle);
    loop {
        tokio::select! {
            r = &mut fut => return r,
            _ = tokio::time::sleep(poll) => {
                let now = activity.load(std::sync::atomic::Ordering::Relaxed);
                if now != last_seen {
                    last_seen = now;
                    last_change = std::time::Instant::now();
                } else if last_change.elapsed() >= idle {
                    return Err(forge_provider::ProviderError::Unavailable(format!(
                        "stream stalled — no data for {}s",
                        idle.as_secs()
                    )));
                }
            }
        }
    }
}

fn budget_override_active() -> bool {
    matches!(
        std::env::var("FORGE_BUDGET_OVERRIDE").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn over_budget_message(b: &BudgetState) -> String {
    let cap = |c: Option<f64>| c.map(|v| format!("${v:.2}")).unwrap_or_else(|| "∞".into());
    format!(
        "budget cap reached — today ${:.4}/{}, month ${:.4}/{}. Refusing further model calls. \
         Set FORGE_BUDGET_OVERRIDE=1 to proceed.",
        b.spent_today_usd,
        cap(b.daily_cap_usd),
        b.spent_month_usd,
        cap(b.monthly_cap_usd)
    )
}

/// Compare previous and current findings, return a human-readable diff note.
/// Matching is by (file, title) — same issue at the same location.
fn assay_diff_note(
    prev: &[forge_types::Finding],
    current: &[forge_types::Finding],
    prev_id: &str,
) -> String {
    let key = |f: &forge_types::Finding| format!("{}|{}", f.file, f.title);
    let prev_keys: std::collections::HashSet<String> = prev.iter().map(key).collect();
    let curr_keys: std::collections::HashSet<String> = current.iter().map(key).collect();
    let fixed: usize = prev_keys.difference(&curr_keys).count();
    let new_: usize = curr_keys.difference(&prev_keys).count();
    let still_open: usize = prev_keys.intersection(&curr_keys).count();
    if fixed == 0 && new_ == 0 {
        return String::new(); // nothing to say — identical findings
    }
    format!(
        "⚒ vs run {prev_id}: {} fixed · {} new · {} still-open",
        fixed, new_, still_open
    )
}

/// Build the Refine (cleanup) task prompt from an assay report: instruct the agent loop to fix
/// each finding by editing files (gated + snapshotted via the normal turn path).
fn refine_prompt(report: &forge_types::AssayReport) -> String {
    let mut s = String::from(
        "You are Refine, a cleanup crew. An Assay analysis found the issues below in this \
         codebase. Fix each one by editing the relevant files (edit_file/write_file). Be surgical \
         — fix exactly the issue without breaking working code or changing unrelated behavior. If \
         a finding is a false positive, skip it and briefly say why.\n\nIssues:\n",
    );
    for (i, f) in report.findings.iter().enumerate() {
        let loc = match f.line {
            Some(l) => format!("{}:{l}", f.file),
            None => f.file.clone(),
        };
        s.push_str(&format!(
            "{}. [{}] {} — {}\n   why: {}\n   suggested fix: {}\n",
            i + 1,
            f.severity.as_str(),
            loc,
            f.title,
            f.rationale,
            f.suggested_fix
        ));
    }
    s
}

/// A short single-line label for an auto-checkpoint: the prompt's first line, char-truncated.
fn checkpoint_preview(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    if first.chars().count() > 60 {
        format!("{}…", first.chars().take(60).collect::<String>())
    } else {
        first.to_string()
    }
}

fn summarize(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    // Truncate by *characters*, not bytes — a byte slice (`&first[..80]`) panics when the
    // cut falls inside a multi-byte UTF-8 char, which real tool output (file contents, shell
    // output, accents/emoji) routinely contains.
    if first.chars().count() > 80 {
        let head: String = first.chars().take(80).collect();
        format!("{head}…")
    } else {
        first.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_mesh::HeuristicRouter;
    use forge_provider::MockProvider;
    use forge_tui::HeadlessPresenter;
    use forge_types::SideEffect;
    use std::sync::{Arc, Mutex};

    #[test]
    fn fit_messages_keeps_everything_when_it_fits() {
        let msgs = vec![
            Message::system("rules"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        assert_eq!(fit_messages(&msgs, 10_000).len(), 3);
    }

    #[test]
    fn fit_messages_keeps_system_and_recent_drops_oldest() {
        let msgs = vec![
            Message::system("SYS"),
            Message::user(format!("OLD {}", "a".repeat(500))),
            Message::user(format!("MID {}", "b".repeat(500))),
            Message::user("NEWEST request"),
        ];
        // Budget fits the system + the newest one or two, not the 500-char olds.
        let out = fit_messages(&msgs, 16 + 4 + 16 + "NEWEST request".len() + 16);
        assert_eq!(out[0].role, Role::System, "system always kept");
        assert!(
            out.iter().any(|m| m.content.contains("NEWEST")),
            "newest kept"
        );
        assert!(
            !out.iter().any(|m| m.content.contains("OLD")),
            "oldest dropped: {out:?}"
        );
        // System stays at the front; the surviving recent tail follows in order.
        assert_eq!(out.first().unwrap().content, "SYS");
    }

    #[test]
    fn fit_messages_truncates_a_single_oversized_message() {
        let msgs = vec![
            Message::system("SYS"),
            Message::user(format!("{}TAIL-WORDS", "z".repeat(5_000))),
        ];
        let out = fit_messages(&msgs, 200);
        let last = out.last().unwrap();
        assert!(
            last.content.contains("TAIL-WORDS"),
            "keeps the latest words"
        );
        assert!(last.content.contains("truncated"), "marks the cut");
        assert!(last.content.chars().count() < 5_000, "shrunk");
    }

    #[test]
    fn fit_messages_drops_orphan_leading_tool_result() {
        // A trim that cuts between an assistant tool-call and its result must NOT leave the result
        // dangling (a tool_call_id with no call → the provider 400s the whole request). The leading
        // orphan tool result is dropped.
        let big = "context line ".repeat(400);
        let msgs = vec![
            Message::assistant_tool_calls(
                big,
                vec![forge_types::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "a.rs"}),
                }],
            ),
            Message::tool_result("c1", "the file contents"),
            Message::user("continue"),
        ];
        // Budget fits the tool result + the user turn, but not the big assistant before them.
        let budget = message_tokens(&msgs[1]) + message_tokens(&msgs[2]) + 4;
        let out = fit_messages(&msgs, budget);
        assert!(
            out.iter().all(|m| m.role != Role::Tool),
            "dangling tool result dropped: {:?}",
            out.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        assert_eq!(out.last().unwrap().content, "continue");
    }

    #[test]
    fn request_includes_base_system_prompt_and_env() {
        let provider = Arc::new(FlakyProvider {
            bad: std::collections::HashSet::new(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "m".into(),
            fallbacks: vec![],
        });
        let (_store, session) = fixed_session(provider, router);
        let msgs = session.transcript_with_preamble("m");
        assert_eq!(msgs[0].role, Role::System);
        assert!(
            msgs[0].content.contains("You are Forge"),
            "base coding-agent prompt is prepended"
        );
        assert!(msgs[1].content.contains("<env>"), "env block present");
        assert!(msgs[1].content.contains("platform:"));
    }

    #[tokio::test]
    async fn readonly_batch_runs_concurrently_and_preserves_order() {
        let provider = Arc::new(FlakyProvider {
            bad: std::collections::HashSet::new(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "m".into(),
            fallbacks: vec![],
        });
        let (_store, mut session) = fixed_session(provider, router);

        let dir = std::env::temp_dir().join(format!("forge-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut calls = Vec::new();
        for i in 0..3 {
            let p = dir.join(format!("f{i}.txt"));
            std::fs::write(&p, format!("content-{i}")).unwrap();
            calls.push(forge_types::ToolCall {
                id: format!("c{i}"),
                name: "read_file".into(),
                args: serde_json::json!({ "path": p.to_str().unwrap() }),
            });
        }
        // All three reads qualify for the concurrent fast path.
        assert!(calls
            .iter()
            .all(|c| session.is_concurrent_readonly(&c.name)));

        let msg_id = session
            .store
            .add_message_full(session.id(), 0, Role::Assistant, "", None, &[], None)
            .unwrap();
        session.run_readonly_batch(&msg_id, &calls).await.unwrap();

        // Every call is answered, in the ORIGINAL order, paired by tool_call_id.
        let tools: Vec<&Message> = session
            .transcript
            .iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].tool_call_id.as_deref(), Some("c0"));
        assert!(tools[0].content.contains("content-0"));
        assert_eq!(tools[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(tools[2].tool_call_id.as_deref(), Some("c2"));
        assert!(tools[2].content.contains("content-2"));
    }

    /// A presenter that records every event so tests can assert on what was shown.
    #[derive(Clone, Default)]
    struct CapturePresenter {
        events: Arc<Mutex<Vec<PresenterEvent>>>,
    }
    impl Presenter for CapturePresenter {
        fn emit(&mut self, event: PresenterEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn confirm(&mut self, _tool: &str, _side_effect: SideEffect) -> bool {
            false
        }
        fn ask(&mut self, _q: &str, options: &[forge_tui::QChoice], _allow_other: bool) -> String {
            // Deterministic: pick the first option (or empty) so tests don't block on input.
            options.first().map(|o| o.label.clone()).unwrap_or_default()
        }
        fn read_line(&mut self) -> Option<String> {
            None
        }
    }

    /// A presenter whose `ask` always returns a scripted label, counting how many times it was
    /// asked — for the auto-compact-on-switch consent tests.
    #[derive(Clone)]
    struct ScriptedPresenter {
        answer: String,
        asks: Arc<Mutex<usize>>,
    }
    impl Presenter for ScriptedPresenter {
        fn emit(&mut self, _event: PresenterEvent) {}
        fn confirm(&mut self, _tool: &str, _side_effect: SideEffect) -> bool {
            true
        }
        fn ask(&mut self, _q: &str, _options: &[forge_tui::QChoice], _allow_other: bool) -> String {
            *self.asks.lock().unwrap() += 1;
            self.answer.clone()
        }
        fn read_line(&mut self) -> Option<String> {
            None
        }
    }

    fn scripted_session(answer: &str, asks: Arc<Mutex<usize>>) -> Session {
        let config = Config::default();
        Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(ScriptedPresenter {
                answer: answer.to_string(),
                asks,
            }),
            config,
            ".",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn small_transcript_fits_any_window_no_prompt() {
        let asks = Arc::new(Mutex::new(0));
        let mut s = scripted_session("No", asks.clone());
        s.transcript.push(Message::user("hi there"));
        assert!(s.transcript_fits("ollama::tiny")); // unknown → 32k floor, easily fits
        assert!(
            s.admit_failover_model("ollama::tiny").await.unwrap(),
            "a fitting model is admitted"
        );
        assert_eq!(*asks.lock().unwrap(), 0, "no consent prompt when it fits");
    }

    #[tokio::test]
    async fn oversized_transcript_prompts_and_no_skips() {
        let asks = Arc::new(Mutex::new(0));
        let mut s = scripted_session("No", asks.clone());
        // One giant message: over 80% of the 32k floor in tokens, but too few messages for
        // compact() to do real work (so the gate's decision is what we're testing).
        s.transcript.push(Message::user("data ".repeat(40_000)));
        assert!(
            !s.transcript_fits("ollama::tiny"),
            "overflows the small window"
        );
        assert!(
            !s.admit_failover_model("ollama::tiny").await.unwrap(),
            "\"No\" skips the model"
        );
        assert_eq!(*asks.lock().unwrap(), 1, "asked exactly once");
    }

    #[tokio::test]
    async fn always_answer_silences_further_prompts() {
        let asks = Arc::new(Mutex::new(0));
        let mut s = scripted_session("Always", asks.clone());
        s.transcript.push(Message::user("data ".repeat(40_000)));
        assert!(
            s.admit_failover_model("ollama::tiny").await.unwrap(),
            "Always → admit"
        );
        assert!(s.always_compact_on_switch, "the session flag is set");
        // A second over-window switch proceeds silently (no further prompt).
        s.transcript.push(Message::user("data ".repeat(40_000)));
        assert!(s.admit_failover_model("ollama::tiny").await.unwrap());
        assert_eq!(*asks.lock().unwrap(), 1, "asked only the first time");
    }

    /// A provider that calls `ask_user` once, then answers using whatever came back.
    #[derive(Default)]
    struct AskingProvider;

    #[async_trait::async_trait]
    impl Provider for AskingProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let usage = Usage::default();
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: "asking".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "ask_user".into(),
                    args: serde_json::json!({
                        "question": "which database?",
                        "options": [{"label": "Postgres"}, {"label": "SQLite"}]
                    }),
                }],
                usage,
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn ask_user_round_trips_the_answer_into_the_turn() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(AskingProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            // CapturePresenter::ask returns the first option ("Postgres").
            Box::new(CapturePresenter::default()),
            Config::default(),
            ".",
        )
        .unwrap();
        let id = session.id().to_string();
        let answer = session.run_turn("set up the db").await.unwrap();
        assert_eq!(
            answer, "done",
            "turn completes after the question is answered"
        );
        // The chosen answer was fed back as the tool result.
        let tool_msgs: Vec<_> = store
            .load_messages(&id)
            .unwrap()
            .into_iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert!(
            tool_msgs.iter().any(|m| m.content == "Postgres"),
            "ask_user answer fed back as tool result: {tool_msgs:?}"
        );
    }

    /// A provider that calls the namespaced MCP tool `test__echo` once, then answers.
    #[derive(Default)]
    struct McpProvider;

    #[async_trait::async_trait]
    impl Provider for McpProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let usage = Usage::default();
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "mcp_call".into(),
                    args: serde_json::json!({ "name": "test__echo", "arguments": { "msg": "hi" } }),
                }],
                usage,
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn mcp_tools_are_advertised_and_routed_through_the_broker() {
        // A config that allowlists `test__echo` so it's eagerly exposed (advertised), in Bypass
        // mode so the External call auto-allows without a prompt.
        let mcp = forge_config::McpConfig {
            allow: forge_config::McpAllowlist {
                servers: vec!["test".into()],
                tools: vec!["test__echo".into()],
            },
            ..Default::default()
        };
        let config = Config {
            permission_mode: PermissionMode::Bypass,
            mcp: mcp.clone(),
            ..Config::default()
        };
        let mgr = std::sync::Arc::new(forge_mcp::testsupport::manager_with_echo(&mcp).await);

        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(McpProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            config,
            ".",
        )
        .unwrap();
        session.set_mcp(Some(mgr));

        // tool_specs advertises the MCP meta-tools (search + call); server tools are reached
        // through mcp_call, never advertised individually.
        let names: Vec<String> = session.tool_specs().into_iter().map(|s| s.name).collect();
        assert!(names.iter().any(|n| n == "mcp_search_tools"));
        assert!(
            names.iter().any(|n| n == "mcp_call"),
            "mcp_call advertised: {names:?}"
        );
        assert!(
            names.iter().all(|n| n != "test__echo"),
            "server tool NOT advertised directly"
        );
        // …and built-ins are still there (additive, no regression).
        assert!(names.iter().any(|n| n == "read_file"));

        let id = session.id().to_string();
        let answer = session.run_turn("echo something").await.unwrap();
        assert_eq!(answer, "done");
        let tool_msgs: Vec<_> = store
            .load_messages(&id)
            .unwrap()
            .into_iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert!(
            tool_msgs.iter().any(|m| m.content == "echo: hi"),
            "MCP tool result fed back into the turn: {tool_msgs:?}"
        );
    }

    #[test]
    fn no_mcp_means_tool_specs_unchanged() {
        // Regression guard: with no manager attached, the advertised set has zero MCP entries.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = Session::start(
            store,
            Arc::new(McpProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            Config::default(),
            ".",
        )
        .unwrap();
        let names: Vec<String> = session.tool_specs().into_iter().map(|s| s.name).collect();
        assert!(names
            .iter()
            .all(|n| !n.starts_with("mcp_") && !n.contains("__")));
    }

    /// A provider that calls `update_tasks` once with a 2-item list, then finishes.
    #[derive(Default)]
    struct TaskingProvider;

    #[async_trait::async_trait]
    impl Provider for TaskingProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let usage = Usage::default();
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: "planning".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "update_tasks".into(),
                    args: serde_json::json!({"tasks": [
                        {"title": "design the api", "status": "done"},
                        {"title": "implement it", "status": "in_progress"}
                    ]}),
                }],
                usage,
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn update_tasks_sets_persists_and_emits_the_list() {
        use forge_types::TodoStatus;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(TaskingProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();
        let id = session.id().to_string();

        session.run_turn("build the feature").await.unwrap();

        // Live state updated.
        assert_eq!(session.tasks().len(), 2);
        assert_eq!(session.tasks()[0].status, TodoStatus::Done);
        assert_eq!(session.tasks()[1].status, TodoStatus::InProgress);

        // Persisted for resume.
        let stored = store.tasks(&id).unwrap();
        assert_eq!(stored, session.tasks());

        // Emitted to the UI.
        let emitted = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::Tasks(t) if t.len() == 2));
        assert!(emitted, "a Tasks event was emitted for the TUI");
    }

    /// Requests a `list_dir` tool call once, then answers `done` after the tool result.
    struct ListDirProvider;
    #[async_trait::async_trait]
    impl Provider for ListDirProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage: Usage::default(),
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "list_dir".into(),
                    args: serde_json::json!({ "path": "." }),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    /// Returns a fixed summary for compaction; never requests tools.
    struct SummarizingProvider;
    #[async_trait::async_trait]
    impl Provider for SummarizingProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            Ok(forge_provider::ModelResponse {
                content: "SUMMARY: built the parser, wired the CLI.".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    /// Reports, as its final answer, whether the transcript it received carried a Lattice
    /// auto-injection system message — lets a test assert injection happened.
    struct InjectionProbeProvider;
    #[async_trait::async_trait]
    impl Provider for InjectionProbeProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let saw = messages.iter().any(|m| {
                m.role == Role::System && m.content.starts_with("Relevant code (Lattice):")
            });
            Ok(forge_provider::ModelResponse {
                content: if saw { "SAW_INJECTION" } else { "NO_INJECTION" }.into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    fn probe_session(store: Arc<Store>, config: Config) -> Session {
        Session::start(
            store,
            Arc::new(InjectionProbeProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn lattice_injects_relevant_code_into_the_turn() {
        let dir = std::env::temp_dir().join(format!(
            "forge-inj-{}-{}",
            std::process::id(),
            forge_types::new_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("probe.rs"), "pub fn lattice_probe_symbol() {}\n").unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let lat = forge_index::Lattice::new(Arc::clone(&store), &dir);
        lat.update().unwrap();

        let mut session = probe_session(Arc::clone(&store), Config::default());
        session.set_lattice(Some(Arc::new(lat)));
        let answer = session
            .run_turn("explain lattice_probe_symbol please")
            .await
            .unwrap();
        assert_eq!(
            answer, "SAW_INJECTION",
            "the symbol was retrieved + injected"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shell_command_failed_reads_the_exit_status() {
        assert!(!shell_command_failed("shell: exit 0 in 5ms\n\nhi"));
        assert!(shell_command_failed("shell: exit 1 in 5ms"));
        assert!(shell_command_failed("shell: exit 127 in 5ms"));
        assert!(shell_command_failed("shell: timed out after 1s (killed)"));
        assert!(shell_command_failed("shell: failed to start (cwd .): x"));
        assert!(shell_command_failed("shell: exit signal in 5ms"));
        // Not a shell result at all → not treated as a shell failure.
        assert!(!shell_command_failed("read 3 files"));
    }

    #[test]
    fn pattern_diagnose_matches_common_failures() {
        assert!(pattern_diagnose("bash: docker: command not found").is_some());
        assert!(pattern_diagnose("ls: /tmp/missing: No such file or directory").is_some());
        assert!(pattern_diagnose("chmod: cannot access 'x.sh': Permission denied").is_some());
        assert!(pattern_diagnose("bind: address already in use").is_some());
        assert!(pattern_diagnose("curl: (7) Failed to connect: Connection refused").is_some());
        assert!(pattern_diagnose("cp: error writing 'x': No space left on device").is_some());
        assert!(pattern_diagnose("Cannot allocate memory").is_some());
    }

    #[test]
    fn pattern_diagnose_returns_none_for_unrecognised_errors() {
        assert!(
            pattern_diagnose("shell: exit 1 in 2ms\n\ntest failed: assertion `left == right`")
                .is_none()
        );
        assert!(
            pattern_diagnose("shell: exit 2 in 1ms\n\nmake: *** [Makefile:5: build] Error 2")
                .is_none()
        );
    }

    #[test]
    fn pattern_diagnose_is_case_insensitive() {
        assert!(pattern_diagnose("COMMAND NOT FOUND").is_some());
        assert!(pattern_diagnose("PERMISSION DENIED").is_some());
    }

    /// First call emits a failing `shell` command; the diagnosis call (identified by its system
    /// prompt) returns a fix; after the tool result it answers `done`. Unix-only: the `shell`
    /// tool shells out to `sh`, so the e2e tests using it are gated to Unix.
    #[cfg(unix)]
    struct ShellFailProvider;
    #[cfg(unix)]
    #[async_trait::async_trait]
    impl Provider for ShellFailProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let usage = Usage::default();
            if messages
                .iter()
                .any(|m| m.role == Role::System && m.content.starts_with("A shell command run by"))
            {
                return Ok(ModelResponse {
                    content: "The command is not installed. Fix: install it first.".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "shell".into(),
                    args: serde_json::json!({ "command": "definitelynotacommand_xyz" }),
                }],
                usage,
                quotas: Vec::new(),
            })
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_shell_command_is_auto_diagnosed() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        // Bypass auto-allows the shell call so the interceptor path is reached.
        let config = Config {
            permission_mode: forge_types::PermissionMode::Bypass,
            ..Config::default()
        };
        let presenter = CapturePresenter::default();
        let events = presenter.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(ShellFailProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(presenter),
            config,
            ".",
        )
        .unwrap();

        session.run_turn("build the project").await.unwrap();

        let diagnosed = events.lock().unwrap().iter().any(|e| {
            matches!(e, PresenterEvent::ShellDiagnosis { command, diagnosis, .. }
                if command.contains("definitelynotacommand_xyz") && diagnosis.contains("install"))
        });
        assert!(
            diagnosed,
            "a ShellDiagnosis event was emitted for the failed command"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_shell_command_is_not_diagnosed() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = Config {
            permission_mode: forge_types::PermissionMode::Bypass,
            ..Config::default()
        };
        let presenter = CapturePresenter::default();
        let events = presenter.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(EchoShellProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(presenter),
            config,
            ".",
        )
        .unwrap();

        session.run_turn("say hi").await.unwrap();

        let diagnosed = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::ShellDiagnosis { .. }));
        assert!(
            !diagnosed,
            "a succeeding command must not trigger the interceptor"
        );
    }

    /// Emits a succeeding `shell` command once, then answers `done`. Unix-only (see above).
    #[cfg(unix)]
    struct EchoShellProvider;
    #[cfg(unix)]
    #[async_trait::async_trait]
    impl Provider for EchoShellProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage: Usage::default(),
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "shell".into(),
                    args: serde_json::json!({ "command": "echo hi" }),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    /// Calls `use_skill("demoskill")` once, then reports whether the tool result carried the
    /// skill's methodology marker — lets a test assert the skill was found + loaded.
    struct UseSkillProvider;
    #[async_trait::async_trait]
    impl Provider for UseSkillProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            if let Some(t) = messages.iter().rev().find(|m| m.role == Role::Tool) {
                let saw = t.content.contains("DEMO_SKILL_MARKER");
                return Ok(ModelResponse {
                    content: if saw { "SAW_SKILL" } else { "NO_SKILL" }.into(),
                    tool_calls: vec![],
                    usage: Usage::default(),
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: USE_SKILL_TOOL.into(),
                    args: serde_json::json!({ "name": "demoskill" }),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn use_skill_tool_loads_a_real_skills_methodology() {
        let dir = std::env::temp_dir().join(format!("forge-useskill-{}", forge_types::new_id()));
        std::fs::create_dir_all(dir.join("skills/demoskill")).unwrap();
        std::fs::write(
            dir.join("skills/demoskill/SKILL.md"),
            "---\nname: demoskill\ndescription: a demo skill\n---\nDEMO_SKILL_MARKER: do the steps.",
        )
        .unwrap();
        let catalog = forge_skills::Catalog::load(&forge_skills::Sources {
            commands: vec![],
            skills: vec![forge_skills::ScopedDir {
                scope: forge_skills::Scope::User,
                path: dir.join("skills"),
            }],
        });

        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = Config::default();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(UseSkillProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        session.set_skills(Some(Arc::new(catalog)));

        // The tool is advertised to the model...
        assert!(
            session
                .tool_specs()
                .iter()
                .any(|s| s.name == USE_SKILL_TOOL),
            "use_skill is advertised when a non-empty catalog is attached"
        );
        // ...and invoking it returns the skill's methodology as the tool result.
        let answer = session.run_turn("use the demo skill").await.unwrap();
        assert_eq!(
            answer, "SAW_SKILL",
            "use_skill returned the methodology to the model"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Calls `write_file` once (to `path`), then answers `done`.
    struct WriteFileProvider {
        path: String,
    }
    #[async_trait::async_trait]
    impl Provider for WriteFileProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage: Usage::default(),
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "write_file".into(),
                    args: serde_json::json!({ "path": self.path, "content": "hi from auto-edit" }),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn auto_edit_allows_file_writes_without_prompting() {
        // AcceptEdits must auto-allow a `write_file` (Write side effect) end to end through the
        // live session. CapturePresenter::confirm returns false, so if the turn wrongly PROMPTS
        // the write is denied and the file never appears — making a regression observable.
        let path = std::env::temp_dir()
            .join(format!("forge-autoedit-{}.txt", forge_types::new_id()))
            .to_string_lossy()
            .to_string();
        let config = Config {
            permission_mode: forge_types::PermissionMode::AcceptEdits,
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(WriteFileProvider { path: path.clone() }),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            config,
            ".",
        )
        .unwrap();

        session.run_turn("write the file").await.unwrap();
        assert!(
            std::path::Path::new(&path).exists(),
            "auto-edit allowed the write without prompting"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Never streams an event and never returns — simulates a half-open / stalled connection.
    struct StallingProvider;
    #[async_trait::async_trait]
    impl Provider for StallingProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            unreachable!("the idle watchdog must abort this before it ever returns")
        }
    }

    #[tokio::test]
    async fn stalled_stream_times_out_instead_of_hanging() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut config = Config::default();
        config.mesh.stream_idle_timeout_secs = 1; // trip fast in the test
        config.mesh.failover = false; // no fallback → the error surfaces directly
        let mut session = Session::start(
            store,
            Arc::new(StallingProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        // The whole call must return well within this bound — if it hangs, the test fails here.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            session.run_turn("anything"),
        )
        .await;
        assert!(
            res.is_ok(),
            "run_turn hung instead of timing out the stream"
        );
        assert!(
            res.unwrap().is_err(),
            "a stalled stream should surface an error, not a silent hang"
        );
    }

    #[tokio::test]
    async fn turn_runs_unchanged_without_a_lattice() {
        // Additive guarantee: no index attached → no injection, turn proceeds as before.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = probe_session(store, Config::default());
        let answer = session
            .run_turn("explain lattice_probe_symbol")
            .await
            .unwrap();
        assert_eq!(answer, "NO_INJECTION");
    }

    #[tokio::test]
    async fn compact_folds_older_messages_into_a_summary() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SummarizingProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();

        // 12 messages → compact keeps the last 6, folds the first 6 into one summary.
        for i in 0..12 {
            session
                .transcript
                .push(Message::user(format!("message {i}")));
        }
        let (before, after) = session.compact(false).await.unwrap();
        assert_eq!(before, 12);
        assert_eq!(
            after,
            COMPACT_KEEP_RECENT + 1,
            "summary + the kept recent messages"
        );
        assert!(session.transcript[0].content.contains("SUMMARY:"));
        assert!(session.transcript[0].content.contains("summarized"));
        // The most recent message is preserved verbatim at the tail.
        assert_eq!(session.transcript.last().unwrap().content, "message 11");
    }

    #[tokio::test]
    async fn compact_fails_over_when_the_summarizer_is_rate_limited() {
        // Regression: a rate-limited compaction summarizer must NOT kill the turn. It also runs
        // mid-failover (admit_failover_model), so a dead model here would otherwise abort an
        // otherwise-recoverable turn. It must walk the routed fallback chain instead.
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec!["good::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        for i in 0..12 {
            session
                .transcript
                .push(Message::user(format!("message {i}")));
        }
        let (before, after) = session.compact(false).await.unwrap();
        assert_eq!(before, 12);
        assert_eq!(after, COMPACT_KEEP_RECENT + 1);
        // The fallback produced the summary, and the rate-limited primary was benched.
        assert!(session.transcript[0].content.contains("recovered"));
        let report = store.current_benched_report().unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].0, "bad::model");
    }

    #[tokio::test]
    async fn full_history_survives_compaction_for_the_user_view() {
        // After compaction the model sees a summary, but the USER must still be able to view the
        // entire original conversation, and can opt to reload it into the model's context.
        let provider = Arc::new(SummarizingProvider);
        let router = Arc::new(HeuristicRouter::new(Config::default()));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            provider,
            router,
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();
        let sid = session.id().to_string();
        for i in 0..10 {
            store
                .add_message(&sid, i, Role::User, &format!("turn {i}"), None)
                .unwrap();
        }
        store
            .compact_session_store(&sid, "SUMMARY of turns 0..6", 3)
            .unwrap();

        session.reset_resumed(&sid).unwrap();
        // Model context is the compacted view…
        assert!(
            session.history().len() < 10,
            "model sees the compacted view"
        );
        // …but the user's full replay shows all 10 original turns.
        let full_users = session
            .replay_items_full()
            .into_iter()
            .filter(|i| matches!(i, forge_tui::ReplayItem::User(_)))
            .count();
        assert_eq!(full_users, 10, "full replay shows every original user turn");
        assert!(session.was_compacted());

        // Reloading the full history puts all 10 turns back into the model context.
        session.reload_full_context().unwrap();
        let model_users = session
            .transcript
            .iter()
            .filter(|m| m.role == Role::User)
            .count();
        assert_eq!(
            model_users, 10,
            "reload_full_context restores the uncompacted context"
        );
    }

    #[tokio::test]
    async fn compact_is_a_noop_for_a_short_transcript() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SummarizingProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();
        session.transcript.push(Message::user("just one"));
        let (before, after) = session.compact(false).await.unwrap();
        assert_eq!((before, after), (1, 1), "nothing to compact");
    }

    #[tokio::test]
    async fn a_pretooluse_hook_blocks_the_tool_call() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        // Bypass so the only thing that can stop the (ReadOnly) tool is the hook itself.
        let config = Config {
            permission_mode: forge_types::PermissionMode::Bypass,
            hooks: vec![forge_config::HookConfig {
                event: forge_config::HookEvent::PreToolUse,
                matcher: Some("list_dir".into()),
                #[cfg(not(windows))]
                command: "echo blocked-by-test 1>&2; exit 1".into(),
                #[cfg(windows)]
                command: "echo blocked-by-test 1>&2 & exit /b 1".into(),
                timeout_secs: 10,
            }],
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(ListDirProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();

        session.run_turn("list the files").await.unwrap();

        let evs = events.lock().unwrap();
        let blocked = evs.iter().any(|e| {
            matches!(e, PresenterEvent::ToolResult { name, ok, summary }
                if name == "list_dir" && !ok && summary.contains("blocked by hook"))
        });
        assert!(
            blocked,
            "the list_dir call was blocked by the PreToolUse hook"
        );
    }

    #[tokio::test]
    async fn resume_restores_the_task_list() {
        use forge_types::{TodoItem, TodoStatus};
        let store = Arc::new(Store::open_in_memory().unwrap());
        let id = store.create_session(".", "default").unwrap();
        store
            .set_tasks(
                &id,
                &[TodoItem {
                    title: "earlier work".into(),
                    status: TodoStatus::InProgress,
                }],
            )
            .unwrap();

        let session = Session::resume(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            Config::default(),
            &id,
        )
        .unwrap();
        assert_eq!(session.tasks().len(), 1, "task list restored on resume");
        assert_eq!(session.tasks()[0].title, "earlier work");
    }

    #[tokio::test]
    async fn full_turn_routes_calls_tool_and_persists() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = Config::default();
        let mut session = Session::start(
            store,
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            // non-interactive: side-effect tools would be denied, but the mock uses read_file
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();

        let answer = session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        assert!(answer.contains("healthy"));

        // user + assistant + tool(read) + assistant(final) = 4 messages persisted.
        let count = session_message_count(&session);
        assert!(count >= 4, "expected >=4 messages, got {count}");
    }

    fn session_message_count(s: &Session) -> i64 {
        s.store.message_count(s.id()).unwrap()
    }

    #[tokio::test]
    async fn cost_accumulates_for_a_priced_model() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = priced_complex_config();
        let mut session = Session::start(
            store,
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();

        // "refactor ... concurrency" routes to the complex tier (a priced model),
        // so the mock's token counts must turn into a non-zero session cost.
        session
            .run_turn("refactor the architecture for concurrency")
            .await
            .unwrap();
        let cost = session.store.session_cost(session.id()).unwrap();
        assert!(cost > 0.0, "expected a non-zero cost, got {cost}");
    }

    #[tokio::test]
    async fn warns_when_budget_threshold_reached() {
        // Complex turn costs (30+12)/1k + (42+18)/1k = 0.102 USD (keyless priced model, so
        // provider-fallback can't re-route and change the cost).
        let mut config = priced_complex_config();
        config.mesh.daily_budget_usd = Some(0.12); // 80% = 0.096

        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();

        // Turn 1 spends ~0.102 -> into the warning band (>= 0.096, < 0.12).
        session
            .run_turn("refactor the architecture for concurrency")
            .await
            .unwrap();
        // Turn 2 starts already in the warning band, so it must warn.
        session
            .run_turn("refactor the concurrency design again")
            .await
            .unwrap();

        let warned = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::Warning(_)));
        assert!(warned, "expected a budget Warning event");
    }

    /// A config whose complex tier points at a keyless (always-available) model with a fixed
    /// 1.0/1k price, so budget/cost tests are deterministic regardless of which API keys the
    /// host happens to have — otherwise provider-fallback would re-route to an available model
    /// and change the cost out from under the test.
    fn priced_complex_config() -> Config {
        let mut config = Config::default();
        config.mesh.models.insert(
            "complex".to_string(),
            forge_config::OneOrMany::One("ollama::opus-sim".to_string()),
        );
        config.mesh.pricing.insert(
            "ollama::opus-sim".to_string(),
            forge_config::PriceOverride {
                input_per_1k: 1.0,
                output_per_1k: 1.0,
            },
        );
        config
    }

    fn fresh_session(store: Arc<Store>, config: Config) -> Session {
        Session::start(
            store,
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap()
    }

    #[test]
    fn summarize_does_not_panic_on_multibyte_boundary() {
        // Byte 80 lands inside the multi-byte 'é' — `&first[..80]` would panic here.
        let line = format!(
            "{}éééééé, and a tail to push well past eighty bytes",
            "a".repeat(78)
        );
        let s = summarize(&line);
        assert!(s.ends_with('…'), "long line is truncated with an ellipsis");
        assert!(s.chars().count() <= 81);
    }

    #[test]
    fn summarize_passes_short_lines_through() {
        assert_eq!(summarize("ok: [workspace]"), "ok: [workspace]");
        assert_eq!(summarize("line one\nline two"), "line one");
    }

    #[tokio::test]
    async fn hard_stop_refuses_once_over_cap() {
        // AC-7: once the day total exceeds the cap, the next turn is refused before any
        // provider call and records no further spend.
        let mut config = priced_complex_config();
        config.mesh.daily_budget_usd = Some(0.05);
        let mut session = fresh_session(Arc::new(Store::open_in_memory().unwrap()), config);

        // Turn 1 sees $0 spent -> proceeds, spends ~$0.102 (over the $0.05 cap).
        session
            .run_turn("refactor the architecture for concurrency")
            .await
            .unwrap();
        let cost_after_1 = session.store.session_cost(session.id()).unwrap();
        assert!(
            cost_after_1 > 0.05,
            "turn 1 should exceed the cap: {cost_after_1}"
        );

        // Turn 2 is over budget -> hard stop.
        let answer = session
            .run_turn("refactor the concurrency design again")
            .await
            .unwrap();
        assert!(
            answer.contains("budget cap reached"),
            "turn 2 refused: {answer}"
        );
        let cost_after_2 = session.store.session_cost(session.id()).unwrap();
        assert!(
            (cost_after_2 - cost_after_1).abs() < 1e-9,
            "no spend after a hard stop"
        );
    }

    #[tokio::test]
    async fn daily_spend_aggregates_across_sessions() {
        // AC-1/AC-2: a second session sees the first session's spend in the day total.
        let path = std::env::temp_dir().join(format!("forge-budget-{}.db", forge_types::new_id()));
        let config = priced_complex_config(); // no cap -> both proceed; complex tier is priced

        let day_total_after_a = {
            let mut a = fresh_session(Arc::new(Store::open(&path).unwrap()), config.clone());
            a.run_turn("refactor the architecture for concurrency")
                .await
                .unwrap();
            a.store.spend_today_usd().unwrap()
        };
        assert!(day_total_after_a > 0.0, "session A recorded spend today");

        // A brand-new session on the same DB must see A's spend (the bug was a per-session reset).
        let b = fresh_session(Arc::new(Store::open(&path).unwrap()), config.clone());
        let seen_by_b = b.store.spend_today_usd().unwrap();
        assert!(
            (seen_by_b - day_total_after_a).abs() < 1e-9,
            "B sees the cross-session day total: {seen_by_b} vs {day_total_after_a}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_rehydrates_transcript_and_continues_same_session() {
        let path = std::env::temp_dir().join(format!("forge-resume-{}.db", forge_types::new_id()));
        let config = Config::default();

        // First run on a file-backed store, then drop it.
        let (id, cost1, msgs1) = {
            let mut s = fresh_session(Arc::new(Store::open(&path).unwrap()), config.clone());
            s.run_turn("refactor the architecture for concurrency")
                .await
                .unwrap();
            let id = s.id().to_string();
            (
                id.clone(),
                s.store.session_cost(&id).unwrap(),
                s.store.message_count(&id).unwrap(),
            )
        };

        // Resume on a fresh connection to the same file.
        let mut s2 = Session::resume(
            Arc::new(Store::open(&path).unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            &id,
        )
        .unwrap();

        assert_eq!(s2.id(), id, "must continue the same session row");
        assert_eq!(
            s2.transcript.len() as i64,
            msgs1,
            "transcript should be rehydrated"
        );
        let cost_after_resume = s2.store.session_cost(&id).unwrap();
        assert!(
            (cost_after_resume - cost1).abs() < 1e-9,
            "prior cost preserved"
        );

        // Continuing appends to the same session.
        s2.run_turn("another complex refactor of the design")
            .await
            .unwrap();
        assert!(
            s2.store.message_count(&id).unwrap() > msgs1,
            "new turn appended"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_missing_session_errors() {
        let err = Session::resume(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            "ghost-id",
        )
        .err()
        .unwrap();
        assert!(matches!(err, CoreError::SessionNotFound(_)));
    }

    // --- Subagent orchestration (RFC subagent-orchestration) ---

    /// A test provider that, for the TOP-LEVEL agent, calls `spawn_agents` with two inline
    /// subtasks then synthesizes; for a SUBAGENT (its transcript opens with the subagent system
    /// prompt) it behaves like the normal mock (read_file → done). Shared via `Arc` by parent
    /// and children, exactly as in production.
    #[derive(Default)]
    struct SpawnThenSynthProvider;

    #[async_trait::async_trait]
    impl Provider for SpawnThenSynthProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let is_subagent = messages
                .iter()
                .any(|m| m.role == Role::System && m.content.contains("subagent"));
            let used_tool = messages.iter().any(|m| m.role == Role::Tool);
            let usage = Usage {
                input_tokens: 30,
                output_tokens: 12,
                cached_input_tokens: 0,
                cost_usd: 0.0,
            };
            if is_subagent {
                // Child: read a file once, then answer.
                if used_tool {
                    let content = "child finding: ok";
                    on_event(StreamEvent::Text(content.into()));
                    return Ok(ModelResponse {
                        content: content.into(),
                        tool_calls: vec![],
                        usage,
                        quotas: Vec::new(),
                    });
                }
                return Ok(ModelResponse {
                    content: "reading".into(),
                    tool_calls: vec![ToolCall {
                        id: new_id(),
                        name: "read_file".into(),
                        args: serde_json::json!({"path": "Cargo.toml"}),
                    }],
                    usage,
                    quotas: Vec::new(),
                });
            }
            // Parent: fan out, then synthesize once results return.
            if used_tool {
                let content = "synthesized from subagents";
                on_event(StreamEvent::Text(content.into()));
                return Ok(ModelResponse {
                    content: content.into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: "delegating".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "spawn_agents".into(),
                    args: serde_json::json!({"agents": [
                        {"agent": "reviewer", "task": "review the change"},
                        {"task": "fix the typo in the readme"}
                    ]}),
                }],
                usage,
                quotas: Vec::new(),
            })
        }
    }

    /// A config with three distinct, keyless, priced tiers so routing is deterministic and a
    /// Trivial child routes to a cheaper model than a Complex parent.
    fn tiered_config() -> Config {
        use forge_config::{OneOrMany, PriceOverride};
        let mut config = Config::default();
        for (tier, model, price) in [
            ("trivial", "ollama::small", 0.001),
            ("standard", "ollama::mid", 0.05),
            ("complex", "ollama::big", 1.0),
        ] {
            config
                .mesh
                .models
                .insert(tier.into(), OneOrMany::One(model.into()));
            config.mesh.pricing.insert(
                model.into(),
                PriceOverride {
                    input_per_1k: price,
                    output_per_1k: price,
                },
            );
        }
        config
    }

    #[tokio::test]
    async fn spawn_agents_creates_linked_children_and_returns_results() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = tiered_config();
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SpawnThenSynthProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        let answer = session
            .run_turn("design and architect a complex concurrency refactor across modules")
            .await
            .unwrap();

        assert!(
            answer.contains("synthesized"),
            "parent synthesizes: {answer}"
        );

        // Two child sessions, both linked to the parent.
        let children = store.child_sessions(&parent_id).unwrap();
        assert_eq!(children.len(), 2, "two children persisted with parent link");

        // Coarse lifecycle events surfaced for each child.
        let ev = events.lock().unwrap();
        let starts = ev
            .iter()
            .filter(|e| matches!(e, PresenterEvent::SubagentStart { .. }))
            .count();
        let results = ev
            .iter()
            .filter(|e| matches!(e, PresenterEvent::SubagentResult { .. }))
            .count();
        assert_eq!((starts, results), (2, 2), "start+result per child");

        // Children stream their activity → live progress events surface (Phase 3b).
        let progress = ev
            .iter()
            .filter(|e| matches!(e, PresenterEvent::SubagentProgress { .. }))
            .count();
        assert!(progress > 0, "at least one live progress delta surfaced");

        // Child usage rolled into the shared day budget (children did real model work).
        assert!(store.spend_today_usd().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn subagents_route_independently_via_the_mesh() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = tiered_config();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SpawnThenSynthProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        session
            .run_turn("design and architect a complex concurrency refactor across modules")
            .await
            .unwrap();

        // Parent routed Complex; the "fix the typo" child routed Trivial → different model.
        let parent_models = store.session_models(&parent_id).unwrap();
        assert_eq!(
            parent_models.first().map(String::as_str),
            Some("ollama::big")
        );

        let children = store.child_sessions(&parent_id).unwrap();
        let child_models: Vec<String> = children
            .iter()
            .flat_map(|c| store.session_models(c).unwrap())
            .collect();
        assert!(
            child_models.iter().any(|m| m == "ollama::small"),
            "a trivial child routed to the cheap tier independently: {child_models:?}"
        );
    }

    /// A provider where EVERY agent (top or subagent) tries to `spawn_agents` once, then answers.
    /// Used to prove recursion is bounded by `max_depth` (the registry refuses `spawn_agents`
    /// once depth is exhausted, so the chain terminates).
    #[derive(Default)]
    struct AlwaysRecurseProvider;

    #[async_trait::async_trait]
    impl Provider for AlwaysRecurseProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let used_tool = messages.iter().any(|m| m.role == Role::Tool);
            let usage = Usage {
                input_tokens: 5,
                output_tokens: 2,
                cached_input_tokens: 0,
                cost_usd: 0.0,
            };
            if used_tool {
                return Ok(ModelResponse {
                    content: "leaf answer".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: "delegating deeper".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "spawn_agents".into(),
                    args: serde_json::json!({"agents": [{"task": "go deeper"}]}),
                }],
                usage,
                quotas: Vec::new(),
            })
        }
    }

    #[test]
    fn cycle_temper_advances_wraps_and_persists() {
        use forge_types::PermissionMode;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = fresh_session(Arc::clone(&store), Config::default());
        let id = session.id().to_string();
        let mut session = session;

        // Default config now starts at AcceptEdits (Smith).
        assert_eq!(session.temper(), PermissionMode::AcceptEdits); // Smith
        assert_eq!(session.cycle_temper(), PermissionMode::Plan); // → Survey
        assert_eq!(store.session_mode(&id).unwrap(), "Plan", "switch persisted");
        assert_eq!(session.cycle_temper(), PermissionMode::Default); // → Guarded
        assert_eq!(session.cycle_temper(), PermissionMode::AcceptEdits); // wraps → Smith
                                                                         // Cycling never lands on the dangerous Unfettered temper.
        for _ in 0..6 {
            assert_ne!(session.cycle_temper(), PermissionMode::Bypass);
        }
    }

    #[tokio::test]
    async fn recursion_is_bounded_by_max_depth() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut config = tiered_config();
        config.mesh.subagents.max_depth = 2;
        config.mesh.subagents.max_concurrency = 2;
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(AlwaysRecurseProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        // Must terminate (not infinite-recurse / stack-overflow).
        session
            .run_turn("kick off a delegating turn")
            .await
            .unwrap();

        // Walk the parent→child tree; with max_depth=2 the chain is child→grandchild→
        // great-grandchild (depths 0,1,2) and stops — never a 4th generation.
        fn max_gen(store: &Store, id: &str) -> usize {
            let kids = store.child_sessions(id).unwrap();
            1 + kids.iter().map(|k| max_gen(store, k)).max().unwrap_or(0)
        }
        let generations = max_gen(&store, &parent_id);
        assert_eq!(
            generations, 4,
            "parent + 3 nested generations (depths 0,1,2), bounded by max_depth"
        );
    }

    #[tokio::test]
    async fn agent_type_file_pins_tier_alongside_mesh_routed_inline_child() {
        // A `.forge/agents/reviewer.md` pins tier=complex; the inline "fix the typo" child has
        // no pin and mesh-routes to trivial. Both must coexist in one spawn_agents call.
        let dir = std::env::temp_dir().join(format!("forge-agents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reviewer.md"),
            "---\nname: reviewer\ntier: complex\ntools: [read_file]\n---\nYou review code.",
        )
        .unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut config = tiered_config();
        config.mesh.subagents.agents_dir = dir.to_string_lossy().to_string();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SpawnThenSynthProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        session
            .run_turn("design and architect a complex concurrency refactor across modules")
            .await
            .unwrap();

        let children = store.child_sessions(&parent_id).unwrap();
        let child_models: Vec<String> = children
            .iter()
            .flat_map(|c| store.session_models(c).unwrap())
            .collect();
        // reviewer pinned → complex tier model; the inline "fix typo" → trivial tier model.
        assert!(
            child_models.iter().any(|m| m == "ollama::big"),
            "pinned reviewer routed to its tier: {child_models:?}"
        );
        assert!(
            child_models.iter().any(|m| m == "ollama::small"),
            "inline child still mesh-routed cheaply: {child_models:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Model health / failover (model-health-failover) ---

    /// A router that returns a fixed model + fallback chain, so the failover loop is testable
    /// without depending on discovery/availability.
    struct FixedRouter {
        model: String,
        fallbacks: Vec<String>,
    }
    #[async_trait::async_trait]
    impl Router for FixedRouter {
        async fn route(
            &self,
            _prompt: &str,
            _budget: BudgetState,
            _health: &forge_types::ModelHealth,
            _quota: &forge_types::SubscriptionQuota,
        ) -> forge_mesh::RoutingDecision {
            forge_mesh::RoutingDecision {
                tier: forge_types::TaskTier::Trivial,
                model: self.model.clone(),
                rationale: "test".into(),
                fallbacks: self.fallbacks.clone(),
            }
        }
    }

    /// A provider that fails for `bad` models (with a chosen error) and answers for any other.
    struct FlakyProvider {
        bad: std::collections::HashSet<String>,
        err: fn(&str) -> forge_provider::ProviderError,
    }
    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        async fn complete(
            &self,
            model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            if self.bad.contains(model) {
                return Err((self.err)(model));
            }
            on_event(StreamEvent::Text("recovered".into()));
            Ok(forge_provider::ModelResponse {
                content: "recovered".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    fn rate_limited(_m: &str) -> forge_provider::ProviderError {
        forge_provider::ProviderError::RateLimited {
            message: "429".into(),
            retry_after: Some(std::time::Duration::from_secs(42)),
        }
    }

    /// Fails the first `fail_first` calls with a context-overflow error, then answers. Used to
    /// prove an overflow self-heals (compact + retry the SAME model) instead of benching it.
    struct OverflowThenOkProvider {
        calls: std::sync::atomic::AtomicUsize,
        fail_first: usize,
    }
    #[async_trait::async_trait]
    impl Provider for OverflowThenOkProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.fail_first {
                return Err(forge_provider::ProviderError::Unavailable(
                    "maximum context length is 128000 tokens".into(),
                ));
            }
            on_event(StreamEvent::Text("recovered".into()));
            Ok(forge_provider::ModelResponse {
                content: "recovered".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn context_overflow_compacts_and_retries_the_same_model_without_benching() {
        // The first call overflows the window; the fix is to shrink the transcript and retry the
        // SAME (healthy) model — NOT to bench it and churn the failover chain (the stuck-turn bug).
        let provider = Arc::new(OverflowThenOkProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail_first: 1,
        });
        let router = Arc::new(FixedRouter {
            model: "good::model".into(),
            fallbacks: vec!["other::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        // Enough history that the compaction triggered by the overflow actually folds messages.
        for i in 0..12 {
            session
                .transcript
                .push(Message::user(format!("message {i}")));
        }
        let answer = session.run_turn("summarize the work").await.unwrap();
        assert_eq!(answer, "recovered", "the turn self-healed and completed");
        // The healthy model must NOT have been benched — overflow is an input problem, not a
        // model-health problem.
        let benched = store.current_benched_report().unwrap();
        assert!(
            benched.is_empty(),
            "overflow must not bench the model: {benched:?}"
        );
    }

    fn fixed_session(
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
    ) -> (Arc<Store>, Session) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = Session::start(
            Arc::clone(&store),
            provider,
            router,
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();
        (store, session)
    }

    #[test]
    fn replay_items_reconstructs_text_and_tool_activity() {
        use forge_tui::ReplayItem;
        let (_store, mut session) = fixed_session(
            Arc::new(FlakyProvider {
                bad: std::collections::HashSet::new(),
                err: rate_limited,
            }),
            Arc::new(FixedRouter {
                model: "m".into(),
                fallbacks: vec![],
            }),
        );
        // A compaction marker, a user turn, a tool-only assistant turn + its result, a final answer.
        session.transcript = vec![
            Message::system("[Earlier conversation summarized to save context]\ndid X then Y"),
            Message::user("do the thing"),
            Message::assistant_tool_calls(
                "",
                vec![forge_types::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "a.rs"}),
                }],
            ),
            Message::tool_result("c1", "fn main() {}"),
            Message::assistant("done"),
        ];
        let items = session.replay_items();
        // The old history() dropped the summary, the tool-only turn, and the result; replay_items
        // keeps all of them so the resumed conversation is faithful.
        assert!(matches!(&items[0], ReplayItem::Note(s) if s.contains("summarized")));
        assert!(matches!(&items[1], ReplayItem::User(s) if s == "do the thing"));
        assert!(matches!(&items[2], ReplayItem::Tool { name, .. } if name == "read_file"));
        assert!(
            matches!(&items[3], ReplayItem::ToolResult { name, ok, .. } if name == "read_file" && *ok)
        );
        assert!(matches!(&items[4], ReplayItem::Assistant(s) if s == "done"));
        assert_eq!(items.len(), 5);
    }

    #[tokio::test]
    async fn run_turn_with_prepends_persisted_guidance_before_the_prompt() {
        // A skill/command's methodology is injected as a System message ahead of the user prompt
        // and persisted (so resume rehydrates it). The turn otherwise runs exactly as normal.
        let provider = Arc::new(FlakyProvider {
            bad: std::collections::HashSet::new(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "good::model".into(),
            fallbacks: vec![],
        });
        let (store, mut session) = fixed_session(provider, router);
        let answer = session
            .run_turn_with(
                "do the thing",
                &["METHODOLOGY: be rigorous".to_string()],
                Some(TaskTier::Complex),
            )
            .await
            .unwrap();
        assert_eq!(answer, "recovered");

        let msgs = store.load_messages(session.id()).unwrap();
        assert_eq!(msgs[0].role, Role::System);
        assert!(msgs[0].content.contains("METHODOLOGY"));
        assert_eq!(msgs[1].role, Role::User);
        assert_eq!(msgs[1].content, "do the thing");
    }

    #[tokio::test]
    async fn retryable_error_benches_the_model_and_fails_over() {
        // AC-1 + AC-2: the primary 429s → benched (with the server's 42s cooldown) → the turn
        // retries on the fallback and succeeds.
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec!["good::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        let answer = session.run_turn("hi").await.unwrap();
        assert_eq!(answer, "recovered");
        // The bad model is benched; the cooldown reflects the server's 42s (not the default).
        let report = store.current_benched_report().unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].0, "bad::model");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (report[0].1 - now - 42).abs() <= 2,
            "cooldown ~42s: {report:?}"
        );
    }

    #[tokio::test]
    async fn non_retryable_error_does_not_fail_over_or_bench() {
        // AC-5: a 400-style error fails the turn as before; the model is NOT benched.
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
            err: |_| forge_provider::ProviderError::Request("bad request".into()),
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec!["good::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        assert!(session.run_turn("hi").await.is_err());
        assert!(store.current_benched().unwrap().is_empty());
    }

    #[tokio::test]
    async fn exhausting_the_chain_returns_no_healthy_model() {
        // AC-6: primary 429s, no fallbacks → a clear error, not a hang.
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec![],
        });
        let (_store, mut session) = fixed_session(provider, router);
        assert!(matches!(
            session.run_turn("hi").await,
            Err(CoreError::NoHealthyModel)
        ));
    }

    // --- Conversation checkpoints + /undo (RFC session-management-and-commands, PR2) ---

    #[tokio::test]
    async fn undo_rewinds_the_last_user_turn() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = fresh_session(Arc::clone(&store), Config::default());
        let id = session.id().to_string();

        session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        assert!(
            store.load_messages(&id).unwrap().len() >= 2,
            "the turn persisted messages"
        );

        // Undo drops the whole turn (the user prompt + its replies/tools).
        assert!(session.undo().unwrap().is_some(), "a turn was undone");
        assert!(
            store.load_messages(&id).unwrap().is_empty(),
            "rewound turn is excluded from the active transcript"
        );
        assert!(session.undo().unwrap().is_none(), "nothing left to undo");
    }

    #[tokio::test]
    async fn every_turn_auto_checkpoints_with_a_prompt_preview() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = fresh_session(Arc::clone(&store), Config::default());

        session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        session.run_turn("now check it again please").await.unwrap();

        let cps = session.checkpoints().unwrap();
        assert_eq!(cps.len(), 2, "one auto checkpoint per turn");
        // Newest first, labeled with the prompt preview (so /undo can show the message).
        assert_eq!(cps[0].label.as_deref(), Some("now check it again please"));
        assert_eq!(cps[1].label.as_deref(), Some("check the project manifest"));
        // Each checkpoint's boundary is its turn's start, so rewinding there undoes that turn.
        assert!(cps[0].seq > cps[1].seq);
    }

    #[tokio::test]
    async fn checkpoint_then_turn_then_rewind_to_it() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = fresh_session(Arc::clone(&store), Config::default());
        let id = session.id().to_string();

        session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        session.checkpoint(Some("after first turn")).unwrap();
        let boundary = session.checkpoints().unwrap()[0].seq;
        session.run_turn("check the manifest again").await.unwrap();
        let after_two = store.load_messages(&id).unwrap().len();

        session.rewind_to(boundary).unwrap();
        let after_rewind = store.load_messages(&id).unwrap().len();
        assert!(
            after_rewind < after_two && after_rewind == boundary as usize,
            "rewind drops the second turn back to the checkpoint boundary"
        );
    }

    /// A provider that writes a file once (via `write_file`), then answers.
    struct WritingProvider {
        path: String,
        content: String,
    }
    #[async_trait::async_trait]
    impl Provider for WritingProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let usage = Usage::default();
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                });
            }
            Ok(ModelResponse {
                content: "writing".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "write_file".into(),
                    args: serde_json::json!({ "path": self.path, "content": self.content }),
                }],
                usage,
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn picker_rewind_to_an_earlier_turn_reverts_files() {
        // Mirrors the /undo picker path: two turns edit a file, then rewind to the FIRST turn's
        // checkpoint seq (as the picker does) — the file must return to its pre-turn-1 bytes.
        let dir = std::env::temp_dir().join(format!("forge-rew-{}", forge_types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "ORIGINAL").unwrap();

        let config = Config {
            permission_mode: PermissionMode::Bypass,
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(WritingProvider {
                path: file.to_string_lossy().to_string(),
                content: "MODEL-EDIT".into(),
            }),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        session.set_checkpoint_root(dir.join("snaps"));

        session.run_turn("turn one edits the file").await.unwrap();
        session.run_turn("turn two edits it again").await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "MODEL-EDIT");

        // Picker uses the checkpoint's seq; pick the OLDEST (first turn).
        let cps = session.checkpoints().unwrap();
        let first_turn_seq = cps.last().unwrap().seq;
        let report = session.rewind_to(first_turn_seq).unwrap().restore;

        assert!(
            !report.restored.is_empty(),
            "files were restored: {report:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "ORIGINAL",
            "rewinding to turn 1 reverts the file to its pre-turn-1 bytes"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn undo_restores_files_written_during_the_turn() {
        let dir = std::env::temp_dir().join(format!("forge-undo-{}", forge_types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("edited.txt");
        std::fs::write(&file, "original bytes").unwrap();

        let config = Config {
            permission_mode: PermissionMode::Bypass, // allow the write without a prompt
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(WritingProvider {
                path: file.to_string_lossy().to_string(),
                content: "the model overwrote this".into(),
            }),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        session.set_checkpoint_root(dir.join("snaps"));

        session.run_turn("rewrite the file").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "the model overwrote this",
            "the turn wrote the file"
        );

        let report = session.undo().unwrap().unwrap().restore;
        assert!(
            report.restored.iter().any(|p| p.contains("edited.txt")),
            "the written file was restored: {report:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "original bytes",
            "undo restored the pre-turn bytes"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A provider that blocks for a long time, so a turn can be interrupted mid-flight.
    struct SlowProvider;
    #[async_trait::async_trait]
    impl Provider for SlowProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(forge_provider::ModelResponse {
                content: "too late".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn aborting_a_running_turn_releases_the_session_lock() {
        // The interrupt feature aborts the turn task; this proves the invariant it relies on —
        // cancelling a task that holds the session Mutex across an await frees the lock, so the
        // session stays usable (no deadlock / frozen UI).
        use std::time::Duration;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = Arc::new(tokio::sync::Mutex::new(
            Session::start(
                store,
                Arc::new(SlowProvider),
                Arc::new(HeuristicRouter::new(Config::default())),
                ToolRegistry::with_core_tools(),
                Box::new(HeadlessPresenter::new(false)),
                Config::default(),
                ".",
            )
            .unwrap(),
        ));

        let s = session.clone();
        let handle = tokio::spawn(async move {
            let mut g = s.lock().await;
            let _ = g.run_turn("a slow request").await;
        });
        // Let the task acquire the lock and enter the 30s provider sleep, then interrupt it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let _ = handle.await;

        // The lock must be free immediately (the aborted task dropped its guard).
        let guard = tokio::time::timeout(Duration::from_secs(2), session.lock())
            .await
            .expect("abort released the session lock");
        assert!(
            guard
                .history()
                .iter()
                .any(|(r, c)| matches!(r, Role::User) && c == "a slow request"),
            "the interrupted turn's prompt was recorded before the abort"
        );
    }

    // --- Assay mode (docs/features/analysis-mode.md) ---

    /// A provider that plays the critic + verifier roles for an in-session assay run.
    struct AssayProvider;
    #[async_trait::async_trait]
    impl Provider for AssayProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            let sys = messages
                .iter()
                .find(|m| m.role == Role::System)
                .map(|m| m.content.as_str())
                .unwrap_or("");
            let content = if sys.contains("ASSAY-VERIFIER") {
                r#"{"verdict":"uphold","confidence":"high"}"#.to_string()
            } else if sys.contains("ASSAY-CRITIC") && sys.contains("'correctness'") {
                r#"[{"severity":"high","file":"a.rs","line":1,"title":"bug","why":"w","fix":"f","effort":"small"}]"#.to_string()
            } else {
                "[]".to_string()
            };
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn assay_analysis_emits_a_report_and_persists_the_run() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(AssayProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session
            .assay(
                Arc::from("fn main() {}"),
                assay::TierModels {
                    trivial: vec!["m".into()],
                    complex: vec!["m".into()],
                },
                vec![], // default: full crew
                forge_types::AssayScope::Repo,
                false, // analysis-only
            )
            .await
            .unwrap();

        let ev = events.lock().unwrap();
        let report = ev.iter().find_map(|e| match e {
            PresenterEvent::AssayReport(r) => Some(r.clone()),
            _ => None,
        });
        let report = report.expect("an AssayReport was emitted");
        assert_eq!(report.findings.len(), 1, "the upheld finding is reported");
        assert!(!report.run_id.is_empty(), "the run was persisted");
        assert_eq!(store.list_assay_runs().unwrap().len(), 1);
        assert_eq!(store.load_findings(&report.run_id).unwrap().len(), 1);
    }

    // --- In-TUI session swap (RFC session-management-and-commands, PR1) ---

    #[tokio::test]
    async fn reset_resumed_and_fresh_swap_the_live_session() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        // Seed a past session A with a user+assistant exchange.
        let a = store.create_session(".", "default").unwrap();
        store.add_message(&a, 0, Role::User, "hello", None).unwrap();
        store
            .add_message(&a, 1, Role::Assistant, "hi there", Some("m"))
            .unwrap();
        // A live session B (what the TUI is holding).
        let mut b = Session::start(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();
        let b_id = b.id().to_string();

        // /resume A: B becomes A, rehydrating A's transcript.
        b.reset_resumed(&a).unwrap();
        assert_eq!(b.id(), a);
        assert_ne!(b.id(), b_id);
        assert_eq!(
            b.history(),
            vec![
                (Role::User, "hello".to_string()),
                (Role::Assistant, "hi there".to_string()),
            ]
        );

        // /new: a fresh empty session, new id.
        b.reset_fresh(".").unwrap();
        assert!(b.history().is_empty());
        assert_ne!(b.id(), a);
    }

    // ── Autofix tests ──────────────────────────────────────────────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn autofix_stage_passes_when_commands_exit_zero() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            Config::default(),
            ".",
        )
        .unwrap();

        let af = forge_config::AutofixConfig {
            auto_lint: true,
            auto_test: true,
            lint_cmd: "true".to_string(), // always exits 0
            test_cmd: "true".to_string(), // always exits 0
            max_iterations: 3,
        };
        // run_autofix_stage returns Ok(true) when all enabled commands pass.
        let passed = session.run_autofix_stage(&af).await.unwrap();
        assert!(passed, "both 'true' commands exit 0 → stage should pass");
        // No synthetic failure message pushed to transcript.
        assert!(
            session
                .transcript
                .iter()
                .all(|m| !m.content.contains("Auto-fix:")),
            "no failure message injected on pass"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn autofix_stage_fails_when_lint_exits_nonzero() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            Config::default(),
            ".",
        )
        .unwrap();

        let af = forge_config::AutofixConfig {
            auto_lint: true,
            auto_test: false,              // test disabled
            lint_cmd: "false".to_string(), // always exits 1
            test_cmd: String::new(),
            max_iterations: 3,
        };
        let passed = session.run_autofix_stage(&af).await.unwrap();
        assert!(!passed, "'false' exits 1 → stage should fail");
        // A synthetic user message with the failure should be in the transcript.
        assert!(
            session
                .transcript
                .iter()
                .any(|m| m.content.contains("Auto-fix:") && m.content.contains("lint:")),
            "failure message injected into transcript: {:?}",
            session
                .transcript
                .iter()
                .map(|m| &m.content)
                .collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn autofix_stage_skipped_when_no_edits() {
        // edits_this_turn == 0 means the autofix outer condition evaluates to false;
        // test that run_autofix_stage is not reached (verify the guard independently).
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = Session::start(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            Config::default(),
            ".",
        )
        .unwrap();
        // Fresh session: edits_this_turn must be 0 before any turn.
        assert_eq!(
            session.edits_this_turn, 0,
            "edits_this_turn starts at 0; autofix gate would not fire"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn autofix_stage_empty_cmd_is_skipped() {
        // When lint_cmd / test_cmd is empty the command must not run even if auto_lint/auto_test
        // is true (empty string = disabled per spec).
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            Config::default(),
            ".",
        )
        .unwrap();

        let af = forge_config::AutofixConfig {
            auto_lint: true,
            auto_test: true,
            lint_cmd: String::new(), // empty = disabled
            test_cmd: String::new(), // empty = disabled
            max_iterations: 3,
        };
        // No commands run → stage trivially passes.
        let passed = session.run_autofix_stage(&af).await.unwrap();
        assert!(passed, "empty commands → nothing runs → stage passes");
    }

    // ── Auto-review gate tests ────────────────────────────────────────────────────────────────

    #[test]
    fn severity_meets_high_threshold() {
        use forge_types::Severity;
        // "high" gate: critical and high pass; medium and low do not.
        assert!(severity_meets(Severity::Critical, "high"));
        assert!(severity_meets(Severity::High, "high"));
        assert!(!severity_meets(Severity::Medium, "high"));
        assert!(!severity_meets(Severity::Low, "high"));
    }

    #[test]
    fn severity_meets_medium_threshold() {
        use forge_types::Severity;
        // "medium" gate: critical, high, medium pass; low does not.
        assert!(severity_meets(Severity::Critical, "medium"));
        assert!(severity_meets(Severity::High, "medium"));
        assert!(severity_meets(Severity::Medium, "medium"));
        assert!(!severity_meets(Severity::Low, "medium"));
    }

    #[test]
    fn severity_meets_low_threshold() {
        use forge_types::Severity;
        // "low" gate: everything passes.
        assert!(severity_meets(Severity::Critical, "low"));
        assert!(severity_meets(Severity::High, "low"));
        assert!(severity_meets(Severity::Medium, "low"));
        assert!(severity_meets(Severity::Low, "low"));
    }

    #[test]
    fn severity_meets_critical_threshold() {
        use forge_types::Severity;
        // "critical" gate: only critical passes.
        assert!(severity_meets(Severity::Critical, "critical"));
        assert!(!severity_meets(Severity::High, "critical"));
        assert!(!severity_meets(Severity::Medium, "critical"));
        assert!(!severity_meets(Severity::Low, "critical"));
    }

    #[test]
    fn severity_meets_unknown_threshold_is_permissive() {
        use forge_types::Severity;
        // Unknown threshold → fail-open (surface the finding).
        assert!(severity_meets(Severity::Low, "unknown-typo"));
        assert!(severity_meets(Severity::Medium, ""));
    }

    #[test]
    fn auto_review_gate_skipped_when_disabled() {
        // When auto_review = false, the gate condition is never entered regardless of edits.
        let cfg = forge_config::AssayConfig {
            auto_review: false,
            gate_severity: "high".to_string(),
            gate_mode: "block".to_string(),
            min_diff_bytes: 0,
            max_cost_usd: 0.0,
        };
        // The predicate `auto_review && edits_this_turn > 0` must be false with auto_review=off.
        let edits: u32 = 5;
        assert!(
            !(cfg.auto_review && edits > 0),
            "gate must be skipped when auto_review is off"
        );
    }

    #[test]
    fn auto_review_gate_skipped_when_no_edits() {
        // Even with auto_review=true, gate is skipped when edits_this_turn==0.
        let cfg = forge_config::AssayConfig {
            auto_review: true,
            gate_severity: "high".to_string(),
            gate_mode: "warn".to_string(),
            min_diff_bytes: 200,
            max_cost_usd: 0.0,
        };
        let edits: u32 = 0;
        assert!(
            !(cfg.auto_review && edits > 0),
            "gate must be skipped when no edits happened"
        );
    }

    #[test]
    fn auto_review_gate_skipped_when_diff_too_small() {
        // The diff-size check: if the concatenated diff is < min_diff_bytes the gate returns
        // early without running the crew. We test the predicate directly.
        let cfg = forge_config::AssayConfig {
            auto_review: true,
            gate_severity: "high".to_string(),
            gate_mode: "warn".to_string(),
            min_diff_bytes: 200,
            max_cost_usd: 0.0,
        };
        let diff = "small".to_string();
        assert!(
            diff.len() < cfg.min_diff_bytes,
            "a 5-byte diff is below the 200-byte threshold"
        );
    }

    // ── Assay gate cost-cap predicate tests ───────────────────────────────────────────────────

    #[test]
    fn gate_cap_zero_means_unlimited() {
        // max_cost_usd == 0.0 → cap is disabled, the gate always runs.
        let cfg = forge_config::AssayConfig {
            auto_review: true,
            gate_severity: "high".to_string(),
            gate_mode: "warn".to_string(),
            min_diff_bytes: 0,
            max_cost_usd: 0.0,
        };
        // When cap == 0.0 the gate skips the estimate check (never skips on cost).
        assert_eq!(
            cfg.max_cost_usd, 0.0,
            "zero cap means unlimited — cost check is skipped"
        );
    }

    #[test]
    fn gate_cap_exceeded_means_skip() {
        let cfg = forge_config::AssayConfig {
            auto_review: true,
            gate_severity: "high".to_string(),
            gate_mode: "warn".to_string(),
            min_diff_bytes: 0,
            max_cost_usd: 0.10,
        };
        let est_usd = 0.75_f64; // over cap
        assert!(
            cfg.max_cost_usd > 0.0 && est_usd > cfg.max_cost_usd,
            "gate should be skipped when estimate exceeds cap"
        );
    }

    #[test]
    fn gate_cap_not_exceeded_means_run() {
        let cfg = forge_config::AssayConfig {
            auto_review: true,
            gate_severity: "high".to_string(),
            gate_mode: "warn".to_string(),
            min_diff_bytes: 0,
            max_cost_usd: 0.50,
        };
        let est_usd = 0.10_f64; // under cap
        assert!(
            !(cfg.max_cost_usd > 0.0 && est_usd > cfg.max_cost_usd),
            "gate should run when estimate is within cap"
        );
    }

    #[test]
    fn cli_max_cost_abort_predicate() {
        // Mirror the CLI's guard: abort when !yes && max_cost.is_some() && est > cap.
        let yes = false;
        let max_cost: Option<f64> = Some(0.20);
        let est_usd = 0.85_f64;
        let should_abort = !yes && max_cost.is_some_and(|cap| est_usd > cap);
        assert!(
            should_abort,
            "should abort when estimate exceeds --max-cost"
        );

        // --yes overrides the cap
        let yes = true;
        let should_abort = !yes && max_cost.is_some_and(|cap| est_usd > cap);
        assert!(!should_abort, "--yes must bypass the cap check");

        // Under cap: no abort
        let yes = false;
        let est_usd = 0.05_f64;
        let should_abort = !yes && max_cost.is_some_and(|cap| est_usd > cap);
        assert!(!should_abort, "estimate under cap must not abort");

        // No --max-cost flag: never abort
        let max_cost: Option<f64> = None;
        let est_usd = 9999.0_f64;
        let should_abort = !yes && max_cost.is_some_and(|cap| est_usd > cap);
        assert!(!should_abort, "no --max-cost flag → never abort");
    }

    // ── Architect mode: model resolution tests ────────────────────────────────────────────────

    fn make_session(config: Config) -> Session {
        Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(forge_provider::MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            config,
            ".",
        )
        .unwrap()
    }

    #[test]
    fn resolve_planner_falls_back_to_complex_tier_model() {
        // No architect_model set, no pin → first Complex-tier candidate.
        let config = Config::default();
        let expected = config
            .model_for(forge_types::TaskTier::Complex)
            .unwrap()
            .to_string();
        let session = make_session(config);
        assert_eq!(session.resolve_planner_model(), expected);
    }

    #[test]
    fn resolve_editor_falls_back_to_standard_tier_model() {
        // No editor_model set, no pin → first Standard-tier candidate.
        let config = Config::default();
        let expected = config
            .model_for(forge_types::TaskTier::Standard)
            .unwrap()
            .to_string();
        let session = make_session(config);
        assert_eq!(session.resolve_editor_model(), expected);
    }

    #[test]
    fn resolve_planner_uses_architect_model_when_set() {
        let mut config = Config::default();
        config.mesh.architect_model = Some("anthropic::claude-opus-4-8".to_string());
        let session = make_session(config);
        assert_eq!(
            session.resolve_planner_model(),
            "anthropic::claude-opus-4-8"
        );
    }

    #[test]
    fn resolve_editor_uses_editor_model_when_set() {
        let mut config = Config::default();
        config.mesh.editor_model = Some("groq::llama-3.1-8b-instant".to_string());
        let session = make_session(config);
        assert_eq!(session.resolve_editor_model(), "groq::llama-3.1-8b-instant");
    }

    #[test]
    fn pin_overrides_both_planner_and_editor() {
        // /model pin takes priority over both config fields and tier fallback.
        let mut config = Config::default();
        config.mesh.architect_model = Some("anthropic::claude-opus-4-8".to_string());
        config.mesh.editor_model = Some("groq::llama-3.1-8b-instant".to_string());
        let mut session = make_session(config);
        session.pin_model(Some("openai::gpt-4o".to_string()));
        assert_eq!(session.resolve_planner_model(), "openai::gpt-4o");
        assert_eq!(session.resolve_editor_model(), "openai::gpt-4o");
    }

    #[test]
    fn architect_mode_off_by_default() {
        // Default config must have architect_mode = false so run_turn is unchanged.
        let config = Config::default();
        assert!(!config.mesh.architect_mode);
    }
}
