//! The `forge` binary: parse arguments, load config, wire the subsystems behind their
//! traits, and drive one agent turn. This is the thin composition root (ADR-0002).

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use forge_core::Session;
use forge_mesh::HeuristicRouter;
use forge_provider::{GenAiProvider, MockProvider, Provider};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::{HeadlessPresenter, Presenter, TuiPresenter};
use forge_types::PermissionMode;

#[derive(Parser)]
#[command(
    name = "forge",
    version,
    about = "Fast, model-agnostic AI coding harness."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a single agent turn against your prompt.
    Run {
        /// The prompt / task for the agent.
        prompt: Vec<String>,
        /// Use the offline deterministic mock provider (no API keys, no network).
        #[arg(long)]
        mock: bool,
        /// Override the permission mode for this run.
        #[arg(long, value_enum)]
        mode: Option<Mode>,
        /// Render the interactive ratatui TUI instead of plain line output.
        #[arg(long)]
        tui: bool,
        /// Resume an existing session by id instead of starting a new one.
        #[arg(long)]
        resume: Option<String>,
    },
    /// Start an interactive multi-turn chat session (reads prompts from stdin).
    Chat {
        /// Use the offline deterministic mock provider.
        #[arg(long)]
        mock: bool,
        /// Override the permission mode.
        #[arg(long, value_enum)]
        mode: Option<Mode>,
        /// Resume an existing session by id.
        #[arg(long)]
        resume: Option<String>,
    },
    /// List past sessions (newest first).
    Sessions,
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Default,
    AcceptEdits,
    Bypass,
    Plan,
}

impl From<Mode> for PermissionMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Default => PermissionMode::Default,
            Mode::AcceptEdits => PermissionMode::AcceptEdits,
            Mode::Bypass => PermissionMode::Bypass,
            Mode::Plan => PermissionMode::Plan,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            prompt,
            mock,
            mode,
            tui,
            resume,
        } => run(prompt.join(" "), mock, mode, tui, resume).await,
        Command::Chat { mock, mode, resume } => chat(mock, mode, resume).await,
        Command::Sessions => sessions(),
    }
}

fn open_store() -> Result<Store> {
    std::fs::create_dir_all(".forge").context("creating .forge directory")?;
    Store::open(Path::new(".forge/forge.db")).context("opening session store")
}

/// Resolve a (possibly abbreviated) session id to a single full id, git-style.
fn resolve_session(store: &Store, prefix: &str) -> Result<String> {
    let mut matches = store
        .matching_session_ids(prefix)
        .context("looking up session")?;
    match matches.len() {
        0 => anyhow::bail!("no session matching '{prefix}' — see `forge sessions`"),
        1 => Ok(matches.remove(0)),
        n => anyhow::bail!("'{prefix}' is ambiguous ({n} sessions match) — use more characters"),
    }
}

fn sessions() -> Result<()> {
    let store = open_store()?;
    let list = store.list_sessions().context("listing sessions")?;
    if list.is_empty() {
        println!("no sessions yet — run `forge run \"<task>\"` to start one");
        return Ok(());
    }
    for s in list {
        let id: String = s.id.chars().take(8).collect();
        let preview = s.preview.unwrap_or_default();
        let preview: String = preview.chars().take(50).collect();
        println!(
            "{id}  ${:>8.4}  {:>3} msgs  {}",
            s.total_cost_usd, s.message_count, preview
        );
    }
    Ok(())
}

/// Build a ready session, either fresh or resumed, wiring all subsystems.
fn build_session(
    mock: bool,
    mode: Option<Mode>,
    tui: bool,
    resume: Option<String>,
) -> Result<Session> {
    let mut config = forge_config::load().context("loading configuration")?;
    if let Some(m) = mode {
        config.permission_mode = m.into();
    }

    let store = open_store()?;

    let provider: Box<dyn Provider> = if mock {
        Box::new(MockProvider)
    } else {
        Box::new(GenAiProvider::new())
    };
    let router = Box::new(HeuristicRouter::new(config.clone()));
    let tools = ToolRegistry::with_core_tools();
    let presenter: Box<dyn Presenter> = if tui && std::io::stdout().is_terminal() {
        Box::new(TuiPresenter::new().context("initializing TUI")?)
    } else {
        if tui {
            eprintln!("forge: --tui needs an interactive terminal; falling back to plain output");
        }
        Box::new(HeadlessPresenter::default())
    };

    match resume {
        Some(prefix) => {
            let full = resolve_session(&store, &prefix)?;
            Session::resume(store, provider, router, tools, presenter, config, &full)
                .with_context(|| format!("resuming session {full}"))
        }
        None => {
            let cwd = std::env::current_dir()?.display().to_string();
            Session::start(store, provider, router, tools, presenter, config, &cwd)
                .context("starting session")
        }
    }
}

async fn run(
    prompt: String,
    mock: bool,
    mode: Option<Mode>,
    tui: bool,
    resume: Option<String>,
) -> Result<()> {
    if prompt.trim().is_empty() {
        anyhow::bail!("empty prompt — usage: forge run \"<your task>\"");
    }
    let mut session = build_session(mock, mode, tui, resume)?;
    session
        .run_turn(&prompt)
        .await
        .context("running agent turn")?;
    Ok(())
}

/// What a line typed at the chat prompt means.
#[derive(Debug, PartialEq, Eq)]
enum ChatAction {
    Quit,
    Skip,
    Run(String),
}

fn chat_action(line: &str) -> ChatAction {
    match line.trim() {
        "" => ChatAction::Skip,
        "/quit" | "/exit" | "/q" => ChatAction::Quit,
        task => ChatAction::Run(task.to_string()),
    }
}

async fn chat(mock: bool, mode: Option<Mode>, resume: Option<String>) -> Result<()> {
    // TUI chat (input box) is a planned follow-up; chat reads prompts from stdin.
    let mut session = build_session(mock, mode, false, resume)?;

    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        println!("forge chat — type a task and press enter; /quit to exit");
    }

    let mut handle = stdin.lock();
    loop {
        if interactive {
            print!("› ");
            std::io::stdout().flush().ok();
        }
        let mut line = String::new();
        if handle.read_line(&mut line)? == 0 {
            break; // EOF
        }
        match chat_action(&line) {
            ChatAction::Quit => break,
            ChatAction::Skip => continue,
            ChatAction::Run(task) => {
                session
                    .run_turn(&task)
                    .await
                    .context("running agent turn")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_action_classifies_lines() {
        assert_eq!(chat_action("  "), ChatAction::Skip);
        assert_eq!(chat_action("\n"), ChatAction::Skip);
        assert_eq!(chat_action("/quit"), ChatAction::Quit);
        assert_eq!(chat_action("/exit\n"), ChatAction::Quit);
        assert_eq!(chat_action("  /q "), ChatAction::Quit);
        assert_eq!(
            chat_action("fix the bug\n"),
            ChatAction::Run("fix the bug".to_string())
        );
    }
}
