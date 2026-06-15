//! The presenter seam (ADR-0004): `forge-core` emits [`PresenterEvent`]s and asks for
//! permission confirmations through the [`Presenter`] trait, never touching a concrete
//! UI. v0.1 ships the [`HeadlessPresenter`] (line output for scripting/pipes/CI); the
//! ratatui+crossterm interactive renderer is the next increment behind this same trait.

use std::io::{IsTerminal, Write};

use forge_types::SideEffect;

pub mod app;
mod driver;
mod render;
mod tui;
pub use app::{banner_lines, handle_key, App, InputOutcome, KeyKind};
pub use driver::{ChannelPresenter, Tui, UiMsg};
pub use tui::TuiPresenter;

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
    },
    /// A proposed file change, emitted by core BEFORE the write is confirmed/applied so the
    /// user reviews the diff before answering the permission prompt.
    Diff(forge_types::FileDiff),
    Done {
        final_text: String,
    },
}

/// A rendering + interaction surface. Implementors decide how to display events and how
/// to obtain a permission decision from the user.
pub trait Presenter: Send {
    fn emit(&mut self, event: PresenterEvent);
    /// Ask the user to confirm a side-effecting tool. Returns true to allow.
    fn confirm(&mut self, tool: &str, side_effect: SideEffect) -> bool;
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
            PresenterEvent::Cost { session_total_usd } => {
                println!("  $ session total: ${session_total_usd:.4}");
            }
            PresenterEvent::Diff(diff) => {
                // Plain unified-diff text for scripting/pipes (no ANSI).
                print!("{}", render::diff_to_plain(&diff));
                let _ = std::io::stdout().flush();
            }
            // The final answer was already streamed via AssistantText; Done is a
            // lifecycle marker, so the headless renderer needs no extra output here.
            PresenterEvent::Done { .. } => {}
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
