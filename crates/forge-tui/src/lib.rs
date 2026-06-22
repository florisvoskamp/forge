//! The presenter seam (ADR-0004): `forge-core` emits [`PresenterEvent`]s and asks for
//! permission confirmations through the [`Presenter`] trait, never touching a concrete
//! UI. v0.1 ships the [`HeadlessPresenter`] (line output for scripting/pipes/CI); the
//! ratatui+crossterm interactive renderer is the next increment behind this same trait.

use std::io::{IsTerminal, Write};

use forge_types::SideEffect;

/// One choice in an [`Presenter::ask`] question (AskUserQuestion). `description` may be empty.
#[derive(Debug, Clone)]
pub struct QChoice {
    pub label: String,
    pub description: String,
}

/// Sentinel returned when a question can't be answered interactively (piped / no tty).
pub const NO_ANSWER: &str = "(no answer — non-interactive)";

/// Resolve a typed answer line against the options: a number `1..=N` picks that option's label;
/// otherwise, if `allow_other`, the trimmed line is a free-text answer. `None` = invalid input
/// (not a valid number and free text not allowed) → the caller should re-prompt.
pub fn resolve_answer(line: &str, options: &[QChoice], allow_other: bool) -> Option<String> {
    let t = line.trim();
    if let Ok(n) = t.parse::<usize>() {
        if n >= 1 && n <= options.len() {
            return Some(options[n - 1].label.clone());
        }
    }
    if allow_other && !t.is_empty() {
        return Some(t.to_string());
    }
    None
}

pub mod app;
mod commands;
mod driver;
pub mod init_wizard;
mod render;
pub mod select;
mod transcript;
mod tui;
pub use app::{
    banner_lines, handle_key, lattice_view_lines, render_mesh_overlay, render_usage_overlay, App,
    InputOutcome, KeyKind, MeshCandRow, MeshOverlay, MeshQuotaRow, RemoteSnapshot, ReplayItem,
    SubagentView, UsageOverlay,
};
pub use commands::{
    at_token_at, filter_commands, parse_command, slash_token_at, AtPathPicker, AtToken, Command,
    CommandAction, Palette, PaletteEntry, Picker, PickerKind, PickerRow, RemoteMode, SlashToken,
    COMMANDS,
};
pub use driver::{ChannelPresenter, InputEvent, Tui, UiMsg};
pub use init_wizard::{BridgeItem, ProviderItem, WizardInput, WizardOutcome};
pub use select::{select_multi, SelectItem};
pub use transcript::{run_subagent_transcript, transcript_lines};
pub use tui::TuiPresenter;

// `QChoice`, `resolve_answer`, `NO_ANSWER` are defined above and re-exported at crate root.

#[cfg(test)]
mod ask_tests {
    use super::*;

    fn opts() -> Vec<QChoice> {
        vec![
            QChoice {
                label: "Postgres".into(),
                description: "relational".into(),
            },
            QChoice {
                label: "SQLite".into(),
                description: String::new(),
            },
        ]
    }

    #[test]
    fn a_number_picks_that_option() {
        assert_eq!(
            resolve_answer("2", &opts(), true).as_deref(),
            Some("SQLite")
        );
        assert_eq!(
            resolve_answer(" 1 ", &opts(), false).as_deref(),
            Some("Postgres")
        );
    }

    #[test]
    fn free_text_allowed_only_when_open() {
        assert_eq!(
            resolve_answer("use mysql", &opts(), true).as_deref(),
            Some("use mysql")
        );
        assert_eq!(resolve_answer("use mysql", &opts(), false), None);
    }

    #[test]
    fn out_of_range_number_is_invalid() {
        assert_eq!(resolve_answer("9", &opts(), false), None);
        // ...but a free-text fallback accepts it as text when open.
        assert_eq!(resolve_answer("9", &opts(), true).as_deref(), Some("9"));
    }

