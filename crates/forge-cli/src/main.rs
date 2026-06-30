//! The `forge` binary: parse arguments, load config, wire the subsystems behind their
//! traits, and drive one agent turn. This is the thin composition root (ADR-0002).

use std::io::IsTerminal;

use clap::Parser;

use forge_core::Session;
use forge_provider::{DispatchProvider, Provider};
use forge_store::Store;
use forge_types::TaskTier;

pub(crate) use cli::args::{
    AssayCmd, AssayFormat, BenchCmd, Cli, Command, ExportScope, FailOnSeverity, GitCmd,
    ImportSource, LatticeOp, LocalCmd, McpCmd, McpScopeArg, McpTransportArg, Mode, OutputFormat,
    PluginCmd, PluginMarketplaceCmd, ProviderCmd, SelfMcpAction, SkillCmd, SkillScope,
};
pub(crate) use cli::commands::assay::{assay_cmd, spawn_assay};
pub(crate) use cli::commands::git::{git_cmd, maybe_install_git_hook, write_active_model};
pub(crate) use cli::commands::import::import_cmd;
pub(crate) use cli::commands::lattice::lattice_cmd;
pub(crate) use cli::commands::local::{
    auth, local_cmd, maybe_autostart_local, needs_onboarding, prompt_line, provider_label, setup,
};
pub(crate) use cli::commands::mcp::{mcp_cmd, plugin_cmd};
pub(crate) use cli::commands::memory::memory_cmd;
pub(crate) use cli::commands::migrate::migrate_cmd;
pub(crate) use cli::commands::models::{
    benchmarks_cmd, build_provider_and_router, discover_catalog, load_cached_catalog, mesh_explain,
    models, save_catalog,
};
pub(crate) use cli::commands::provider::provider_cmd;
pub(crate) use cli::commands::replay::{
    open_store, replay_cmd, replay_rerun_cmd, resolve_resume_mode, resolve_session, sessions,
    ResumeMode,
};
pub(crate) use cli::commands::run::{
    build_session, chat, fmt_age, nl_cmd, run, session_title, DoneGuard,
};
pub(crate) use cli::commands::self_mcp::self_mcp_cmd;
pub(crate) use cli::commands::skill::{commands_cmd, skill_cmd};

mod assay_output;
mod balance;
mod bench;
mod benchmarks;
mod bridge_stats;
mod cli;
mod context_windows;
mod doctor;
mod image_input;
pub(crate) mod live_observer;
mod local;
mod mcp_agent;
mod mcp_serve;
mod remote;
mod replay;
mod update;
mod update_check;

/// Env var carrying the current subagent nesting depth across the process boundary (forge →
/// claude/codex → `forge mcp-serve`). mcp-serve advertises `spawn_agents` only while
/// `depth < max_depth`, and bumps it for any children it spawns (RFC subagent-orchestration 3c).
pub(crate) const FORGE_SUBAGENT_DEPTH_ENV: &str = "FORGE_SUBAGENT_DEPTH";

/// Where diagnostic logs go. On an interactive terminal we must NEVER write to stderr — the
/// inline TUI shares the screen, and a library log (e.g. genai dumping a 429 body via
/// `tracing::error!`) would shred the display. There, logs go to a file; otherwise stderr.
#[derive(Debug, PartialEq, Eq)]
enum LogTarget {
    Stderr,
    File,
}

fn log_target(interactive: bool) -> LogTarget {
    if interactive {
        LogTarget::File
    } else {
        LogTarget::Stderr
    }
}

/// Install the tracing subscriber. Interactive → a log file under `.forge/` (so nothing ever
/// leaks onto the TUI); non-interactive (pipe/CI) → stderr as before. Default level is `warn`
/// unless `RUST_LOG` overrides.
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    match log_target(std::io::stdout().is_terminal()) {
        LogTarget::Stderr => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
        }
        LogTarget::File => {
            let _ = std::fs::create_dir_all(".forge");
            match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(".forge/forge.log")
            {
                Ok(file) => tracing_subscriber::fmt()
                    .with_env_filter(filter)
                    .with_ansi(false)
                    .with_writer(move || file.try_clone().expect("clone forge.log handle"))
                    .init(),
                // Can't open the log file → stay silent rather than corrupt the TUI.
                Err(_) => tracing_subscriber::fmt()
                    .with_env_filter(EnvFilter::new("off"))
                    .init(),
            }
        }
    }
}

#[tokio::main]
async fn main() {
    init_tracing();

    let cli = Cli::parse();
    if let Err(e) = cli::dispatch::dispatch(cli.command).await {
        print_top_level_error(&e);
        std::process::exit(1);
    }
}

/// Print a top-level error as a readable one-liner (Display + a compact cause chain) instead of the
/// raw multi-line Debug anyhow dump, followed by a single next-step hint. Config/provider failures
/// point at `forge doctor`.
fn print_top_level_error(e: &anyhow::Error) {
    let mut msg = format!("{e}");
    let causes: Vec<String> = e.chain().skip(1).map(|c| c.to_string()).collect();
    if !causes.is_empty() {
        msg.push_str(&format!("  ({})", causes.join(": ")));
    }
    eprintln!("\x1b[31m✖ error:\x1b[0m {msg}");
    eprintln!("  → {}", error_next_step(e));
}

/// One-line next-step hint for a failed command. Provider/config/auth failures get the `forge
/// doctor` pointer; anything else gets a generic detail hint.
fn error_next_step(e: &anyhow::Error) -> &'static str {
    let text = e
        .chain()
        .map(|c| c.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let config_or_provider = [
        "api key",
        "provider",
        "config",
        "no usable model",
        "auth",
        "model",
    ]
    .iter()
    .any(|k| text.contains(k));
    if config_or_provider {
        "run `forge doctor` to check your provider keys + config"
    } else {
        "run `forge doctor` for a health check, or re-run with RUST_LOG=debug for detail"
    }
}
