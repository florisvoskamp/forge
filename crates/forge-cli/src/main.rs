//! The `forge` binary: parse arguments, load config, wire the subsystems behind their
//! traits, and drive one agent turn. This is the thin composition root (ADR-0002).

use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use forge_core::Session;
use forge_mesh::HeuristicRouter;
use forge_provider::{GenAiProvider, MockProvider, Provider};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::HeadlessPresenter;
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
    },
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
        Command::Run { prompt, mock, mode } => run(prompt.join(" "), mock, mode).await,
    }
}

async fn run(prompt: String, mock: bool, mode: Option<Mode>) -> Result<()> {
    if prompt.trim().is_empty() {
        anyhow::bail!("empty prompt — usage: forge run \"<your task>\"");
    }

    let mut config = forge_config::load().context("loading configuration")?;
    if let Some(m) = mode {
        config.permission_mode = m.into();
    }

    // Per-project local state under ./.forge (gitignored).
    std::fs::create_dir_all(".forge").context("creating .forge directory")?;
    let store = Store::open(Path::new(".forge/forge.db")).context("opening session store")?;

    let provider: Box<dyn Provider> = if mock {
        Box::new(MockProvider)
    } else {
        Box::new(GenAiProvider::new())
    };
    let router = Box::new(HeuristicRouter::new(config.clone()));
    let tools = ToolRegistry::with_core_tools();
    let presenter = Box::new(HeadlessPresenter::default());

    let cwd = std::env::current_dir()?.display().to_string();
    let mut session = Session::start(store, provider, router, tools, presenter, config, &cwd)
        .context("starting session")?;

    session
        .run_turn(&prompt)
        .await
        .context("running agent turn")?;
    Ok(())
}