    #[test]
    fn non_interactive_headless_returns_the_sentinel() {
        let mut p = HeadlessPresenter::new(false);
        assert_eq!(p.ask("which db?", &opts(), true), NO_ANSWER);
    }
}

/// Things the core wants to show the user as a turn progresses.
#[derive(Debug, Clone)]
pub enum PresenterEvent {
    SessionStarted {
        id: String,
    },
    Routing {
        tier: String,
        model: String,
        rationale: String,
    },
    AssistantText(String),
    /// A streamed fragment of the assistant's reply (tokens as they arrive).
    AssistantDelta(String),
    /// A streamed fragment of the model's reasoning/thinking (shown live, dim; not the answer).
    Reasoning(String),
    /// The assistant's streamed reply for this step is complete.
    AssistantDone,
    /// A non-fatal advisory (e.g. budget threshold reached).
    Warning(String),
    ToolStart {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        summary: String,
    },
    Cost {
        session_total_usd: f64,
        /// Session-total input/output tokens (live counter).
        session_in: u64,
        session_out: u64,
        /// Tokens in the current live context (≈ the last call's input size).
        context_tokens: u64,
        /// The active model's context-window limit, if known (`None` → no gauge denominator).
        context_limit: Option<u32>,
    },
    /// A subagent (child agent) was spawned for a subtask (RFC subagent-orchestration).
    SubagentStart {
        id: String,
        agent: String,
        task: String,
    },
    /// A live activity snippet from a still-running subagent (streamed text/reasoning).
    SubagentProgress {
        id: String,
        snippet: String,
    },
    /// A subagent finished, with its one-line result summary and its cost.
    SubagentResult {
        id: String,
        agent: String,
        ok: bool,
        summary: String,
        cost_usd: f64,
    },
    /// A proposed file change, emitted by core BEFORE the write is confirmed/applied so the
    /// user reviews the diff before answering the permission prompt.
    Diff(forge_types::FileDiff),
    /// Live Assay progress (started / other top-level events) — shown in scrollback.
    AssayProgress(String),
    /// Structured per-critic status update for the live assay panel in the TUI.
    AssayCriticRow(forge_types::AssayCriticRow),
    /// Verification phase started — shown in the assay panel header, not in scrollback.
    AssayVerifying {
        candidates: usize,
    },
    /// A finished Assay analysis report, for inline rendering (docs/features/analysis-mode.md).
    AssayReport(forge_types::AssayReport),
    /// The agent's task list changed (`update_tasks`); render the checklist into scrollback.
    Tasks(Vec<forge_types::TodoItem>),
    /// MCP server connection status changed / was requested (`/mcp`); render the listing.
    McpStatus(Vec<forge_types::McpServerLine>),
    /// Lattice auto-retrieval injected relevant code ahead of the model call (code-intelligence.md).
    ContextInjected {
        symbols: usize,
        files: usize,
        tokens: usize,
    },
    /// A failed shell command was auto-diagnosed by the model (shell-error-interceptor.md):
    /// a short likely-cause + suggested fix, surfaced alongside the tool result.
    ShellDiagnosis {
        command: String,
        diagnosis: String,
        /// A concrete shell command that fixes the failure, if the model identified one.
        /// The TUI shows "press F to populate fix" and pressing F inserts it into the input.
        fix: Option<String>,
    },
    Done {
        final_text: String,
    },
    /// A subscription quota observation arrived this turn (rate_limit_event / Codex rollout).
    /// Used to update the /usage overlay in real-time without waiting for the DB refresh cycle.
    QuotaUpdate {
        provider: String,
        window: String,
        fraction: f64,
    },
    /// Compaction (summarizing older messages) started — drives the animated progress band in the
    /// TUI. `auto` distinguishes a silent auto-compact from an explicit `/compact`.
    CompactionStarted {
        auto: bool,
    },
    /// Compaction finished, with the message counts before/after — clears the progress band.
    CompactionFinished {
        before: usize,
        after: usize,
    },
}

