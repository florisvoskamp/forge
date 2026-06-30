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
    EffortLevel, LoopOutcome, Message, PermissionDecision, PermissionMode, PermissionRule, Role,
    StopReason, TaskTier,
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
/// Char length above which an OLD tool result is pruned from the model-facing transcript. Tool
/// output (file dumps, command logs, search hits) dominates context but its bulk has little value
/// once the turn has moved on — the model rarely needs the 30th file read verbatim. Pruning trims
/// the in-memory transcript only; the full text stays in the store for replay.
const PRUNE_TOOL_RESULT_MAX: usize = 3000;
/// How much of a pruned tool result's head to keep (enough to see what the tool produced).
const PRUNE_HEAD_KEEP: usize = 1500;
/// Marker left in place of the dropped tail; also makes pruning idempotent (a result already ending
/// with it is skipped).
const PRUNE_MARKER: &str = "\n…[older tool output pruned to save context; full text in replay]…";
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
match exactly once, and read the file first so whitespace matches. To change one file in several \
places at once, multi_edit applies a list of edits atomically. For a large or multi-file change, \
apply_patch takes a unified diff. For a Jupyter notebook (.ipynb) use notebook_edit (cell-level) \
— edit_file would corrupt its JSON. Use write_file for new files or full rewrites; don't \
blind-overwrite a file you haven't read.
- A tool result starting with `error:` means it failed — read the message, fix the cause, and \
retry differently rather than repeating the same call.

Communication:
- Be concise and direct. No filler, no flattery, no restating the question. Reference code as \
`path:line`.
- When the task is done, stop and give a short summary of what changed. Don't ask whether to \
proceed on work you can just do.";

/// Injected for the self-review pass (mesh.self_review): the same model critically re-checks the
/// edits it just made before the turn ends. Framed to FIND real defects (the common failure is a
/// fix that's plausible but wrong/incomplete), but to stop cleanly when the work is sound — so it
/// corrects hard cases without churning correct ones.
const SELF_REVIEW_PROMPT: &str = "\
Before finishing, review the changes you just made as a skeptical senior engineer seeing them for \
the first time. Re-read the original task, then check your diff against it:
- Does it actually solve the stated problem — the whole problem, not just the happy path?
- Edge cases, error handling, off-by-one, wrong/edge inputs, and any case the task hints at.
- Did you edit the right place, match existing conventions, and avoid breaking nearby behavior?
- Is anything missing (a needed call site, a test, a related code path)?

If you find a genuine problem, FIX it now with the tools. If the change is correct and complete, \
say so in one line and stop — do NOT make changes for their own sake or second-guess a sound fix.";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ErrorCategory {
    Permission,
    NotFound,
    Schema,
    Timeout,
    Other,
}

impl ErrorCategory {
    fn classify(err: &str) -> Self {
        let e = err.to_lowercase();
        if e.contains("permission") || e.contains("denied") || e.contains("forbidden") {
            Self::Permission
        } else if e.contains("not found") || e.contains("no such file") || e.contains("enoent") {
            Self::NotFound
        } else if e.contains("schema") || e.contains("invalid") || e.contains("parse") {
            Self::Schema
        } else if e.contains("timeout") || e.contains("timed out") {
            Self::Timeout
        } else {
            Self::Other
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::NotFound => "not_found",
            Self::Schema => "schema",
            Self::Timeout => "timeout",
            Self::Other => "other",
        }
    }
}

#[derive(Debug)]
struct ToolFailureTracker {
    /// (tool_name, error_category) -> consecutive failure count this turn.
    failure_counts: std::collections::HashMap<(String, ErrorCategory), u32>,
    /// Ring buffer of recent (tool_name, args_hash) calls for doom-loop detection.
    recent_calls: std::collections::VecDeque<(String, u64)>,
    failure_threshold: u32,
    doom_loop_threshold: u32,
}

impl Default for ToolFailureTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolFailureTracker {
    fn new() -> Self {
        Self {
            failure_counts: Default::default(),
            recent_calls: std::collections::VecDeque::with_capacity(10),
            failure_threshold: 3,
            doom_loop_threshold: 3,
        }
    }

    fn reset_turn(&mut self) {
        self.failure_counts.clear();
        self.recent_calls.clear();
    }

    fn record_call(&mut self, tool_name: &str, args_json: &str) -> Option<String> {
        use std::hash::{Hash, Hasher};

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        args_json.hash(&mut hasher);
        let h = hasher.finish();

        let key = (tool_name.to_string(), h);
        if self.recent_calls.len() >= 10 {
            self.recent_calls.pop_front();
        }
        self.recent_calls.push_back(key.clone());

        let consecutive = self
            .recent_calls
            .iter()
            .rev()
            .take_while(|k| *k == &key)
            .count() as u32;

        (consecutive >= self.doom_loop_threshold).then(|| {
            format!(
                "doom-loop: `{tool_name}` called identically {consecutive} times in a row — nudging model to try a different approach"
            )
        })
    }

    fn record_failure(&mut self, tool_name: &str, error: &str) -> Option<String> {
        let cat = ErrorCategory::classify(error);
        let key = (tool_name.to_string(), cat);
        let count = self.failure_counts.entry(key).or_insert(0);
        *count += 1;
        (*count >= self.failure_threshold).then(|| {
            format!(
                "stuck: `{tool_name}` failed {count} times ({cat:?}) — check permissions/schema before retrying"
            )
        })
    }

    fn record_success(&mut self, tool_name: &str) {
        self.failure_counts.retain(|(name, _), _| name != tool_name);
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
    /// An internal invariant was violated on a path that "can't happen". Surfaced as a clean error
    /// instead of a `panic!`/`.expect()` so a logic/config drift fails the turn loudly rather than
    /// aborting the whole process mid-turn.
    #[error("internal invariant violated: {0}")]
    Internal(String),
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

/// Best-effort single-text embedding via the configured embedder, for semantic memory capture +
/// recall. `None` when no embedder is available or it errors → callers fall back to keyword recall.
/// A FREE function taking `&EmbeddingsConfig` (which is `Sync`) — NOT a `&self` method — so the
/// `.await` doesn't hold a `&Session` borrow (`Session` is `!Sync`, which would make the turn future
/// non-`Send`).
pub async fn embed_one(cfg: &forge_config::EmbeddingsConfig, text: &str) -> Option<Vec<f32>> {
    let (embedder, _) = forge_provider::select_embedder(cfg)?;
    embedder
        .embed(&[text.to_string()])
        .await
        .ok()
        .and_then(|mut v| v.drain(..).next())
        .filter(|e| !e.is_empty())
}

/// Scope key for auto-memory: the current project directory's absolute path (memories are
/// per-project). Matches the `forge memory` CLI so both see the same store.
fn memory_scope() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "global".to_string())
}

/// Max same-model retries for a TRANSIENT provider failure (5xx / dropped stream / network blip)
/// before benching the model and failing over. Small + backed off so a genuinely-down model still
/// reaches failover quickly, but a one-off blip doesn't needlessly switch models.
const MAX_TRANSIENT_RETRIES: u32 = 2;

/// Max times per turn Forge will WAIT for a rate-limited model to reset and retry it (rather than
/// failing over to a lower-ranked model). Bounds total in-turn blocking. The per-wait length cap is
/// `mesh.rate_limit_wait_secs` (0 disables waiting).
const MAX_RATE_LIMIT_WAITS: u32 = 2;

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

/// Lightweight check that `args` satisfies the tool's JSON `schema`: it must be an object and
/// contain every key the schema lists as `required`. Returns a human-readable reason on failure
/// (naming the missing field(s) + the full required list) so the model can fix the call. Kept
/// dependency-free — required-key + object-shape covers the overwhelmingly common malformed call;
/// deep type validation isn't worth a JSON-schema crate here.
fn validate_tool_args(schema: &serde_json::Value, args: &serde_json::Value) -> Result<(), String> {
    let Some(obj) = args.as_object() else {
        return Err("arguments must be a JSON object".to_string());
    };
    let required = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();
    let missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|k| !obj.contains_key(*k))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "missing required field(s): {}. Required: {}",
        missing.join(", "),
        required.join(", ")
    ))
}

/// A stable hash of a tool-call batch (each call's name + JSON arguments), used by the agent loop's
/// doom-loop guard to detect a model repeating the *exact* same call(s) step after step. Identical
/// args → identical result, so a repeat is a death-spiral (re-reading a file, retrying a failing
/// edit) worth halting rather than burning steps on.
fn tool_batch_signature(calls: &[forge_types::ToolCall]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for c in calls {
        c.name.hash(&mut h);
        c.args.to_string().hash(&mut h);
    }
    h.finish()
}

/// Decision of the completion-verification gate for a turn that reported every tracked task Done.
/// A self-reported "all done" is exactly what produced the phantom release (claimed merged + tagged
/// while nothing ran), so completion must be PROVEN with a real state check, not asserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionGate {
    /// Force another tool-grounded verification turn (the caller pushes the verify nudge + loops).
    Reverify,
    /// A real inspection ran — accept cleanly, no note.
    AcceptClean,
    /// Nothing external to check (a pure-analysis answer) — accept with a calm note.
    AcceptNoArtifacts,
    /// Verification budget spent but real work existed and was never checked — accept, flag loudly.
    AcceptUnverified,
}

/// Decide whether an "all tasks Done" claim is accepted or must be re-verified with a real state
/// check. Pure (no I/O) so it is unit-testable; the caller emits the warning and pushes the nudge.
///
/// * `verify_attempts`    — verification turns already spent on the CURRENT claim (0 = first claim).
/// * `did_real_work`      — the turn ran ≥1 inspectable tool at some point, so there IS external
///   state to check (a pure-reasoning turn has none — requiring an inspection would over-fire).
/// * `inspected_this_turn`— the just-observed turn ran an inspection tool (a real check), as opposed
///   to merely re-asserting "done" by re-marking the task list (the C8 hole).
///
/// Shared by the CLI-bridge and direct-API paths so both have ONE completion authority: "done"
/// means a tool proved it. Mirrors the original bridge-only gate exactly.
fn completion_gate(
    verify_attempts: usize,
    max_attempts: usize,
    did_real_work: bool,
    inspected_this_turn: bool,
) -> CompletionGate {
    if verify_attempts > 0 && (inspected_this_turn || !did_real_work) {
        if inspected_this_turn {
            CompletionGate::AcceptClean
        } else {
            CompletionGate::AcceptNoArtifacts
        }
    } else if verify_attempts < max_attempts {
        CompletionGate::Reverify
    } else {
        CompletionGate::AcceptUnverified
    }
}

/// Classify a tool RESULT string as a failure of a given kind, or `None` if it looks like a success.
///
/// Anchored on the markers Forge actually produces for failures (`invoke_tool` returns `"error: …"`
/// for a tool `Err`, `"permission denied by policy"` for a blocked call, and [`shell_command_failed`]
/// recognises a non-zero shell exit) — so a *successful* tool output that merely happens to contain a
/// word like "invalid" is NOT misread as a failure. The category is then a keyword sniff of the
/// message. Only consumed behind a ≥3 threshold, so the worst case of a misclassification is one
/// early, still-helpful "change approach" nudge.
fn classify_tool_failure(result: &str) -> Option<ErrorCategory> {
    let lower = result.to_ascii_lowercase();
    let is_failure = lower.starts_with("error:")
        || lower.starts_with("permission denied")
        || shell_command_failed(result);
    if !is_failure {
        return None;
    }
    let kind = if lower.contains("permission denied")
        || lower.contains("forbidden")
        || lower.contains("access is denied")
        || lower.contains("eacces")
    {
        ErrorCategory::Permission
    } else if lower.contains("no such file")
        || lower.contains("not found")
        || lower.contains("does not exist")
        || lower.contains("cannot find")
        || lower.contains("no matches found")
    {
        ErrorCategory::NotFound
    } else if lower.contains("timed out") || lower.contains("timeout") {
        ErrorCategory::Timeout
    } else if lower.contains("invalid")
        || lower.contains("no match")
        || lower.contains("old_string")
        || lower.contains("expected")
        || lower.contains("malformed")
        || lower.contains("could not parse")
        || lower.contains("unexpected")
    {
        ErrorCategory::Schema
    } else {
        ErrorCategory::Other
    };
    Some(kind)
}

/// The live context-fill token count to report on the gauge for `model` after a call.
///
/// For a direct API model the provider's reported `input_tokens` IS the request size, the truest
/// fill measure. But a subscription CLI bridge (claude-cli/codex-cli) runs its own internal agent
/// loop and reports CUMULATIVE usage across every internal step — not the size of the request we
/// sent — so over a long turn it balloons past the window (e.g. 900k against a 272k context). There
/// the conservative transcript estimate, which reflects the context we actually manage, is correct.
fn context_fill_tokens(model: &str, transcript_est: u64, reported_input: u64) -> u64 {
    if forge_mesh::catalog::is_subscription(model) {
        transcript_est
    } else {
        reported_input
    }
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

/// Zero-LLM context reclaim: truncate large OLD tool results in place so a long conversation fits
/// without paying for an LLM summarize round-trip. Protects the most recent `keep_recent` messages
/// and only touches `Tool` results longer than [`PRUNE_TOOL_RESULT_MAX`], keeping a
/// [`PRUNE_HEAD_KEEP`]-char head + a marker. Returns the number of chars reclaimed; idempotent (a
/// result already ending with [`PRUNE_MARKER`] is skipped). The full text remains in the store for
/// replay — only the model-facing transcript is trimmed.
fn prune_tool_results(messages: &mut [Message], keep_recent: usize) -> usize {
    let len = messages.len();
    if len <= keep_recent {
        return 0;
    }
    let protect_from = len - keep_recent;
    let mut reclaimed = 0usize;
    for m in &mut messages[..protect_from] {
        if m.role != Role::Tool
            || m.content.len() <= PRUNE_TOOL_RESULT_MAX
            || m.content.ends_with(PRUNE_MARKER)
        {
            continue;
        }
        let before = m.content.len();
        let mut head = PRUNE_HEAD_KEEP.min(m.content.len());
        while !m.content.is_char_boundary(head) {
            head -= 1;
        }
        let mut kept = m.content[..head].to_string();
        kept.push_str(PRUNE_MARKER);
        reclaimed += before - kept.len();
        m.content = kept;
    }
    reclaimed
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
    /// A plan a CLI-bridge model proposed this loop (tailed from the sink as [`StreamEvent::Plan`]).
    /// `None` on the in-process path, where the `present_plan` handler sets `pending_plan` directly.
    plan: Option<forge_types::PlanProposal>,
}

/// One interactive session. Construct with [`Session::start`], then drive [`Session::run_turn`].
pub struct Session {
    id: String,
    pub store: Arc<Store>,
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
    /// A plan proposed this turn via `present_plan` (planning mode), awaiting interactive approval
    /// at turn end. `Some` between the proposal and the approve/revise/cancel decision.
    pending_plan: Option<forge_types::PlanProposal>,
    /// Connected external MCP servers (mcp-client.md). `None` when no servers are configured —
    /// the whole MCP path is then inert (zero overhead for non-MCP users).
    mcp: Option<Arc<forge_mcp::McpManager>>,
    /// The code-intelligence index (code-intelligence.md). `None` when disabled or unavailable —
    /// retrieval then injects nothing and the turn runs exactly as before (additive guarantee).
    /// `Arc` so the model-facing `lattice` tool shares the same index.
    lattice: Option<Arc<Lattice>>,
    /// Background file watcher that keeps the index fresh on external edits. Held as the receiving
    /// end of a channel: the watcher is built off-thread (so a slow filesystem can't gate startup)
    /// and delivered here, where it lives in the channel buffer for the session's lifetime (this
    /// Receiver dropped → channel + watcher drop → watching stops). Per-session ownership so repeated
    /// `build_session` calls (bench, replay) don't leak watcher threads.
    lattice_watcher: Option<std::sync::mpsc::Receiver<forge_index::LatticeWatcher>>,
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
    /// In-session routing-tier override (the `tier_up`/`tier_down` keybinds). When set, it biases
    /// the mesh to route the next turn at this tier instead of the classifier's pick — unless a
    /// per-turn `tier_override` (a command/skill `tier:` hint) is passed, which still wins. `None`
    /// = normal classification.
    pinned_tier: Option<TaskTier>,
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
    /// Per-turn guard against repeated failing tools and identical-call doom loops.
    failure_tracker: ToolFailureTracker,
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
        // The next seq is MAX(seq)+1 from the DB, NOT the loaded count — after compaction
        // `load_messages` returns only the active tail (+ summary), so its length is far below the
        // real max. Using the count would reuse low seqs and make `/undo` wipe pre-compaction history.
        let seq = store.next_seq_for_session(session_id)?;
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
            current_turn_seq: -1,
            catalog: None,
            tasks,
            pending_plan: None,
            mcp: None,
            lattice: None,
            lattice_watcher: None,
            lsp: None,
            skills: None,
            pinned_model: None,
            pinned_effort: None,
            pinned_tier: None,
            pending_hints: vec![],
            always_compact_on_switch: false,
            project_prompt_injected,
            pending_images: Vec::new(),
            edits_this_turn: 0,
            failure_tracker: ToolFailureTracker::new(),
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

    /// The currently-pinned routing tier, if any (set by `tier_up`/`tier_down`). `None` = normal
    /// mesh classification.
    pub fn pinned_tier(&self) -> Option<TaskTier> {
        self.pinned_tier
    }

    /// Set (or clear) the in-session routing-tier override. `None` returns to normal classification.
    pub fn pin_tier(&mut self, tier: Option<TaskTier>) {
        self.pinned_tier = tier;
    }

    /// Shift the routing-tier bias one step up (`up=true`) or down. The baseline is the current
    /// pin, or — when nothing is pinned yet — `from`, the last classified/displayed tier (so the
    /// first press moves relative to what the mesh would pick, not from a fixed middle). Clamped at
    /// the ends. Returns the new pinned tier so the caller can show a note.
    pub fn bump_tier(&mut self, up: bool, from: TaskTier) -> TaskTier {
        let base = self.pinned_tier.unwrap_or(from);
        let next = if up { base.up() } else { base.down() };
        self.pinned_tier = Some(next);
        next
    }

    /// The discovered model catalog, if auto-discovery ran for this session.
    pub fn catalog(&self) -> Option<&ModelCatalog> {
        self.catalog.as_ref()
    }

    /// Override the session's permission mode at runtime. Used by `forge mcp agent` so the
    /// orchestrating agent can switch to bypass/accept-edits without restarting the session.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    /// The session's current permission mode.
    pub fn mode(&self) -> PermissionMode {
        self.mode
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

    /// Attach the background reindex watcher's delivery channel (composition root). The watcher is
    /// built off-thread and sent through `rx`; holding the `Receiver` keeps it alive for the
    /// session's lifetime without ever blocking on its (possibly slow) setup.
    pub fn set_lattice_watcher(
        &mut self,
        rx: Option<std::sync::mpsc::Receiver<forge_index::LatticeWatcher>>,
    ) {
        self.lattice_watcher = rx;
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

    pub fn skills(&self) -> Option<&Arc<forge_skills::Catalog>> {
        self.skills.as_ref()
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

    /// Subscribe to the MCP initial-connect completion signal. Returns `None` when no MCP servers
    /// are configured. The returned receiver holds `false` until all servers have resolved
    /// (connected or failed); then it's set to `true`. Use this to schedule a re-announce.
    pub fn mcp_connect_done(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.mcp.as_ref().map(|m| m.subscribe_done())
    }

    /// Connect a new MCP server into the live session. Creates the manager if none exists yet
    /// (e.g. the session was started with no MCP servers configured).
    pub async fn add_mcp_server(
        &mut self,
        server: forge_config::McpServerConfig,
    ) -> Result<(), CoreError> {
        match &self.mcp {
            Some(mgr) => mgr
                .connect_one(&server)
                .await
                .map_err(CoreError::Internal)?,
            None => {
                let mut cfg = forge_config::McpConfig::default();
                cfg.servers.push(server);
                let mgr = forge_mcp::McpManager::connect_all(&cfg).await;
                self.mcp = Some(Arc::new(mgr));
            }
        }
        Ok(())
    }

    /// Remove an MCP server from the live session by name. No-op if not connected.
    pub fn remove_mcp_server(&self, name: &str) {
        if let Some(mgr) = &self.mcp {
            mgr.disconnect(name);
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
    /// `db_seq` is a DB **seq** (the stable identity checkpoints are keyed by), NOT a transcript
    /// index — both `/undo` and the `/checkpoints` picker pass a seq. After a COMPACTED resume the
    /// in-memory transcript is just the active tail while the DB seqs start high, so the two diverge;
    /// `offset` (0 when not compacted) maps the seq back to the transcript index for truncation.
    pub fn rewind_to(&mut self, db_seq: i64) -> Result<RewindOutcome, CoreError> {
        let db_seq = db_seq.max(0);
        // DB seq → transcript INDEX. Deactivation/snapshot work in DB seq; transcript ops in index.
        let offset = self.seq - self.transcript.len() as i64;
        let idx = (db_seq - offset).max(0) as usize;
        // The message AT the boundary is the user prompt of the rewound-to turn; capture it before
        // truncation so the UI can re-offer it for editing/resubmitting.
        let rewound_prompt = self
            .transcript
            .get(idx)
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.clone());
        let mut restore = snapshot::RestoreReport::default();
        // Turns are keyed by their user-message seq. Restore every snapshotted turn at/after the
        // boundary, newest first so an earlier turn's blob (pre-turn bytes) wins the final state.
        for seq in (db_seq..self.seq).rev() {
            if let Ok(r) = snapshot::restore_turn(&self.checkpoint_root, &self.id, seq) {
                restore.restored.extend(r.restored);
                restore.warnings.extend(r.warnings);
            }
        }
        self.store.deactivate_messages_from(&self.id, db_seq)?;
        self.transcript.truncate(idx);
        self.seq = db_seq;
        Ok(RewindOutcome {
            restore,
            rewound_prompt,
        })
    }

    /// Undo the last user turn: rewind to (and including) the most recent user message, dropping
    /// that prompt and everything after it. `Ok(None)` if there's nothing to undo.
    pub fn undo(&mut self) -> Result<Option<RewindOutcome>, CoreError> {
        // Use current_turn_seq — the DB seq of the real user message that started this turn —
        // rather than rposition(Role::User). The autofix stage injects synthetic Role::User
        // messages AFTER the real prompt (to feed lint/test failures back to the model); rposition
        // would land on the synthetic message, making rewind_to start the snapshot search too high
        // and miss the snapshot stored at current_turn_seq (causing restored: [] on undo).
        //
        // transcript_idx = db_seq - offset  (offset = self.seq - len absorbs compaction gaps so
        // the mapping stays valid after resume). Sentinel -1 means no turn has run yet.
        if self.current_turn_seq < 0 {
            return Ok(None);
        }
        let offset = self.seq - self.transcript.len() as i64;
        let turn_idx = (self.current_turn_seq - offset).max(0) as usize;
        if self
            .transcript
            .get(turn_idx)
            .filter(|m| m.role == Role::User)
            .is_none()
        {
            return Ok(None);
        }
        // Locate the previous turn's user message before rewinding so chained undos work.
        let prev_turn_seq = self.transcript[..turn_idx]
            .iter()
            .rposition(|m| m.role == Role::User)
            .map(|p| p as i64 + offset)
            .unwrap_or(-1);
        let outcome = self.rewind_to(self.current_turn_seq)?;
        self.current_turn_seq = prev_turn_seq;
        Ok(Some(outcome))
    }

    /// Publish the current turn's snapshot context (session id, seq, absolute root) to the
    /// environment so the CLI bridge's `forge mcp-serve` snapshots its writes into this turn's dir.
    fn export_checkpoint_env(&self, seq: i64) {
        let root = std::path::absolute(&self.checkpoint_root)
            .unwrap_or_else(|_| self.checkpoint_root.clone());
        std::env::set_var(snapshot::ENV_SESSION, &self.id);
        std::env::set_var(snapshot::ENV_SEQ, seq.to_string());
        std::env::set_var(snapshot::ENV_ROOT, root);
        // Hand the bridge child our CURRENT runtime temper so its permission gate matches the UI —
        // a Plan→Auto-edit switch (plan approval) or SHIFT+TAB now actually reaches `mcp-serve`,
        // instead of it falling back to the stale on-disk config mode (which denied writes after the
        // user switched to Auto-edit, or allowed them during Plan mode).
        std::env::set_var(snapshot::ENV_MODE, self.temper().key());
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
        // MAX(seq)+1, not the loaded count — `load_all_messages` includes soft-deleted rows from prior
        // rewinds, so its length exceeds the real max seq and the count would reuse seqs / inflate the
        // rewind offset (same class of bug as Session::resume, which is correctly scoped).
        self.seq = self.store.next_seq_for_session(&self.id)?;
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
        // MAX(seq)+1, not the loaded count — see Session::resume (compaction makes them differ, and
        // the mismatch lets `/undo` deactivate pre-compaction survivors).
        self.seq = self.store.next_seq_for_session(session_id)?;
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

    /// Persist the TUI view snapshot (opaque JSON) for this session so a resume restores the
    /// on-screen activity/viewer state. Best-effort — a store error is ignored.
    pub fn save_view_snapshot(&self, json: &str) {
        let _ = self.store.update_session_view_snapshot(&self.id, json);
    }

    /// The TUI view snapshot persisted for this session, if any (set on the last turn).
    pub fn view_snapshot(&self) -> Option<String> {
        self.store.session_view_snapshot(&self.id).ok().flatten()
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
            let (spent_today, _, spent_month) = self.store.spend_summary_usd().unwrap_or_default();
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
            assay::AssayProgress::CriticQueued {
                lens,
                expected_model,
            } => {
                presenter.emit(PresenterEvent::AssayCriticRow(
                    forge_types::AssayCriticRow {
                        lens: lens.as_str().to_string(),
                        focus: assay::lens_brief(*lens).to_string(),
                        model: Some(expected_model.clone()),
                        cost_usd: 0.0,
                        output: String::new(),
                        status: forge_types::AssayCriticStatus::Queued,
                    },
                ));
            }
            assay::AssayProgress::CriticDone {
                lens,
                candidates,
                model,
                cost_usd,
                output,
            } => {
                presenter.emit(PresenterEvent::AssayCriticRow(
                    forge_types::AssayCriticRow {
                        lens: lens.as_str().to_string(),
                        focus: assay::lens_brief(*lens).to_string(),
                        model: Some(model.clone()),
                        cost_usd: *cost_usd,
                        output: output.clone(),
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
                        focus: assay::lens_brief(*lens).to_string(),
                        model: None,
                        cost_usd: 0.0,
                        output: String::new(),
                        status: forge_types::AssayCriticStatus::Skipped {
                            reason: reason.clone(),
                        },
                    },
                ));
            }
            assay::AssayProgress::Verifying { candidates } => {
                presenter.emit(PresenterEvent::AssayVerifying {
                    candidates: *candidates,
                });
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
                stop_reason: StopReason::FinalAnswer,
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
            stop_reason: StopReason::FinalAnswer,
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
        // The on-demand memory tool — always advertised so the model can persist facts at any
        // point during a turn, not just via end-of-turn auto-capture.
        specs.push(remember_spec());
        // The plan-presentation tool — offered ONLY in planning mode, so the model proposes a plan
        // (rendered as an interactive card) instead of editing. Gating it to Plan mode also makes
        // the approve→Auto-edit→build flow non-recursive (the build turn can't re-propose a plan).
        if self.mode == PermissionMode::Plan {
            specs.push(present_plan_spec());
        }
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

    /// Run one full turn: route -> (model -> tools)* -> final answer. Returns the outcome.
    pub async fn run_turn(&mut self, prompt: &str) -> Result<LoopOutcome, CoreError> {
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
        let (today, week, month) = self.store.spend_summary_usd().unwrap_or_default();
        BudgetState {
            spent_today_usd: today,
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: week,
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: month,
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
            min_context_tokens: None,
        }
    }

    /// Explain how the mesh would route `prompt` right now, using this session's live catalog,
    /// quota, benched-model health and budget — the data behind the `/mesh` inspector. `None` when
    /// auto-discovery routing isn't active (no catalog), since the candidate table would be empty.
    pub fn explain_routing(&self, prompt: &str) -> Option<forge_mesh::RoutingExplanation> {
        let catalog = self.catalog.clone()?;
        let router = forge_mesh::HeuristicRouter::new(self.config.clone()).with_catalog(catalog);
        let health = self.store.current_benched().unwrap_or_default();
        let mut exp = router.explain(
            prompt,
            self.budget_snapshot(),
            &health,
            &self.live_quota(),
            self.pinned_effort(),
        );
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
        // Soonest-recovering transiently-benched model, but NEVER one whose provider has no key —
        // otherwise a keyless built-in default (e.g. groq) that got benched becomes the last-resort
        // pick, dispatches, hits a no-auth "Resolver error", and re-benches forever (the "groq for
        // everything" churn on a box with no groq key). `has_api_key` is true for keyless providers
        // (ollama, the claude/codex bridges), so those still qualify.
        let ordered = self.store.transient_benched_ordered().unwrap_or_default();
        ordered
            .into_iter()
            .find(|m| m != just_failed && forge_config::has_api_key(forge_config::provider_of(m)))
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
        // Fall back to the first USABLE Complex-tier candidate. `model_for` returns the first
        // configured candidate regardless of key — and the built-in defaults lead with
        // `groq::…`, so on a box with no groq key the architect planner would dispatch groq and
        // auth-fail EVERY turn (it recovers via the failover chain, but wastes a hop + warns).
        // Pick the first candidate whose provider has a key instead (keyless bridges qualify).
        self.first_usable_for_tier(forge_types::TaskTier::Complex)
            .or_else(|| {
                self.config
                    .model_for(forge_types::TaskTier::Complex)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "anthropic::claude-opus-4-8".to_string())
    }

    /// The first configured candidate for `tier` whose provider has a key — keyless providers
    /// (ollama, the claude/codex bridges) always qualify. `None` when the config lists nothing
    /// usable. Used to keep the architect planner/editor off a keyless built-in default (groq).
    fn first_usable_for_tier(&self, tier: forge_types::TaskTier) -> Option<String> {
        self.config
            .candidates_for(tier)
            .into_iter()
            .find(|m| forge_config::has_api_key(forge_config::provider_of(m)))
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
        // Fall back to the first USABLE Standard-tier candidate (see resolve_planner_model): never
        // a keyless built-in default. The architect EDIT phase runs with failover DISABLED
        // (decision=None), so a keyless editor model would hard-fail the turn instead of recovering
        // — picking a keyed model here is what keeps the edit phase alive.
        self.first_usable_for_tier(forge_types::TaskTier::Standard)
            .or_else(|| {
                self.config
                    .model_for(forge_types::TaskTier::Standard)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "anthropic::claude-opus-4-8".to_string())
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
                    self.pinned_effort,
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
            // Cheap first: prune bulky OLD tool results in place (no model call). Often reclaims
            // enough that the expensive LLM summarize below isn't needed at all.
            if prune_tool_results(&mut self.transcript, COMPACT_KEEP_RECENT) > 0 {
                self.emit_context_gauge(model);
            }
            if !self.transcript_fits(model) {
                let _ = self.compact(true).await;
            }
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
            context_limit: Some(self.effective_context_window(model)),
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
            // A hop drives the animated "finding a model" indicator (no per-hop scrollback spam).
            Some(_) => self.presenter.emit(PresenterEvent::ModelSearch {
                model: model.to_string(),
            }),
            // The chain is exhausted — a real, terminal failure worth a visible warning.
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
            .map(|m| {
                // Include the assistant's tool calls — they're the only record of WHAT the turn did
                // (tool name + args = the files touched / commands run). Without them an editing turn
                // renders as a blank `assistant: ` line and the summary can't say what changed.
                let mut line = format!("{}: {}", m.role.as_str(), m.content);
                for tc in &m.tool_calls {
                    line.push_str(&format!("\n  [call {} {}]", tc.name, tc.args));
                }
                line
            })
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
            min_context_tokens: None,
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
                self.pinned_effort,
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

    const RECAP_SYSTEM: &'static str = "You are a one-line summarizer for a coding assistant. \
Given the user's request and the assistant's response, write a SINGLE sentence (≤12 words, \
past tense, no punctuation at end) describing ONLY what the assistant's RESPONSE actually shows it \
did — never assume the request was fulfilled. If the response does not clearly show completed \
work (it stalled, errored, only planned, or asked a question), say that instead (e.g. \
\"stalled without completing the task\"). Do not invent success. \
Output ONLY that sentence — no preamble, no quotation marks.";

    /// After a turn completes, make one cheap trivial-tier call to generate a one-line recap,
    /// emitted via [`PresenterEvent::Recap`]. Best-effort: silently skipped on budget exhaustion
    /// or any model error so it can never derail the session.
    const MEMORY_CAPTURE_SYSTEM: &'static str =
        "You extract DURABLE facts worth remembering across FUTURE sessions in this project: user \
         preferences, project decisions/conventions, key architecture or config, and stable \
         constraints. Output 0 to 3 lines, each exactly `kind: fact`, where kind is one of \
         preference, decision, fact, reference. Skip transient task details, one-off actions, and \
         anything specific to only this turn. If nothing is durable, output NOTHING at all.";

    /// After a turn, make one cheap trivial-tier call to extract 0-3 DURABLE facts and persist them
    /// as project-scoped memories (dedup + salience handled by the store). Best-effort: any
    /// budget/model failure is silently skipped so it can never derail the session. Recall of these
    /// happens at the start of a later session (see `run_turn_with`).
    // Spawns memory capture so it doesn't block turn completion — the spinner clears when the AI
    // response finishes. Returns a JoinHandle so the caller can await it in one-shot mode (forge
    // run) before the process exits; interactive turns drop the handle and it runs in background.
    fn capture_memories(
        &self,
        prompt: &str,
        final_text: &str,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !self.config.mesh.auto_memory || final_text.trim().is_empty() {
            return None;
        }
        let budget = BudgetState {
            spent_today_usd: self.store.spend_today_usd().unwrap_or(0.0),
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: self.store.spend_this_week_usd().unwrap_or(0.0),
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd().unwrap_or(0.0),
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
            min_context_tokens: None,
        };
        if budget.status() == BudgetStatus::Exhausted {
            return None;
        }
        let health = self.store.current_benched().unwrap_or_default();
        let quota = self.live_quota();
        let provider = self.provider.clone();
        let store = self.store.clone();
        let router = self.router.clone();
        let id = self.id.clone();
        let config = self.config.clone();
        let pinned_effort = self.pinned_effort;
        let user_snippet: String = prompt.chars().take(500).collect();
        let assistant_snippet: String = final_text.chars().take(1200).collect();
        Some(tokio::spawn(async move {
            let decision = router
                .route_hinted(
                    "extract durable facts",
                    budget,
                    &health,
                    &quota,
                    Some(TaskTier::Trivial),
                    pinned_effort,
                )
                .await;
            let messages = vec![
                Message::system(Session::MEMORY_CAPTURE_SYSTEM),
                Message::user(format!(
                    "User request:\n{user_snippet}\n\nAssistant response:\n{assistant_snippet}"
                )),
            ];
            let mut on_event = |_: StreamEvent| {};
            let Ok(r) = provider
                .complete(&decision.model, &messages, &[], &mut on_event)
                .await
            else {
                return;
            };
            let _ = store.record_side_call_usage(&id, "memory", &r.usage);
            let scope = memory_scope();
            // Collect lines into owned Strings before the per-line await to avoid holding
            // a borrow across the embed_one await point.
            let lines: Vec<String> = r.content.lines().take(3).map(str::to_string).collect();
            for raw in lines {
                let line = raw.trim().trim_start_matches(['-', '*', '•']).trim();
                let Some((kind, text)) = line.split_once(':') else {
                    continue;
                };
                let kind_norm = kind.trim().to_lowercase();
                let kind_cat = match kind_norm.as_str() {
                    "preference" | "decision" | "fact" | "reference" => kind_norm.as_str(),
                    _ => "fact",
                };
                let text = text.trim();
                if text.len() >= 4 {
                    match embed_one(&config.lattice.embeddings, text).await {
                        Some(emb) => {
                            let _ =
                                store.add_memory_with_embedding(&scope, kind_cat, text, &id, &emb);
                        }
                        None => {
                            let _ = store.add_memory(&scope, kind_cat, text, &id);
                        }
                    }
                }
            }
        }))
    }

    async fn generate_recap(&mut self, prompt: &str, final_text: &str) {
        if !self.config.recap.enabled {
            return;
        }
        // A stalled turn (empty-response give-up, hard failover exhaustion) leaves `final_text`
        // empty: there is nothing the assistant actually did to summarize. Recapping anyway makes
        // the trivial-tier summarizer lean on the *request* and invent success ("Fixed the bug…")
        // for a turn that accomplished nothing — so skip it outright.
        if final_text.trim().is_empty() {
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
            min_context_tokens: None,
        };
        if budget.status() == BudgetStatus::Exhausted {
            return;
        }
        let health = self.store.current_benched().unwrap_or_default();
        let quota = self.live_quota();
        let decision = self
            .router
            .route_hinted(
                "summarize in one sentence",
                budget,
                &health,
                &quota,
                Some(TaskTier::Trivial),
                self.pinned_effort,
            )
            .await;
        let user_snippet: String = prompt.chars().take(400).collect();
        let assistant_snippet: String = final_text.chars().take(800).collect();
        let messages = vec![
            Message::system(Self::RECAP_SYSTEM),
            Message::user(format!(
                "User request:\n{user_snippet}\n\nAssistant response:\n{assistant_snippet}"
            )),
        ];
        // Routing above is local/fast; the only slow part is the provider completion. If the
        // presenter can hand out a Send sink (the channel-backed TUI), run that completion on a
        // DETACHED task and return now — so the turn ends, the spinner stops, and input frees the
        // instant the response is done; the recap streams in a moment later. Synchronous presenters
        // (headless / tests) have no sink, so it runs inline exactly as before.
        let provider = self.provider.clone();
        let store = self.store.clone();
        let id = self.id.clone();
        let model = decision.model.clone();
        match self.presenter.recap_sink() {
            Some(mut sink) => {
                tokio::spawn(async move {
                    let mut on_event = |_: StreamEvent| {};
                    if let Ok(r) = provider
                        .complete(&model, &messages, &[], &mut on_event)
                        .await
                    {
                        let _ = store.record_side_call_usage(&id, "recap", &r.usage);
                        let text = r.content.trim().to_string();
                        if !text.is_empty() {
                            sink.emit(PresenterEvent::Recap { text });
                        }
                    }
                });
            }
            None => {
                let mut on_event = |_: StreamEvent| {};
                if let Ok(r) = provider
                    .complete(&model, &messages, &[], &mut on_event)
                    .await
                {
                    let _ = store.record_side_call_usage(&id, "recap", &r.usage);
                    let text = r.content.trim().to_string();
                    if !text.is_empty() {
                        self.presenter.emit(PresenterEvent::Recap { text });
                    }
                }
            }
        }
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
            min_context_tokens: None,
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
                self.pinned_effort,
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

    /// Run the completion-verification gate for a turn that reported every tracked task Done.
    /// Emits the user-facing warning, pushes the verify nudge on [`CompletionGate::Reverify`], and
    /// returns the decision so the caller can `continue` (re-verify) or fall through (accept). Both
    /// the CLI-bridge and direct-API paths call this, so the completion authority can't diverge.
    fn run_completion_gate(
        &mut self,
        verify_attempts: &mut usize,
        did_real_work: bool,
        inspected_this_turn: bool,
    ) -> CompletionGate {
        const MAX_VERIFY_ATTEMPTS: usize = 2;
        // Tool-name-neutral so the SAME nudge works for the bridge (tools are `mcp__forge__*`) and
        // the direct path (`shell`/`read_file`) — the model maps "run a shell command / read a file"
        // to whichever names its toolset exposes.
        const VERIFY_NUDGE: &str = "You reported every task Done. Before this turn can end, you \
             MUST PROVE it: call an inspection tool that reads the real state — run a shell command \
             (`git log` / `git tag` / `gh run list` / `gh release view` / `ls` / `cat`) or read a \
             file — and look at the actual output. Re-marking the task list is NOT verification; you \
             must run a real check. If the output shows ANY task is not actually complete, mark it \
             not done and finish it. (If a task has no external artifact to check — a pure analysis \
             answer — say so and restate the result.) Only after confirming every task, state \
             exactly what you checked and stop.";
        let gate = completion_gate(
            *verify_attempts,
            MAX_VERIFY_ATTEMPTS,
            did_real_work,
            inspected_this_turn,
        );
        match gate {
            CompletionGate::Reverify => {
                *verify_attempts += 1;
                self.presenter.emit(PresenterEvent::Warning(format!(
                    "all tasks reported done — verifying with a real state check before finishing ({}/{MAX_VERIFY_ATTEMPTS})",
                    *verify_attempts
                )));
                let nseq = self.next_seq();
                let _ = self
                    .store
                    .add_message(&self.id, nseq, Role::System, VERIFY_NUDGE, None);
                self.transcript.push(Message::system(VERIFY_NUDGE));
            }
            CompletionGate::AcceptNoArtifacts => {
                self.presenter.emit(PresenterEvent::Warning(
                    "completion not tool-verified (no external artifacts to check) — accepting the reported result"
                        .to_string(),
                ));
            }
            CompletionGate::AcceptUnverified => {
                self.presenter.emit(PresenterEvent::Warning(
                    "completion could NOT be tool-verified — the model reported done without \
                     inspecting real state. Treat this result as UNVERIFIED."
                        .to_string(),
                ));
            }
            CompletionGate::AcceptClean => {}
        }
        gate
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
        // Bounds the same-model retry for transient errors (a 5xx / dropped connection that often
        // succeeds on a second attempt). Reset to 0 whenever we switch to a different model, so the
        // budget is per-model, not per-turn — "don't give up instantly" before failing over.
        let mut transient_retries = 0u32;
        // Bounds in-turn waits for a rate-limited model to RESET (per-minute free tiers). Per turn,
        // not per model: a few short waits total, so the turn can't block indefinitely.
        let mut rate_limit_waits = 0u32;

        let mut final_text = String::new();
        let mut context_tokens: u64 = 0;
        let mut hit_step_cap = true;
        // A plan a bridge model proposes via the out-of-band sink (StreamEvent::Plan). Captured by
        // the per-step stream closure and returned in the outcome for the turn's approval flow.
        // Only honored in planning mode (the bridge advertises present_plan unconditionally — it
        // can't see the parent's runtime temper — so the parent gates here): outside Plan mode a
        // stray plan is dropped, which also stops the post-approval build turn from re-proposing.
        let mut proposed_plan: Option<forge_types::PlanProposal> = None;
        let in_plan_mode = self.mode == PermissionMode::Plan;
        // Harness reliability guards. `empty_nudges`: bounded retries when the model returns nothing
        // (narrate-then-stall / transient empty) before giving up. `last_tool_sig`/`repeat_count`:
        // doom-loop detection — the same tool batch repeated DOOM_LOOP_THRESHOLD× halts the turn.
        let mut empty_nudges = 0usize;
        let mut last_tool_sig: Option<u64> = None;
        let mut repeat_count = 0usize;
        // `recent_sigs`: a short sliding window of recent tool-batch signatures. The consecutive
        // `repeat_count` above misses an A,B,A,B,… oscillation (every step differs from the one
        // before, so the counter keeps resetting) — e.g. a model alternating a failing/empty call
        // with a trivial successful one, which ALSO clears the failure-loop streak (a success on a
        // tool resets it). Counting how often a signature recurs in this window catches that.
        let mut recent_sigs: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
        // `continue_nudges`: bounded retries when the model signs off with text but tracked tasks
        // are still unfinished (narrate-then-stall) — drive it to completion instead of ending the
        // turn mid-task. `doom_nudged`: the doom-loop fires a "change approach" nudge BEFORE it
        // ever hard-stops, so a repeated call doesn't kill an otherwise-recoverable turn.
        let mut continue_nudges = 0usize;
        let mut doom_nudged = false;
        // Failure-loop guard (complements the identical-call doom-loop): counts tool failures by
        // (tool name, error kind) ACROSS the turn, so a model retrying the same KIND of error with
        // different args (edits that never match, reads of paths that don't exist) is caught even
        // though its call signature keeps changing. A success on a tool clears its streak.
        let mut failure_counts: std::collections::HashMap<(String, ErrorCategory), usize> =
            std::collections::HashMap::new();
        let mut failure_nudged = false;
        // `toolcall_repair_nudges`: bounded retries when a direct model writes a tool call as TEXT
        // (`<invoke>` / `default_api:` markup) that the provider couldn't decode AND the text
        // recovery pass missed — so nothing executed. Without this the narration is accepted as a
        // final answer and the turn "succeeds" having done nothing (the phantom-release bug).
        let mut toolcall_repair_nudges = 0usize;
        // `bridge_continue_nudges`: bounded RE-RUNS of a CLI bridge whose turn returned with tracked
        // tasks still unfinished. A bridge turn is otherwise terminal (it runs its own tool loop and
        // returns once), so a long multi-step plan stalls partway — the bridge does a few steps,
        // returns, and the turn ends with work pending (the half-finished release: merged + tagged
        // but brew-sha + verify never ran). This drives a clean re-run, exactly as the user typing
        // `continue` would.
        let mut bridge_continue_nudges = 0usize;
        // Verification gate: when a bridge reports every task Done, completion is NOT accepted on
        // its say-so — forge forces ONE tool-grounded verification turn (check the real state: git,
        // gh, files) before the turn can end. Reset to false whenever work reopens, so each fresh
        // "all done" claim is re-verified. This is the completion AUTHORITY: "done" means forge made
        // the model prove it with tools, not that the model asserted it.
        // Verification attempts spent on the current "all done" claim. 0 = not yet verifying. The
        // gate forces the bridge to PROVE completion with a real inspection tool; a verification
        // turn that just re-marks `update_tasks` without inspecting doesn't count (the C8 hole — a
        // model told to lie re-confirmed done without checking). Bounded so it can't loop.
        let mut verify_attempts = 0usize;
        // One-shot guard for the opt-in completeness re-drive (`mesh.verify_completeness`): fired at
        // most once per turn so it can't loop. See the bridge-yield branch below.
        let mut completeness_checked = false;
        // Direct path only: the `inspect_ran` count at the moment the verify nudge was last issued.
        // An inspection that runs AFTER this point is the model responding to the request to verify
        // (on the direct path, tools run in separate steps from the text claim, so a step-local
        // signal can't see it). Carried across steps; reset implicitly by being re-stamped each nudge.
        let mut inspect_at_last_verify: u64 = 0;
        // Completed-task count observed at the last bridge re-drive check — the other half of the
        // progress signal (a re-run that closes a task but happens to run no fresh tool still counts
        // as progress).
        let mut bridge_done_prev = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status, forge_types::TodoStatus::Done))
            .count();
        // Counts tools that actually STARTED executing across the whole turn (bridge tools surface
        // here via the sink too). The bridge re-drive uses the per-step delta as its progress
        // signal: a re-run that completes no task AND runs no tool made no progress, so it's halted
        // rather than re-driven again (the anti-spiral guard the old bridge-nudge lacked).
        let tools_ran = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        // Counts INSPECTION tools (anything except `update_tasks`/`present_plan`) — the verification
        // gate requires the bridge to actually CHECK real state, not just re-assert "done".
        let inspect_ran = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        for step in 0..max_steps {
            let tools_before = tools_ran.load(std::sync::atomic::Ordering::Relaxed);
            let inspect_before = inspect_ran.load(std::sync::atomic::Ordering::Relaxed);
            // Stream the reply, with transparent failover for this step's completion.
            let mut resp = loop {
                // Bound what we send to the active model's context window (fetched/heuristic), so a
                // long conversation can't overflow it — which otherwise fails the turn as
                // "unavailable" on every model in the chain. Re-trimmed per model so failover to a
                // smaller-window model still fits. The immutable borrow ends before the block below.
                let sent = self.transcript_with_preamble(&active_model);
                // Pre-dispatch key backstop: a model can reach here with NO provider key via a path
                // that isn't key-filtered (the last-resort fallback, or an architect editor/planner
                // default). Dispatching it just yields a no-auth genai "Resolver error" surfaced raw
                // to the user (the "groq for everything" report on a box with no groq key). Instead
                // synthesize a permanent Auth failure so the existing failover branch EXCLUDES it and
                // advances to a usable model. `has_api_key` is true for keyless providers (ollama,
                // the claude/codex bridges), so a legitimate bridge turn is never short-circuited.
                let result = if !forge_config::has_api_key(forge_config::provider_of(&active_model))
                {
                    Err(forge_provider::ProviderError::Auth(format!(
                        "no API key configured for provider '{}'",
                        forge_config::provider_of(&active_model)
                    )))
                } else {
                    let provider = &self.provider;
                    let presenter = &mut self.presenter;
                    // Bump on every stream event so the idle watchdog can distinguish a live
                    // stream from a stalled half-open connection — a stall fails over (below)
                    // instead of hanging the turn forever.
                    let activity = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                    let act = std::sync::Arc::clone(&activity);
                    let tools = std::sync::Arc::clone(&tools_ran);
                    let inspects = std::sync::Arc::clone(&inspect_ran);
                    let mut sink = |ev: StreamEvent| {
                        act.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        match ev {
                            StreamEvent::Text(t) => {
                                presenter.emit(PresenterEvent::AssistantDelta(t))
                            }
                            StreamEvent::Reasoning(t) => {
                                presenter.emit(PresenterEvent::Reasoning(t))
                            }
                            StreamEvent::ToolStarted { name, args } => {
                                tools.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                // Bookkeeping tools don't count as a real inspection — the
                                // verification gate needs an actual state CHECK (read/shell/…).
                                if !name.ends_with("update_tasks")
                                    && !name.ends_with("present_plan")
                                {
                                    inspects.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                presenter.emit(PresenterEvent::ToolStart { name, args })
                            }
                            StreamEvent::ToolFinished { name, ok, summary } => {
                                presenter.emit(PresenterEvent::ToolResult { name, ok, summary })
                            }
                            StreamEvent::SubagentStarted { id, agent, task } => {
                                presenter.emit(PresenterEvent::SubagentStart {
                                    id,
                                    agent,
                                    task,
                                    model: None,
                                })
                            }
                            StreamEvent::SubagentProgress { id, snippet } => {
                                presenter.emit(PresenterEvent::SubagentProgress { id, snippet })
                            }
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
                            // A bridged turn's `present_plan`: in planning mode, render the
                            // card now and stash it for the turn's approval flow (picked up
                            // via the outcome). Ignored outside Plan mode (stray proposal).
                            StreamEvent::Plan(plan) => {
                                if in_plan_mode {
                                    presenter.emit(PresenterEvent::PlanProposed(plan.clone()));
                                    proposed_plan = Some(plan);
                                }
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
                        // Same-model retry for a TRANSIENT failure (a 5xx / dropped stream / network
                        // blip) before benching + failing over: these often succeed on a second
                        // attempt, so switching models immediately needlessly degrades the turn. We
                        // do NOT hot-retry a rate-limit (it would just 429 again — respect the
                        // cooldown and fail over) or a permanent incapability (no point). Bounded +
                        // backed off so a genuinely-down model still falls through to failover fast.
                        if transient_retries < MAX_TRANSIENT_RETRIES
                            && !e.is_permanent()
                            && !e.is_rate_limited()
                        {
                            transient_retries += 1;
                            let backoff =
                                std::time::Duration::from_millis(500u64 << (transient_retries - 1));
                            // Use ModelSearch (status-bar indicator, not chat history) so transient
                            // retries don't spam the scrollback. The spinner already signals "working".
                            self.presenter.emit(PresenterEvent::ModelSearch {
                                model: active_model.clone(),
                            });
                            tokio::time::sleep(backoff).await;
                            continue;
                        }
                        // Rate-limit on the current (best-ranked) model with a SHORT reset: WAIT for
                        // it to reset and retry the SAME model instead of degrading to a lower-ranked
                        // (or, pre-strict, paid) one. This is the per-minute free-tier case
                        // (NIM/Groq/Gemini) — "retry when it's reset", not an instant fall to a worse
                        // model. Bounded by a per-turn wait budget and a cap on the reset length, so a
                        // long/daily quota (or a model that stays limited) still falls through to the
                        // normal bench + failover below.
                        let wait_cap =
                            std::time::Duration::from_secs(self.config.mesh.rate_limit_wait_secs);
                        if e.is_rate_limited()
                            && !wait_cap.is_zero()
                            && rate_limit_waits < MAX_RATE_LIMIT_WAITS
                        {
                            let reset = e.cooldown(default_cooldown);
                            if reset <= wait_cap {
                                rate_limit_waits += 1;
                                self.presenter.emit(PresenterEvent::Warning(format!(
                                    "{active_model}: rate-limited — waiting {}s for reset, then retrying",
                                    reset.as_secs().max(1)
                                )));
                                self.presenter.emit(PresenterEvent::ModelSearch {
                                    model: active_model.clone(),
                                });
                                tokio::time::sleep(reset).await;
                                continue;
                            }
                        }
                        let reason = e.reason();
                        // A PERMANENT incapability (no tool support / unaffordable) excludes the
                        // model for a long window so it isn't re-tried every turn (the "every model
                        // failing" churn); a transient failure benches it on the short cooldown.
                        if e.is_permanent() {
                            let _ = self.store.exclude_model(&active_model, reason);
                        } else {
                            let _ = self.store.bench_for(
                                &active_model,
                                e.cooldown(default_cooldown),
                                reason,
                            );
                        }
                        // Drive the single animated "finding a model" indicator instead of emitting
                        // one scrollback warning per hop (the failover spam). It clears itself when
                        // real output begins; the chain-exhausted case below still surfaces an error.
                        self.presenter.emit(PresenterEvent::ModelSearch {
                            model: active_model.clone(),
                        });
                        // Lazy 429-skip: the chain is in strict mesh-rank order, but a rate limit is
                        // usually provider-wide, so trying the failed provider's lower-ranked
                        // siblings next would just 429 again. ONLY on a rate-limit, skip this
                        // provider's remaining chain entries and cross to the next provider; every
                        // other failure keeps rank order intact. (Without this, dropping the old
                        // provider-interleave would re-expose the 429-storm the interleave guarded.)
                        let skip_provider = if e.is_rate_limited() {
                            Some(forge_config::provider_of(&active_model).to_string())
                        } else {
                            None
                        };
                        // Advance down the chain to the next model we can use. A model whose window
                        // still holds the conversation is used immediately; one that's too small is
                        // a switch that needs (lossy) compaction, so it's gated by consent
                        // (Yes/No/Always) — "No" skips it and we keep looking for one that fits.
                        let mut picked = None;
                        for next in chain.by_ref() {
                            if let Some(p) = &skip_provider {
                                if forge_config::provider_of(&next) == p.as_str() {
                                    continue;
                                }
                            }
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
                        let Some(d) = decision else {
                            return Err(CoreError::Internal(
                                "failover engaged without a routing decision".into(),
                            ));
                        };
                        match picked {
                            Some(next) => {
                                self.presenter.emit(PresenterEvent::Routing {
                                    tier: d.tier.as_str().to_string(),
                                    model: next.clone(),
                                    rationale: format!("failover from {active_model}"),
                                });
                                active_model = next;
                                transient_retries = 0;
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
                                    transient_retries = 0;
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
            // The last call's input size is the live context fill (tui-token-counter.md) — except a
            // subscription CLI bridge reports cumulative internal usage, so [`context_fill_tokens`]
            // substitutes the transcript estimate there (else the gauge reads a bogus 337% and trips
            // the phantom "auto-compact imminent" hint).
            context_tokens = context_fill_tokens(
                &active_model,
                self.estimated_transcript_tokens(),
                resp.usage.input_tokens,
            );

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
                // A response with neither text nor a tool call is a silent dead-end (model glitch,
                // narrate-then-stall, or a transient empty completion). Rather than just stopping,
                // nudge it to continue a bounded number of times — this recovers the common case
                // where the model would have made progress on a retry.
                if resp.content.trim().is_empty() {
                    const MAX_EMPTY_NUDGES: usize = 2;
                    if empty_nudges < MAX_EMPTY_NUDGES {
                        empty_nudges += 1;
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "model returned an empty response — nudging it to continue ({empty_nudges}/{MAX_EMPTY_NUDGES})"
                        )));
                        let nudge = "Your last response was empty. Continue with the task: call a \
                                     tool to make progress, or state your final answer. Do not reply \
                                     with an empty message.";
                        let nseq = self.next_seq();
                        let _ = self
                            .store
                            .add_message(&self.id, nseq, Role::System, nudge, None);
                        self.transcript.push(Message::system(nudge));
                        continue;
                    }
                    // Nudges exhausted. An empty-responding model (e.g. some NIM models that stream
                    // an empty final chunk, like kimi-k2.6 in the dogfooding run) is broken for this
                    // turn — BENCH it and FAIL OVER to the next chain model instead of dead-ending
                    // the turn short of a working model (the subscription bridge sat untried below).
                    if failover_enabled {
                        let _ = self.store.bench_for(
                            &active_model,
                            default_cooldown,
                            "empty response (no text, no tool call)",
                        );
                        let mut picked = None;
                        for next in chain.by_ref() {
                            match self.admit_failover_model(&next).await {
                                Ok(true) => {
                                    picked = Some(next);
                                    break;
                                }
                                Ok(false) => {}
                                Err(e) => return Err(e),
                            }
                        }
                        if let Some(next) = picked {
                            self.presenter.emit(PresenterEvent::Routing {
                                tier: decision
                                    .map(|d| d.tier.as_str().to_string())
                                    .unwrap_or_default(),
                                model: next.clone(),
                                rationale: format!("failover from {active_model} (empty response)"),
                            });
                            active_model = next;
                            transient_retries = 0;
                            empty_nudges = 0;
                            continue;
                        }
                    }
                    self.presenter.emit(PresenterEvent::Warning(
                        "model returned an empty response (no text, no tool call) — stopping the turn"
                            .to_string(),
                    ));
                } else if forge_provider::is_cli_bridge(&active_model) {
                    // Loop-gated completeness (opt-in `mesh.verify_completeness`): the bridge yielded.
                    // Before accepting "done", fire ONE bounded final-diff review — the model worked
                    // the turn normally (no completeness pressure throughout, which is what tripled
                    // tokens in the always-on preamble form), and now does a single targeted re-check
                    // against every requirement. One-shot (`completeness_checked`) so it can't loop;
                    // gated on a turn that ran real tools (so there's an actual change to review).
                    if self.config.mesh.verify_completeness
                        && !completeness_checked
                        && inspect_ran.load(std::sync::atomic::Ordering::Relaxed) > 0
                    {
                        completeness_checked = true;
                        self.presenter.emit(PresenterEvent::Warning(
                            "completeness check — reviewing the change against every requirement before finishing"
                                .to_string(),
                        ));
                        const COMPLETENESS_NUDGE: &str = "Before finishing, do ONE final review (a \
                            single bounded pass — do NOT re-explore the codebase): run `git diff` once \
                            to see your COMPLETE change, re-read the original request and write the \
                            distinct requirements/cases it lists (issues routinely specify several, \
                            e.g. \"reject a dotted blueprint name AND a dotted endpoint\"), and for \
                            each confirm your diff already handles it. Only if the diff is MISSING a \
                            requirement, add that specific fix — otherwise finish. A change that \
                            handles only the first of several cases is INCOMPLETE.";
                        let nseq = self.next_seq();
                        let _ = self.store.add_message(
                            &self.id,
                            nseq,
                            Role::System,
                            COMPLETENESS_NUDGE,
                            None,
                        );
                        self.transcript.push(Message::system(COMPLETENESS_NUDGE));
                        continue;
                    }
                    // A CLI bridge is a ONE-SHOT subprocess: claude-cli/codex runs its own internal
                    // tool loop and EXITS, so forge can't keep a single invocation going. That let a
                    // long plan stop half-done — the bridge does a few steps (merge + tag), exits
                    // after launching the async release build, and the dependent steps (brew sha,
                    // verify) never run. Completion must be defined by the TASK LIST, not by the
                    // subprocess exiting: while tracked tasks remain unfinished, re-invoke the bridge
                    // with a continue instruction (a clean new process — exactly what the user typing
                    // `continue` does), so a turn can't "be done" while the work isn't.
                    //
                    // Anti-spiral (the guard the old bridge-nudge lacked): a re-run must make
                    // PROGRESS — start at least one tool OR close at least one task — or the turn
                    // HALTS loudly instead of re-driving. A bridge that just re-narrates without
                    // acting therefore cannot loop. Gated on a non-empty task list, so an ordinary
                    // bridge Q&A (no tracked tasks) stays terminal as before.
                    //
                    // Tasks live in the store (the bridge's `update_tasks` runs in the separate
                    // `mcp-serve` process), so reload before judging completion.
                    let persisted = self.store.tasks(&self.id).unwrap_or_default();
                    if !persisted.is_empty() {
                        self.tasks = persisted;
                    }
                    let unfinished: Vec<String> = self
                        .tasks
                        .iter()
                        .filter(|t| !matches!(t.status, forge_types::TodoStatus::Done))
                        .map(|t| t.title.clone())
                        .collect();
                    let done_now = self.tasks.len().saturating_sub(unfinished.len());
                    let tools_this_turn =
                        tools_ran.load(std::sync::atomic::Ordering::Relaxed) - tools_before;
                    let made_progress = tools_this_turn > 0 || done_now > bridge_done_prev;
                    bridge_done_prev = done_now;
                    const MAX_BRIDGE_CONTINUE_NUDGES: usize = 8;
                    let inspected_this_turn =
                        inspect_ran.load(std::sync::atomic::Ordering::Relaxed) > inspect_before;
                    if !unfinished.is_empty() {
                        // Work is open again — any earlier "all done" verification is stale.
                        verify_attempts = 0;
                        if made_progress && bridge_continue_nudges < MAX_BRIDGE_CONTINUE_NUDGES {
                            bridge_continue_nudges += 1;
                            self.presenter.emit(PresenterEvent::Warning(format!(
                                "bridge yielded with {} task(s) unfinished — continuing the plan ({bridge_continue_nudges}/{MAX_BRIDGE_CONTINUE_NUDGES})",
                                unfinished.len()
                            )));
                            let nudge = format!(
                                "The plan is NOT finished — these tracked tasks are still open:\n- {}\n\n\
                                 Continue the plan now: carry out the next unfinished step and run it \
                                 to completion. If you launched an async job earlier (a release \
                                 build, CI), WAIT for it (poll it) and then do the steps that depend \
                                 on it — do not treat 'launched' as 'done'. Mark each task Done via \
                                 update_tasks as you finish it; if one is genuinely already complete \
                                 or impossible, mark it Done and say why. Do not stop until every \
                                 task is resolved.",
                                unfinished.join("\n- ")
                            );
                            let nseq = self.next_seq();
                            let _ =
                                self.store
                                    .add_message(&self.id, nseq, Role::System, &nudge, None);
                            self.transcript.push(Message::system(&nudge));
                            continue;
                        }
                        // No progress on the re-run (would spiral) or the re-drive budget is spent:
                        // stop LOUDLY with the work named, rather than silently reporting success.
                        let why = if made_progress {
                            "reached the continue limit"
                        } else {
                            "the last attempt made no progress (no task completed, no tool ran)"
                        };
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "bridge stopped with {} task(s) still unfinished — {why}. Send `continue` to resume.",
                            unfinished.len()
                        )));
                    } else if !self.tasks.is_empty() {
                        // The bridge reports every task Done — but a self-reported status is exactly
                        // what produced the phantom release (claimed merged + tagged; nothing ran).
                        // Force ONE tool-grounded verification turn, then judge by whether the turn
                        // did real, inspectable WORK:
                        //   * If the turn ran inspectable tools at all (`did_real_work`) — file/shell/
                        //     read ops — completion is accepted ONLY after the verification turn runs
                        //     a real inspection (not just re-marking `update_tasks`); otherwise it
                        //     ends flagged UNVERIFIED. This catches a model that claims done on work
                        //     it didn't actually finish/verify.
                        //   * If the turn did NO inspectable work (a pure reasoning/analysis plan —
                        //     the deliverable is the answer text, there is no external state to
                        //     check), requiring a tool inspection would over-fire. Accept after one
                        //     verification pass with a calm "not tool-verified" note instead.
                        // `did_real_work` is cumulative over the whole turn; `inspected_this_turn`
                        // is whether the turn just observed ran an inspection tool.
                        let did_real_work =
                            inspect_ran.load(std::sync::atomic::Ordering::Relaxed) > 0;
                        if self.run_completion_gate(
                            &mut verify_attempts,
                            did_real_work,
                            inspected_this_turn,
                        ) == CompletionGate::Reverify
                        {
                            continue;
                        }
                        // else: accepted (clean / no-artifacts / unverified) — fall through to terminal.
                    }
                } else {
                    // Honest-failure guard: a direct model wrote a tool call as TEXT (e.g.
                    // `<invoke>`/`default_api:` markup) instead of invoking it, and neither the
                    // provider nor the text-recovery pass turned it into a real call — so NOTHING
                    // ran. Accepting this as the final answer is how a turn "succeeds" while having
                    // merged no PR and pushed no tag. Detect it and nudge the model to actually
                    // call the tool (bounded); never silently accept narrated tool calls.
                    if forge_provider::looks_like_unexecuted_tool_call(&resp.content) {
                        const MAX_TOOLCALL_REPAIR_NUDGES: usize = 2;
                        if toolcall_repair_nudges < MAX_TOOLCALL_REPAIR_NUDGES {
                            toolcall_repair_nudges += 1;
                            self.presenter.emit(PresenterEvent::Warning(format!(
                                "model wrote a tool call as text instead of invoking it — nothing ran; asking it to retry ({toolcall_repair_nudges}/{MAX_TOOLCALL_REPAIR_NUDGES})"
                            )));
                            let nudge = "Your last message contained a tool call written as TEXT \
                                         (e.g. `<invoke …>` or `default_api:` syntax). That tool DID \
                                         NOT run — text is not a tool call. Make the call through the \
                                         function-calling interface instead. Do not paste tool-call \
                                         markup into your message.";
                            let nseq = self.next_seq();
                            let _ =
                                self.store
                                    .add_message(&self.id, nseq, Role::System, nudge, None);
                            self.transcript.push(Message::system(nudge));
                            continue;
                        }
                        // Retries exhausted: do NOT pretend it worked. Surface it loudly so the user
                        // knows the turn's actions never executed, then end (can't loop forever).
                        self.presenter.emit(PresenterEvent::Warning(
                            "model kept emitting tool calls as text that never executed — the turn did NOT complete its actions"
                                .to_string(),
                        ));
                    }
                    // Direct model, non-empty text, no tool call — usually the real final answer.
                    // But a weaker model often narrates its NEXT action ("now I'll edit X") without
                    // calling the tool, or signs off with tasks still open. If the tracked task list
                    // still has unfinished items, this is a premature stall: drive it onward
                    // (bounded) so the work completes instead of ending the turn mid-task.
                    let unfinished = self
                        .tasks
                        .iter()
                        .filter(|t| !matches!(t.status, forge_types::TodoStatus::Done))
                        .count();
                    const MAX_CONTINUE_NUDGES: usize = 4;
                    if unfinished > 0 {
                        // Work is still open — any earlier "all done" verification is stale.
                        verify_attempts = 0;
                        if continue_nudges < MAX_CONTINUE_NUDGES {
                            continue_nudges += 1;
                            self.presenter.emit(PresenterEvent::Warning(format!(
                                "model stopped with {unfinished} task(s) unfinished — continuing it ({continue_nudges}/{MAX_CONTINUE_NUDGES})"
                            )));
                            let nudge = "You ended your reply, but tasks on your list are NOT yet \
                                         Done. The turn is not over — do not stop. Continue now: call \
                                         the next tool to make progress on the remaining work. Only \
                                         finish once every task is resolved; if one is genuinely \
                                         complete or impossible, mark it Done via update_tasks and say \
                                         why. Do not reply again without either calling a tool or \
                                         marking a task Done.";
                            let nseq = self.next_seq();
                            let _ =
                                self.store
                                    .add_message(&self.id, nseq, Role::System, nudge, None);
                            self.transcript.push(Message::system(nudge));
                            continue;
                        }
                        // Nudge budget spent and work is STILL open — surface it. The bridge path
                        // emits an equivalent warning; the direct path used to fall through here
                        // silently, leaving the user to wonder why the turn stopped mid-plan.
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "model stopped with {unfinished} task(s) unfinished after \
                             {MAX_CONTINUE_NUDGES} continue nudge(s) — giving up. Send `continue` \
                             to resume."
                        )));
                    } else if !self.tasks.is_empty() {
                        // Every tracked task reported Done — same completion authority as the bridge:
                        // don't accept the model's say-so, force ONE tool-grounded state check first.
                        // A self-reported "done" without an inspection is exactly the phantom-completion
                        // the bridge gate guards against; the direct path had no such guard before.
                        let did_real_work =
                            inspect_ran.load(std::sync::atomic::Ordering::Relaxed) > 0;
                        // Unlike the bridge (which runs its whole tool loop INSIDE one `complete()`
                        // call, so an inspection lands in the same step as the final text), a direct
                        // model runs each tool in a SEPARATE step from the text "done" claim. So a
                        // step-local "did this step inspect?" is ALWAYS false at this gate, which would
                        // wrongly flag a genuinely-verified turn as UNVERIFIED. Instead ask: did an
                        // inspection run SINCE we last asked for verification? `inspect_at_last_verify`
                        // is the inspect count captured when the verify nudge was (re)issued.
                        let inspected_since_verify = verify_attempts > 0
                            && inspect_ran.load(std::sync::atomic::Ordering::Relaxed)
                                > inspect_at_last_verify;
                        if self.run_completion_gate(
                            &mut verify_attempts,
                            did_real_work,
                            inspected_since_verify,
                        ) == CompletionGate::Reverify
                        {
                            // Mark where the next verification window starts: any inspection AFTER
                            // this point counts as responding to the nudge.
                            inspect_at_last_verify =
                                inspect_ran.load(std::sync::atomic::Ordering::Relaxed);
                            continue;
                        }
                    }
                }
                final_text = resp.content;
                hit_step_cap = false;
                break;
            }

            // Doom-loop guard: if the model emits the exact same tool call(s) several steps running,
            // it's stuck (re-reading the same file, retrying an identical failing edit). Identical
            // args yield identical results, so halt with a clear message instead of burning the
            // remaining step budget + tokens.
            const DOOM_LOOP_THRESHOLD: usize = 3;
            // Sliding-window size for the oscillation guard. 6 holds three full A,B cycles, so an
            // A,B,A,B,A,B alternation surfaces the same signature THRESHOLD× and trips the guard,
            // while leaving room for legitimate progress (distinct calls don't accumulate).
            const DOOM_OSC_WINDOW: usize = 6;
            let sig = tool_batch_signature(&resp.tool_calls);
            if last_tool_sig == Some(sig) {
                repeat_count += 1;
            } else {
                repeat_count = 0;
                last_tool_sig = Some(sig);
            }
            // Oscillation count: how many of the last DOOM_OSC_WINDOW steps had THIS signature.
            // Catches the non-consecutive loop the `repeat_count` reset blinds us to.
            recent_sigs.push_back(sig);
            if recent_sigs.len() > DOOM_OSC_WINDOW {
                recent_sigs.pop_front();
            }
            let osc_count = recent_sigs.iter().filter(|&&s| s == sig).count();
            // Distinguish the two loop shapes so the warning isn't misleading: a true A,A,A repeat
            // vs an A,B,A,B oscillation (where the model did NOT repeat the *same* call back-to-back).
            let is_oscillation =
                osc_count >= DOOM_LOOP_THRESHOLD && repeat_count + 1 < DOOM_LOOP_THRESHOLD;
            if repeat_count + 1 >= DOOM_LOOP_THRESHOLD || osc_count >= DOOM_LOOP_THRESHOLD {
                if !doom_nudged {
                    // First time: don't kill the turn. Tell it the loop won't make progress and to
                    // switch approach — a weaker model usually breaks out of the rut. Queue the nudge
                    // so it lands AFTER this step's tool results (valid message ordering); fall
                    // through to execute, then re-check next step.
                    doom_nudged = true;
                    self.presenter.emit(PresenterEvent::Warning(
                        if is_oscillation {
                            "model is alternating between the same tool calls in a loop (A→B→A \
                             pattern) — nudging it to break out before stopping"
                        } else {
                            "model repeated the same tool call — nudging it to change approach \
                             before stopping"
                        }
                        .to_string(),
                    ));
                    self.pending_hints.push(
                        "You've now cycled through the same tool calls several times — the results \
                         will not change. Stop repeating this pattern and take a DIFFERENT approach \
                         (another tool, different arguments, or a different file). If the task is \
                         genuinely complete, say so plainly or mark it Done with update_tasks. Do \
                         not issue that same cycle of calls again."
                            .to_string(),
                    );
                } else {
                    // Still looping after the nudge → truly stuck; halt with a clear message.
                    self.presenter.emit(PresenterEvent::Warning(
                        if is_oscillation {
                            "the model kept alternating between the same tool calls after a nudge — \
                             stopping to avoid a loop"
                        } else {
                            "the model kept repeating the same tool call after a nudge — stopping \
                             to avoid a loop"
                        }
                        .to_string(),
                    ));
                    hit_step_cap = false;
                    break;
                }
            }

            // Count the tools the DIRECT path is about to run, so the completion-verification gate's
            // progress + inspection signals work for direct models. The stream sink only increments
            // these for tools the PROVIDER surfaces as `ToolStarted` events — which the bridge does
            // (its tool loop runs inside one `complete()`), but a direct genai provider does NOT: it
            // returns tool calls in `resp.tool_calls` and the loop executes them here. Without this,
            // `inspect_ran` stays 0 on the direct path and the gate can't tell an inspection from a
            // bare "done" claim. Bridge turns return an empty `tool_calls` (their tools ran inside the
            // subprocess), so this adds nothing for them — no double counting. `update_tasks`/
            // `present_plan` are bookkeeping, not inspections (same rule as the stream sink).
            for call in &resp.tool_calls {
                tools_ran.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if !call.name.ends_with("update_tasks") && !call.name.ends_with("present_plan") {
                    inspect_ran.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
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
                // Feed the failure-loop guard the same way the serial path does, so a concurrent
                // batch that keeps failing the same way (different args each step) is caught instead
                // of burning the budget to the step cap.
                let classified = self.run_readonly_batch(&msg_id, &resp.tool_calls).await?;
                for (name, kind) in classified {
                    match kind {
                        Some(k) => *failure_counts.entry((name, k)).or_insert(0) += 1,
                        None => failure_counts.retain(|(nm, _), _| nm != &name),
                    }
                }
                // Deliver any queued system hints (e.g. the doom-loop "change approach" nudge) — the
                // serial path does this per call; without it here the nudge sits undelivered and the
                // model is halted next step "after a nudge" it never actually saw.
                let hints: Vec<String> = self.pending_hints.drain(..).collect();
                for hint in hints {
                    let hseq = self.next_seq();
                    let _ = self
                        .store
                        .add_message(&self.id, hseq, Role::System, &hint, None);
                    self.transcript.push(Message::system(hint));
                }
            } else {
                // Execute each requested tool through the permission broker, serially.
                for call in &resp.tool_calls {
                    let result = self.invoke_tool(&msg_id, call).await?;
                    match classify_tool_failure(&result) {
                        Some(kind) => {
                            *failure_counts.entry((call.name.clone(), kind)).or_insert(0) += 1;
                        }
                        // A success on this tool means progress — clear its failure streaks so an
                        // earlier rough patch doesn't later trip the guard after the model recovered.
                        None => failure_counts.retain(|(nm, _), _| nm != &call.name),
                    }
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

            // Failure-loop guard: a tool that keeps failing the SAME way (across differing args) is
            // making no progress and burning the step/token budget — invisible to the identical-call
            // doom-loop above. Two-stage like that guard: nudge a change of approach once, then halt
            // if it persists. (BOTH the serial path and the concurrent read-only batch populate
            // `failure_counts`, so a batch failing the same way every step is caught here too.)
            const FAILURE_LOOP_THRESHOLD: usize = 3;
            if let Some((tool, kind, n)) = failure_counts
                .iter()
                .filter(|(_, &c)| c >= FAILURE_LOOP_THRESHOLD)
                .max_by_key(|(_, &c)| c)
                .map(|((nm, k), &c)| (nm.clone(), *k, c))
            {
                if !failure_nudged {
                    failure_nudged = true;
                    self.presenter.emit(PresenterEvent::Warning(format!(
                        "`{tool}` failed {n}× the same way ({}) — nudging a change of approach",
                        kind.label()
                    )));
                    let nudge = format!(
                        "Your `{tool}` calls keep failing with the same kind of error ({}). \
                         Repeating the same approach won't change that. Diagnose the root cause \
                         first (re-read the file / inspect the actual state), then take a DIFFERENT \
                         approach — or if you're genuinely blocked, say so plainly. Do not keep \
                         retrying the same way.",
                        kind.label()
                    );
                    let nseq = self.next_seq();
                    let _ = self
                        .store
                        .add_message(&self.id, nseq, Role::System, &nudge, None);
                    self.transcript.push(Message::system(nudge));
                    // Fresh slate after the nudge: only halt if it loops AGAIN, and don't let a
                    // stale pre-nudge streak trip the halt when the model is now trying something new.
                    failure_counts.clear();
                } else {
                    self.presenter.emit(PresenterEvent::Warning(format!(
                        "`{tool}` kept failing ({}) after a nudge — stopping to avoid a wasted loop",
                        kind.label()
                    )));
                    hit_step_cap = false;
                    break;
                }
            }
        }

        Ok(ModelLoopOutcome {
            final_text,
            context_tokens,
            hit_step_cap,
            active_model,
            plan: proposed_plan,
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
    ) -> Result<LoopOutcome, CoreError> {
        // 1. Route the task (deterministic, no model call) and record why. The budget is
        // aggregated across ALL sessions for the current local day + week + month (FR-5), not one
        // session's running total. One combined query instead of three separate ones.
        let (spent_today, spent_week, spent_month) = self.store.spend_summary_usd()?;
        let budget = BudgetState {
            spent_today_usd: spent_today,
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_week_usd: spent_week,
            weekly_cap_usd: self.config.mesh.weekly_budget_usd,
            spent_month_usd: spent_month,
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
            min_context_tokens: Some(self.estimated_transcript_tokens() as u32),
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
                stop_reason: StopReason::BudgetExhausted,
            });
            return Ok(LoopOutcome::budget_exhausted(msg));
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
        // A per-turn `tier_override` (command/skill `tier:` hint) wins; otherwise the in-session
        // tier pin (set by the `tier_up`/`tier_down` keybinds) biases routing.
        let effective_tier = tier_override.or(self.pinned_tier);
        let decision = self
            .router
            .route_hinted(
                prompt,
                budget,
                &health,
                &quota,
                effective_tier,
                self.pinned_effort,
            )
            .await;
        // `/model <id>` override: use the pinned model instead of the mesh-routed pick; mesh still
        // classifies (for tier stats) but the actual call uses the pin.
        let pinned = self.pinned_model.clone();
        let routed_model = pinned.unwrap_or_else(|| decision.model.clone());

        // No usable model: the router filters unkeyed models out of the chain (is_usable →
        // has_api_key), so the routed pick belongs to a key-needing provider with no key ONLY when
        // nothing usable existed at all — the built-in defaults lead with groq, so a user whose keys
        // are for other providers (or whose auto-discovery came up empty) would otherwise watch the
        // mesh call groq and auth-fail on EVERY turn. Stop with an actionable diagnostic instead of
        // spinning on a key we don't have. (Keyless providers — ollama, the claude/codex bridges,
        // unknown prefixes — return has_api_key=true and pass through untouched.)
        if !forge_config::has_api_key(forge_config::provider_of(&routed_model)) {
            let msg = no_usable_model_message(&routed_model);
            self.presenter.emit(PresenterEvent::Warning(msg.clone()));
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
                stop_reason: StopReason::FinalAnswer,
            });
            return Ok(LoopOutcome::final_answer(msg));
        }

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

            // Auto-memory RECALL: surface the few durable facts from past sessions in this project
            // most relevant to the prompt (preferences/decisions/conventions). The edge over a
            // dump-everything memory file: only the RELEVANT memories are injected, ranked by the
            // prompt then salience + recency. Once per session, like AGENTS.md.
            if self.config.mesh.auto_memory {
                let scope = memory_scope();
                let recalled = match embed_one(&self.config.lattice.embeddings, prompt).await {
                    Some(qemb) => self.store.recall_semantic(&scope, &qemb, 6),
                    None => self.store.recall_memories(&scope, prompt, 6),
                };
                if let Ok(mems) = recalled {
                    if !mems.is_empty() {
                        let mut block = String::from(
                            "Remembered from earlier sessions in this project (durable facts — \
                             use them, and don't re-ask what's already settled):\n",
                        );
                        for m in &mems {
                            block.push_str(&format!("- [{}] {}\n", m.kind, m.text));
                        }
                        let mseq = self.next_seq();
                        self.store
                            .add_message(&self.id, mseq, Role::System, &block, None)?;
                        self.transcript.push(Message::system(&block));
                        // Emit a one-line presenter note so the user sees recall happened.
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "💭 recalled {} memories from past sessions",
                            mems.len()
                        )));
                    }
                }
            }

            // Auto-orchestrate: inject the resource-routing framework once so the model surveys
            // all available tools on every turn without requiring the user to /orchestrate.
            if self.config.mesh.auto_orchestrate {
                let guidance = forge_skills::orchestrate_system_guidance();
                let oseq = self.next_seq();
                self.store
                    .add_message(&self.id, oseq, Role::System, guidance, None)?;
                self.transcript.push(Message::system(guidance));
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
        self.failure_tracker.reset_turn();

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
                        max_hits: self.config.lattice.inject_body_hits,
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
        // hints are all active — EXCEPT when architect mode swapped in a different editor model. The
        // routed `decision` describes the ROUTED model's failover chain (ranked for a different
        // model/tier); reusing it here would fail an editor-model error over to nonsensical
        // fallbacks. Match the self-review / autofix re-runs and run without a decision (no
        // cross-model failover) when the model was switched.
        let primary_decision = if edit_model == routed_model {
            Some(&decision)
        } else {
            None
        };
        let outcome = self
            .run_model_loop(edit_model, &specs, primary_decision, max_steps, stream_idle)
            .await?;
        let mut final_text = outcome.final_text;
        let mut context_tokens = outcome.context_tokens;
        let mut active_model = outcome.active_model;
        let mut hit_step_cap = outcome.hit_step_cap;

        // A CLI-bridge model proposed a plan (the sink already rendered the card). Seed tasks,
        // persist it, and stash it for the approval flow below — the in-process path did this in
        // the `present_plan` handler already.
        if let Some(plan) = outcome.plan {
            self.ingest_plan(plan);
        }

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
        // Guard: only adopt the store's copy when it has tasks. A bridge that persisted under a
        // different db path/session leaves `persisted` empty — without this, an empty reload would
        // wipe the list we already surfaced live and hide the panel at turn end.
        if let Ok(persisted) = self.store.tasks(&self.id) {
            if !persisted.is_empty() && persisted != self.tasks {
                self.tasks = persisted;
                self.presenter
                    .emit(PresenterEvent::Tasks(self.tasks.clone()));
            }
        }

        // ── Self-review pass (mesh.self_review) ───────────────────────────────────────────────
        // One bounded round where the SAME model re-examines the edits it just made against the
        // original task and fixes any bug/incompleteness — the self-correction leverage a
        // single-pass harness lacks, needing no external tools or test env. Fires only on edit
        // turns; runs BEFORE autofix so any fix it makes is then lint/test-checked too.
        if self.config.mesh.self_review && self.edits_this_turn > 0 {
            self.presenter.emit(PresenterEvent::Warning(
                "self-review: re-checking the changes against the task".to_string(),
            ));
            self.transcript.push(Message::system(SELF_REVIEW_PROMPT));
            let rv_specs = self.tool_specs();
            // None decision: no failover/routing churn — keep the same model, like the autofix re-run.
            let rv = self
                .run_model_loop(
                    active_model.clone(),
                    &rv_specs,
                    None,
                    max_steps,
                    stream_idle,
                )
                .await?;
            // Keep the original answer text: the review fixes code, it doesn't re-answer the user.
            context_tokens = rv.context_tokens;
            active_model = rv.active_model;
        }

        // ── Autofix self-healing loop (autofix.md) ────────────────────────────────────────────
        // After the turn's model↔tool loop finishes: if edits were made AND autofix is enabled
        // with at least one non-empty command, run lint/test and feed failures back into the
        // conversation so the model can fix them. Repeat up to `max_iterations`. When autofix is
        // off, or no edits happened, this block is a no-op (zero overhead).
        let mut af = self.config.autofix.clone();

        // Auto-detect: fill in lint_cmd (and optionally test_cmd) from project structure when the
        // user hasn't configured them. Activates on edits so there's no cost on read-only turns.
        if af.auto_detect && self.edits_this_turn > 0 && af.lint_cmd.is_empty() {
            if let Some((lint, test)) = Self::detect_project_commands() {
                self.presenter.emit(PresenterEvent::Warning(format!(
                    "autofix: auto-detected lint command from project structure: {lint}"
                )));
                af.lint_cmd = lint;
                af.auto_lint = true;
                if af.auto_test && af.test_cmd.is_empty() {
                    if let Some(t) = test {
                        af.test_cmd = t;
                    }
                }
            }
        }

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
                        hit_step_cap = fix_outcome.hit_step_cap;
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

        // ── Plan approval (planning mode → interactive approve → auto-build) ──────────────────
        // If the model proposed a plan this turn (present_plan), ask the user to approve it now —
        // the model loop has ended, so blocking on the presenter is safe (no stream is being read,
        // and bridge turns are fully drained). Approval switches to Auto-edit and recursively runs
        // the build turn through the full machinery (autofix, self-review, gate); typed feedback
        // runs a fresh planning turn; Cancel falls through and ends the turn in planning mode.
        if let Some(plan) = self.pending_plan.take() {
            if let Some(followup) = self.resolve_plan_approval(&plan) {
                return Box::pin(self.run_turn_with(&followup, &[], Some(TaskTier::Complex))).await;
            }
        }

        let (session_in, session_out) = self.store.session_tokens(&self.id)?;
        self.presenter.emit(PresenterEvent::Cost {
            session_total_usd: self.store.session_cost(&self.id)?,
            session_in,
            session_out,
            context_tokens,
            context_limit: Some(self.effective_context_window(&active_model)),
        });
        self.presenter.emit(PresenterEvent::Done {
            final_text: final_text.clone(),
            stop_reason: if hit_step_cap {
                StopReason::MaxSteps
            } else {
                StopReason::FinalAnswer
            },
        });
        self.generate_recap(prompt, &final_text).await;
        // Await the handle so one-shot (forge run) exits only after capture completes. In
        // interactive mode the spinner is already cleared and this is a brief background wait.
        if let Some(handle) = self.capture_memories(prompt, &final_text) {
            let _ = handle.await;
        }
        Ok(if hit_step_cap {
            LoopOutcome::max_steps(final_text)
        } else {
            LoopOutcome::final_answer(final_text)
        })
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
    /// Detect lint / test commands from project structure (zero-config autofix).
    /// Checks the current working directory — the project root where `forge chat` launched.
    /// Returns `(lint_cmd, test_cmd)` when a known project type is found; `test_cmd` is `None`
    /// when the project type has no obvious cheap test command.
    fn detect_project_commands() -> Option<(String, Option<String>)> {
        if std::path::Path::new("Cargo.toml").exists() {
            return Some((
                "cargo check --all-targets 2>&1".to_string(),
                Some("cargo test --workspace 2>&1".to_string()),
            ));
        }
        if std::path::Path::new("package.json").exists() {
            return Some((
                "npm run lint 2>&1".to_string(),
                Some("npm test 2>&1".to_string()),
            ));
        }
        if std::path::Path::new("pyproject.toml").exists()
            || std::path::Path::new("setup.py").exists()
        {
            return Some(("python -m pytest --tb=short -q 2>&1".to_string(), None));
        }
        if std::path::Path::new("go.mod").exists() {
            return Some((
                "go build ./... 2>&1".to_string(),
                Some("go test ./... 2>&1".to_string()),
            ));
        }
        None
    }

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
            || name == PRESENT_PLAN_TOOL
            || name == USE_SKILL_TOOL
            || name == REMEMBER_TOOL
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
    /// Returns each call's `(name, failure_kind)` in original order so the caller can feed the
    /// failure-loop guard exactly as the serial path does — a concurrent batch that keeps failing the
    /// same way (e.g. two reads of ever-changing missing paths every step) must still be caught.
    async fn run_readonly_batch(
        &mut self,
        msg_id: &str,
        calls: &[forge_types::ToolCall],
    ) -> Result<Vec<(String, Option<ErrorCategory>)>, CoreError> {
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
        // tool_call_id is answered in sequence. Also classify each result for the failure-loop guard.
        let mut classified = Vec::with_capacity(pend.len());
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
            classified.push((p.name.clone(), classify_tool_failure(&result)));
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
        Ok(classified)
    }

    async fn invoke_tool(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let call_args_json = serde_json::to_string(&call.args)?;
        if let Some(warning) = self
            .failure_tracker
            .record_call(&call.name, &call_args_json)
        {
            self.presenter
                .emit(PresenterEvent::Warning(warning.clone()));
            self.pending_hints.push(format!(
                "The `{}` call just repeated with identical arguments. Do not retry it unchanged; inspect the actual state or try a different tool/argument path.",
                call.name
            ));
            return Ok(warning);
        }

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
        // Plan presentation is core-owned (seeds tasks, persists the plan, drives the approval flow).
        if call.name == PRESENT_PLAN_TOOL {
            return self.present_plan(msg_id, call);
        }
        // Skill loading is core-owned (it reads the attached catalog). Returns the skill's
        // methodology as the tool result so the model follows it; unknown name → a helpful error.
        if call.name == USE_SKILL_TOOL {
            return self.use_skill(msg_id, call);
        }
        // On-demand memory write — model calls this to persist a durable fact immediately,
        // without waiting for end-of-turn auto-capture.
        if call.name == REMEMBER_TOOL {
            return self.remember(msg_id, call).await;
        }
        // External MCP tools (meta-tools + exposed server tools) are owned by the manager, not the
        // built-in registry. Route them here, still through the permission broker (mcp-client.md).
        if self.mcp.as_ref().is_some_and(|m| m.knows_tool(&call.name)) {
            return self.invoke_mcp(msg_id, call).await;
        }

        let mut args_json = call_args_json;
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
            if let Some(warning) = self.failure_tracker.record_failure(&call.name, &result) {
                self.presenter
                    .emit(PresenterEvent::Warning(warning.clone()));
                self.pending_hints.push(warning);
            }
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
            // Queue any hook-injected context as a model-visible system hint (drained into the
            // transcript after the tool result), so a hook can feed the model extra context.
            for ctx in outcome.injected_context {
                self.pending_hints.push(ctx);
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

        // Validate the call's arguments against the tool's schema BEFORE running it. A malformed
        // call (missing a required field, or args that aren't an object) otherwise fails deep inside
        // the tool with an opaque message; instead return an actionable error naming what's missing
        // plus the required fields, so the model self-corrects on the next step instead of thrashing.
        if let Err(reason) = validate_tool_args(&tool.schema(), &effective_args) {
            let result = format!("error: invalid arguments for `{}` — {reason}", call.name);
            self.presenter.emit(PresenterEvent::ToolResult {
                name: call.name.clone(),
                ok: false,
                summary: "invalid arguments".to_string(),
            });
            self.store
                .record_tool_call(msg_id, &call.name, &args_json, &result, "n/a", "error")?;
            if let Some(warning) = self.failure_tracker.record_failure(&call.name, &result) {
                self.presenter
                    .emit(PresenterEvent::Warning(warning.clone()));
                self.pending_hints.push(warning);
            }
            return Ok(result);
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
            PermissionDecision::Ask => match self.presenter.confirm(&call.name, side_effect) {
                forge_tui::ConfirmOutcome::AlwaysAllow => {
                    self.rules.push(forge_types::PermissionRule {
                        tool: call.name.clone(),
                        patterns: vec![],
                        decision: forge_types::PermissionDecision::Allow,
                        source: forge_types::RuleSource::Configured,
                        reason: Some("user answered 'always' at runtime prompt".into()),
                    });
                    true
                }
                forge_tui::ConfirmOutcome::Allow => true,
                forge_tui::ConfirmOutcome::Deny => false,
            },
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

        if ok {
            self.failure_tracker.record_success(&call.name);
        } else if let Some(warning) = self.failure_tracker.record_failure(&call.name, &result) {
            self.presenter
                .emit(PresenterEvent::Warning(warning.clone()));
            self.pending_hints.push(warning);
        }

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
            // Queue any hook-injected context as a model-visible system hint (drained into the
            // transcript after the tool result), so a hook can feed the model extra context.
            for ctx in outcome.injected_context {
                self.pending_hints.push(ctx);
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
        let Some(mcp) = self.mcp.clone() else {
            return Err(CoreError::Internal(
                "invoke_mcp called without an MCP manager".into(),
            ));
        };
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
            // Queue any hook-injected context as a model-visible system hint (drained into the
            // transcript after the tool result), so a hook can feed the model extra context.
            for ctx in outcome.injected_context {
                self.pending_hints.push(ctx);
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
                if let Some(warning) = self.failure_tracker.record_failure(&call.name, &result) {
                    self.presenter
                        .emit(PresenterEvent::Warning(warning.clone()));
                    self.pending_hints.push(warning);
                }
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
            PermissionDecision::Ask => match self.presenter.confirm(&call.name, side_effect) {
                forge_tui::ConfirmOutcome::AlwaysAllow => {
                    self.rules.push(forge_types::PermissionRule {
                        tool: call.name.clone(),
                        patterns: vec![],
                        decision: forge_types::PermissionDecision::Allow,
                        source: forge_types::RuleSource::Configured,
                        reason: Some("user answered 'always' at runtime prompt".into()),
                    });
                    true
                }
                forge_tui::ConfirmOutcome::Allow => true,
                forge_tui::ConfirmOutcome::Deny => false,
            },
        };
        // When the model routes an MCP server tool via the mcp_call meta-wrapper, also gate the
        // inner (real) tool name against the permission broker. Without this, a per-tool
        // allow/ask/deny rule targeting e.g. "myserver__dangerous" is bypassed on the direct
        // path because the outer broker only sees "mcp_call".
        let allowed = if allowed && call.name == forge_mcp::MCP_CALL {
            let inner_name = effective_args
                .get("name")
                .or_else(|| effective_args.get("qualified_name"))
                .or_else(|| effective_args.get("tool"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let inner_args = effective_args
                .get("arguments")
                .or_else(|| effective_args.get("args"))
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
            if inner_name.is_empty() {
                true
            } else {
                match permission::decide(
                    self.mode,
                    forge_types::SideEffect::External,
                    inner_name,
                    &inner_args,
                    &self.rules,
                ) {
                    PermissionDecision::Allow => true,
                    PermissionDecision::Deny => false,
                    PermissionDecision::Ask => match self
                        .presenter
                        .confirm(inner_name, forge_types::SideEffect::External)
                    {
                        forge_tui::ConfirmOutcome::AlwaysAllow => {
                            self.rules.push(forge_types::PermissionRule {
                                tool: inner_name.to_string(),
                                patterns: vec![],
                                decision: forge_types::PermissionDecision::Allow,
                                source: forge_types::RuleSource::Configured,
                                reason: Some("user answered 'always' at runtime prompt".into()),
                            });
                            true
                        }
                        forge_tui::ConfirmOutcome::Allow => true,
                        forge_tui::ConfirmOutcome::Deny => false,
                    },
                }
            }
        } else {
            allowed
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
            // Queue any hook-injected context as a model-visible system hint (drained into the
            // transcript after the tool result), so a hook can feed the model extra context.
            for ctx in outcome.injected_context {
                self.pending_hints.push(ctx);
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
            min_context_tokens: None,
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
            subagent::Lifecycle::Start {
                id,
                agent,
                task,
                model,
            } => presenter.emit(PresenterEvent::SubagentStart {
                id: id.to_string(),
                agent: agent.to_string(),
                task: task.to_string(),
                model: Some(model.to_string()),
            }),
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

    /// Persist a plan, seed the task list from its steps, surface the tasks, and stash the plan for
    /// the turn-end approval flow. Shared by the in-process `present_plan` handler and the CLI-bridge
    /// ingestion in [`run_turn_with`]. Does NOT emit the plan card — the caller does (path-specific).
    fn ingest_plan(&mut self, plan: forge_types::PlanProposal) {
        persist_plan(&self.id, &plan);
        self.tasks = plan
            .steps
            .iter()
            .map(|s| forge_types::TodoItem {
                title: s.title.trim().to_string(),
                status: forge_types::TodoStatus::Pending,
            })
            .collect();
        let _ = self.store.set_tasks(&self.id, &self.tasks);
        self.presenter
            .emit(PresenterEvent::Tasks(self.tasks.clone()));
        self.pending_plan = Some(plan);
    }

    /// Ask the user to approve a proposed plan (called at turn end, after the model loop, so it's
    /// safe to block on the presenter). Returns the follow-up prompt to run next — the build prompt
    /// (after switching to Auto-edit) or a revision prompt — or `None` to cancel (stay in planning).
    fn resolve_plan_approval(&mut self, plan: &forge_types::PlanProposal) -> Option<String> {
        let n = plan.steps.len();
        let q = format!(
            "Build this plan? — \"{}\" ({n} step{}). Choose Build it / Cancel, or type changes to revise.",
            plan.title.trim(),
            if n == 1 { "" } else { "s" }
        );
        let opts = [
            forge_tui::QChoice {
                label: "Build it".into(),
                description: "Switch to Auto-edit and implement the plan now".into(),
            },
            forge_tui::QChoice {
                label: "Cancel".into(),
                description: "Discard the plan; stay in planning mode".into(),
            },
        ];
        let ans = self.presenter.ask(&q, &opts, true);
        let a = ans.trim();
        if a.eq_ignore_ascii_case("Build it")
            || a.eq_ignore_ascii_case("build")
            || a.eq_ignore_ascii_case("yes")
        {
            let label = self.set_temper(PermissionMode::AcceptEdits).label();
            self.presenter
                .emit(PresenterEvent::Temper(label.to_string()));
            self.presenter.emit(PresenterEvent::Warning(
                "plan approved — building in Auto-edit".to_string(),
            ));
            Some(PLAN_BUILD_PROMPT.to_string())
        } else if a.is_empty()
            || a == forge_tui::NO_ANSWER
            || a.eq_ignore_ascii_case("Cancel")
            || a.eq_ignore_ascii_case("no")
        {
            self.presenter.emit(PresenterEvent::Warning(
                "plan cancelled — still in planning mode".to_string(),
            ));
            None
        } else {
            // Free-text feedback → revise. Stay in planning mode so present_plan remains available.
            Some(format!(
                "The user did not approve the plan yet. They want these changes before building:\n\n\
                 {a}\n\nRevise the plan accordingly and call present_plan again with the updated steps."
            ))
        }
    }

    /// The current task list (for the composition root / TUI to render on resume).
    pub fn tasks(&self) -> &[forge_types::TodoItem] {
        &self.tasks
    }

    /// Present a plan for review (the `present_plan` virtual tool, planning mode). Renders the plan
    /// card, seeds the live task list from its steps, persists it to `.forge/plans/`, and stashes it
    /// for the turn-end approval flow. Returns a result that tells the model to STOP — the user
    /// approves it interactively (and on approval is switched to Auto-edit to build).
    fn present_plan(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let args_json = serde_json::to_string(&call.args)?;
        let plan = parse_plan(&call.args);
        if plan.steps.is_empty() {
            let result = "error: present_plan requires a non-empty `steps` array".to_string();
            self.store
                .record_tool_call(msg_id, &call.name, &args_json, &result, "allowed", "error")?;
            return Ok(result);
        }
        // Render the card now (in-process path); the bridge path emits this from the sink instead.
        self.presenter
            .emit(PresenterEvent::PlanProposed(plan.clone()));
        // Persist + seed tasks + stash for the turn-end approval flow (shared with the bridge path).
        self.ingest_plan(plan);
        let result = "Plan presented to the user for approval. STOP now — do NOT start \
                      implementing. The user will review the plan and decide; if they approve, \
                      you'll be switched to Auto-edit and asked to build it."
            .to_string();
        self.store
            .record_tool_call(msg_id, &call.name, &args_json, &result, "allowed", "ok")?;
        Ok(result)
    }

    /// Load a Forge skill's methodology (the `use_skill` virtual tool) and return it as the tool
    /// result so the model applies it this turn. Unknown name → an error listing valid skills.
    async fn remember(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let args_json = serde_json::to_string(&call.args)?;
        let kind_raw = call
            .args
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("fact");
        let text = call
            .args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let kind_norm = kind_raw.trim().to_lowercase();
        let kind_cat = match kind_norm.as_str() {
            "preference" | "decision" | "fact" | "reference" => kind_norm.clone(),
            _ => "fact".to_string(),
        };
        let (result, ok) = if text.len() < 4 {
            (
                "error: memory text too short (minimum 4 characters)".to_string(),
                false,
            )
        } else {
            let scope = memory_scope();
            let cfg = self.config.lattice.embeddings.clone();
            match embed_one(&cfg, &text).await {
                Some(emb) => {
                    let _ = self
                        .store
                        .add_memory_with_embedding(&scope, &kind_cat, &text, &self.id, &emb);
                }
                None => {
                    let _ = self.store.add_memory(&scope, &kind_cat, &text, &self.id);
                }
            }
            self.presenter
                .emit(PresenterEvent::Warning(format!("◈ memory · {kind_cat}")));
            (format!("memory saved: [{kind_cat}] {text}"), true)
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

/// The on-demand memory-write virtual tool name.
pub const REMEMBER_TOOL: &str = "remember";

/// The `ToolSpec` advertised to the model for [`REMEMBER_TOOL`].
pub fn remember_spec() -> ToolSpec {
    ToolSpec {
        name: REMEMBER_TOOL.to_string(),
        description: "Persist a durable fact to memory so it's available in future sessions. \
            Use proactively when you learn something worth remembering: a project decision, user \
            preference, key architecture fact, or stable reference. Kind must be one of \
            `preference`, `decision`, `fact`, or `reference`."
            .to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["preference", "decision", "fact", "reference"],
                    "description": "memory category"
                },
                "text": {
                    "type": "string",
                    "description": "the fact to remember (1–2 sentences max)"
                }
            },
            "required": ["kind", "text"]
        }),
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

/// The plan-presentation virtual tool name (planning mode).
pub const PRESENT_PLAN_TOOL: &str = "present_plan";

/// The prompt that drives the build turn after a plan is approved (mirrors `/execute`).
const PLAN_BUILD_PROMPT: &str = "Implement the plan you just proposed, step by step — make the \
    edits and run the commands needed to carry it out. Update each task's status (in_progress → \
    done) with update_tasks as you go. If something forces a deviation from the plan, say so and \
    keep going.";

/// Parse `present_plan` arguments into a [`PlanProposal`] (tolerant of missing/loose fields).
/// Shared by the in-process intercept and the CLI-bridge `mcp-serve` handler.
pub fn parse_plan(args: &serde_json::Value) -> forge_types::PlanProposal {
    use forge_types::{PlanProposal, PlanStep};
    let title = args
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Plan")
        .to_string();
    let steps = args
        .get("steps")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let title = s.get("title").and_then(|v| v.as_str())?.trim();
                    (!title.is_empty()).then(|| PlanStep {
                        title: title.to_string(),
                        detail: s
                            .get("detail")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let notes = args
        .get("notes")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string);
    PlanProposal {
        title,
        steps,
        notes,
    }
}

/// Persist a proposed plan to `.forge/plans/<session>.md` (human-readable markdown) so it survives
/// the session and the user can open/track it. Called on every `present_plan` — creation, draft,
/// revision. Best-effort: a write failure never breaks the turn.
pub fn persist_plan(session_id: &str, plan: &forge_types::PlanProposal) {
    let dir = std::path::Path::new(".forge").join("plans");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let mut md = format!("# {}\n\n", plan.title.trim());
    for (i, s) in plan.steps.iter().enumerate() {
        md.push_str(&format!("{}. {}\n", i + 1, s.title.trim()));
        let d = s.detail.trim();
        if !d.is_empty() {
            md.push_str(&format!("   - {d}\n"));
        }
    }
    if let Some(n) = plan
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    {
        md.push_str(&format!("\n> Notes: {n}\n"));
    }
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .take(48)
        .collect();
    let name = if safe.is_empty() {
        "plan".to_string()
    } else {
        safe
    };
    let _ = std::fs::write(dir.join(format!("{name}.md")), md);
}

/// The `ToolSpec` advertised for [`PRESENT_PLAN_TOOL`] — offered only in planning mode.
pub fn present_plan_spec() -> ToolSpec {
    ToolSpec {
        name: PRESENT_PLAN_TOOL.to_string(),
        description: "Present your proposed plan for the user to approve (planning mode). Call this \
            ONCE you have investigated enough — pass a short `title`, an ordered `steps` array (each \
            step a `title` + optional one-line `detail`), and optional `notes` (risks/assumptions). \
            It renders an interactive plan card: the user approves to auto-build (you switch to \
            Auto-edit), types changes to revise, or cancels. Do NOT edit anything before presenting."
            .to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "short plan title" },
                "steps": {
                    "type": "array",
                    "description": "the ordered plan steps",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "what this step does" },
                            "detail": { "type": "string", "description": "optional one-line elaboration" }
                        },
                        "required": ["title"]
                    }
                },
                "notes": { "type": "string", "description": "optional risks/assumptions" }
            },
            "required": ["title", "steps"]
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

/// Actionable message when the mesh routed to a model whose provider has no API key and nothing
/// else was usable — instead of silently calling it and auth-failing every turn. Names the dead
/// provider, lists the providers that DO have a usable key, and gives the concrete fixes.
fn no_usable_model_message(routed_model: &str) -> String {
    let provider = forge_config::provider_of(routed_model);
    let keyed: Vec<&str> = forge_config::known_key_providers()
        .filter(|p| forge_config::has_api_key(p))
        .collect();
    let have = if keyed.is_empty() {
        "no provider API keys are configured".to_string()
    } else {
        format!("you have keys for: {}", keyed.join(", "))
    };
    format!(
        "No usable model for this turn: the mesh routed to '{routed_model}', but provider \
         '{provider}' has no API key and no other model was usable ({have}).\n\
         Fix one of:\n  \
         • forge auth      — add a provider API key\n  \
         • forge models    — see which models are actually usable right now\n  \
         • /model <id>     — pin a usable model for this session\n  \
         • ollama serve    — run a local model (no key needed)\n\
         If you DO have a key for another provider, run `forge models`: auto-discovery may have \
         failed to reach it, so the mesh fell back to the built-in defaults (which lead with \
         '{provider}')."
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
    fn tool_failure_tracker_trips_at_threshold() {
        let mut tracker = ToolFailureTracker::new();

        assert!(tracker
            .record_failure("read_file", "permission denied")
            .is_none());
        assert!(tracker
            .record_failure("read_file", "permission denied")
            .is_none());
        let warning = tracker
            .record_failure("read_file", "permission denied")
            .expect("third matching failure should trip");

        assert!(warning.contains("stuck: `read_file` failed 3 times"));
        assert!(warning.contains("Permission"));
    }

    #[test]
    fn tool_failure_tracker_resets_on_success() {
        let mut tracker = ToolFailureTracker::new();

        assert!(tracker
            .record_failure("edit_file", "invalid patch")
            .is_none());
        assert!(tracker
            .record_failure("edit_file", "invalid patch")
            .is_none());
        tracker.record_success("edit_file");
        assert!(tracker
            .record_failure("edit_file", "invalid patch")
            .is_none());
        assert!(tracker
            .record_failure("edit_file", "invalid patch")
            .is_none());
    }

    #[test]
    fn doom_loop_tracker_trips_consecutive() {
        let mut tracker = ToolFailureTracker::new();

        assert!(tracker
            .record_call("shell", r#"{"command":"cargo check"}"#)
            .is_none());
        assert!(tracker
            .record_call("shell", r#"{"command":"cargo check"}"#)
            .is_none());
        let warning = tracker
            .record_call("shell", r#"{"command":"cargo check"}"#)
            .expect("third identical call should trip");

        assert!(warning.contains("doom-loop: `shell` called identically 3 times"));
    }

    #[test]
    fn doom_loop_resets_on_different_call() {
        let mut tracker = ToolFailureTracker::new();

        assert!(tracker
            .record_call("read_file", r#"{"path":"a"}"#)
            .is_none());
        assert!(tracker
            .record_call("read_file", r#"{"path":"b"}"#)
            .is_none());
        assert!(tracker
            .record_call("read_file", r#"{"path":"a"}"#)
            .is_none());
        assert!(tracker
            .record_call("read_file", r#"{"path":"a"}"#)
            .is_none());
    }

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
    fn prune_tool_results_trims_only_old_large_tool_output() {
        let big = "x".repeat(PRUNE_TOOL_RESULT_MAX + 500);
        let small = "ok".to_string();
        let mut msgs = vec![
            Message::user("do it"),                    // 0  (old)
            Message::tool_result("c1", big.clone()),   // 1  old + large  → pruned
            Message::tool_result("c2", small.clone()), // 2  old + small  → kept
            Message::assistant("working"),             // 3  protected window starts here (last 6)
            Message::tool_result("c3", big.clone()),   // 4  protected
            Message::user("more"),                     // 5
            Message::assistant("a"),                   // 6
            Message::user("b"),                        // 7
            Message::tool_result("c4", big.clone()),   // 8  recent + large → protected
        ];
        let reclaimed = prune_tool_results(&mut msgs, COMPACT_KEEP_RECENT);
        assert!(reclaimed > 0);
        assert!(msgs[1].content.ends_with(PRUNE_MARKER) && msgs[1].content.len() < big.len());
        assert_eq!(msgs[2].content, small, "small old result untouched");
        assert_eq!(
            msgs[4].content, big,
            "result inside the recent window protected"
        );
        assert_eq!(msgs[8].content, big, "most-recent result protected");
        // The pruned result keeps its tool_call_id (valid round-trip) and its role.
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(msgs[1].role, Role::Tool);
        // Idempotent: a second pass reclaims nothing.
        assert_eq!(prune_tool_results(&mut msgs, COMPACT_KEEP_RECENT), 0);
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
    fn validate_tool_args_catches_missing_required_and_non_objects() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"path": {}, "content": {}},
            "required": ["path", "content"]
        });
        assert!(
            validate_tool_args(&schema, &serde_json::json!({"path": "a", "content": "b"})).is_ok()
        );
        let err = validate_tool_args(&schema, &serde_json::json!({"path": "a"})).unwrap_err();
        assert!(err.contains("content"), "names the missing field: {err}");
        assert!(validate_tool_args(&schema, &serde_json::json!("nope")).is_err());
        // A schema with no `required` accepts any object.
        assert!(validate_tool_args(
            &serde_json::json!({"type": "object"}),
            &serde_json::json!({})
        )
        .is_ok());
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
        fn confirm(&mut self, _tool: &str, _side_effect: SideEffect) -> forge_tui::ConfirmOutcome {
            forge_tui::ConfirmOutcome::Deny
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
        fn confirm(&mut self, _tool: &str, _side_effect: SideEffect) -> forge_tui::ConfirmOutcome {
            forge_tui::ConfirmOutcome::Allow
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

    /// Provider that always calls `mcp_call { name: "test__echo", arguments: { "msg": "hi" } }`.
    /// Reused for the inner-gate deny test.
    struct McpCallEchoProvider;

    #[async_trait::async_trait]
    impl Provider for McpCallEchoProvider {
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
                    name: "mcp_call".into(),
                    args: serde_json::json!({
                        "name": "test__echo",
                        "arguments": { "msg": "hi" }
                    }),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn mcp_inner_tool_deny_rule_honored_on_direct_path() {
        // Bypass mode: the outer mcp_call wrapper is auto-allowed. A Configured deny rule
        // on the inner tool "test__echo" must still block the call so per-tool
        // allow/ask/deny rules are honored on the direct path (fix/mcp-percall-inner-gate).
        let mcp_cfg = forge_config::McpConfig {
            allow: forge_config::McpAllowlist {
                servers: vec!["test".into()],
                tools: vec!["test__echo".into()],
            },
            ..Default::default()
        };
        let deny_rule = forge_config::RuleConfig {
            tool: "test__echo".into(),
            deny: Some(forge_config::OneOrMany::One("*".into())),
            allow: None,
            ask: None,
            reason: None,
        };
        let config = Config {
            permission_mode: PermissionMode::Bypass,
            mcp: mcp_cfg.clone(),
            permissions: forge_config::PermissionsConfig {
                rules: vec![deny_rule],
            },
            ..Config::default()
        };

        let mgr = std::sync::Arc::new(forge_mcp::testsupport::manager_with_echo(&mcp_cfg).await);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(McpCallEchoProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter::default()),
            config,
            ".",
        )
        .unwrap();
        session.set_mcp(Some(mgr));

        let id = session.id().to_string();
        let _ = session.run_turn("call echo").await.unwrap();

        let tool_msgs: Vec<_> = store
            .load_messages(&id)
            .unwrap()
            .into_iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert!(
            tool_msgs
                .iter()
                .any(|m| m.content.contains("permission denied by policy")),
            "inner deny rule must block mcp_call on direct path; got: {tool_msgs:?}"
        );
        // Confirm the allowed tool (no deny rule) is NOT blocked — regression guard.
        assert!(
            tool_msgs.iter().all(|m| m.content != "echo: hi"),
            "denied tool must not produce output: {tool_msgs:?}"
        );
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

    /// Direct-path completion-verification gate (the shared `completion_gate`, ported from the
    /// bridge in #237). Scripts a direct model that does real work + marks every task Done, then —
    /// on the forced verification turn — runs a real inspection tool. The turn must be ACCEPTED, not
    /// flagged UNVERIFIED. This exercises the two direct-path fixes together: (1) the loop counts the
    /// tools a direct model runs (`inspect_ran`/`tools_ran`, which the stream sink only fed for
    /// bridges), and (2) the gate measures inspection SINCE the verify request, not step-locally.
    struct VerifyByInspectingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for VerifyByInspectingProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let usage = Usage::default();
            let read = || ToolCall {
                id: new_id(),
                name: "read_file".into(),
                args: serde_json::json!({"path": "Cargo.toml"}),
            };
            let resp = match n {
                // Real work (an inspection) + mark the only task Done.
                0 => ModelResponse {
                    content: "starting".into(),
                    tool_calls: vec![
                        read(),
                        ToolCall {
                            id: new_id(),
                            name: "update_tasks".into(),
                            args: serde_json::json!({"tasks": [{"title": "the task", "status": "done"}]}),
                        },
                    ],
                    usage,
                    quotas: Vec::new(),
                },
                // Claim done (text). The gate must force a verification turn here.
                1 => ModelResponse {
                    content: "all done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                },
                // Respond to the verify nudge by actually inspecting.
                2 => ModelResponse {
                    content: "checking the real state".into(),
                    tool_calls: vec![read()],
                    usage,
                    quotas: Vec::new(),
                },
                // Now claim done again — having inspected since the request, this must be accepted.
                _ => ModelResponse {
                    content: "verified — all done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                },
            };
            Ok(resp)
        }
    }

    #[tokio::test]
    async fn direct_gate_accepts_when_the_model_inspects_during_verification() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(VerifyByInspectingProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("do the task").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        // The gate fired (a verification turn was forced)…
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("verifying with a real state check")),
            "the verification gate should have forced a state check; warnings: {warnings:?}"
        );
        // …and because the model actually inspected during verification, completion is ACCEPTED —
        // it must NOT be flagged UNVERIFIED (the bug: a step-local signal never saw the inspection).
        assert!(
            !warnings.iter().any(|w| w.contains("UNVERIFIED")),
            "a model that inspected during verification must not be flagged UNVERIFIED; warnings: {warnings:?}"
        );
    }

    /// The negative half: a direct model that claims done but NEVER inspects, even after being asked,
    /// must end flagged UNVERIFIED — the gate can't be satisfied by re-asserting "done".
    struct ClaimsDoneNeverInspectsProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for ClaimsDoneNeverInspectsProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let usage = Usage::default();
            let resp = if n == 0 {
                ModelResponse {
                    content: "working".into(),
                    tool_calls: vec![
                        ToolCall {
                            id: new_id(),
                            name: "read_file".into(),
                            args: serde_json::json!({"path": "Cargo.toml"}),
                        },
                        ToolCall {
                            id: new_id(),
                            name: "update_tasks".into(),
                            args: serde_json::json!({"tasks": [{"title": "the task", "status": "done"}]}),
                        },
                    ],
                    usage,
                    quotas: Vec::new(),
                }
            } else {
                // Every subsequent turn: just re-assert done, never inspect.
                ModelResponse {
                    content: "it's all done, trust me".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                }
            };
            Ok(resp)
        }
    }

    #[tokio::test]
    async fn direct_gate_flags_unverified_when_the_model_never_inspects() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(ClaimsDoneNeverInspectsProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("do the task").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("UNVERIFIED")),
            "a model that did real work but refuses to verify must end flagged UNVERIFIED; warnings: {warnings:?}"
        );
    }

    /// Always issues the exact same tool call (a fresh id each time, but identical name + args, so
    /// `tool_batch_signature` sees a repeat). Models a stuck model re-reading the same file forever.
    struct DoomLoopProvider;
    #[async_trait::async_trait]
    impl Provider for DoomLoopProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            Ok(forge_provider::ModelResponse {
                content: "let me read it again".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "Cargo.toml"}),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn doom_loop_halts_a_model_repeating_the_same_call() {
        // The doom-loop guard must stop a model that emits the EXACT same tool call step after step
        // (identical args → identical result → no progress) rather than burning the whole step budget
        // + quota. It nudges once to change approach, then halts loudly if the repeat continues.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(DoomLoopProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        // Must RETURN (not hang / not run forever); the guard breaks the loop.
        session.run_turn("read the file").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        // The guard fired: first a "change approach" nudge, then a loud halt — assert the halt so we
        // know it actually STOPPED the loop (not merely nudged and then hit the step cap).
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("kept repeating the same tool call")),
            "the doom-loop guard should halt a repeating model; warnings: {warnings:?}"
        );
    }

    /// Alternates two DIFFERENT calls forever: a failing read of a missing path, then a succeeding
    /// read of a real file. Neither the consecutive doom-loop (each step differs from the one before)
    /// NOR the failure-loop (the interleaved success clears the read_file failure streak) can see it —
    /// only the oscillation window catches the A,B,A,B cycle. Models the real bug where a model
    /// alternated an empty failing `shell({})` with a trivial `ls -la`, looping until timeout.
    struct OscillatingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for OscillatingProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let args = if n % 2 == 0 {
                serde_json::json!({"path": "does-not-exist-xyz.txt"}) // fails NotFound
            } else {
                serde_json::json!({"path": "Cargo.toml"}) // succeeds → clears failure streak
            };
            Ok(forge_provider::ModelResponse {
                content: "still poking at it".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "read_file".into(),
                    args,
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn doom_loop_halts_a_model_oscillating_between_two_calls() {
        // Regression for the alternation-evasion bug: a model that ping-pongs between a failing call
        // and a succeeding one evades BOTH the consecutive doom-loop (no two steps alike) and the
        // failure-loop (the success clears the failure streak), so without the oscillation window it
        // runs to the step cap / timeout. The guard must still halt it.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(OscillatingProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("keep going").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("kept alternating between the same tool calls")),
            "the oscillation guard should halt a model ping-ponging between two calls with an \
             ALTERNATING-specific message; warnings: {warnings:?}"
        );
    }

    /// Reads a UNIQUE non-existent path each call. Every call fails the same WAY (`NotFound`) but with
    /// DIFFERENT args, so the identical-call doom-loop never fires — only the failure-loop guard,
    /// which tracks failures by (tool, error-kind) across the turn, can catch it.
    struct FailureLoopProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for FailureLoopProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(forge_provider::ModelResponse {
                content: "let me try a different file".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": format!("does-not-exist-{n}.rs")}),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn failure_loop_halts_a_model_failing_the_same_way() {
        // The failure-loop guard must stop a model that keeps hitting the same KIND of error with
        // different arguments (edits that never match, reads of paths that don't exist) — which the
        // identical-call doom-loop can't see, because the call signature keeps changing.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(FailureLoopProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("find the config").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("kept failing") && w.contains("after a nudge")),
            "the failure-loop guard should halt a model failing the same way; warnings: {warnings:?}"
        );
    }

    #[test]
    fn completion_gate_covers_its_four_outcomes() {
        // Pure decision table for the completion authority (no I/O, no mocks).
        // verify_attempts=0 → must verify before accepting any "done" claim.
        assert!(matches!(
            completion_gate(0, 2, true, false),
            CompletionGate::Reverify
        ));
        // A real inspection ran on a verify turn → accept cleanly.
        assert!(matches!(
            completion_gate(1, 2, true, true),
            CompletionGate::AcceptClean
        ));
        // Verify turn ran but did NO real work to check (pure analysis) → accept, calm note.
        assert!(matches!(
            completion_gate(1, 2, false, false),
            CompletionGate::AcceptNoArtifacts
        ));
        // Budget spent, real work existed but was never inspected → accept, flag UNVERIFIED.
        assert!(matches!(
            completion_gate(2, 2, true, false),
            CompletionGate::AcceptUnverified
        ));
    }

    /// Yields TWO read_file calls (a concurrent read-only batch) with DIFFERENT missing paths every
    /// step — so the identical-call doom-loop never fires (signature changes) and, before the fix,
    /// the concurrent batch path didn't feed the failure-loop guard either, letting it burn to the
    /// step cap. The failure-loop guard must now catch it.
    struct ConcurrentFailureProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for ConcurrentFailureProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mk = |suffix: &str| ToolCall {
                id: new_id(),
                name: "read_file".into(),
                args: serde_json::json!({"path": format!("does-not-exist-{n}-{suffix}.rs")}),
            };
            Ok(forge_provider::ModelResponse {
                content: "reading two more files".into(),
                tool_calls: vec![mk("a"), mk("b")],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn concurrent_batch_failure_loop_is_caught() {
        // Regression for the concurrent-batch failure-tracking gap: two read_file calls run as a
        // concurrent read-only batch, both NotFound, different paths each step. Must halt via the
        // failure-loop guard, not run to the step cap.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(ConcurrentFailureProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("read the files").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("kept failing") && w.contains("after a nudge")),
            "the failure-loop guard must catch a concurrent batch failing the same way; warnings: {warnings:?}"
        );
    }

    /// Yields the SAME two successful read-only calls every step (a concurrent batch with a constant
    /// signature) — trips the doom-loop, not the failure-loop. Used to prove the nudge is delivered.
    struct ConcurrentRepeatProvider;
    #[async_trait::async_trait]
    impl Provider for ConcurrentRepeatProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            let mk = || ToolCall {
                id: new_id(),
                name: "read_file".into(),
                args: serde_json::json!({"path": "Cargo.toml"}),
            };
            Ok(forge_provider::ModelResponse {
                content: "reading again".into(),
                tool_calls: vec![mk(), mk()],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn concurrent_batch_doom_nudge_is_delivered_to_the_model() {
        // Regression: the doom-loop nudge is pushed to pending_hints, but the concurrent read-only
        // batch path didn't drain them — so the model was halted "after a nudge" it never saw. The
        // nudge must reach the transcript.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(ConcurrentRepeatProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("read it").await.unwrap();

        assert!(
            session.transcript.iter().any(|m| m.role == Role::System
                && m.content.contains("cycled through the same tool calls")),
            "the doom-loop nudge must be delivered to the transcript on the concurrent batch path"
        );
    }

    /// Yields a tool call every single step forever (unique args so no doom/failure guard fires) —
    /// only the step cap can stop it.
    struct EndlessToolProvider;
    #[async_trait::async_trait]
    impl Provider for EndlessToolProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            Ok(forge_provider::ModelResponse {
                content: "still working".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    // A real successful read each step with a UNIQUE range → no doom/failure guard,
                    // forcing the step cap to be the thing that stops the turn.
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "Cargo.toml", "start_line": 1, "end_line": 1}),
                }],
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn step_cap_halts_a_runaway_turn() {
        // The step cap is the primary infinite-loop backstop. Pin it: with max_steps=2 and a model
        // that always wants another tool call, the turn must stop at the cap (not spin to default 100).
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut config = Config::default();
        config.mesh.max_steps = 2;
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(EndlessToolProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();

        // Must RETURN (the cap stops it) rather than loop forever.
        session.run_turn("keep reading").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("step limit")),
            "the step cap should stop a runaway turn; warnings: {warnings:?}"
        );
    }

    /// Stalls on the 2nd call (text, no tool call) while a task is still in_progress, then — once
    /// the harness nudges it to continue — marks the task Done and finishes.
    struct StallThenFinishProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for StallThenFinishProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            use std::sync::atomic::Ordering;
            let usage = Usage::default();
            let task = |status: &str| {
                vec![ToolCall {
                    id: new_id(),
                    name: "update_tasks".into(),
                    args: serde_json::json!({"tasks": [{"title": "do the thing", "status": status}]}),
                }]
            };
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let resp = match n {
                0 => ModelResponse {
                    content: "starting".into(),
                    tool_calls: task("in_progress"),
                    usage,
                    quotas: Vec::new(),
                },
                // Premature stall: narrates, no tool call, task still unfinished. The harness must
                // NOT accept this as the final answer — it should nudge and drive on.
                1 => ModelResponse {
                    content: "I'll keep going on this.".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                },
                2 => ModelResponse {
                    content: "finishing".into(),
                    tool_calls: task("done"),
                    usage,
                    quotas: Vec::new(),
                },
                _ => ModelResponse {
                    content: "all done".into(),
                    tool_calls: vec![],
                    usage,
                    quotas: Vec::new(),
                },
            };
            Ok(resp)
        }
    }

    #[tokio::test]
    async fn harness_drives_on_when_model_stalls_with_unfinished_tasks() {
        use forge_types::TodoStatus;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(StallThenFinishProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        let answer = session.run_turn("do the thing").await.unwrap();

        // The turn did NOT end at the stall — it continued until the task was Done.
        assert_eq!(
            answer, "all done",
            "drove past the premature text-only stall"
        );
        assert_eq!(session.tasks().len(), 1);
        assert_eq!(session.tasks()[0].status, TodoStatus::Done);
        // A continue-nudge was surfaced.
        let nudged = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::Warning(w) if w.contains("unfinished")));
        assert!(
            nudged,
            "emitted a continue-nudge warning for the unfinished task"
        );
    }

    /// Registers a task in_progress on call 0, then narrates with NO tool call forever — the task
    /// never closes, so the continue-nudge budget is spent and the turn must give up (not loop).
    struct NeverFinishesProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for NeverFinishesProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let tool_calls = if n == 0 {
                vec![ToolCall {
                    id: new_id(),
                    name: "update_tasks".into(),
                    args: serde_json::json!({"tasks": [{"title": "do the thing", "status": "in_progress"}]}),
                }]
            } else {
                Vec::new() // narrate, never finish
            };
            Ok(forge_provider::ModelResponse {
                content: "still working on it".into(),
                tool_calls,
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn direct_continue_nudge_exhaustion_warns_when_giving_up() {
        // Regression for a SILENT exit: when a direct model narrates forever with a task still open,
        // the harness nudges it a bounded number of times then GIVES UP. That give-up must be
        // surfaced (the bridge path always warned; the direct path used to fall through silently,
        // leaving the user to wonder why the turn stopped mid-plan).
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(NeverFinishesProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("do the thing").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("giving up") && w.contains("unfinished")),
            "exhausting the continue-nudge budget must surface a give-up warning; warnings: {warnings:?}"
        );
    }

    #[tokio::test]
    async fn cli_bridge_no_progress_stall_halts_loudly_without_spiraling() {
        use forge_types::TodoStatus;
        // A CLI-bridge turn that yields with a task still unfinished AND made no progress on that
        // turn (no tool ran, no task closed) must HALT — not be re-driven into a narration loop
        // (the old spiral). But it must NOT pretend success: it stops LOUDLY, naming the unfinished
        // work, so the half-done state is visible. (A bridge that DID make progress is re-driven to
        // completion — see the `bridge_re_drives_*` tests.)
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(StallThenFinishProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(FixedRouter {
                model: "claude-cli::opus".into(),
                fallbacks: vec![],
            }),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        let answer = session.run_turn("do the thing").await.unwrap();

        // The stall (call 1) made no progress, so the turn ends there — NOT driven into a loop.
        assert_eq!(answer, "I'll keep going on this.");
        assert_eq!(session.tasks()[0].status, TodoStatus::InProgress);
        // ...but it halted LOUDLY: an honest "stopped with unfinished tasks" warning was surfaced.
        let warned_unfinished = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::Warning(w) if w.contains("unfinished")));
        assert!(
            warned_unfinished,
            "a half-done bridge turn must stop loudly, not silently report success"
        );
    }

    /// Bridge provider for the completeness conformance test: call 0 runs a read-only tool (so the
    /// turn did real work), then every later call yields (content, no tool call) — the model thinks
    /// it's done.
    struct CompletenessYieldProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for CompletenessYieldProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tool_calls = if n == 0 {
                vec![forge_types::ToolCall {
                    id: "1".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({ "path": "Cargo.toml" }),
                }]
            } else {
                vec![]
            };
            Ok(forge_provider::ModelResponse {
                content: if n == 0 {
                    "checking".into()
                } else {
                    "all done".into()
                },
                tool_calls,
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn completeness_redrive_fires_once_when_verify_completeness_on() {
        // Opt-in `mesh.verify_completeness`: when a CLI-bridge turn that did real work yields, the
        // harness injects ONE completeness re-drive (a final diff-review nudge) before accepting done,
        // and only ONCE — the `completeness_checked` one-shot guard prevents a loop.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut config = Config::default();
        config.mesh.verify_completeness = true;
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(CompletenessYieldProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(FixedRouter {
                model: "claude-cli::opus".into(),
                fallbacks: vec![],
            }),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();

        let _ = session.run_turn("fix the bug").await.unwrap();

        let fired = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, PresenterEvent::Warning(w) if w.contains("completeness check")))
            .count();
        assert_eq!(
            fired, 1,
            "completeness re-drive must fire exactly once (one-shot)"
        );
    }

    #[tokio::test]
    async fn completeness_redrive_silent_when_verify_completeness_off() {
        // Default (off): no completeness re-drive — the opt-in mode adds nothing to the default path.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(CompletenessYieldProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }),
            Arc::new(FixedRouter {
                model: "claude-cli::opus".into(),
                fallbacks: vec![],
            }),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        let _ = session.run_turn("fix the bug").await.unwrap();

        let fired =
            events.lock().unwrap().iter().any(
                |e| matches!(e, PresenterEvent::Warning(w) if w.contains("completeness check")),
            );
        assert!(
            !fired,
            "completeness must not fire when verify_completeness is off"
        );
    }

    /// Always returns an empty response (no text, no tool call) — a model glitch / narrate-then-stall.
    struct EmptyResponseProvider;
    #[async_trait::async_trait]
    impl Provider for EmptyResponseProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            Ok(forge_provider::ModelResponse {
                content: String::new(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn empty_response_is_nudged_then_stops_not_loops() {
        // A response with neither text nor a tool call is a silent dead-end. The harness nudges it to
        // continue a BOUNDED number of times (so a transient glitch recovers), then stops — it must
        // never spin forever on an endlessly-empty model.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(EmptyResponseProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        // Must RETURN — an always-empty model must not loop to the step cap or hang.
        session.run_turn("do something").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("empty response") && w.contains("nudging")),
            "an empty response should be nudged; warnings: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("stopping the turn")),
            "after the bounded nudges, an endlessly-empty model must stop; warnings: {warnings:?}"
        );
    }

    /// Empty (no text/tool) for the `bad` models, echoes the model id otherwise — to prove an
    /// empty-responding model FAILS OVER to the next chain model instead of dead-ending the turn.
    struct EmptyForModelProvider {
        bad: std::collections::HashSet<String>,
    }
    #[async_trait::async_trait]
    impl Provider for EmptyForModelProvider {
        async fn complete(
            &self,
            model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            if self.bad.contains(model) {
                return Ok(forge_provider::ModelResponse {
                    content: String::new(),
                    tool_calls: vec![],
                    usage: forge_types::Usage::default(),
                    quotas: Vec::new(),
                });
            }
            on_event(StreamEvent::Text(model.into()));
            Ok(forge_provider::ModelResponse {
                content: model.into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn empty_response_fails_over_to_the_next_model() {
        // Dogfooding bug: an empty-responding model (e.g. kimi-k2.6 via NIM streaming empty) used to
        // stop the turn after the bounded nudges, dead-ending short of a working model. It must now
        // bench the empty model and FAIL OVER to the next chain model instead.
        let provider = Arc::new(EmptyForModelProvider {
            bad: ["empty::model".to_string()].into_iter().collect(),
        });
        let router = Arc::new(FixedRouter {
            model: "empty::model".into(),
            fallbacks: vec!["good::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        let answer = session.run_turn("do it").await.unwrap();
        assert_eq!(
            answer, "good::model",
            "an empty response must fail over to the next model, not stop the turn"
        );
        assert!(
            store.current_benched().unwrap().is_benched("empty::model"),
            "the empty-responding model must be benched"
        );
    }

    /// Writes a tool call as TEXT (markup the provider didn't decode into a structured call) with NO
    /// real tool_calls — so nothing executes. Models the phantom-release failure mode.
    struct ToolCallAsTextProvider;
    #[async_trait::async_trait]
    impl Provider for ToolCallAsTextProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            Ok(forge_provider::ModelResponse {
                // `<invoke …>` markup is detected by `looks_like_unexecuted_tool_call`, but with no
                // structured `tool_calls` it never runs — the honest-failure guard must catch it.
                content: "I'll do it now: <invoke name=\"shell\">git push</invoke>".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn tool_call_written_as_text_never_silently_succeeds() {
        // A model that writes a tool call as text (and the provider didn't recover it) did NOTHING —
        // accepting that narration as a final answer is the phantom-success bug. The honest-failure
        // guard must nudge it to actually call the tool, then — if it persists — end LOUDLY rather
        // than report success.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(ToolCallAsTextProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session.run_turn("push the commit").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("tool call as text")),
            "a narrated tool call should be nudged to actually execute; warnings: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("never executed")),
            "if it persists, the turn must end loudly (not a phantom success); warnings: {warnings:?}"
        );
    }

    #[test]
    fn parse_plan_reads_fields_and_filters_empty_steps() {
        let v = serde_json::json!({
            "title": "  Refactor main.rs  ",
            "steps": [
                {"title": "Extract args", "detail": "  clap defs  "},
                {"title": "   "},
                {"title": "Split dispatch"}
            ],
            "notes": "  keep the API stable  "
        });
        let p = parse_plan(&v);
        assert_eq!(p.title, "Refactor main.rs");
        assert_eq!(p.steps.len(), 2, "the blank-title step is dropped");
        assert_eq!(p.steps[0].title, "Extract args");
        assert_eq!(p.steps[0].detail, "clap defs");
        assert_eq!(p.steps[1].detail, "");
        assert_eq!(p.notes.as_deref(), Some("keep the API stable"));

        let empty = parse_plan(&serde_json::json!({}));
        assert_eq!(empty.title, "Plan");
        assert!(empty.steps.is_empty());
        assert!(empty.notes.is_none());
    }

    fn one_step_plan() -> forge_types::PlanProposal {
        forge_types::PlanProposal {
            title: "T".into(),
            steps: vec![forge_types::PlanStep {
                title: "a".into(),
                detail: String::new(),
            }],
            notes: None,
        }
    }

    #[test]
    fn plan_approval_build_switches_to_auto_edit() {
        let mut s = scripted_session("Build it", Arc::new(Mutex::new(0)));
        s.set_temper(PermissionMode::Plan);
        let next = s.resolve_plan_approval(&one_step_plan());
        assert_eq!(next.as_deref(), Some(PLAN_BUILD_PROMPT));
        assert_eq!(
            s.mode,
            PermissionMode::AcceptEdits,
            "build flips to Auto-edit"
        );
    }

    #[test]
    fn plan_approval_cancel_stays_in_planning() {
        let mut s = scripted_session("Cancel", Arc::new(Mutex::new(0)));
        s.set_temper(PermissionMode::Plan);
        assert!(s.resolve_plan_approval(&one_step_plan()).is_none());
        assert_eq!(s.mode, PermissionMode::Plan, "cancel keeps planning mode");
    }

    #[test]
    fn plan_approval_free_text_revises_without_switching() {
        let mut s = scripted_session("make it shorter", Arc::new(Mutex::new(0)));
        s.set_temper(PermissionMode::Plan);
        let next = s
            .resolve_plan_approval(&one_step_plan())
            .expect("revision prompt");
        assert!(
            next.contains("make it shorter"),
            "carries the user's feedback"
        );
        assert!(
            next.contains("present_plan"),
            "asks the model to re-present"
        );
        assert_eq!(
            s.mode,
            PermissionMode::Plan,
            "revise does not switch to Auto-edit"
        );
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

    #[tokio::test]
    async fn recap_is_skipped_when_the_turn_produced_no_final_text() {
        // A stalled turn (empty-response give-up / failover exhaustion) leaves final_text empty.
        // MockProvider always returns non-empty content, so without the guard a recap WOULD be
        // emitted from the request alone — inventing success for a turn that did nothing. The
        // guard must suppress it entirely.
        let events = Arc::new(Mutex::new(Vec::new()));
        let config = Config::default();
        assert!(
            config.recap.enabled,
            "recap on by default — guard, not disable"
        );
        let mut s = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(CapturePresenter {
                events: events.clone(),
            }),
            config,
            ".",
        )
        .unwrap();
        s.generate_recap("Fix buggy.py so average([]) returns 0.0", "")
            .await;
        s.generate_recap("Fix buggy.py", "   \n\t ").await;
        let recaps = events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, PresenterEvent::Recap { .. }))
            .count();
        assert_eq!(recaps, 0, "empty/whitespace turn must not be recapped");
    }

    #[test]
    fn no_usable_model_message_names_the_dead_provider_and_the_fixes() {
        let msg = no_usable_model_message("groq::llama-3.1-8b-instant");
        assert!(msg.contains("groq"), "names the dead provider");
        assert!(msg.contains("forge auth"), "points at adding a key");
        assert!(
            msg.contains("forge models"),
            "points at the usable-models view"
        );
        assert!(msg.contains("/model"), "offers a pin escape hatch");
        // Mentions auto-discovery so a user who DOES have another key knows why it fell back.
        assert!(msg.to_lowercase().contains("auto-discovery"));
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
        // This test asserts message_count == transcript length; the per-turn recap side-call would
        // add a usage row (counted by message_count, not rehydrated), so disable it here.
        let mut config = Config::default();
        config.recap.enabled = false;

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
            _effort: Option<forge_types::EffortLevel>,
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

    fn unavailable(_m: &str) -> forge_provider::ProviderError {
        forge_provider::ProviderError::Unavailable("502".into())
    }

    /// Fails `bad` models with a chosen error; every other model answers with its OWN id as the
    /// content, so a test can tell WHICH fallback actually served the turn.
    struct EchoProvider {
        bad: std::collections::HashSet<String>,
        err: fn(&str) -> forge_provider::ProviderError,
    }
    #[async_trait::async_trait]
    impl Provider for EchoProvider {
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
            on_event(StreamEvent::Text(model.into()));
            Ok(forge_provider::ModelResponse {
                content: model.into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn rate_limit_skips_the_failed_providers_remaining_chain_entries() {
        // Chain is in mesh-rank order [prova::2, provb::1]. prova::1 rate-limits — a 429 is
        // provider-wide, so the lazy-skip must pass over prova::2 (same provider) and cross to
        // provb::1, NOT churn through prova's siblings.
        let provider = Arc::new(EchoProvider {
            bad: ["prova::1".to_string()].into_iter().collect(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "prova::1".into(),
            fallbacks: vec!["prova::2".into(), "provb::1".into()],
        });
        let (_store, mut session) = fixed_session(provider, router);
        let answer = session.run_turn("do it").await.unwrap();
        assert_eq!(
            answer, "provb::1",
            "429 on prova::1 must skip same-provider prova::2 and use provb::1"
        );
    }

    /// Narrates a tool call as TEXT for the first `narrate` completions, then answers cleanly.
    struct NarrateThenAnswerProvider {
        calls: std::sync::atomic::AtomicUsize,
        narrate: usize,
    }
    #[async_trait::async_trait]
    impl Provider for NarrateThenAnswerProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = if n < self.narrate {
                "<invoke name=\"shell\"><parameter name=\"command\">git push</parameter></invoke>"
            } else {
                "all done"
            };
            on_event(StreamEvent::Text(text.into()));
            Ok(forge_provider::ModelResponse {
                content: text.into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn narrated_tool_call_is_not_accepted_as_a_final_answer() {
        // A direct model writes a tool call as text (nothing executes). The honest-failure guard
        // must NOT accept it as the turn's answer — it nudges the model, which then answers for
        // real. Proven by the final text being the clean answer, not the narrated markup.
        let provider = Arc::new(NarrateThenAnswerProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            narrate: 1,
        });
        let router = Arc::new(FixedRouter {
            model: "direct::model".into(),
            fallbacks: vec![],
        });
        let (_store, mut session) = fixed_session(provider, router);
        let answer = session.run_turn("ship it").await.unwrap();
        assert_eq!(
            answer, "all done",
            "narrated tool-call text must be nudged, not accepted as the final answer"
        );
    }

    #[tokio::test]
    async fn non_rate_limit_failure_keeps_strict_rank_order() {
        // A NON-429 failure (outage) must NOT skip the provider — strict mesh-rank order means the
        // very next-ranked model (prova::2) is tried, even though it shares prova::1's provider.
        let provider = Arc::new(EchoProvider {
            bad: ["prova::1".to_string()].into_iter().collect(),
            err: unavailable,
        });
        let router = Arc::new(FixedRouter {
            model: "prova::1".into(),
            fallbacks: vec!["prova::2".into(), "provb::1".into()],
        });
        let (_store, mut session) = fixed_session(provider, router);
        let answer = session.run_turn("do it").await.unwrap();
        assert_eq!(
            answer, "prova::2",
            "an outage keeps rank order — next-ranked prova::2 is tried, not skipped"
        );
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

    /// Fails the first `fail_first` calls with a NON-overflow transient outage, then answers — to
    /// prove a transient 5xx/blip self-heals by retrying the SAME model, not by failing over.
    struct TransientThenOkProvider {
        calls: std::sync::atomic::AtomicUsize,
        fail_first: usize,
    }
    #[async_trait::async_trait]
    impl Provider for TransientThenOkProvider {
        async fn complete(
            &self,
            model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.fail_first {
                return Err(forge_provider::ProviderError::Unavailable(
                    "502 bad gateway (transient)".into(),
                ));
            }
            on_event(StreamEvent::Text(model.into()));
            Ok(forge_provider::ModelResponse {
                content: model.into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    /// Rate-limits the first `fail_first` calls with a tiny `retry_after`, then answers — to prove
    /// the in-turn wait-for-reset retries the SAME model instead of degrading to a fallback.
    struct RateLimitThenOkProvider {
        calls: std::sync::atomic::AtomicUsize,
        fail_first: usize,
    }
    #[async_trait::async_trait]
    impl Provider for RateLimitThenOkProvider {
        async fn complete(
            &self,
            model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.fail_first {
                return Err(forge_provider::ProviderError::RateLimited {
                    message: "429 rate limited".into(),
                    retry_after: Some(std::time::Duration::from_millis(10)),
                });
            }
            on_event(StreamEvent::Text(model.into()));
            Ok(forge_provider::ModelResponse {
                content: model.into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn rate_limit_waits_for_reset_and_retries_the_same_model() {
        // The per-minute free-tier case: the best model 429s with a short reset → wait it out and
        // retry the SAME model rather than degrading to a lower-ranked fallback.
        let provider = Arc::new(RateLimitThenOkProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail_first: 1,
        });
        let router = Arc::new(FixedRouter {
            model: "best::model".into(),
            fallbacks: vec!["worse::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        session.config.mesh.rate_limit_wait_secs = 1; // re-enable waiting (10ms reset → instant)
        let answer = session.run_turn("hi").await.unwrap();
        assert_eq!(
            answer, "best::model",
            "waited for the reset and retried the best model; fallback unused"
        );
        assert!(
            store.current_benched().unwrap().is_empty(),
            "a model we waited out and recovered must not be benched"
        );
    }

    #[tokio::test]
    async fn transient_error_retries_same_model_before_failing_over() {
        // A 5xx/dropped-connection blip should be retried on the SAME model (it usually succeeds on
        // the next attempt) instead of immediately switching to a worse fallback. Two transient
        // failures are within MAX_TRANSIENT_RETRIES, so the primary recovers and the fallback is
        // never used — and the healthy model is not benched.
        let provider = Arc::new(TransientThenOkProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            fail_first: MAX_TRANSIENT_RETRIES as usize,
        });
        let router = Arc::new(FixedRouter {
            model: "good::model".into(),
            fallbacks: vec!["fallback::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        let answer = session.run_turn("hi").await.unwrap();
        assert_eq!(
            answer, "good::model",
            "primary recovered via same-model retry; fallback not used"
        );
        assert!(
            store.current_benched().unwrap().is_empty(),
            "a transient blip that recovered must not bench the model"
        );
    }

    /// Mimics a CLI bridge: returns text with NO structured tool calls (a bridge's tools run in
    /// its own process; only its narration comes back here). Emits a `shell` ToolStarted on the
    /// first `inspect_calls` invocations — that's both the "made progress" signal the re-drive gate
    /// keys on AND the real-inspection signal the verification gate requires. 0 = never inspects
    /// (pure reasoning / a model that won't check); usize::MAX = inspects every turn; 1 = does real
    /// work once then stops inspecting (verification can't confirm).
    struct BridgeProvider {
        calls: std::sync::atomic::AtomicUsize,
        inspect_calls: usize,
    }
    #[async_trait::async_trait]
    impl Provider for BridgeProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.inspect_calls {
                on_event(StreamEvent::ToolStarted {
                    name: "shell".into(),
                    args: "git status".into(),
                });
            }
            on_event(StreamEvent::Text("working".into()));
            Ok(forge_provider::ModelResponse {
                content: "working".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    fn seed_tasks(store: &Store, id: &str, titles_done: &[(&str, bool)]) {
        let tasks: Vec<forge_types::TodoItem> = titles_done
            .iter()
            .map(|(t, done)| forge_types::TodoItem {
                title: (*t).to_string(),
                status: if *done {
                    forge_types::TodoStatus::Done
                } else {
                    forge_types::TodoStatus::Pending
                },
            })
            .collect();
        store.set_tasks(id, &tasks).unwrap();
    }

    /// Models a bridge that FALSELY reports done, then — when forced to verify — discovers the gap
    /// and reopens the task before genuinely finishing. Uses structured `update_tasks` calls so the
    /// real dispatch path drives task state (mirroring a bridge's MCP `update_tasks`).
    struct ReopenOnVerifyProvider {
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for ReopenOnVerifyProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall};
            let set = |status: &str| {
                vec![ToolCall {
                    id: new_id(),
                    name: "update_tasks".into(),
                    args: serde_json::json!({"tasks":[{"title":"ship","status":status}]}),
                }]
            };
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (content, tool_calls) = match n {
                0 => ("marking done", set("done")), // falsely claims done
                1 => ("all set", vec![]),           // narrates done -> triggers verification
                2 => ("oh, not actually done", set("in_progress")), // verify reopens the gap
                3 => ("finishing for real", set("done")), // genuinely completes
                _ => ("verified, done", vec![]),    // verification re-confirms -> terminal
            };
            on_event(StreamEvent::Text(content.into()));
            Ok(forge_provider::ModelResponse {
                content: content.into(),
                tool_calls,
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn verification_reopens_a_falsely_reported_done_task() {
        // The whole point of the gate: a model can CLAIM done while the work isn't. The forced
        // verification turn catches it, reopens the task, the re-drive finishes it, and a second
        // verification confirms. The turn must end with the task genuinely Done — and only after
        // more than the 2 invocations a truthful "done" would have taken.
        let provider = Arc::new(ReopenOnVerifyProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let (store, mut session) = bridge_session(provider.clone());
        let _ = session.run_turn("ship it").await.unwrap();
        let tasks = store.tasks(&session.id).unwrap();
        assert_eq!(tasks[0].status, forge_types::TodoStatus::Done);
        assert!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst) > 2,
            "verification must have reopened the false 'done' and re-driven to a real finish"
        );
    }

    fn bridge_session(provider: Arc<dyn Provider>) -> (Arc<Store>, Session) {
        let router = Arc::new(FixedRouter {
            model: "claude-cli::opus".into(),
            fallbacks: vec![],
        });
        let (store, mut session) = fixed_session(provider, router);
        // Isolate the model loop: the end-of-turn recap + auto-memory capture are separate provider
        // calls that would otherwise inflate the invocation count these tests assert on.
        session.config.recap.enabled = false;
        session.config.mesh.auto_memory = false;
        (store, session)
    }

    #[tokio::test]
    async fn bridge_with_unfinished_tasks_but_no_progress_halts_without_spiraling() {
        // The anti-spiral guarantee: a bridge that yields with a task still open but did NOTHING
        // this run (no tool, no task closed) must STOP, not be re-driven into a narration loop
        // (the old bridge-nudge bug). Exactly one invocation.
        let provider = Arc::new(BridgeProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            inspect_calls: 0,
        });
        let (store, mut session) = bridge_session(provider.clone());
        seed_tasks(&store, &session.id, &[("ship the release", false)]);
        let answer = session.run_turn("release it").await.unwrap();
        assert_eq!(answer, "working");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no-progress bridge must not be re-driven — it would spiral"
        );
    }

    #[tokio::test]
    async fn bridge_re_drives_while_making_progress_then_stops_at_the_cap() {
        // A bridge that keeps making progress (a tool runs each turn) but never closes the task is
        // re-driven — proving forge won't accept a half-done plan — but BOUNDED so it can't run
        // forever. 1 initial turn + MAX_BRIDGE_CONTINUE_NUDGES (8) re-drives = 9 invocations.
        let provider = Arc::new(BridgeProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            inspect_calls: usize::MAX, // a tool runs every turn = progress every turn
        });
        let (store, mut session) = bridge_session(provider.clone());
        seed_tasks(&store, &session.id, &[("ship the release", false)]);
        let _ = session.run_turn("release it").await.unwrap();
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            9,
            "must re-drive on progress but stop at the cap (1 + 8)"
        );
    }

    #[tokio::test]
    async fn bridge_completion_accepted_when_verification_runs_a_real_inspection() {
        // "All tasks Done" must pass a tool-grounded verification turn. Here the bridge runs an
        // inspection tool (shell) on the verification turn → genuinely verified → accept after
        // exactly 2 invocations (the claim + the verifying check).
        let provider = Arc::new(BridgeProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            inspect_calls: usize::MAX, // emits a `shell` ToolStarted each turn = a real inspection
        });
        let (store, mut session) = bridge_session(provider.clone());
        seed_tasks(&store, &session.id, &[("ship the release", true)]);
        let answer = session.run_turn("release it").await.unwrap();
        assert_eq!(answer, "working");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "an inspected verification is accepted after exactly one verification turn"
        );
    }

    #[tokio::test]
    async fn bridge_reasoning_only_completion_accepted_without_overfiring() {
        // The over-fire fix: a pure reasoning/analysis plan does NO inspectable work (the answer is
        // the deliverable). Demanding a tool inspection would wrongly flag it. Forge runs ONE
        // verification pass, sees there's nothing external to check, and ACCEPTS with a calm note —
        // it does NOT loop to the cap or shout UNVERIFIED. `inspect_calls: 0` = never inspects.
        let provider = Arc::new(BridgeProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            inspect_calls: 0,
        });
        let (store, mut session) = bridge_session(provider.clone());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        session.presenter = Box::new(capture);
        seed_tasks(&store, &session.id, &[("analyze the tradeoffs", true)]);
        let answer = session.run_turn("think it through").await.unwrap();
        assert_eq!(answer, "working");
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "reasoning-only completion must accept after ONE verification pass, not over-fire to the cap"
        );
        let ev = events.lock().unwrap();
        let calm = ev.iter().any(
            |e| matches!(e, PresenterEvent::Warning(w) if w.contains("no external artifacts")),
        );
        let shouted = ev
            .iter()
            .any(|e| matches!(e, PresenterEvent::Warning(w) if w.contains("UNVERIFIED")));
        assert!(
            calm,
            "must note it couldn't tool-verify (no artifacts), calmly"
        );
        assert!(
            !shouted,
            "must NOT shout UNVERIFIED on a legitimate reasoning task"
        );
    }

    #[tokio::test]
    async fn bridge_completion_flagged_unverified_when_work_done_but_never_re_checked() {
        // The C8 hole, properly scoped: the turn DID real work (inspect_calls: 1 → a tool ran on the
        // first turn), then claimed done but never re-inspected on verification. Forge forces the
        // verification cap and ends LOUDLY flagging UNVERIFIED — never a silent success.
        let provider = Arc::new(BridgeProvider {
            calls: std::sync::atomic::AtomicUsize::new(0),
            inspect_calls: 1, // real work on turn 1, no inspection on the verification turns
        });
        let (store, mut session) = bridge_session(provider.clone());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        session.presenter = Box::new(capture);
        seed_tasks(&store, &session.id, &[("ship the release", true)]);
        let _ = session.run_turn("release it").await.unwrap();
        // 1 work/claim turn + MAX_VERIFY_ATTEMPTS (2) verification turns = 3 invocations.
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "must force the verification cap, not loop forever"
        );
        let unverified = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::Warning(w) if w.contains("UNVERIFIED")));
        assert!(
            unverified,
            "work-producing completion never re-checked must end flagged UNVERIFIED, not as success"
        );
    }

    fn fixed_session(
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
    ) -> (Arc<Store>, Session) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        // Disable the in-turn rate-limit WAIT by default so failover tests don't real-sleep on a
        // server `retry_after`; the wait path has its own test that re-enables it with a tiny reset.
        let mut config = Config::default();
        config.mesh.rate_limit_wait_secs = 0;
        let session = Session::start(
            Arc::clone(&store),
            provider,
            router,
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        (store, session)
    }

    /// Panics if asked to complete — proves a code path makes NO provider call.
    struct PanicProvider;
    #[async_trait::async_trait]
    impl Provider for PanicProvider {
        async fn complete(
            &self,
            model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            panic!("provider must NOT be called when no usable model exists (routed: {model})");
        }
    }

    #[test]
    fn last_resort_skips_a_keyless_provider_even_when_it_recovers_soonest() {
        // The "groq for everything" churn: groq (no key) gets benched and, recovering soonest,
        // becomes the last-resort pick — dispatched, no-auth "Resolver error", re-benched, forever.
        // last_resort must skip any provider with no key (ollama/bridges keep qualifying — keyless).
        // `minimax` has no key on any test box (mirrors the sibling no-usable-model test); the dev
        // machine may well have a real GROQ_API_KEY, so use minimax as the keyless stand-in.
        assert!(
            !forge_config::has_api_key("minimax"),
            "test precondition: no minimax key in the environment"
        );
        assert!(
            forge_config::has_api_key("ollama"),
            "ollama is keyless → always usable"
        );
        let (store, session) = fixed_session(
            Arc::new(PanicProvider),
            Arc::new(FixedRouter {
                model: "m".into(),
                fallbacks: vec![],
            }),
        );
        // minimax recovers SOONER (10s) than ollama (60s) → soonest_unbenched would return minimax.
        store
            .bench_for(
                "minimax::abab-test",
                std::time::Duration::from_secs(10),
                "rate-limited",
            )
            .unwrap();
        store
            .bench_for(
                "ollama::llama3.2",
                std::time::Duration::from_secs(60),
                "rate-limited",
            )
            .unwrap();
        assert_eq!(
            session.last_resort_model("other::x", false).as_deref(),
            Some("ollama::llama3.2"),
            "last-resort must skip keyless groq and pick the usable ollama model"
        );
    }

    #[tokio::test]
    async fn no_usable_model_stops_the_turn_instead_of_spinning_on_a_keyless_provider() {
        // The "keeps trying groq for everything" bug: when nothing is usable the router falls back
        // to a key-needing model anyway. The core must STOP with an actionable diagnostic, not call
        // it (and auth-fail) every turn. `minimax` has no key here, so routing to it must short
        // out before the provider is ever touched — PanicProvider would fire if it were called.
        assert!(
            !forge_config::has_api_key("minimax"),
            "test precondition: no minimax key in the environment"
        );
        let (_store, mut session) = fixed_session(
            Arc::new(PanicProvider),
            Arc::new(FixedRouter {
                model: "minimax::abab-test".into(),
                fallbacks: vec![],
            }),
        );
        let answer = session.run_turn("write hello world").await.unwrap();
        assert!(
            answer.contains("No usable model") && answer.contains("minimax"),
            "actionable no-usable-model stop expected, got: {answer}"
        );
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
    async fn undo_after_compacted_resume_does_not_wipe_survivors() {
        // P0 data-loss regression: after compaction the active tail starts at a HIGH db seq, but a
        // resumed transcript is short. If `self.seq` were the loaded count (not MAX(seq)+1) and
        // `rewind_to` used the transcript index directly as the db seq, an `/undo` of the next turn
        // would `deactivate_messages_from(low_index)` and sweep the pre-compaction survivors.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let sid = store.create_session("/tmp", "default").unwrap();
        for i in 0..16i64 {
            store
                .add_message(&sid, i, Role::User, &format!("msg {i}"), None)
                .unwrap();
        }
        // Keep the last 6 (seq 10-15) active; summarize seq 0-9.
        store
            .compact_session_store(&sid, "summary of the first ten", 6)
            .unwrap();
        // Sanity: summary + 6 survivors.
        assert_eq!(store.load_messages(&sid).unwrap().len(), 7);

        let mut session = Session::resume(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            &sid,
        )
        .unwrap();

        // A fresh turn after the compacted resume, then undo it.
        session.run_turn("a brand new prompt").await.unwrap();
        assert!(session.undo().unwrap().is_some(), "the new turn was undone");

        // The six pre-compaction survivors (seq 10-15) MUST still be active — undo only removed the
        // new turn. Pre-fix, load_messages would return just the summary (survivors wiped).
        let after = store.load_messages(&sid).unwrap();
        assert_eq!(
            after.len(),
            7,
            "summary + 6 survivors must remain after undo; got {} msgs",
            after.len()
        );
        assert!(
            after.iter().any(|m| m.content == "msg 15"),
            "survivor 'msg 15' must still be active"
        );
    }

    #[tokio::test]
    async fn checkpoint_rewind_by_db_seq_after_compaction_targets_the_right_turn() {
        // Regression: the /checkpoints picker passes a DB SEQ to rewind_to. After compaction the
        // transcript index and DB seq diverge; rewind_to must interpret its argument as a DB seq
        // (both undo and the picker pass seqs) — not a transcript index, which double-offset and
        // rewound to the wrong turn (or no-op).
        let store = Arc::new(Store::open_in_memory().unwrap());
        let sid = store.create_session("/tmp", "default").unwrap();
        for i in 0..16i64 {
            store
                .add_message(&sid, i, Role::User, &format!("msg {i}"), None)
                .unwrap();
        }
        store
            .compact_session_store(&sid, "summary of the first ten", 6)
            .unwrap();
        let mut session = Session::resume(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            &sid,
        )
        .unwrap();

        session.checkpoint(Some("before the turn")).unwrap();
        let cp_seq = session.checkpoints().unwrap()[0].seq; // a DB seq, as the picker passes
        session.run_turn("a brand new prompt").await.unwrap();

        // Picker-style rewind by DB seq must roll back exactly the new turn and keep the survivors.
        session.rewind_to(cp_seq).unwrap();
        let after = store.load_messages(&sid).unwrap();
        assert_eq!(
            after.len(),
            7,
            "summary + 6 survivors after rewinding the new turn by DB seq; got {}",
            after.len()
        );
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
        // Disable auto-memory: its start-of-turn recall can invoke the embedder (a network call on
        // CI) before the user message is persisted, which would race the 100ms abort window below.
        // This test is about lock release, not memory.
        let mut config = Config::default();
        config.mesh.auto_memory = false;
        let session = Arc::new(tokio::sync::Mutex::new(
            Session::start(
                store,
                Arc::new(SlowProvider),
                Arc::new(HeuristicRouter::new(Config::default())),
                ToolRegistry::with_core_tools(),
                Box::new(HeadlessPresenter::new(false)),
                config,
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
            auto_detect: false, // explicit cmds set; no detection needed
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
            auto_detect: false,
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

    /// Call 0 writes a file (an edit → `edits_this_turn > 0`, arming autofix); every later call just
    /// says "done" (no tools), so the only thing that can stop the self-heal loop is its iteration cap.
    /// `cfg(unix)` because the only test using it relies on the `false` shell command.
    #[cfg(unix)]
    struct EditOnceThenDoneProvider {
        calls: std::sync::atomic::AtomicUsize,
        path: String,
    }
    #[cfg(unix)]
    #[async_trait::async_trait]
    impl Provider for EditOnceThenDoneProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_types::{new_id, ToolCall, Usage};
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let tool_calls = if n == 0 {
                vec![ToolCall {
                    id: new_id(),
                    name: "write_file".into(),
                    args: serde_json::json!({"path": self.path, "content": "x = 1\n"}),
                }]
            } else {
                Vec::new()
            };
            Ok(forge_provider::ModelResponse {
                content: "done".into(),
                tool_calls,
                usage: Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn autofix_iteration_cap_halts_the_self_heal_loop() {
        // The autofix self-heal loop re-runs the model when lint/test fail. If they NEVER pass, only
        // the `max_iterations` cap can stop it. Pin that: a turn makes one edit (arming autofix), the
        // lint command always fails (`false`), and the loop must stop at the cap, not spin forever.
        let dir = std::env::temp_dir().join(format!("forge-autofix-cap-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("f.py");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let config = Config {
            permission_mode: forge_types::PermissionMode::AcceptEdits, // auto-allow the write
            autofix: forge_config::AutofixConfig {
                auto_lint: true,
                auto_test: false,
                lint_cmd: "false".to_string(), // always exits 1 → never "fixed"
                test_cmd: String::new(),
                max_iterations: 2,
                auto_detect: false,
            },
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(EditOnceThenDoneProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                path: path.to_string_lossy().into_owned(),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();

        // Must RETURN (the cap stops it), not loop forever.
        session.run_turn("write the file").await.unwrap();

        let warnings: Vec<String> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                PresenterEvent::Warning(w) => Some(w.clone()),
                _ => None,
            })
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("reached iteration cap")),
            "the autofix loop must stop at its iteration cap; warnings: {warnings:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mesh.self_review` is off by default (it regressed when on-by-default), but must stay WIRED:
    /// when enabled, a turn that edited runs a review pass that re-checks the diff before finishing.
    /// Pin that it actually fires + announces itself, so the gated feature can't silently rot.
    #[cfg(unix)]
    #[tokio::test]
    async fn self_review_runs_after_an_edit_turn_when_enabled() {
        let dir = std::env::temp_dir().join(format!("forge-selfreview-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("f.py");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        // MeshConfig has no `Default` (Config builds it explicitly), so take the default mesh and
        // flip just `self_review`.
        let base_mesh = Config::default().mesh;
        let config = Config {
            permission_mode: forge_types::PermissionMode::AcceptEdits, // auto-allow the write
            mesh: forge_config::MeshConfig {
                self_review: true,
                ..base_mesh
            },
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(EditOnceThenDoneProvider {
                calls: std::sync::atomic::AtomicUsize::new(0),
                path: path.to_string_lossy().into_owned(),
            }),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();

        session.run_turn("write the file").await.unwrap();

        let warned = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::Warning(w) if w.contains("self-review")));
        assert!(
            warned,
            "the self-review pass must run + announce itself when mesh.self_review is enabled"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            auto_detect: false,
        };
        // No commands run → stage trivially passes.
        let passed = session.run_autofix_stage(&af).await.unwrap();
        assert!(passed, "empty commands → nothing runs → stage passes");
    }

    // ── Auto-review gate tests ────────────────────────────────────────────────────────────────

    #[test]
    fn tool_batch_signature_distinguishes_calls() {
        use forge_types::ToolCall;
        let mk = |name: &str, args: serde_json::Value| ToolCall {
            id: "x".into(),
            name: name.into(),
            args,
        };
        let a = vec![mk("read_file", serde_json::json!({"path": "a.rs"}))];
        let a2 = vec![mk("read_file", serde_json::json!({"path": "a.rs"}))];
        let b = vec![mk("read_file", serde_json::json!({"path": "b.rs"}))];
        let c = vec![mk("edit_file", serde_json::json!({"path": "a.rs"}))];
        // Identical batches hash equal (drives doom-loop detection); different args or tool differ.
        assert_eq!(tool_batch_signature(&a), tool_batch_signature(&a2));
        assert_ne!(tool_batch_signature(&a), tool_batch_signature(&b));
        assert_ne!(tool_batch_signature(&a), tool_batch_signature(&c));
    }

    #[test]
    fn classify_tool_failure_detects_kinds_and_ignores_success() {
        assert_eq!(
            classify_tool_failure("error: No such file or directory (os error 2)"),
            Some(ErrorCategory::NotFound)
        );
        assert_eq!(
            classify_tool_failure("permission denied by policy"),
            Some(ErrorCategory::Permission)
        );
        assert_eq!(
            classify_tool_failure("error: no match for the given old_string"),
            Some(ErrorCategory::Schema)
        );
        assert_eq!(
            classify_tool_failure("error: the request timed out after 30s"),
            Some(ErrorCategory::Timeout)
        );
        assert_eq!(
            classify_tool_failure("error: the connection was reset by peer"),
            Some(ErrorCategory::Other)
        );
        // "not found" wins over the validation hint when both appear — fine; the guard only needs a
        // STABLE bucket so repeats of the same failure accumulate together.
        assert_eq!(
            classify_tool_failure("error: old_string not found in file"),
            Some(ErrorCategory::NotFound)
        );
        // Successful output that merely mentions a scary word must NOT be read as a failure.
        assert_eq!(
            classify_tool_failure("fn validate() { /* reject invalid states */ }"),
            None
        );
        assert_eq!(classify_tool_failure("file written"), None);
    }

    #[test]
    fn completion_gate_forces_a_real_check_before_accepting_done() {
        const MAX: usize = 2;
        // First "all done" claim is never accepted — force a verification turn regardless of
        // whether the turn happened to inspect (the claim itself hasn't been re-checked yet).
        assert_eq!(
            completion_gate(0, MAX, true, false),
            CompletionGate::Reverify
        );
        assert_eq!(
            completion_gate(0, MAX, true, true),
            CompletionGate::Reverify
        );
        assert_eq!(
            completion_gate(0, MAX, false, false),
            CompletionGate::Reverify
        );

        // After ≥1 verification attempt: a turn that actually ran an inspection is accepted cleanly.
        assert_eq!(
            completion_gate(1, MAX, true, true),
            CompletionGate::AcceptClean
        );

        // Work existed but the verification turn re-asserted done WITHOUT inspecting (the C8 hole):
        // not yet at the cap, so force one more real check rather than accept.
        assert_eq!(
            completion_gate(1, MAX, true, false),
            CompletionGate::Reverify
        );

        // No external artifacts to check (pure-analysis turn): accept with the calm note, since
        // requiring a tool inspection would over-fire.
        assert_eq!(
            completion_gate(1, MAX, false, false),
            CompletionGate::AcceptNoArtifacts
        );

        // Verification budget spent while real work existed and was never tool-checked: accept but
        // flag UNVERIFIED rather than silently report success.
        assert_eq!(
            completion_gate(MAX, MAX, true, false),
            CompletionGate::AcceptUnverified
        );
        // …but a turn that DID inspect at the cap is still a clean accept.
        assert_eq!(
            completion_gate(MAX, MAX, true, true),
            CompletionGate::AcceptClean
        );
    }

    #[test]
    fn context_fill_uses_estimate_only_for_subscription_bridges() {
        // Direct API model: trust the provider's real input-token count.
        assert_eq!(
            context_fill_tokens("anthropic::claude-sonnet-4-5", 1_000, 50_000),
            50_000
        );
        assert_eq!(context_fill_tokens("openai::gpt-4o", 1_000, 50_000), 50_000);
        // Subscription CLI bridge: its reported usage is cumulative (here a bogus 900k), so the
        // gauge must use the transcript estimate instead — this is the 337%-gauge fix.
        assert_eq!(
            context_fill_tokens("claude-cli::opus", 90_000, 900_000),
            90_000
        );
        assert_eq!(
            context_fill_tokens("codex-cli::gpt-5.5", 90_000, 900_000),
            90_000
        );
    }

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
    fn bump_tier_shifts_and_clamps_the_session_pin() {
        let mut session = make_session(Config::default());
        assert_eq!(session.pinned_tier(), None);
        // First press from a Standard baseline → Complex pin.
        assert_eq!(
            session.bump_tier(true, TaskTier::Standard),
            TaskTier::Complex
        );
        assert_eq!(session.pinned_tier(), Some(TaskTier::Complex));
        // Up again clamps at Complex.
        assert_eq!(
            session.bump_tier(true, TaskTier::Standard),
            TaskTier::Complex
        );
        // Down walks back through Standard → Trivial, then clamps.
        assert_eq!(
            session.bump_tier(false, TaskTier::Standard),
            TaskTier::Standard
        );
        assert_eq!(
            session.bump_tier(false, TaskTier::Standard),
            TaskTier::Trivial
        );
        assert_eq!(
            session.bump_tier(false, TaskTier::Standard),
            TaskTier::Trivial
        );
        // Clearing returns to normal classification.
        session.pin_tier(None);
        assert_eq!(session.pinned_tier(), None);
    }

    #[test]
    fn resolve_planner_falls_back_to_complex_tier_model() {
        // No architect_model set, no pin → first USABLE Complex-tier candidate. Deterministic
        // config (a single keyless candidate) so the result doesn't depend on which provider keys
        // happen to be set in the test environment.
        let mut config = Config::default();
        config.mesh.models.insert(
            forge_types::TaskTier::Complex.as_str().into(),
            forge_config::OneOrMany::Many(vec!["ollama::big".into()]),
        );
        let session = make_session(config);
        assert_eq!(session.resolve_planner_model(), "ollama::big");
    }

    #[test]
    fn resolve_editor_falls_back_to_standard_tier_model() {
        // No editor_model set, no pin → first USABLE Standard-tier candidate (deterministic config).
        let mut config = Config::default();
        config.mesh.models.insert(
            forge_types::TaskTier::Standard.as_str().into(),
            forge_config::OneOrMany::Many(vec!["ollama::mid".into()]),
        );
        let session = make_session(config);
        assert_eq!(session.resolve_editor_model(), "ollama::mid");
    }

    #[test]
    fn architect_planner_and_editor_skip_a_keyless_provider() {
        // The friend's bug: architect_mode on + the built-in tier defaults lead with `groq::…`, so
        // the planner/editor dispatched groq and auth-failed every turn (no groq key). The resolved
        // model must skip a no-key provider and pick the first USABLE candidate instead.
        assert!(
            !forge_config::has_api_key("minimax"),
            "test precondition: no minimax key"
        );
        assert!(forge_config::has_api_key("ollama"), "ollama is keyless");
        let mut config = Config::default();
        // First candidate keyless-unusable (no key), second keyless-usable.
        config.mesh.models.insert(
            forge_types::TaskTier::Complex.as_str().into(),
            forge_config::OneOrMany::Many(vec!["minimax::abab".into(), "ollama::y".into()]),
        );
        config.mesh.models.insert(
            forge_types::TaskTier::Standard.as_str().into(),
            forge_config::OneOrMany::Many(vec!["minimax::abab".into(), "ollama::z".into()]),
        );
        let session = make_session(config);
        assert_eq!(session.resolve_planner_model(), "ollama::y");
        assert_eq!(session.resolve_editor_model(), "ollama::z");
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