/// A rendering + interaction surface. Implementors decide how to display events and how
/// to obtain a permission decision from the user.
pub trait Presenter: Send {
    fn emit(&mut self, event: PresenterEvent);
    /// Ask the user to confirm a side-effecting tool. Returns true to allow.
    fn confirm(&mut self, tool: &str, side_effect: SideEffect) -> bool;
    /// Ask the user a question with suggested `options` (AskUserQuestion). Returns the chosen
    /// option's label, or — when `allow_other` — a free-text answer; [`NO_ANSWER`] if it can't
    /// be asked interactively.
    fn ask(&mut self, question: &str, options: &[QChoice], allow_other: bool) -> String;
    /// Read the next prompt line from the user. `None` means quit / end-of-input.
    fn read_line(&mut self) -> Option<String>;
}

/// Plain line-based renderer for non-interactive use.
pub struct HeadlessPresenter {
    /// When false (e.g. piped, non-tty), confirmations default to deny (safe).
    interactive: bool,
}

impl Default for HeadlessPresenter {
    fn default() -> Self {
        Self {
            interactive: std::io::stdin().is_terminal(),
        }
    }
}

impl HeadlessPresenter {
    pub fn new(interactive: bool) -> Self {
        Self { interactive }
    }
}

impl Presenter for HeadlessPresenter {
    fn emit(&mut self, event: PresenterEvent) {
        match event {
            PresenterEvent::SessionStarted { id } => {
                println!("● session {id}");
            }
            PresenterEvent::Routing {
                tier,
                model,
                rationale,
            } => {
                println!("⚒ mesh → [{tier}] {model}  ({rationale})");
            }
            PresenterEvent::AssistantText(text) => {
                println!("\n{text}");
            }
            PresenterEvent::AssistantDelta(delta) => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            PresenterEvent::Reasoning(delta) => {
                // Dim so reasoning is visually distinct from the answer.
                print!("\x1b[2m{delta}\x1b[0m");
                let _ = std::io::stdout().flush();
            }
            PresenterEvent::AssistantDone => {
                println!();
            }
            PresenterEvent::Warning(msg) => {
                println!("  ⚠ {msg}");
            }
            PresenterEvent::ToolStart { name, args } => {
                println!("  ↳ {name}({args})");
            }
            PresenterEvent::ToolResult { name, ok, summary } => {
                let mark = if ok { "✓" } else { "✗" };
                println!("  {mark} {name}: {summary}");
            }
            PresenterEvent::Cost {
                session_total_usd,
                session_in,
                session_out,
                ..
            } => {
                println!(
                    "  $ session total: ${session_total_usd:.4} · ↑{session_in} ↓{session_out} tok"
                );
            }
            PresenterEvent::SubagentStart { agent, task, .. } => {
                println!("  ⤷ spawn [{agent}]: {task}");
            }
            // Live per-child deltas are for the interactive TUI row; the line-based renderer
            // stays quiet and shows the final SubagentResult.
            PresenterEvent::SubagentProgress { .. } => {}
            PresenterEvent::SubagentResult {
                agent,
                ok,
                summary,
                cost_usd,
                ..
            } => {
                let mark = if ok { "✓" } else { "✗" };
                println!("  {mark} agent [{agent}] (${cost_usd:.4}): {summary}");
            }
            PresenterEvent::Diff(diff) => {
                // Plain unified-diff text for scripting/pipes (no ANSI).
                print!("{}", render::diff_to_plain(&diff));
                let _ = std::io::stdout().flush();
            }
            PresenterEvent::AssayProgress(msg) => {
                println!("  {msg}");
            }
            PresenterEvent::AssayCriticRow(row) => {
                use forge_types::AssayCriticStatus;
                let status = match &row.status {
                    AssayCriticStatus::Queued => "queued".to_string(),
                    AssayCriticStatus::Done { candidates } => {
                        let model = row.model.as_deref().unwrap_or("?");
                        format!("done ({candidates}) [{model}] ${:.4}", row.cost_usd)
                    }
                    AssayCriticStatus::Skipped { reason } => format!("skipped ({reason})"),
                };
                println!("  {} — {status}", row.lens);
            }
            PresenterEvent::AssayVerifying { candidates } => {
                println!("  ⚖ verifying {candidates} candidate(s)…");
            }
            PresenterEvent::AssayReport(report) => {
                print!("{}", render::assay_report_plain(&report));
                let _ = std::io::stdout().flush();
            }
            PresenterEvent::Tasks(tasks) => {
                let done = tasks
                    .iter()
                    .filter(|t| t.status == forge_types::TodoStatus::Done)
                    .count();
                println!("  tasks ({done}/{} done):", tasks.len());
                for t in &tasks {
                    println!("    {} {}", t.status.marker(), t.title);
                }
            }
            PresenterEvent::McpStatus(servers) => {
                if servers.is_empty() {
                    println!("  no MCP servers configured");
                } else {
                    println!("  MCP servers ({} configured)", servers.len());
                    for s in &servers {
                        let detail = s
                            .detail
                            .as_deref()
                            .map(|d| format!("  {d}"))
                            .unwrap_or_default();
                        println!(
                            "    {} {} {} — {} tools · {} resources · {} prompts{detail}",
                            s.name, s.status, s.transport, s.tools, s.resources, s.prompts
                        );
                    }
                }
            }
            PresenterEvent::ContextInjected {
                symbols,
                files,
                tokens,
            } => {
                println!(
                    "  ⌬ lattice → injected {symbols} symbols · {files} files (~{tokens} tok)"
                );
            }
            PresenterEvent::ShellDiagnosis {
                command,
                diagnosis,
                fix,
            } => {
                println!("  ⚠ shell failed: {command}");
                for line in diagnosis.lines() {
                    println!("    {line}");
                }
                if let Some(cmd) = fix {
                    println!("    fix: {cmd}");
                }
            }
            // The final answer was already streamed via AssistantText; Done is a
            // lifecycle marker, so the headless renderer needs no extra output here.
            PresenterEvent::Done { .. } => {}
            // Real-time quota updates are for the TUI overlay; headless ignores them.
            PresenterEvent::QuotaUpdate { .. } => {}
            PresenterEvent::CompactionStarted { auto } => {
                println!("  ⟳ compacting{}…", if auto { " (auto)" } else { "" });
            }
            PresenterEvent::CompactionFinished { before, after } => {
                println!("  ⟳ compacted {before} → {after} messages");
            }
        }
    }

    fn confirm(&mut self, tool: &str, side_effect: SideEffect) -> bool {
        if !self.interactive {
            println!("  ⚠ denying {tool} ({side_effect:?}) — non-interactive session");
            return false;
        }
        print!("  ⚠ allow {tool} ({side_effect:?})? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes")
    }

    fn ask(&mut self, question: &str, options: &[QChoice], allow_other: bool) -> String {
        if !self.interactive {
            return NO_ANSWER.to_string();
        }
        // Re-prompt a couple of times on invalid input, then give up gracefully.
        for _ in 0..3 {
            println!("\n❓ {question}");
            for (i, o) in options.iter().enumerate() {
                if o.description.is_empty() {
                    println!("  {}) {}", i + 1, o.label);
                } else {
                    println!("  {}) {} — {}", i + 1, o.label, o.description);
                }
            }
            if allow_other {
                print!("  choose a number, or type your own answer: ");
            } else {
                print!("  choose a number: ");
            }
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return NO_ANSWER.to_string();
            }
            if let Some(ans) = resolve_answer(&line, options, allow_other) {
                return ans;
            }
        }
        NO_ANSWER.to_string()
    }

    fn read_line(&mut self) -> Option<String> {
        if self.interactive {
            print!("› ");
            let _ = std::io::stdout().flush();
        }
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None, // EOF or read error -> end
            Ok(_) => Some(line),
        }
    }
}
