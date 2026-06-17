//! The `forge` binary: parse arguments, load config, wire the subsystems behind their
//! traits, and drive one agent turn. This is the thin composition root (ADR-0002).

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::sync::Arc;

use forge_config::ClassifierKind;
use forge_core::{LlmRouter, Session};
use forge_mesh::{HeuristicRouter, Router};
use forge_provider::{DispatchProvider, MockProvider, Provider};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::{HeadlessPresenter, Presenter, TuiPresenter};
use forge_types::PermissionMode;
use forge_types::TaskTier;

mod mcp_serve;
mod replay;

/// Env var carrying the current subagent nesting depth across the process boundary (forge →
/// claude/codex → `forge mcp-serve`). mcp-serve advertises `spawn_agents` only while
/// `depth < max_depth`, and bumps it for any children it spawns (RFC subagent-orchestration 3c).
pub(crate) const FORGE_SUBAGENT_DEPTH_ENV: &str = "FORGE_SUBAGENT_DEPTH";

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
        /// Pin a specific model (e.g. `openai::gpt-4o`), bypassing mesh classification.
        #[arg(long)]
        model: Option<String>,
    },
    /// Start an interactive multi-turn chat session.
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
        /// Force plain line output instead of the interactive TUI.
        #[arg(long)]
        plain: bool,
        /// Pin a specific model (e.g. `openai::gpt-4o`), bypassing mesh classification.
        #[arg(long)]
        model: Option<String>,
    },
    /// List past sessions (newest first).
    Sessions,
    /// Replay a past session from the record: one id prints its turn-by-turn transcript
    /// (model, tokens, cost, time per turn); two ids diff their summaries.
    Replay {
        /// One session id to reconstruct, or two to diff (git-style prefixes accepted).
        ids: Vec<String>,
        /// Emit the transcript as machine-readable JSON instead of the human-readable format.
        /// Only valid with a single session id.
        #[arg(long)]
        json: bool,
    },
    /// List discovered slash commands + skills (project and user scope) with their descriptions.
    Commands,
    /// Show the auto-discovered model catalog and the mesh's best pick per tier.
    Models {
        /// Actively ping every discovered model and persist the result: clear healthy ones,
        /// bench the ones that rate-limit / fail auth (so the mesh routes around them).
        #[arg(long)]
        probe: bool,
        /// Clear all stale model benches (forget every rate-limited/unavailable mark) and exit.
        #[arg(long)]
        clear: bool,
    },
    /// Internal: run Forge's tool registry as an MCP server on stdio (spawned by the CLI
    /// bridge so claude/codex use Forge's tools under Forge's permission gate). Not for direct use.
    #[command(hide = true)]
    McpServe,
    /// Store a provider API key securely in the OS keyring (reads the key from stdin), or remove
    /// it with `--remove`.
    Auth {
        /// Provider: anthropic, openai, gemini, xai, deepseek, or openrouter.
        provider: String,
        /// Delete the stored key for this provider instead of setting one.
        #[arg(long)]
        remove: bool,
    },
    /// Interactive first-run setup: enable providers (enter API keys) and declare which
    /// subscription plan backs each installed CLI bridge, so the mesh knows your usage headroom.
    Init,
    /// Connect to the configured MCP servers and show their status (or one server's tools, or
    /// import a Claude-Code `.mcp.json`).
    Mcp {
        #[command(subcommand)]
        cmd: Option<McpCmd>,
    },
    /// Lattice — native code-intelligence graph (tree-sitter + SQLite). Build it, then query.
    Lattice {
        #[command(subcommand)]
        op: LatticeOp,
    },
    /// Import commands + skills from another AI CLI into Forge's scopes.
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
}

#[derive(Subcommand)]
enum ImportSource {
    /// Copy `~/.claude/commands/*.md` and `~/.claude/skills/*/` into Forge (user scope by
    /// default). Existing definitions are kept; malformed files are skipped.
    Claude {
        /// Import into the project (`./.forge`) instead of the user config dir.
        #[arg(long)]
        project: bool,
    },
    /// Copy Codex CLI custom prompts (`~/.codex/prompts/*.md`) into Forge as commands (user
    /// scope by default). Existing definitions are kept; malformed files are skipped.
    Codex {
        /// Import into the project (`./.forge`) instead of the user config dir.
        #[arg(long)]
        project: bool,
    },
}

#[derive(Subcommand)]
enum LatticeOp {
    /// (Re)index the working directory into the graph — incremental by file content hash.
    Update {
        /// Root to index (default: current directory).
        path: Option<String>,
    },
    /// Find symbols by name (case-insensitive); prints kind, location, and signature.
    Query {
        /// Symbol name or fragment.
        query: String,
    },
    /// Blast radius: everything that references a symbol (transitively) — "what breaks if I change X".
    Impact {
        /// Symbol name.
        symbol: String,
    },
    /// Shortest call/reference chain of symbol names from A to B.
    Path {
        /// Source symbol name.
        from: String,
        /// Target symbol name.
        to: String,
    },
    /// Decision provenance: who last changed a symbol's definition, when, and in which commit.
    Why {
        /// Symbol name.
        symbol: String,
    },
    /// Compute + store embeddings for nodes lacking them (semantic retrieval). Uses the
    /// `[lattice.embeddings]` backend (ollama by default). Runs automatically on startup when
    /// embeddings are enabled — this is the manual trigger / one-off.
    Embed,
    /// Show index counts (files, symbols, edges, refs).
    Status,
}

#[derive(Subcommand)]
enum McpCmd {
    /// Show the full discovered tool list for one connected server.
    Tools {
        /// Server name (as declared in `.forge/mcp.toml`).
        server: String,
    },
    /// Import a Claude-Code-style `.mcp.json` into `.forge/mcp.toml` (secrets are NOT copied).
    Import {
        /// Path to the `.mcp.json` (default: `./.mcp.json`).
        path: Option<String>,
    },
    /// Obtain OAuth tokens for an OAuth-protected HTTP MCP server (browser-based flow).
    Login {
        /// Server name (as declared in `.forge/mcp.toml`).
        server: String,
    },
    /// Remove stored OAuth tokens for a server (`forge mcp logout <server>`).
    Logout {
        /// Server name (as declared in `.forge/mcp.toml`).
        server: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    #[value(alias = "ask")]
    Default,
    #[value(alias = "auto-edit", alias = "autoedit")]
    AcceptEdits,
    #[value(alias = "full")]
    Bypass,
    #[value(alias = "read-only", alias = "readonly")]
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

/// Keep the command palette in sync with the `/command` token at the cursor (input end): open +
/// filter when one is present anywhere on the line, close when not (`//` escape yields no token).
fn sync_palette_to_slash_token(app: &mut forge_tui::App) {
    match forge_tui::slash_token_at(&app.input, app.input.len()) {
        Some(tok) if app.palette.open => {
            app.palette.query = tok.name;
            app.palette.clamp();
        }
        Some(tok) => app.palette.open_with(&tok.name),
        None => app.palette.close(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            prompt,
            mock,
            mode,
            tui,
            resume,
            model,
        } => run(prompt.join(" "), mock, mode, tui, resume, model).await,
        Command::Chat {
            mock,
            mode,
            resume,
            plain,
            model,
        } => chat(mock, mode, resume, plain, model).await,
        Command::Sessions => sessions(),
        Command::Replay { ids, json } => replay_cmd(&ids, json),
        Command::Commands => commands_cmd(),
        Command::Models { probe, clear } => models(probe, clear).await,
        Command::Auth { provider, remove } => auth(&provider, remove),
        Command::Init => init(),
        Command::Mcp { cmd } => mcp_cmd(cmd).await,
        Command::McpServe => mcp_serve::run().await,
        Command::Lattice { op } => lattice_cmd(op).await,
        Command::Import { source } => import_cmd(source),
    }
}

/// Tally of an import run: copied vs. already-present, for commands and skills.
#[derive(Debug, Default, PartialEq, Eq)]
struct ImportCounts {
    copied_commands: usize,
    skipped_commands: usize,
    copied_skills: usize,
    skipped_skills: usize,
    copied_agents: usize,
    skipped_agents: usize,
}

/// `forge import <source> [--project]` — copy another AI CLI's commands/skills/agents into a
/// Forge scope, reusing the CC-compatible readers to validate before copying. Claude imports
/// commands + skills + agents; Codex imports its prompts as commands.
fn import_cmd(source: ImportSource) -> Result<()> {
    let (label, project, home, commands_sub, skills_sub, agents_sub) = match source {
        ImportSource::Claude { project } => (
            "claude",
            project,
            forge_config::claude_dir().context("no home directory — cannot locate ~/.claude")?,
            "commands",
            Some("skills"),
            Some("agents"),
        ),
        ImportSource::Codex { project } => (
            "codex",
            project,
            forge_config::codex_dir().context("no home directory — cannot locate ~/.codex")?,
            "prompts",
            None,
            None,
        ),
    };
    if !home.exists() {
        println!("nothing to import: {} does not exist", home.display());
        return Ok(());
    }

    let (cmd_dst, skill_dst, agent_dst) = if project {
        (
            std::path::PathBuf::from("./.forge/commands"),
            std::path::PathBuf::from("./.forge/skills"),
            std::path::PathBuf::from("./.forge/agents"),
        )
    } else {
        let base = forge_config::config_dir().context("no config directory on this platform")?;
        (
            base.join("commands"),
            base.join("skills"),
            base.join("agents"),
        )
    };

    // Validate assets with the real catalog readers (malformed files are skipped + warned).
    let sources = forge_skills::Sources {
        commands: vec![forge_skills::ScopedDir {
            scope: forge_skills::Scope::User,
            path: home.join(commands_sub),
        }],
        skills: skills_sub
            .map(|s| {
                vec![forge_skills::ScopedDir {
                    scope: forge_skills::Scope::User,
                    path: home.join(s),
                }]
            })
            .unwrap_or_default(),
    };
    let cat = forge_skills::Catalog::load(&sources);
    let mut counts = copy_catalog_assets(&cat, &cmd_dst, &skill_dst);

    // Agents: CC and Forge use the same .md front-matter format — direct file copy.
    if let Some(asub) = agents_sub {
        let agent_src = home.join(asub);
        if agent_src.is_dir() {
            count_copy_md_files(&agent_src, &agent_dst, &mut counts);
        }
    }

    let scope = if project { "./.forge" } else { "the user config" };
    println!(
        "✓ imported {} command(s) + {} skill(s) + {} agent(s) from {label} into {scope} \
         ({} command(s), {} skill(s), {} agent(s) already present, skipped)",
        counts.copied_commands,
        counts.copied_skills,
        counts.copied_agents,
        counts.skipped_commands,
        counts.skipped_skills,
        counts.skipped_agents,
    );
    for w in cat.warnings() {
        eprintln!("skipped (malformed): {w}");
    }
    Ok(())
}

/// Copy `*.md` files from `src` into `dst`, skipping any that already exist. Updates `counts`.
fn count_copy_md_files(
    src: &std::path::Path,
    dst: &std::path::Path,
    counts: &mut ImportCounts,
) {
    std::fs::create_dir_all(dst).ok();
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let from = entry.path();
        if from.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(fname) = from.file_name() else {
            continue;
        };
        let to = dst.join(fname);
        if to.exists() {
            counts.skipped_agents += 1;
        } else if std::fs::copy(&from, &to).is_ok() {
            counts.copied_agents += 1;
        }
    }
}

/// Copy a loaded catalog's command files + skill directories into the target scope, keeping any
/// definition already present. Pure over the filesystem so it's unit-testable with temp dirs.
fn copy_catalog_assets(
    cat: &forge_skills::Catalog,
    cmd_dst: &std::path::Path,
    skill_dst: &std::path::Path,
) -> ImportCounts {
    let mut counts = ImportCounts::default();
    std::fs::create_dir_all(cmd_dst).ok();
    for cmd in cat.all_commands() {
        let Some(fname) = cmd.path.file_name() else {
            continue;
        };
        let dest = cmd_dst.join(fname);
        if dest.exists() {
            counts.skipped_commands += 1;
            continue;
        }
        if std::fs::copy(&cmd.path, &dest).is_ok() {
            counts.copied_commands += 1;
        }
    }

    std::fs::create_dir_all(skill_dst).ok();
    for skill in cat.all_skills() {
        let dest = skill_dst.join(&skill.name);
        if dest.exists() {
            counts.skipped_skills += 1;
            continue;
        }
        if copy_dir(&skill.dir, &dest).is_ok() {
            counts.copied_skills += 1;
        }
    }
    counts
}

/// Recursively copy a directory tree (used to import a skill's SKILL.md + its resource files).
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// `forge lattice <op>` — build / query / inspect the code-intelligence graph.
async fn lattice_cmd(op: LatticeOp) -> Result<()> {
    let config = forge_config::load().context("loading configuration")?;
    if !config.lattice.enabled {
        println!("lattice is disabled (set [lattice] enabled = true)");
        return Ok(());
    }
    let store = std::sync::Arc::new(open_store()?);
    let cwd = std::env::current_dir()?;
    match op {
        LatticeOp::Embed => {
            let emb = &config.lattice.embeddings;
            if !emb.enabled {
                println!("embeddings are off (set [lattice.embeddings] enabled = true)");
                return Ok(());
            }
            let lat = forge_index::Lattice::new(store, &cwd);
            lat.update().map_err(|e| anyhow::anyhow!("{e}"))?;
            let Some((embedder, label)) = forge_provider::select_embedder(emb) else {
                println!(
                    "no embedding backend available — set [lattice.embeddings] backend + a provider key, or run ollama"
                );
                return Ok(());
            };
            let n = lat
                .embed_pending(embedder.as_ref(), 64)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "⌬ embedded {n} node(s) via {label}; {} total",
                lat.embedding_count().map_err(|e| anyhow::anyhow!("{e}"))?
            );
        }
        LatticeOp::Update { path } => {
            let root = path.map(std::path::PathBuf::from).unwrap_or(cwd);
            let lat = forge_index::Lattice::new(store, &root);
            let stats = lat.update().map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "⌬ lattice updated — {} file(s) indexed, {} skipped, {} symbol(s)",
                stats.files_indexed, stats.files_skipped, stats.symbols
            );
        }
        LatticeOp::Query { query } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            let hits = lat.query(&query, 20).map_err(|e| anyhow::anyhow!("{e}"))?;
            if hits.is_empty() {
                println!("no symbols match '{query}' — run `forge lattice update` first?");
            } else {
                for h in hits {
                    let sig = h.signature.unwrap_or_else(|| h.name.clone());
                    println!("{:<8} {}:{}  {}", h.kind, h.rel_path, h.line, sig);
                }
            }
        }
        LatticeOp::Impact { symbol } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            let blast = lat.impact(&symbol, 4).map_err(|e| anyhow::anyhow!("{e}"))?;
            if blast.roots.is_empty() {
                println!("no symbol named '{symbol}' — run `forge lattice update` first?");
            } else if blast.dependents.is_empty() {
                println!("⌬ {symbol}: no known references (leaf, or callers not yet indexed)");
            } else {
                println!(
                    "⌬ impact · {symbol} — {} site(s) across {} file(s)",
                    blast.total_sites,
                    blast.files.len()
                );
                for d in &blast.dependents {
                    println!("  ← {:<8} {} {}:{}", d.kind, d.name, d.rel_path, d.line);
                }
            }
        }
        LatticeOp::Path { from, to } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            match lat
                .path(&from, &to, 8)
                .map_err(|e| anyhow::anyhow!("{e}"))?
            {
                Some(chain) => println!("⌬ path · {}", chain.join(" → ")),
                None => println!("no reference path from '{from}' to '{to}' within 8 hops"),
            }
        }
        LatticeOp::Why { symbol } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            match lat.why(&symbol).map_err(|e| anyhow::anyhow!("{e}"))? {
                Some(p) => println!(
                    "⌬ why · {} ({}:{})\n  {} · {} · {} · {}",
                    p.name, p.rel_path, p.line, p.author, p.date, p.commit, p.subject
                ),
                None => println!(
                    "no provenance for '{symbol}' — unknown symbol, or the tree isn't under git"
                ),
            }
        }
        LatticeOp::Status => {
            let lat = forge_index::Lattice::new(store, &cwd);
            let s = lat.status().map_err(|e| anyhow::anyhow!("{e}"))?;
            let embedded = lat.embedding_count().map_err(|e| anyhow::anyhow!("{e}"))?;
            let emb = if config.lattice.embeddings.enabled {
                format!("{embedded} embedded")
            } else {
                "embeddings off".to_string()
            };
            println!(
                "⌬ lattice — {} file(s), {} symbol(s), {} edge(s), {} ref(s) · {} languages · {emb}",
                s.files,
                s.nodes,
                s.edges,
                s.refs,
                forge_index::supported_languages().len()
            );
        }
    }
    Ok(())
}

fn auth(provider: &str, remove: bool) -> Result<()> {
    let known_provider = forge_config::known_key_providers().any(|p| p == provider);
    let known_search = forge_config::known_search_providers().any(|p| p == provider);
    if !known_provider && !known_search {
        let mut known: Vec<_> = forge_config::known_key_providers().collect();
        known.extend(forge_config::known_search_providers());
        anyhow::bail!(
            "unknown provider '{provider}' — known providers are: {}",
            known.join(", ")
        );
    }
    if remove {
        let removed = forge_config::remove_api_key(provider)
            .with_context(|| format!("removing {provider} key from the OS keyring"))?;
        if removed {
            println!("removed {provider} key from the OS keyring");
        } else {
            println!("no {provider} key was stored — nothing to remove");
        }
        return Ok(());
    }
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        print!("paste {provider} API key (input hidden is not supported; press enter): ");
        std::io::Write::flush(&mut std::io::stdout()).ok();
    }
    let mut key = String::new();
    std::io::stdin()
        .read_line(&mut key)
        .context("reading key from stdin")?;
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("no key provided");
    }
    forge_config::store_api_key(provider, key).with_context(|| {
        format!("storing {provider} key (is an OS keyring / secret service available?)")
    })?;
    println!("stored {provider} key in the OS keyring");
    Ok(())
}

/// A human label + free/paid hint for a key-based provider, shown in `forge init`.
fn provider_label(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "Anthropic (Claude API) — paid",
        "openai" => "OpenAI (GPT API) — paid",
        "gemini" => "Google Gemini — free tier + paid",
        "xai" => "xAI (Grok) — paid",
        "deepseek" => "DeepSeek — paid",
        "openrouter" => "OpenRouter (gateway, many models) — paid + some :free",
        "groq" => "Groq — free tier (fast)",
        "opencode_go" => "OpenCode Zen — free curated coding models",
        "github_copilot" => "GitHub Models — free inference",
        "mimo" => "Xiaomi MiMo — free",
        "minimax" => "MiniMax — free tier",
        "cerebras" => "Cerebras — free tier (fast)",
        _ => "provider",
    }
}

/// The subscription plans a CLI bridge can be backed by: `(human label, stored slug)`. Captured
/// by `forge init` so the mesh knows the usage headroom (quota-aware routing, L3). The exact
/// quota numbers aren't asserted here — only which plan the user holds.
fn bridge_plans(kind: forge_provider::CliKind) -> &'static [(&'static str, &'static str)] {
    match kind {
        forge_provider::CliKind::ClaudeCode => &[
            ("Free", "free"),
            ("Pro", "pro"),
            ("Max 5×", "max-5x"),
            ("Max 20×", "max-20x"),
            ("API credits / unsure", "unknown"),
        ],
        forge_provider::CliKind::Codex => &[
            ("Plus", "plus"),
            ("Pro", "pro"),
            ("Team", "team"),
            ("Enterprise", "enterprise"),
            ("API credits / unsure", "unknown"),
        ],
    }
}

/// Whether the user looks un-onboarded: no provider key, no installed bridge, and no saved
/// config. Pure so it's testable; the caller adds the tty check before auto-launching `init`.
fn needs_onboarding(has_any_key: bool, any_bridge: bool, config_exists: bool) -> bool {
    !has_any_key && !any_bridge && !config_exists
}

/// Read one trimmed line from stdin with a prompt (no echo suppression — same as `auth`).
fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("reading stdin")?;
    Ok(line.trim().to_string())
}

/// `forge init`: interactive first-run setup. Walks the key-based providers (offering to store a
/// key for each), then each installed CLI bridge (asking which subscription plan backs it), and
/// writes the plans to the user config. Keys go to the OS keyring, never the config (ADR-0007).
fn init() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("`forge init` is interactive — run it in a terminal");
    }
    let outcome =
        forge_tui::init_wizard::run(wizard_input()).context("running the setup wizard")?;
    if outcome.cancelled {
        println!("Setup cancelled — run `forge init` anytime.");
        return Ok(());
    }
    let path = apply_wizard_outcome(&outcome)?;
    println!("✓ Setup saved to {}", path.display());
    println!(
        "  {} key(s) stored · {} bridge plan(s) recorded.",
        outcome.keys.len(),
        outcome.plans.len()
    );
    println!("  The mesh routes across these by task tier + cost. Try `forge models`.");
    Ok(())
}

/// Build the config-wizard inputs from what Forge knows: key-based model providers, search-API
/// providers (for `web_search`), and every INSTALLED CLI bridge (with its subscription plans).
/// Shared by `forge init` and the in-chat `/config` command.
fn wizard_input() -> forge_tui::WizardInput {
    let providers = forge_config::known_key_providers()
        .map(|p| forge_tui::ProviderItem {
            id: p.to_string(),
            label: provider_label(p).to_string(),
            had_key: forge_config::has_api_key(p),
        })
        .collect();
    let search = forge_config::known_search_providers()
        .map(|p| forge_tui::ProviderItem {
            id: p.to_string(),
            label: forge_config::search_provider_label(p).to_string(),
            had_key: forge_config::has_search_key(p),
        })
        .collect();
    let bridges = forge_provider::CliKind::all()
        .into_iter()
        .filter(|k| k.available())
        .map(|k| forge_tui::BridgeItem {
            prefix: k.prefix().to_string(),
            plans: bridge_plans(k)
                .iter()
                .map(|(l, s)| (l.to_string(), s.to_string()))
                .collect(),
        })
        .collect();
    forge_tui::WizardInput {
        providers,
        search,
        bridges,
    }
}

/// Persist a wizard outcome: keys → OS keyring (ADR-0007), plans → user config; then inject
/// both into this process's env so a running session picks them up immediately. Returns the
/// config path. Shared by `forge init` and `/config`.
fn apply_wizard_outcome(outcome: &forge_tui::WizardOutcome) -> Result<std::path::PathBuf> {
    for (provider, key) in &outcome.keys {
        forge_config::store_api_key(provider, key)
            .with_context(|| format!("storing {provider} key"))?;
    }
    let path = forge_config::write_subscriptions(&outcome.plans).context("writing config")?;
    forge_config::inject_provider_keys();
    forge_config::inject_search_keys();
    Ok(path)
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

/// `forge replay <id>` reconstructs a session's transcript; `forge replay <a> <b>` diffs two.
fn replay_cmd(ids: &[String], json: bool) -> Result<()> {
    let store = open_store()?;
    let resolve = |prefix: &str| -> Result<String> {
        let mut matches = store
            .matching_session_ids(prefix)
            .with_context(|| format!("resolving session {prefix}"))?;
        match matches.len() {
            0 => anyhow::bail!("no session matches '{prefix}' — see `forge sessions`"),
            1 => Ok(matches.remove(0)),
            n => anyhow::bail!("'{prefix}' is ambiguous ({n} sessions) — use more characters"),
        }
    };
    match ids {
        [one] => {
            let id = resolve(one)?;
            let entries = store.load_replay(&id).context("loading replay")?;
            if entries.is_empty() {
                if json {
                    println!("{{\"session_id\":\"{}\",\"turns\":[]}}", &id[..id.len().min(8)]);
                } else {
                    println!("session {} has no messages", &id[..id.len().min(8)]);
                }
                return Ok(());
            }
            if json {
                println!("{}", replay::render_json(&id, &entries));
            } else {
                print!(
                    "{}",
                    replay::render_transcript(&id[..id.len().min(8)], &entries)
                );
            }
        }
        [a, b] => {
            if json {
                anyhow::bail!("--json is only valid with a single session id");
            }
            let (ida, idb) = (resolve(a)?, resolve(b)?);
            let ea = store.load_replay(&ida).context("loading replay a")?;
            let eb = store.load_replay(&idb).context("loading replay b")?;
            let d = replay::diff(&ea, &eb);
            let fa8 = &ida[..ida.len().min(8)];
            let fb8 = &idb[..idb.len().min(8)];
            print!("{}", replay::render_diff(fa8, fb8, &d));
            print!("\n{}", replay::render_turn_diff(fa8, fb8, &ea, &eb));
        }
        _ => anyhow::bail!("usage: forge replay <id> [<id-to-diff-against>]"),
    }
    Ok(())
}

/// `forge commands` — list discovered slash commands + skills with scope and collision markers.
fn commands_cmd() -> Result<()> {
    let catalog = forge_skills::Catalog::load(&forge_config::command_sources());
    let entries = catalog.entries();
    if entries.is_empty() {
        println!(
            "no commands or skills found — add markdown to ./.forge/commands, ./.forge/skills,\n\
             or the same dirs under your user config (see `forge commands` docs)"
        );
    } else {
        for e in &entries {
            let kind = if e.is_skill { "skill  " } else { "command" };
            let shadow = if e.shadows {
                "  (shadows lower scope)"
            } else {
                ""
            };
            println!(
                "/{:<16} {kind}  [{}]  {}{shadow}",
                e.name,
                e.scope.label(),
                e.description
            );
        }
    }
    for w in catalog.warnings() {
        eprintln!("warning: {w}");
    }
    Ok(())
}

/// Resolve health-aware tier models from the discovery catalog, then spawn the Assay task (like
/// `spawn_turn`): the crew runs in the background while the spinner ticks, emits its report to the
/// TUI, and — when `cleanup` — runs a permission-gated, undoable Refine fix turn. Returns the task
/// handle (so Esc can interrupt it), or `None` if it couldn't start (no source / no live models).
#[allow(clippy::too_many_arguments)]
async fn spawn_assay(
    cleanup: bool,
    lenses: Vec<forge_types::FindingCategory>,
    session: &Arc<tokio::sync::Mutex<Session>>,
    done_tx: &std::sync::mpsc::Sender<u64>,
    gen: u64,
    app: &mut forge_tui::App,
    busy: &mut bool,
    busy_since: &mut std::time::Instant,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let source = bundle_source(Path::new("."), 200_000);
    if source.trim().is_empty() {
        app.note("assay: no analyzable source files under the working directory");
        return Ok(None);
    }
    let config = forge_config::load().unwrap_or_default();
    let pricing = forge_mesh::pricing::Pricing::from_config(&config);
    let store = open_store()?;
    let cat = discover_catalog(&config).await;
    if cat.is_empty() {
        app.note("assay: no models available — `forge auth <provider>` or run ollama");
        return Ok(None);
    }
    // Route critics around rate-limited / benched models, like the agent loop does.
    let benched = store.current_benched().unwrap_or_default();
    // Build a CHAIN per tier (ranked, health-filtered): the crew tries them in order and fails
    // over when one rate-limits, instead of giving up on a single dead model.
    let chain = |tier| {
        let mut models: Vec<String> = cat
            .ranked_for(tier, &pricing, 8)
            .into_iter()
            .filter(|m| !benched.is_benched(m))
            .collect();
        if models.is_empty() {
            if let Some(m) = config.model_for(tier) {
                models.push(m.to_string());
            }
        }
        models
    };
    let (trivial, complex) = (chain(TaskTier::Trivial), chain(TaskTier::Complex));
    if trivial.is_empty() && complex.is_empty() {
        app.note(
            "assay: every model is rate-limited/benched — try /mode or `forge models --probe`",
        );
        return Ok(None);
    }
    let models = forge_core::assay::TierModels { trivial, complex };

    app.submit_user(if cleanup {
        "/assay → full cleanup (Refine)"
    } else {
        "/assay → analysis"
    });
    app.done = false;
    app.tick = 0;
    *busy = true;
    *busy_since = std::time::Instant::now();
    let s = session.clone();
    let dt = done_tx.clone();
    let src: Arc<str> = Arc::from(source.as_str());
    Ok(Some(tokio::spawn(async move {
        let _done = DoneGuard(dt, gen);
        let mut sess = s.lock().await;
        if let Err(e) = sess.assay(src, models, lenses, cleanup).await {
            sess.notify_error(&format!("assay failed: {e}"));
        }
    })))
}

/// Concatenate the analyzable source under `root` (capped) with `// FILE:` headers, for the crew
/// prompt. Skips VCS/build/vendor dirs; deterministic order. A single file is bundled directly.
fn bundle_source(root: &Path, max_bytes: usize) -> String {
    fn is_skip_dir(name: &str) -> bool {
        matches!(
            name,
            ".git" | "target" | ".forge" | "node_modules" | "graphify-out" | ".idea" | ".vscode"
        )
    }
    fn is_source(ext: &str) -> bool {
        matches!(
            ext,
            "rs" | "toml"
                | "md"
                | "py"
                | "js"
                | "ts"
                | "tsx"
                | "go"
                | "java"
                | "c"
                | "cpp"
                | "h"
                | "hpp"
                | "sh"
                | "yaml"
                | "yml"
                | "json"
                | "sql"
        )
    }
    fn append(out: &mut String, path: &Path) {
        if let Ok(content) = std::fs::read_to_string(path) {
            out.push_str(&format!("// FILE: {}\n{content}\n\n", path.display()));
        }
    }

    let mut out = String::new();
    if root.is_file() {
        append(&mut out, root);
        out.truncate(floor_char_boundary(&out, max_bytes));
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= max_bytes {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for p in paths {
            if out.len() >= max_bytes {
                break;
            }
            if p.is_dir() {
                if !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(is_skip_dir)
                    .unwrap_or(false)
                {
                    stack.push(p);
                }
            } else if p
                .extension()
                .and_then(|e| e.to_str())
                .map(is_source)
                .unwrap_or(false)
            {
                append(&mut out, &p);
            }
        }
    }
    out.truncate(floor_char_boundary(&out, max_bytes));
    out
}

/// Largest index ≤ `max` that is a char boundary (so truncation never splits a UTF-8 char).
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Construct the model backend + router from config. Shared by interactive sessions and the
/// `mcp-serve` subagent path (RFC subagent-orchestration Phase 3), so both route identically.
pub(crate) fn build_provider_and_router(
    config: &forge_config::Config,
    mock: bool,
    pin: Option<String>,
    catalog: Option<forge_mesh::ModelCatalog>,
) -> (Arc<dyn Provider>, Arc<dyn Router>) {
    let provider: Arc<dyn Provider> = if mock {
        Arc::new(MockProvider)
    } else {
        // Routes API models to genai and `claude-cli::`/`codex-cli::` to the subscription CLI
        // bridge. `harness` mode runs the bridge's tools through Forge's MCP server (RFC Phase 2).
        let harness = config.mesh.bridge_mode == forge_config::BridgeMode::Harness;
        Arc::new(DispatchProvider::new(harness))
    };
    let mut heuristic = HeuristicRouter::new(config.clone()).with_pin(pin);
    if let Some(cat) = catalog {
        heuristic = heuristic.with_catalog(cat);
    }
    let router: Arc<dyn Router> = if config.mesh.classifier == ClassifierKind::Llm {
        // Opt-in cheap-LLM classifier: a separate (stateless) provider labels the tier, then
        // the heuristic router does the cost-aware selection; any failure falls back to it.
        let classifier_model = config
            .mesh
            .classifier_model
            .clone()
            .or_else(|| config.model_for(TaskTier::Trivial).map(String::from))
            .unwrap_or_default();
        let classify_provider: Arc<dyn Provider> = if mock {
            Arc::new(MockProvider)
        } else {
            Arc::new(DispatchProvider::new(false)) // classification needs no tools/harness
        };
        Arc::new(LlmRouter::new(
            classify_provider,
            classifier_model,
            heuristic,
        ))
    } else {
        Arc::new(heuristic)
    };
    (provider, router)
}

/// Build a session around a caller-provided presenter, wiring all subsystems.
/// Discover the models the user can actually use, as a [`forge_mesh::ModelCatalog`] for
/// auto-discovery routing: query each provider that has a key (plus keyless local `ollama`) for
/// its model list, with a short per-provider timeout, and skip any that error. Cheap providers
/// usually number 1–3, so this runs sequentially at session start (cached for the process).
async fn discover_catalog(config: &forge_config::Config) -> forge_mesh::ModelCatalog {
    use std::time::Duration;
    let mut models = Vec::new();
    // Keyless local first, then every key-holding provider.
    let mut providers = vec!["ollama".to_string()];
    providers.extend(
        forge_config::known_key_providers()
            .filter(|p| forge_config::has_api_key(p))
            .map(str::to_string),
    );
    for p in providers {
        match tokio::time::timeout(Duration::from_secs(4), forge_provider::list_models(&p)).await {
            Ok(Ok(list)) => models.extend(list),
            Ok(Err(e)) => tracing::debug!("model discovery skipped {p}: {e}"),
            Err(_) => tracing::debug!("model discovery timed out for {p}"),
        }
    }
    // Always-available subscription bridges (claude-cli/codex-cli) if their CLI is installed.
    // They don't rate-limit like the free API tiers, so the mesh can rely on them — and being
    // $0 subscriptions they rank first (prefer_subscription), so routing reaches a working model
    // instead of erroring out when metered providers are throttled. Each installed bridge
    // contributes its bare default id PLUS one id per model alias (config override, else the
    // bridge's built-in defaults) so the mesh can size each turn (haiku/mini ↔ opus) instead of
    // seeing a single model. A stale alias just benches itself via failover — never a hard error.
    for k in forge_provider::CliKind::all()
        .into_iter()
        .filter(|k| k.available())
    {
        let prefix = k.prefix();
        models.push(k.default_model_id());
        match config.mesh.bridge_models.get(prefix) {
            Some(custom) if !custom.is_empty() => {
                models.extend(custom.iter().map(|m| format!("{prefix}::{m}")));
            }
            _ => models.extend(k.default_models().iter().map(|m| format!("{prefix}::{m}"))),
        }
    }
    // Dedup while preserving discovery order (a provider could list the same id twice).
    let mut seen = std::collections::HashSet::new();
    models.retain(|m| seen.insert(m.clone()));
    // Drop any model/provider the user disabled (`[mesh] disabled`), so the mesh never routes to
    // or fails over onto it (known-issues.md: disable a flaky model without deleting its key).
    models.retain(|m| !forge_config::is_model_disabled(m, &config.mesh.disabled));
    forge_mesh::ModelCatalog::new(models)
}

/// `forge models [--probe]`: discover the usable models + show the mesh's capability-ranked pick
/// per tier. With `--probe`, also ping each model and persist health (the user-driven rescan).
async fn models(probe: bool, clear: bool) -> Result<()> {
    if clear {
        let store = open_store()?;
        let n = store
            .clear_all_model_health()
            .context("clearing model benches")?;
        println!("cleared {n} model bench(es) — the mesh will reconsider every model");
        return Ok(());
    }
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();
    let cat = discover_catalog(&config).await;
    if cat.is_empty() {
        println!(
            "no models discovered — set a provider key (`forge auth <provider>`) or run ollama"
        );
        return Ok(());
    }
    let store = open_store()?;

    if probe {
        probe_models(&cat, &config, &store).await?;
        println!();
    }

    let pricing = forge_mesh::pricing::Pricing::from_config(&config);
    let benched = store.current_benched().unwrap_or_default();
    let s = cat.stats(&pricing);
    println!(
        "{} models · {} frontier · {} free · {} subscription · {} paid · {} providers\n",
        s.total, s.frontier, s.free, s.subscription, s.paid, s.providers
    );
    for g in cat.by_provider(&pricing) {
        println!("{} ({} models)", g.provider, g.total());
        for m in &g.models {
            let name = if m.name.is_empty() {
                "(default)"
            } else {
                m.name.as_str()
            };
            let mut tags: Vec<String> = Vec::new();
            if m.subscription {
                tags.push("subscription".into());
            }
            if m.frontier {
                tags.push("frontier".into());
            }
            if m.free {
                tags.push("free".into());
            }
            if m.cost > f64::EPSILON {
                tags.push(format!("paid ~${:.4}/turn", m.cost));
            } else if m.paid {
                tags.push("paid".into());
            }
            if benched.is_benched(&m.id) {
                tags.push("benched".into());
            }
            println!("  {name:<30} {}", tags.join(" · "));
        }
    }
    println!("\nmesh auto-pick per tier:");
    for tier in [TaskTier::Trivial, TaskTier::Standard, TaskTier::Complex] {
        // Mirror routing: skip benched models so the shown pick is the one the mesh would
        // actually use right now (model-health-failover).
        let pick = cat
            .ranked_for(tier, &pricing, 5)
            .into_iter()
            .find(|m| !benched.is_benched(m))
            .unwrap_or_else(|| "—".into());
        println!("  {:<9} {pick}", tier.as_str());
    }
    if !probe {
        println!("\ntip: `forge models --probe` pings each model and benches the dead ones.");
    }
    Ok(())
}

/// Ping every discovered model with a 1-token request; clear the healthy ones and bench the
/// ones that rate-limit / fail auth / are down, so the mesh routes around them.
async fn probe_models(
    cat: &forge_mesh::ModelCatalog,
    config: &forge_config::Config,
    store: &Store,
) -> Result<()> {
    use std::time::Duration;
    let harness = config.mesh.bridge_mode == forge_config::BridgeMode::Harness;
    let provider = DispatchProvider::new(harness);
    let default_cooldown = Duration::from_secs(config.mesh.failover_cooldown_secs);
    let ping = [forge_types::Message::user("ping")];
    let mut sink = |_: forge_provider::StreamEvent| {};

    println!("probing {} models…", cat.models().len());
    for m in cat.models() {
        let res = tokio::time::timeout(
            Duration::from_secs(20),
            provider.complete(m, &ping, &[], &mut sink),
        )
        .await;
        match res {
            Ok(Ok(_)) => {
                store.clear_model_health(m).ok();
                println!("  ✓ {m}");
            }
            Ok(Err(e)) if e.is_retryable() => {
                let cooldown = e.cooldown(default_cooldown);
                store.bench_for(m, cooldown, e.reason()).ok();
                println!("  ✗ {m} — {} (benched {}s)", e.reason(), cooldown.as_secs());
            }
            Ok(Err(e)) => {
                // Non-retryable (e.g. the ping payload upset the model) → don't bench it.
                println!("  ? {m} — {} (not benched)", e.reason());
            }
            Err(_) => {
                store.bench_for(m, default_cooldown, "probe timeout").ok();
                println!(
                    "  ✗ {m} — timeout (benched {}s)",
                    default_cooldown.as_secs()
                );
            }
        }
    }
    Ok(())
}

/// `forge mcp [tools <server> | import [path]]` — connect to the configured MCP servers and show
/// their status, list one server's tools, or import servers from your installed AI CLIs.
async fn mcp_cmd(cmd: Option<McpCmd>) -> Result<()> {
    // Import / Login / Logout need no connection. Resolve to the listing path otherwise.
    let tools_server = match cmd {
        Some(McpCmd::Import { path }) => return mcp_import(path),
        Some(McpCmd::Login { server }) => return mcp_login(&server).await,
        Some(McpCmd::Logout { server }) => return mcp_logout(&server),
        Some(McpCmd::Tools { server }) => Some(server),
        None => None,
    };

    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();
    if let Err(e) = config.mcp.validate() {
        anyhow::bail!("{e}");
    }
    if config.mcp.active_servers().next().is_none() {
        println!("no MCP servers configured. Declare them in .forge/mcp.toml, or run `forge mcp import`.");
        return Ok(());
    }

    let manager = forge_mcp::McpManager::connect_all(&config.mcp).await;
    match tools_server {
        Some(server) => {
            let tools = manager.tool_lines(&server);
            if tools.is_empty() {
                println!("no tools for server '{server}' (not connected, or it exposes none)");
            } else {
                println!("{} tool(s) on '{server}':", tools.len());
                for (name, desc) in tools {
                    println!("  {name} — {desc}");
                }
            }
        }
        None => {
            let lines = manager.status_lines();
            println!("MCP servers ({} configured)", lines.len());
            for s in &lines {
                let detail = s
                    .detail
                    .as_deref()
                    .map(|d| format!("  {d}"))
                    .unwrap_or_default();
                println!(
                    "  {:<12} {:<13} {:<6} {} tools · {} resources · {} prompts{detail}",
                    s.name, s.status, s.transport, s.tools, s.resources, s.prompts
                );
            }
            println!(
                "\ntools load on demand — `forge mcp tools <server>` to see a server's full list."
            );
        }
    }
    manager.shutdown().await;
    Ok(())
}

/// `forge mcp import [path]`. With an explicit `path`, import that one JSON file. With no path,
/// auto-scan every installed AI-CLI MCP config (Claude Code/Desktop, Codex, Cursor, Windsurf,
/// VS Code) and let the user pick which servers to import. Selected servers are merged into
/// `.forge/mcp.toml`; secrets are NEVER copied (ADR-0007).
fn mcp_import(path: Option<String>) -> Result<()> {
    let out = std::path::Path::new(".forge/mcp.toml");

    // Explicit single-file import (back-compat / scripting).
    if let Some(src) = path {
        let parsed = forge_config::import_mcp_json(std::path::Path::new(&src))
            .with_context(|| format!("importing {src}"))?;
        return finish_import(out, parsed.servers, parsed.secrets);
    }

    // Auto-scan mode.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let sources = forge_config::discover_import_sources(&cwd);
    if sources.is_empty() {
        println!(
            "No MCP servers found in any known AI-CLI config.\n\
             Scanned: ~/.claude.json, ~/.codex/config.toml, ~/.cursor/mcp.json (+ project), \
             Claude Desktop, Windsurf, ./.mcp.json, ./.vscode/mcp.json.\n\
             You can also import a specific file: `forge mcp import <path-to-.mcp.json>`."
        );
        return Ok(());
    }

    // Flatten + dedup by server name (first source wins), carrying the captured secret from the
    // SAME source the kept server came from.
    let mut flat: Vec<(String, forge_config::McpServerConfig, Option<String>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for s in &sources {
        for srv in &s.servers {
            if seen.insert(srv.name.clone()) {
                flat.push((
                    s.label.clone(),
                    srv.clone(),
                    s.secrets.get(&srv.name).cloned(),
                ));
            }
        }
    }

    // Pick: animated TUI multi-select on a real terminal; import-all when piped/CI.
    let selection = if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let items: Vec<forge_tui::SelectItem> = flat
            .iter()
            .map(|(label, srv, secret)| forge_tui::SelectItem {
                label: srv.name.clone(),
                hint: format!(
                    "[{}]  {}{}",
                    srv.transport_label(),
                    label,
                    if secret.is_some() {
                        "  · token → keyring"
                    } else {
                        ""
                    }
                ),
                preselected: true,
            })
            .collect();
        match forge_tui::select_multi("Import MCP servers", &items)
            .context("running the import picker")?
        {
            None => {
                println!("cancelled — nothing imported.");
                return Ok(());
            }
            Some(idx) => idx,
        }
    } else {
        println!(
            "Discovered {} MCP server(s); importing all (non-interactive).",
            flat.len()
        );
        (0..flat.len()).collect()
    };

    let mut servers = Vec::new();
    let mut secrets = std::collections::HashMap::new();
    for i in selection {
        let (_, srv, secret) = &flat[i];
        if let Some(val) = secret {
            secrets.insert(srv.name.clone(), val.clone());
        }
        servers.push(srv.clone());
    }
    if servers.is_empty() {
        println!("nothing selected.");
        return Ok(());
    }
    finish_import(out, servers, secrets)
}

/// Store each captured token in the OS keyring, merge the servers into `.forge/mcp.toml`, and
/// report. Forge does the secret-handling itself (ADR-0007): the token goes to the keyring, the
/// config only references it — the user is never asked to move anything by hand.
fn finish_import(
    out: &std::path::Path,
    servers: Vec<forge_config::McpServerConfig>,
    secrets: std::collections::HashMap<String, String>,
) -> Result<()> {
    let mut stored = Vec::new();
    let mut store_failed = Vec::new();
    for srv in &servers {
        let Some(value) = secrets.get(&srv.name) else {
            continue;
        };
        let key = srv
            .auth
            .as_ref()
            .and_then(|a| a.token_keyring.clone())
            .unwrap_or_else(|| format!("mcp:{}", srv.name));
        match forge_config::store_secret(&key, value) {
            Ok(()) => stored.push(srv.name.clone()),
            Err(e) => store_failed.push((srv.name.clone(), e.to_string())),
        }
    }

    let mut config = forge_config::load_mcp_toml(out);
    let existing: std::collections::HashSet<String> =
        config.servers.iter().map(|s| s.name.clone()).collect();
    let (mut added, mut skipped) = (Vec::new(), Vec::new());
    for srv in servers {
        if existing.contains(&srv.name) {
            skipped.push(srv.name);
        } else {
            added.push(srv.name.clone());
            config.servers.push(srv);
        }
    }
    forge_config::write_mcp_toml(out, &config).context("writing .forge/mcp.toml")?;

    if added.is_empty() {
        println!(
            "nothing new imported (all selected servers already in {}).",
            out.display()
        );
    } else {
        println!(
            "✓ imported {} server(s) → {}: {}",
            added.len(),
            out.display(),
            added.join(", ")
        );
    }
    if !skipped.is_empty() {
        println!("  • skipped (already present): {}", skipped.join(", "));
    }
    if !stored.is_empty() {
        println!(
            "  🔐 stored {} token(s) in the OS keyring: {}",
            stored.len(),
            stored.join(", ")
        );
    }
    for (name, err) in &store_failed {
        println!(
            "  ⚠ couldn't store '{name}' token in the keyring ({err}) — export it via the server's \
             token_env, or run `forge auth`. The server is imported but won't authenticate yet."
        );
    }
    Ok(())
}

/// Remove a server's stored OAuth tokens (`forge mcp logout <server>`).
fn mcp_logout(server: &str) -> Result<()> {
    match forge_config::clear_oauth_tokens(server) {
        Ok(true) => println!("✓ OAuth tokens for '{server}' removed from the keyring."),
        Ok(false) => println!("no stored OAuth tokens found for '{server}'."),
        Err(e) => anyhow::bail!("keyring error: {e}"),
    }
    Ok(())
}

/// Interactive OAuth 2.0 login for an OAuth-protected MCP server (`forge mcp login <server>`).
/// Opens the authorization URL in the user's browser, starts a loopback listener for the
/// redirect, exchanges the code for tokens, and stores them in the OS keyring (ADR-0007).
async fn mcp_login(server: &str) -> Result<()> {
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();

    // Find the server by name.
    let srv = config
        .mcp
        .servers
        .iter()
        .find(|s| s.name == server)
        .ok_or_else(|| anyhow::anyhow!("no server '{server}' in .forge/mcp.toml"))?;

    // Must have an oauth config entry.
    let oauth_cfg = srv
        .auth
        .as_ref()
        .and_then(|a| a.oauth.as_ref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "server '{server}' has no [auth.oauth] config — add it to .forge/mcp.toml"
            )
        })?;

    let http = reqwest::Client::new();

    // Discover the authorization server issuer.
    let issuer = if let Some(i) = &oauth_cfg.issuer {
        i.clone()
    } else {
        // Probe the server's well-known resource-metadata endpoint (RFC 9728).
        let url = match &srv.transport {
            forge_config::McpTransport::Http { url, .. } => {
                let base = url.trim_end_matches('/');
                format!("{base}/.well-known/oauth-protected-resource/mcp")
            }
            _ => anyhow::bail!("OAuth login only supported for HTTP transports"),
        };
        println!("Discovering auth server from {url} …");
        let meta = forge_mcp::oauth::fetch_resource_metadata(&http, &url)
            .await
            .map_err(|e| anyhow::anyhow!("fetching resource metadata from {url}: {e}"))?;
        meta.authorization_servers
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("resource metadata has no authorization_servers"))?
    };

    println!("Auth server: {issuer}");

    // Fetch auth server metadata (RFC 8414).
    let as_meta = forge_mcp::oauth::fetch_auth_server_metadata(&http, &issuer)
        .await
        .map_err(|e| anyhow::anyhow!("fetching auth server metadata from {issuer}: {e}"))?;

    // Choose client_id (from config or a fallback public client).
    let client_id = oauth_cfg
        .client_id
        .clone()
        .unwrap_or_else(|| "forge-mcp-client".to_string());

    // Bind a loopback listener to get the redirect port.
    let redirect_port = oauth_cfg.redirect_port.unwrap_or(0);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", redirect_port))
        .await
        .context("binding loopback redirect listener")?;
    let bound_port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{bound_port}/callback");

    // PKCE + state.
    let pkce = forge_config::Pkce::generate();
    let state = forge_config::random_state();
    let scopes = if oauth_cfg.scopes.is_empty() {
        vec!["mcp".to_string(), "offline_access".to_string()]
    } else {
        oauth_cfg.scopes.clone()
    };

    let auth_url = forge_config::authorize_url(
        &as_meta.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &scopes,
        &state,
        &pkce.challenge,
    );

    // Open the browser (cross-platform).
    println!("Opening browser for authorization …\n  {auth_url}");
    if let Err(e) = open_browser(&auth_url) {
        println!("(could not open browser automatically: {e})");
        println!("Please open the URL above manually.");
    }

    // Wait for the redirect callback on the loopback listener.
    println!("Waiting for authorization callback on http://127.0.0.1:{bound_port}/callback …");
    let (mut stream, _) =
        tokio::time::timeout(std::time::Duration::from_secs(120), listener.accept())
            .await
            .context("timed out waiting for OAuth callback (120 s)")?
            .context("accepting callback connection")?;

    // Read the HTTP request line to extract `code` and `state`.
    let (code, returned_state) = read_callback_params(&mut stream).await?;

    // Send a minimal HTTP 200 response so the browser doesn't show an error.
    let _ = tokio::io::AsyncWriteExt::write_all(
        &mut stream,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
          <html><body><h2>Authorization complete. You can close this tab.</h2></body></html>",
    )
    .await;
    drop(stream);

    // CSRF check.
    if returned_state != state {
        anyhow::bail!("OAuth state mismatch — possible CSRF. Login aborted.");
    }

    // Exchange the code for tokens.
    println!("Exchanging authorization code …");
    let tokens = forge_mcp::oauth::exchange_code(
        &http,
        &as_meta.token_endpoint,
        &code,
        &redirect_uri,
        &client_id,
        &pkce.verifier,
    )
    .await
    .map_err(|e| anyhow::anyhow!("token exchange: {e}"))?;

    // Store in keyring.
    forge_config::store_oauth_tokens(server, &tokens).context("storing OAuth tokens in keyring")?;

    println!("✓ OAuth tokens stored for '{server}'. Forge will refresh them automatically.");
    Ok(())
}

/// Parse `code` and `state` query params from the loopback HTTP GET request.
async fn read_callback_params(stream: &mut tokio::net::TcpStream) -> Result<(String, String)> {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 4096];
    let n = stream
        .read(&mut buf)
        .await
        .context("reading callback request")?;
    let request = std::str::from_utf8(&buf[..n]).unwrap_or_default();
    // First line: `GET /callback?code=XYZ&state=ABC HTTP/1.1`
    let path = request
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default();
    let query = path.splitn(2, '?').nth(1).unwrap_or_default();
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "code" => code = Some(url_decode(v)),
            "state" => state = Some(url_decode(v)),
            _ => {}
        }
    }
    let code = code.ok_or_else(|| anyhow::anyhow!("no `code` in OAuth callback URL"))?;
    let state = state.ok_or_else(|| anyhow::anyhow!("no `state` in OAuth callback URL"))?;
    Ok((code, state))
}

/// Minimal percent-decode (ASCII only, handles `%XX` and `+` → space).
fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 3;
                    continue;
                }
            }
        } else if bytes[i] == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Open `url` in the default system browser (cross-platform best-effort).
fn open_browser(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", url])
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        // Linux / BSD: try xdg-open, then sensible-browser, then wslview.
        let browsers = ["xdg-open", "sensible-browser", "wslview"];
        let mut launched = false;
        for b in browsers {
            if std::process::Command::new(b).arg(url).spawn().is_ok() {
                launched = true;
                break;
            }
        }
        if !launched {
            return Err(
                "no browser launcher found (tried xdg-open, sensible-browser, wslview)".into(),
            );
        }
    }
    Ok(())
}

async fn build_session_with(
    presenter: Box<dyn Presenter>,
    mock: bool,
    mode: Option<Mode>,
    resume: Option<String>,
    pin: Option<String>,
) -> Result<Session> {
    // Make any keyring-stored provider keys visible to the provider client.
    forge_config::inject_provider_keys();
    // …and the search-API key visible to the web_search tool.
    forge_config::inject_search_keys();

    let mut config = forge_config::load().context("loading configuration")?;
    if let Some(m) = mode {
        config.permission_mode = m.into();
    }
    // Capture the MCP config before `config` is moved into the Session; connect after the session
    // is built so its presenter can show the connection status.
    let mcp_config = config.mcp.clone();
    let config_has_mcp = mcp_config.active_servers().next().is_some();
    let lattice_enabled = config.lattice.enabled;
    let config_lattice_watch = config.lattice.watch;

    let store = Arc::new(open_store()?);
    let store_for_lattice = Arc::clone(&store);
    // Startup hint: if models are benched from a prior run/probe, tell the user how to recheck
    // (model-health-failover — we never auto-probe, so a stale bench is the user's to clear).
    let mut presenter = presenter;
    if let Ok(report) = store.current_benched_report() {
        if !report.is_empty() {
            presenter.emit(forge_tui::PresenterEvent::Warning(format!(
                "{} model(s) benched (rate-limited/unavailable) — `forge models --probe` to recheck",
                report.len()
            )));
        }
    }

    // Auto-discovery: build a live model catalog so the mesh routes to the best usable model
    // (docs/features/auto-discovery-mesh.md). Skipped for the offline mock and when disabled.
    let catalog = if !mock && config.mesh.auto_discover {
        Some(discover_catalog(&config).await)
    } else {
        None
    };
    let (provider, router) = build_provider_and_router(&config, mock, pin, catalog.clone());

    // Build the code-intelligence index up front so it can be shared between the model-facing
    // `lattice` tool and the turn's auto-injection (code-intelligence.md). Cheap to construct; it
    // reads whatever `forge lattice update` last persisted.
    let lattice = (!mock && lattice_enabled).then(|| {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Arc::new(forge_index::Lattice::new(store_for_lattice, &root))
    });
    let mut tools = ToolRegistry::with_core_tools();
    if let Some(lat) = &lattice {
        tools.register(Box::new(forge_tools::LatticeTool::new(Arc::clone(lat))));
        // Auto-index (and auto-embed when enabled) in the background so the graph is fresh without
        // a manual `forge lattice update` — "automatic under the hood". Incremental + non-blocking;
        // the watcher keeps it fresh thereafter. Errors are swallowed (best-effort, additive).
        let lat_bg = Arc::clone(lat);
        let embeddings = config.lattice.embeddings.clone();
        tokio::spawn(async move {
            if lat_bg.update().is_ok() {
                if let Some((embedder, _)) = forge_provider::select_embedder(&embeddings) {
                    let _ = lat_bg.embed_pending(embedder.as_ref(), 64).await;
                }
            }
        });
    }

    let mut session = match resume {
        Some(prefix) => {
            let full = resolve_session(&store, &prefix)?;
            Session::resume(store, provider, router, tools, presenter, config, &full)
                .with_context(|| format!("resuming session {full}"))?
        }
        None => {
            let cwd = std::env::current_dir()?.display().to_string();
            Session::start(store, provider, router, tools, presenter, config, &cwd)
                .context("starting session")?
        }
    };
    session.set_catalog(catalog);
    // Share the index with the session so turns auto-inject relevant code and agent edits reindex
    // in-turn (code-intelligence.md). Empty index → nothing injected (additive guarantee).
    // Also start the background watcher so external editor edits reindex automatically.
    if let Some(lat) = &lattice {
        if config_lattice_watch {
            let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            match forge_index::spawn_watcher(
                Arc::clone(lat),
                &root,
                std::time::Duration::from_millis(400),
            ) {
                Ok(w) => session.set_lattice_watcher(Some(w)),
                Err(e) => session.notify_error(&format!("lattice watcher disabled: {e}")),
            }
        }
    }
    session.set_lattice(lattice);

    // Attach the command/skill catalog so the model can discover + load Forge's own skills via
    // the `use_skill` tool (instead of hunting ~/.claude). Cheap, sync, pure.
    let skill_catalog = forge_skills::Catalog::load(&forge_config::command_sources());
    session.set_skills(Some(std::sync::Arc::new(skill_catalog)));

    // Connect external MCP servers (mcp-client.md). Skipped for the offline mock. Per-server
    // failures are isolated inside connect_all (each lands `failed` with a reason); we surface the
    // whole listing once so connection state — including failures — is visible at startup.
    if !mock && config_has_mcp {
        let manager = std::sync::Arc::new(forge_mcp::McpManager::connect_all(&mcp_config).await);
        session.set_mcp(Some(manager));
        session.announce_mcp();
    }
    Ok(session)
}

/// Build a session with the default surface (TUI on a tty, else plain).
async fn build_session(
    mock: bool,
    mode: Option<Mode>,
    tui: bool,
    resume: Option<String>,
    pin: Option<String>,
) -> Result<Session> {
    let presenter: Box<dyn Presenter> = if tui && std::io::stdout().is_terminal() {
        Box::new(TuiPresenter::new().context("initializing TUI")?)
    } else {
        if tui {
            eprintln!("forge: --tui needs an interactive terminal; falling back to plain output");
        }
        Box::new(HeadlessPresenter::default())
    };
    build_session_with(presenter, mock, mode, resume, pin).await
}

async fn run(
    prompt: String,
    mock: bool,
    mode: Option<Mode>,
    tui: bool,
    resume: Option<String>,
    pin: Option<String>,
) -> Result<()> {
    if prompt.trim().is_empty() {
        anyhow::bail!("empty prompt — usage: forge run \"<your task>\"");
    }
    let mut session = build_session(mock, mode, tui, resume, pin).await?;
    session
        .run_turn(&prompt)
        .await
        .context("running agent turn")?;
    // In the TUI, hold the final frame until the user quits (Esc / Ctrl-C).
    if tui {
        let _ = session.read_line();
    }
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

/// On a fresh machine (no keys, no bridge, no config) offer the `forge init` wizard before the
/// first chat. Skipped for `--mock`, non-interactive shells, and once anything is configured.
/// Declining writes an (empty) config so we don't nag on every launch.
fn maybe_first_run_setup(mock: bool) -> Result<()> {
    if mock || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    let has_any_key = forge_config::known_key_providers().any(forge_config::has_api_key);
    let any_bridge = forge_provider::CliKind::all().iter().any(|k| k.available());
    if !needs_onboarding(has_any_key, any_bridge, forge_config::user_config_exists()) {
        return Ok(());
    }
    println!("⚒ Welcome to Forge — no providers are configured yet.");
    let yes = prompt_line("Run interactive setup now? [Y/n]: ")?;
    if yes.is_empty() || yes.eq_ignore_ascii_case("y") || yes.eq_ignore_ascii_case("yes") {
        init()?;
    } else {
        // Mark onboarded so we don't ask again; the user can re-run `forge init` anytime.
        let _ = forge_config::write_subscriptions(&std::collections::HashMap::new());
        println!("Skipped. Run `forge init` anytime, or `forge auth <provider>` to add a key.");
    }
    Ok(())
}

async fn chat(
    mock: bool,
    mode: Option<Mode>,
    resume: Option<String>,
    plain: bool,
    pin: Option<String>,
) -> Result<()> {
    maybe_first_run_setup(mock)?;
    // Default to the interactive (animated) TUI on a real terminal.
    if !plain && std::io::stdout().is_terminal() {
        return run_chat_tui(mock, mode, resume, pin).await;
    }

    // Plain line mode: read prompts from stdin.
    let mut session = build_session_with(
        Box::new(HeadlessPresenter::default()),
        mock,
        mode,
        resume,
        pin,
    )
    .await?;
    if std::io::stdin().is_terminal() {
        println!("forge chat — type a task and press enter; /quit to exit");
    }
    while let Some(line) = session.read_line() {
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

/// Sends the turn-complete signal (carrying the turn's generation) on drop — so `busy` is released
/// even if the turn task panics or is aborted. The loop only acts on a signal whose generation
/// matches the current turn, so an interrupted turn's late signal can't end a *later* turn.
struct DoneGuard(std::sync::mpsc::Sender<u64>, u64);
impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.send(self.1);
    }
}

/// Animated TUI chat loop: renders at ~16fps, runs each turn on a task so a spinner
/// ticks (and streamed tokens flow) while the model works.
async fn run_chat_tui(
    mock: bool,
    mode: Option<Mode>,
    resume: Option<String>,
    pin: Option<String>,
) -> Result<()> {
    use forge_tui::{
        banner_lines, handle_key, App, ChannelPresenter, InputOutcome, KeyKind, Tui, UiMsg,
    };
    use std::time::{Duration, Instant};

    let (tx, rx) = std::sync::mpsc::channel::<UiMsg>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<u64>();
    let session =
        build_session_with(Box::new(ChannelPresenter::new(tx)), mock, mode, resume, pin).await?;
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(session));

    let mut tui = Tui::new().context("initializing TUI")?;
    // The welcome banner is a one-time print into scrollback (not a render branch).
    tui.insert_lines(banner_lines(tui.width()));
    let mut app = App::default();
    app.temper = session.lock().await.temper().label().to_string();

    // Discover file-based slash commands + skills (command-skill-system.md). Feed them into the
    // palette alongside the builtins; surface any malformed-file warnings once.
    let catalog = forge_skills::Catalog::load(&forge_config::command_sources());
    app.palette.extra = catalog
        .entries()
        .iter()
        .map(|e| forge_tui::PaletteEntry {
            name: e.name.clone(),
            desc: if e.is_skill {
                format!("{}  (skill)", e.description)
            } else {
                e.description.clone()
            },
        })
        .collect();
    for w in catalog.warnings() {
        app.note(&format!("⚠ {w}"));
    }
    let trust_project = session.lock().await.commands_trust_project();
    // Project-scope commands/skills can steer the model; their first use this session is gated
    // unless trusted. Re-running a gated command confirms it (its name lands here).
    let mut armed_project: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut busy = false;
    // Each turn gets a monotonic generation; the abort handle lets Esc interrupt it (RFC
    // session-management). The current gen gates the done-signal so an aborted turn's late
    // signal is ignored once a new turn has started.
    let mut turn_gen: u64 = 0;
    let mut turn_handle: Option<tokio::task::JoinHandle<()>> = None;
    // `/loop` state: when set, each completed turn of this generation is re-run until the model
    // signals completion or the iteration cap is hit.
    let mut loop_state: Option<LoopState> = None;
    let mut pending: Option<std::sync::mpsc::Sender<bool>> = None;
    let mut pending_question: Option<std::sync::mpsc::Sender<String>> = None;
    // Lens filter set by `/assay --only`/`--skip`; consumed when the AssayChoice picker resolves.
    let mut assay_lenses: Vec<forge_types::FindingCategory> = Vec::new();
    // Baseline for the spinner: deriving the tick from elapsed time keeps the animation
    // speed independent of the loop frequency (one frame per 60ms, exactly as before).
    let mut busy_since = Instant::now();
    // Only redraw when state actually changed: idle frames cost nothing and the whole
    // conversation isn't rebuilt 16×/sec for no reason.
    let mut dirty = true;
    let mut quit = false;

    while !quit {
        if dirty {
            app.busy = busy;
            tui.draw(&app);
            dirty = false;
        }

        // Drain *all* buffered keystrokes this iteration. Reading one per frame throttled
        // fast typing to the frame rate (~16 keys/sec) — the source of the input lag.
        while let Some(key) = tui.poll_key().context("reading input")? {
            dirty = true;

            // The command palette is modal while open: it owns every key. Esc dismisses it
            // (so the user isn't surprised by a quit); Ctrl-C still maps to Esc → here it just
            // closes the palette, and a second Esc with the palette closed quits as usual.
            if app.palette.open {
                match key {
                    KeyKind::Esc => {
                        app.palette.close();
                        app.input.clear();
                    }
                    KeyKind::Up => app.palette.move_up(),
                    KeyKind::Down => app.palette.move_down(),
                    KeyKind::Tab => {
                        if let Some(name) = app.palette.selected_name().map(|s| s.to_string()) {
                            // Replace the `/command` token in place (mid-line aware), not the
                            // whole input — so `run /or<Tab>` completes to `run /orchestrate`.
                            if let Some(tok) =
                                forge_tui::slash_token_at(&app.input, app.input.len())
                            {
                                app.input
                                    .replace_range(tok.start..tok.end, &format!("/{name}"));
                            } else {
                                app.input = format!("/{name}");
                            }
                            app.palette.query = name;
                            app.palette.clamp();
                        }
                    }
                    KeyKind::Enter => {
                        let leading = app.input.starts_with('/') && !app.input.starts_with("//");
                        if !leading {
                            // Mid-line `/command`: Enter accepts the highlighted suggestion in
                            // place (replacing just the token) and keeps editing — it does NOT
                            // dispatch, so the surrounding prose is preserved. A leading command
                            // still dispatches (the branch below).
                            if let Some(name) = app.palette.selected_name().map(|s| s.to_string()) {
                                if let Some(tok) =
                                    forge_tui::slash_token_at(&app.input, app.input.len())
                                {
                                    app.input
                                        .replace_range(tok.start..tok.end, &format!("/{name}"));
                                }
                            }
                            app.palette.close();
                            continue;
                        }
                        // If the user typed args after the command, dispatch exactly what they
                        // wrote (`/loop do it`); only autocomplete-to-selection when the line is
                        // the bare command token, so args are never dropped.
                        let has_args = app.input.trim().contains(char::is_whitespace);
                        let line = if has_args {
                            app.input.clone()
                        } else {
                            app.palette
                                .selected_name()
                                .map(|n| format!("/{n}"))
                                .unwrap_or_else(|| app.input.clone())
                        };
                        app.palette.close();
                        app.input.clear();
                        match dispatch_command(
                            &line,
                            &session,
                            &mut tui,
                            &mut app,
                            &catalog,
                            &mut armed_project,
                            trust_project,
                            busy,
                            &mut assay_lenses,
                        )
                        .await?
                        {
                            DispatchOutcome::Quit => {
                                quit = true;
                                break;
                            }
                            DispatchOutcome::Handled => {}
                            DispatchOutcome::RunTurn {
                                prompt,
                                guidance,
                                tier,
                            } => {
                                turn_gen += 1;
                                turn_handle = Some(spawn_turn_with(
                                    prompt,
                                    guidance,
                                    tier,
                                    &session,
                                    &done_tx,
                                    turn_gen,
                                    &mut app,
                                    &mut busy,
                                    &mut busy_since,
                                ));
                            }
                            DispatchOutcome::RunCompact => {
                                turn_gen += 1;
                                turn_handle = Some(spawn_compact(
                                    &session,
                                    &done_tx,
                                    turn_gen,
                                    &mut app,
                                    &mut busy,
                                    &mut busy_since,
                                ));
                            }
                            DispatchOutcome::StartLoop { prompt } => {
                                turn_gen += 1;
                                loop_state = Some(LoopState {
                                    gen: turn_gen,
                                    iter: 1,
                                });
                                app.note("↻ loop started — Esc to stop");
                                turn_handle = Some(spawn_turn_with(
                                    prompt,
                                    vec![LOOP_GUIDANCE.to_string()],
                                    None,
                                    &session,
                                    &done_tx,
                                    turn_gen,
                                    &mut app,
                                    &mut busy,
                                    &mut busy_since,
                                ));
                            }
                        }
                    }
                    KeyKind::Char(c) => {
                        app.input.push(c);
                        sync_palette_to_slash_token(&mut app);
                    }
                    KeyKind::Backspace => {
                        app.input.pop();
                        sync_palette_to_slash_token(&mut app);
                    }
                    KeyKind::CycleTemper | KeyKind::ToggleSubagentDetail => {}
                }
                continue;
            }

            // The session/checkpoint picker is modal too: arrows navigate, typing filters, Enter
            // acts on the selection (resume / rewind), Esc cancels.
            if app.picker.open {
                match key {
                    KeyKind::Esc => {
                        // In the models browser, Esc from a drilled-in provider steps back to the
                        // provider list rather than closing the whole picker.
                        if app.picker.kind == Some(forge_tui::PickerKind::Models)
                            && app.models_drilled.is_some()
                        {
                            open_models_root(&session, &mut app).await?;
                        } else {
                            app.models_drilled = None;
                            app.picker.close();
                        }
                    }
                    KeyKind::Up => app.picker.move_up(),
                    KeyKind::Down => app.picker.move_down(),
                    KeyKind::Enter => {
                        let chosen = app.picker.selected_row().cloned();
                        let kind = app.picker.kind;
                        // The models browser drills (provider → models) on Enter instead of
                        // resolving; model rows are terminal. Keep the picker open either way.
                        if kind == Some(forge_tui::PickerKind::Models) {
                            if let Some(row) = chosen {
                                if app.models_drilled.is_none() && !row.id.contains("::") {
                                    open_models_provider(&session, &mut app, &row.id).await?;
                                }
                            }
                            continue;
                        }
                        app.picker.close();
                        if let (Some(row), Some(kind)) = (chosen, kind) {
                            if kind == forge_tui::PickerKind::AssayChoice {
                                // Assay runs as a background task (like a turn) so the spinner
                                // ticks while critics + verification run.
                                turn_gen += 1;
                                let lenses = std::mem::take(&mut assay_lenses);
                                turn_handle = spawn_assay(
                                    row.id == "cleanup",
                                    lenses,
                                    &session,
                                    &done_tx,
                                    turn_gen,
                                    &mut app,
                                    &mut busy,
                                    &mut busy_since,
                                )
                                .await?;
                            } else {
                                picker_accept(kind, &row, &session, &mut tui, &mut app).await?;
                            }
                        }
                    }
                    KeyKind::Char(c) => {
                        app.picker.query.push(c);
                        app.picker.clamp();
                    }
                    KeyKind::Backspace => {
                        app.picker.query.pop();
                        app.picker.clamp();
                    }
                    KeyKind::Tab | KeyKind::CycleTemper | KeyKind::ToggleSubagentDetail => {}
                }
                continue;
            }

            // When the subagent picker overlay is open, ↑↓ navigate, Enter opens that agent's
            // full-screen transcript, Esc/Ctrl+O closes the picker.
            if app.subagent_picking {
                match key {
                    KeyKind::Up => {
                        app.subagent_pick_idx = app.subagent_pick_idx.saturating_sub(1);
                    }
                    KeyKind::Down => {
                        let n = app.subagent_views().len();
                        app.subagent_pick_idx =
                            (app.subagent_pick_idx + 1).min(n.saturating_sub(1));
                    }
                    KeyKind::Enter => {
                        let idx = app.subagent_pick_idx;
                        app.subagent_picking = false;
                        tui.run_fullscreen(|| {
                            forge_tui::run_subagent_transcript(idx, || {
                                while let Ok(msg) = rx.try_recv() {
                                    match msg {
                                        UiMsg::Event(e) => app.apply(e),
                                        UiMsg::Permission { reply, .. } => {
                                            let _ = reply.send(false);
                                        }
                                        UiMsg::Question { reply, .. } => {
                                            let _ = reply.send(forge_tui::NO_ANSWER.to_string());
                                        }
                                    }
                                }
                                app.subagent_views()
                            })
                        })?;
                    }
                    KeyKind::Esc | KeyKind::ToggleSubagentDetail => {
                        app.subagent_picking = false;
                    }
                    _ => {}
                }
                dirty = true;
                continue;
            }

            // Ctrl+O: open the subagent transcript browser. With a single agent, open it directly;
            // with multiple agents, open the picker overlay first so the user can choose which one.
            if matches!(key, KeyKind::ToggleSubagentDetail) {
                let views = app.subagent_views();
                if !views.is_empty() {
                    if views.len() == 1 {
                        tui.run_fullscreen(|| {
                            forge_tui::run_subagent_transcript(0, || {
                                while let Ok(msg) = rx.try_recv() {
                                    match msg {
                                        UiMsg::Event(e) => app.apply(e),
                                        UiMsg::Permission { reply, .. } => {
                                            let _ = reply.send(false);
                                        }
                                        UiMsg::Question { reply, .. } => {
                                            let _ = reply.send(forge_tui::NO_ANSWER.to_string());
                                        }
                                    }
                                }
                                app.subagent_views()
                            })
                        })?;
                    } else {
                        app.subagent_picking = true;
                        app.subagent_pick_idx = 0;
                    }
                }
                dirty = true;
                continue;
            }

            // Esc / Ctrl-C: while a turn is running it INTERRUPTS the AI (stops the response,
            // keeps Forge alive); while idle it quits. Checked before any prompt handling so the
            // user can never get wedged — interrupting also clears a pending permission/question.
            if matches!(key, KeyKind::Esc) {
                if busy {
                    if let Some(h) = turn_handle.take() {
                        h.abort(); // cancel the turn task; its DoneGuard drop releases the lock
                    }
                    turn_gen += 1; // discard the aborted turn's (now stale) done-signal
                    busy = false;
                    loop_state = None; // a `/loop` in progress stops on interrupt
                    pending = None;
                    pending_question = None;
                    app.prompt = None;
                    app.clear_question();
                    app.apply(forge_tui::PresenterEvent::AssistantDone); // flush any partial reply
                    app.note("⏹ interrupted — stopped responding");
                    dirty = true;
                    continue;
                }
                quit = true;
                break;
            }
            if let Some(reply) = pending.take() {
                // Answering a permission prompt.
                let yes = matches!(
                    key,
                    KeyKind::Char('y') | KeyKind::Char('Y') | KeyKind::Enter
                );
                let _ = reply.send(yes);
                app.prompt = None;
            } else if app.awaiting_question() {
                // Answering an AskUserQuestion (the turn task is blocked in `ask()`): the input
                // line collects a number or free-text answer; submit resolves + replies.
                match handle_key(&mut app.input, key) {
                    InputOutcome::Submit(line) => {
                        if let Some(ans) = app.resolve_question(&line) {
                            if let Some(tx) = pending_question.take() {
                                let _ = tx.send(ans);
                            }
                        } else {
                            app.input.clear(); // invalid → re-prompt (question stays open)
                        }
                    }
                    InputOutcome::Quit => {
                        quit = true;
                        break;
                    }
                    InputOutcome::Editing => {}
                }
            } else if busy {
                // Mid-turn: ignore typing (quit is already handled above).
            } else if matches!(key, KeyKind::CycleTemper) {
                // SHIFT+TAB: cycle the operating temper (idle only — never mid-turn).
                let new = {
                    let mut sess = session.lock().await;
                    sess.cycle_temper()
                };
                app.set_temper(new.label());
            } else {
                match handle_key(&mut app.input, key) {
                    InputOutcome::Submit(line) => {
                        // `//foo` escapes to a literal prompt `/foo`; a bare `/cmd` typed without
                        // the palette still dispatches as a command; everything else is a prompt.
                        if let Some(rest) = line.strip_prefix("//") {
                            turn_gen += 1;
                            turn_handle = Some(spawn_turn(
                                &format!("/{rest}"),
                                &session,
                                &done_tx,
                                turn_gen,
                                &mut app,
                                &mut busy,
                                &mut busy_since,
                            ));
                        } else if line.starts_with('/') {
                            match dispatch_command(
                                &line,
                                &session,
                                &mut tui,
                                &mut app,
                                &catalog,
                                &mut armed_project,
                                trust_project,
                                busy,
                                &mut assay_lenses,
                            )
                            .await?
                            {
                                DispatchOutcome::Quit => {
                                    quit = true;
                                    break;
                                }
                                DispatchOutcome::Handled => {}
                                DispatchOutcome::RunTurn {
                                    prompt,
                                    guidance,
                                    tier,
                                } => {
                                    turn_gen += 1;
                                    turn_handle = Some(spawn_turn_with(
                                        prompt,
                                        guidance,
                                        tier,
                                        &session,
                                        &done_tx,
                                        turn_gen,
                                        &mut app,
                                        &mut busy,
                                        &mut busy_since,
                                    ));
                                }
                                DispatchOutcome::RunCompact => {
                                    turn_gen += 1;
                                    turn_handle = Some(spawn_compact(
                                        &session,
                                        &done_tx,
                                        turn_gen,
                                        &mut app,
                                        &mut busy,
                                        &mut busy_since,
                                    ));
                                }
                                DispatchOutcome::StartLoop { prompt } => {
                                    turn_gen += 1;
                                    loop_state = Some(LoopState {
                                        gen: turn_gen,
                                        iter: 1,
                                    });
                                    app.note("↻ loop started — Esc to stop");
                                    turn_handle = Some(spawn_turn_with(
                                        prompt,
                                        vec![LOOP_GUIDANCE.to_string()],
                                        None,
                                        &session,
                                        &done_tx,
                                        turn_gen,
                                        &mut app,
                                        &mut busy,
                                        &mut busy_since,
                                    ));
                                }
                            }
                        } else {
                            turn_gen += 1;
                            turn_handle = Some(spawn_turn(
                                &line,
                                &session,
                                &done_tx,
                                turn_gen,
                                &mut app,
                                &mut busy,
                                &mut busy_since,
                            ));
                        }
                    }
                    InputOutcome::Quit => {
                        quit = true;
                        break;
                    }
                    InputOutcome::Editing => {
                        // A `/command` token anywhere on the line opens the palette (not only at
                        // the start) — mid-line autocomplete + highlighting.
                        if let Some(tok) = forge_tui::slash_token_at(&app.input, app.input.len()) {
                            app.palette.open_with(&tok.name);
                        }
                    }
                }
            }
        }
        if quit {
            break;
        }

        while let Ok(msg) = rx.try_recv() {
            dirty = true;
            match msg {
                UiMsg::Event(e) => app.apply(e),
                UiMsg::Permission {
                    tool,
                    side_effect,
                    reply,
                } => {
                    app.prompt = Some(format!("allow {tool} ({side_effect:?})"));
                    pending = Some(reply);
                }
                UiMsg::Question {
                    question,
                    options,
                    allow_other,
                    reply,
                } => {
                    app.set_question(&question, &options, allow_other);
                    pending_question = Some(reply);
                }
            }
        }

        // Clear busy only on the *current* turn's done-signal; a stale signal from an interrupted
        // (aborted) turn carries an older generation and is ignored.
        while let Ok(g) = done_rx.try_recv() {
            if busy && g == turn_gen {
                busy = false;
                turn_handle = None;
                dirty = true;
                // `/loop`: if this was a loop turn, decide whether to run another iteration.
                if let Some(ls) = loop_state.take() {
                    if ls.gen == g {
                        let last = {
                            session
                                .lock()
                                .await
                                .last_assistant_text()
                                .map(str::to_string)
                        };
                        match loop_stop_reason(last.as_deref(), ls.iter) {
                            Some(reason) => app.note(reason),
                            None => {
                                turn_gen += 1;
                                loop_state = Some(LoopState {
                                    gen: turn_gen,
                                    iter: ls.iter + 1,
                                });
                                turn_handle = Some(spawn_turn_with(
                                    "Continue toward completion.".to_string(),
                                    vec![LOOP_GUIDANCE.to_string()],
                                    None,
                                    &session,
                                    &done_tx,
                                    turn_gen,
                                    &mut app,
                                    &mut busy,
                                    &mut busy_since,
                                ));
                            }
                        }
                    } else {
                        loop_state = Some(ls); // a different turn finished; keep waiting
                    }
                }
            }
        }
        if busy {
            let t = (busy_since.elapsed().as_millis() / 60) as usize;
            if t != app.tick {
                app.tick = t;
                dirty = true;
            }
        }
        // Animate the command palette's / picker's ease-in reveal while open.
        if app.palette.open && app.palette.anim < 1.0 {
            app.palette.tick_anim();
            dirty = true;
        }
        if app.picker.open && app.picker.anim < 1.0 {
            app.picker.tick_anim();
            dirty = true;
        }

        // Push any finalized lines into native scrollback (above the pinned live region).
        let flushed = app.drain_flush();
        if !flushed.is_empty() {
            tui.insert_lines(flushed);
            dirty = true;
        }
        tokio::time::sleep(Duration::from_millis(16)).await;
    }
    Ok(())
}

/// `/loop` runtime state: the generation of the in-flight loop turn and how many iterations have
/// run, so completion can be detected and capped.
struct LoopState {
    gen: u64,
    iter: usize,
}

/// Iteration cap so a loop that never signals completion can't run forever.
const LOOP_MAX_ITERS: usize = 25;
/// The token the model is told to emit when the looped task is fully complete.
const LOOP_DONE_SENTINEL: &str = "LOOP_COMPLETE";
/// Guidance injected on every loop turn: make progress, and signal completion explicitly.
const LOOP_GUIDANCE: &str = "You are running in an autonomous loop. Make concrete progress on the \
task each turn. When — and ONLY when — the task is fully complete, end your final message with \
the token LOOP_COMPLETE on its own line. While work remains, keep going and do NOT emit that token.";

/// Decide whether a loop should stop after a turn. Returns `Some(reason)` to stop (shown to the
/// user), or `None` to run another iteration. Pure so it's unit-testable.
fn loop_stop_reason(last_assistant: Option<&str>, iter: usize) -> Option<&'static str> {
    if last_assistant.is_some_and(|t| t.contains(LOOP_DONE_SENTINEL)) {
        Some("◆ loop complete")
    } else if iter >= LOOP_MAX_ITERS {
        Some("◆ loop stopped — hit the iteration cap")
    } else {
        None
    }
}

/// Echo a prompt + spawn the turn task (shared by normal submit and the `//` literal escape).
#[allow(clippy::too_many_arguments)]
fn spawn_turn(
    prompt: &str,
    session: &Arc<tokio::sync::Mutex<Session>>,
    done_tx: &std::sync::mpsc::Sender<u64>,
    gen: u64,
    app: &mut forge_tui::App,
    busy: &mut bool,
    busy_since: &mut std::time::Instant,
) -> tokio::task::JoinHandle<()> {
    app.submit_user(prompt);
    app.done = false;
    app.tick = 0;
    *busy = true;
    *busy_since = std::time::Instant::now();
    let s = session.clone();
    let dt = done_tx.clone();
    let prompt = prompt.to_string();
    tokio::spawn(async move {
        // DoneGuard fires on the way out — normal return, panic unwind, OR abort (interrupt) —
        // so the UI can never stay stuck "working". It carries this turn's generation.
        let _done = DoneGuard(dt, gen);
        let mut sess = s.lock().await;
        if let Err(e) = sess.run_turn(&prompt).await {
            sess.notify_error(&format!("turn failed: {e}"));
        }
    })
}

/// Like [`spawn_turn`] but runs an expanded command/skill: prepends `guidance` and biases routing
/// with the `tier` hint. The displayed user line is the original `/command` (echoed by the
/// dispatcher), so the model receives the expanded `prompt` while the transcript shows the turn.
#[allow(clippy::too_many_arguments)]
fn spawn_turn_with(
    prompt: String,
    guidance: Vec<String>,
    tier: Option<forge_types::TaskTier>,
    session: &Arc<tokio::sync::Mutex<Session>>,
    done_tx: &std::sync::mpsc::Sender<u64>,
    gen: u64,
    app: &mut forge_tui::App,
    busy: &mut bool,
    busy_since: &mut std::time::Instant,
) -> tokio::task::JoinHandle<()> {
    app.submit_user(&prompt);
    app.done = false;
    app.tick = 0;
    *busy = true;
    *busy_since = std::time::Instant::now();
    let s = session.clone();
    let dt = done_tx.clone();
    tokio::spawn(async move {
        let _done = DoneGuard(dt, gen);
        let mut sess = s.lock().await;
        if let Err(e) = sess.run_turn_with(&prompt, &guidance, tier).await {
            sess.notify_error(&format!("turn failed: {e}"));
        }
    })
}

/// Spawn `/compact` as a background task (it makes a cheap model call): the spinner ticks while the
/// older transcript is summarized, exactly like a turn.
fn spawn_compact(
    session: &Arc<tokio::sync::Mutex<Session>>,
    done_tx: &std::sync::mpsc::Sender<u64>,
    gen: u64,
    app: &mut forge_tui::App,
    busy: &mut bool,
    busy_since: &mut std::time::Instant,
) -> tokio::task::JoinHandle<()> {
    app.done = false;
    app.tick = 0;
    *busy = true;
    *busy_since = std::time::Instant::now();
    let s = session.clone();
    let dt = done_tx.clone();
    tokio::spawn(async move {
        let _done = DoneGuard(dt, gen);
        let mut sess = s.lock().await;
        if let Err(e) = sess.compact().await {
            sess.notify_error(&format!("compact failed: {e}"));
        }
    })
}

/// What the render loop must do after [`dispatch_command`].
enum DispatchOutcome {
    /// Command fully handled in-loop (palette, picker, note, …) — keep going.
    Handled,
    /// `/quit` — exit the TUI.
    Quit,
    /// A file command/skill expanded into a model turn the caller should spawn.
    RunTurn {
        prompt: String,
        guidance: Vec<String>,
        tier: Option<forge_types::TaskTier>,
    },
    /// `/compact` — summarize older messages in a background task (it makes a model call).
    RunCompact,
    /// `/loop <task>` — run the task, then re-run each turn until the model signals completion.
    StartLoop { prompt: String },
}

/// Execute a slash command (command-skill-system.md). Builtins are matched first; an unrecognised
/// `/name` falls through to the file-based command/skill [`forge_skills::Catalog`]. Returns
/// [`DispatchOutcome`]. Session-mutating commands (`/new`, `/resume`, `/clear`) and file
/// commands/skills are gated while a turn holds the session `Mutex`. All session access is
/// `lock().await` — no blocking on the render-loop thread (the #45 invariant).
#[allow(clippy::too_many_arguments)]
async fn dispatch_command(
    line: &str,
    session: &Arc<tokio::sync::Mutex<Session>>,
    tui: &mut forge_tui::Tui,
    app: &mut forge_tui::App,
    catalog: &forge_skills::Catalog,
    armed: &mut std::collections::HashSet<String>,
    trust_project: bool,
    busy: bool,
    assay_lenses: &mut Vec<forge_types::FindingCategory>,
) -> Result<DispatchOutcome> {
    use forge_tui::CommandAction;
    let action = forge_tui::parse_command(line);
    // Everything that touches the live `Session` (lock().await) or swaps it is gated while a turn
    // holds the Mutex — opening the read-only `/sessions` picker is the one exception.
    let mutates = !matches!(
        action,
        CommandAction::Help
            | CommandAction::Quit
            | CommandAction::Unknown(_)
            | CommandAction::ListSessions
            | CommandAction::Resume(_)
            | CommandAction::ClearScreen
            | CommandAction::PinModel(_)
            | CommandAction::Replay(_, _)
    );
    if busy && mutates {
        app.note("⚠ finish or Esc the current turn first");
        return Ok(DispatchOutcome::Handled);
    }
    match action {
        CommandAction::Help => app.palette.open_with(""),
        CommandAction::Quit => return Ok(DispatchOutcome::Quit),
        CommandAction::ClearScreen => {
            tui.clear_screen();
            app.note("— screen cleared —");
        }
        CommandAction::New => {
            let cwd = std::env::current_dir()?.display().to_string();
            {
                let mut s = session.lock().await;
                s.reset_fresh(&cwd).map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            tui.clear_screen();
            app.note("● new session");
        }
        // `/mode` opens the operating-mode (temper) picker — a reliable, discoverable alternative
        // to SHIFT+TAB. Enter sets the chosen temper in picker_accept.
        CommandAction::Mode => {
            let current = {
                let s = session.lock().await;
                s.temper().label()
            };
            let rows = forge_types::PermissionMode::all()
                .iter()
                .map(|m| {
                    let mark = if m.label() == current {
                        "   ● current"
                    } else {
                        ""
                    };
                    forge_tui::PickerRow {
                        id: m.label().to_string(),
                        title: m.label().to_string(),
                        subtitle: format!("{}{mark}", m.description()),
                    }
                })
                .collect();
            app.picker.open_with(
                forge_tui::PickerKind::Tempers,
                "switch operating mode",
                rows,
            );
        }
        // `/assay` enters Assay mode: pick analysis-only vs full cleanup; the crew then runs as a
        // background task (spawned in the picker-Enter handler so the spinner ticks).
        CommandAction::Assay { only, skip } => {
            // Compute the lens set from --only/--skip and store for picker resolution.
            let crew = forge_types::FindingCategory::crew();
            *assay_lenses = if !only.is_empty() {
                crew.iter()
                    .filter(|l| only.iter().any(|o| o == l.as_str()))
                    .copied()
                    .collect()
            } else if !skip.is_empty() {
                crew.iter()
                    .filter(|l| !skip.iter().any(|s| s == l.as_str()))
                    .copied()
                    .collect()
            } else {
                Vec::new() // empty = use full crew (default)
            };
            let rows = vec![
                forge_tui::PickerRow {
                    id: "analysis".into(),
                    title: "Analysis only".into(),
                    subtitle: "review & ranked report — no edits".into(),
                },
                forge_tui::PickerRow {
                    id: "cleanup".into(),
                    title: "Full cleanup (Refine)".into(),
                    subtitle: "analyze, then auto-fix findings — permission-gated, /undo to revert"
                        .into(),
                },
            ];
            app.picker
                .open_with(forge_tui::PickerKind::AssayChoice, "⚒ assay — choose", rows);
        }
        // `/resume [prefix]` and `/sessions` both open the interactive picker; a prefix pre-fills
        // its filter. Resolving + swapping the session happens on Enter (picker_accept).
        CommandAction::Resume(prefix) => open_sessions_picker(app, &prefix)?,
        CommandAction::ListSessions => open_sessions_picker(app, "")?,
        // `/model <id>` pins a specific model for the rest of this session (or clears the pin).
        // Works while a turn is running (pin takes effect on the NEXT turn).
        CommandAction::PinModel(model_id) => {
            let mut s = session.lock().await;
            s.pin_model(model_id.clone());
            match model_id {
                Some(id) => app.note(&format!("⊕ model pinned: {id} (clears with /model)")),
                None => app.note("⊖ model pin cleared — mesh routing restored"),
            }
        }
        // `/models` opens the interactive model browser: a provider list (with global counts in
        // the heading) that drills into each provider's models on Enter; Esc steps back.
        CommandAction::ListModels => open_models_root(session, app).await?,
        // `/config` launches the animated setup wizard full-screen (reconfigure mode): set
        // provider + search API keys and bridge plans, then return to chat. Keys are stored +
        // injected live so the current session picks them up without a restart.
        CommandAction::Config => {
            let outcome = tui
                .run_fullscreen(|| forge_tui::init_wizard::run(wizard_input()))
                .map_err(|e| anyhow::anyhow!("config wizard: {e}"))?;
            if outcome.cancelled {
                app.note("config cancelled");
            } else {
                apply_wizard_outcome(&outcome)?;
                app.note(&format!(
                    "✓ config saved — {} key(s), {} bridge plan(s)",
                    outcome.keys.len(),
                    outcome.plans.len()
                ));
            }
        }
        CommandAction::Mcp(server) => {
            let s = session.lock().await;
            match server {
                Some(srv) => {
                    let tools = s.mcp_tool_lines(&srv);
                    if tools.is_empty() {
                        app.note(&format!("no tools for MCP server '{srv}' (not connected?)"));
                    } else {
                        app.note(&format!("{} tool(s) on '{srv}':", tools.len()));
                        for (name, desc) in tools {
                            app.note(&format!("  {name} — {desc}"));
                        }
                    }
                }
                None => app.apply(forge_tui::PresenterEvent::McpStatus(s.mcp_status())),
            }
        }
        // `/undo` and `/checkpoints` both open the same interactive picker over the per-turn
        // checkpoints — pick any past message to rewind (chat + files) to. Enter acts in
        // picker_accept.
        CommandAction::Undo => open_checkpoint_picker(session, app, "rewind to a message").await?,
        CommandAction::ListCheckpoints => {
            open_checkpoint_picker(session, app, "restore a checkpoint").await?
        }
        CommandAction::Checkpoint(name) => {
            {
                let mut s = session.lock().await;
                s.checkpoint(name.as_deref())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            match name {
                Some(n) => app.note(&format!("✓ checkpoint saved: {n}")),
                None => app.note("✓ checkpoint saved"),
            }
        }
        // `/compact` makes a model call → run it as a background task so the spinner ticks.
        CommandAction::Compact => return Ok(DispatchOutcome::RunCompact),
        CommandAction::Lattice(symbol) => {
            if symbol.is_empty() {
                app.note("usage: /lattice <symbol>");
            } else {
                let view = { session.lock().await.lattice_view(&symbol)? };
                match view {
                    None => app.note("lattice is disabled (set [lattice] enabled = true)"),
                    Some(v) => {
                        let rows = |hits: &[forge_index::NodeHit]| {
                            hits.iter()
                                .map(|h| {
                                    (h.kind.clone(), h.name.clone(), h.rel_path.clone(), h.line)
                                })
                                .collect::<Vec<_>>()
                        };
                        let why = v.why.map(|p| (p.author, p.date, p.commit, p.subject));
                        let lines = forge_tui::lattice_view_lines(
                            &v.query,
                            &rows(&v.roots),
                            &rows(&v.dependents),
                            why,
                        );
                        tui.insert_lines(lines);
                    }
                }
            }
        }
        // `/goal <objective>` — pin a persisted north-star, then run a turn that decomposes it
        // into a tracked task plan (update_tasks).
        CommandAction::Goal(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                app.note("usage: /goal <objective> — sets the goal and breaks it into tasks");
                return Ok(DispatchOutcome::Handled);
            }
            {
                let mut s = session.lock().await;
                s.prime_guidance(&[format!(
                    "Session goal: {text}\nKeep every step aligned to this goal until it is fully met."
                )])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            app.note(&format!("🎯 goal set — {text}"));
            return Ok(DispatchOutcome::RunTurn {
                prompt: format!(
                    "Break this goal into a concrete, ordered plan and record it with the \
                     update_tasks tool, then start on the first step.\n\nGoal: {text}"
                ),
                guidance: Vec::new(),
                tier: Some(forge_types::TaskTier::Complex),
            });
        }
        // `/loop <task>` — autonomous re-run until the model signals completion.
        CommandAction::Loop(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                app.note("usage: /loop <task> — re-runs until the model signals it's complete");
                return Ok(DispatchOutcome::Handled);
            }
            return Ok(DispatchOutcome::StartLoop { prompt: text });
        }
        // `/replay <id>` — show a transcript inline; `/replay <a> <b>` diffs two sessions.
        CommandAction::Replay(id_a, id_b) => {
            if id_a.is_empty() {
                app.note("usage: /replay <id>  or  /replay <id-a> <id-b>");
                return Ok(DispatchOutcome::Handled);
            }
            let text = {
                let s = session.lock().await;
                match id_b {
                    None => {
                        // resolve prefix → full id, load, render
                        let ids = s
                            .matching_session_ids(&id_a)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        match ids.first() {
                            None => format!("no session matching '{id_a}'"),
                            Some(full) => {
                                let entries = s
                                    .load_replay(full)
                                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                                crate::replay::render_transcript(
                                    &full[..full.len().min(8)],
                                    &entries,
                                )
                            }
                        }
                    }
                    Some(id_b) => {
                        let ids_a = s
                            .matching_session_ids(&id_a)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        let ids_b = s
                            .matching_session_ids(&id_b)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        match (ids_a.first(), ids_b.first()) {
                            (Some(fa), Some(fb)) => {
                                let ea = s
                                    .load_replay(fa)
                                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                                let eb = s
                                    .load_replay(fb)
                                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                                let d = crate::replay::diff(&ea, &eb);
                                let fa8 = &fa[..fa.len().min(8)];
                                let fb8 = &fb[..fb.len().min(8)];
                                let mut out =
                                    crate::replay::render_diff(fa8, fb8, &d);
                                out.push('\n');
                                out.push_str(&crate::replay::render_turn_diff(
                                    fa8, fb8, &ea, &eb,
                                ));
                                out
                            }
                            (None, _) => format!("no session matching '{id_a}'"),
                            (_, None) => format!("no session matching '{id_b}'"),
                        }
                    }
                }
            };
            tui.print_text(&text);
        }
        // Not a builtin → try the file-based command/skill catalog.
        CommandAction::Unknown(_) => {
            return dispatch_catalog(line, catalog, session, app, armed, trust_project, busy).await
        }
    }
    Ok(DispatchOutcome::Handled)
}

/// Resolve a `/line` that isn't a builtin against the file catalog: expand a command, load a
/// skill's methodology, or report a missing-arg / unknown error. A project-scope definition is
/// gated on first use (re-run confirms) unless `trust_project`.
async fn dispatch_catalog(
    line: &str,
    catalog: &forge_skills::Catalog,
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
    armed: &mut std::collections::HashSet<String>,
    trust_project: bool,
    busy: bool,
) -> Result<DispatchOutcome> {
    use forge_skills::Resolved;
    match catalog.resolve(line) {
        Resolved::Command {
            cmd,
            prompt,
            guidance,
        } => {
            if busy {
                app.note("⚠ finish or Esc the current turn first");
                return Ok(DispatchOutcome::Handled);
            }
            if !project_trust_ok(&cmd.name, cmd.scope, trust_project, armed, app) {
                return Ok(DispatchOutcome::Handled);
            }
            app.note(&format!(
                "⚒ command · /{} ({})",
                cmd.name,
                cmd.scope.label()
            ));
            Ok(DispatchOutcome::RunTurn {
                prompt,
                guidance,
                tier: cmd.tier,
            })
        }
        Resolved::Skill { meta, prompt } => {
            if busy {
                app.note("⚠ finish or Esc the current turn first");
                return Ok(DispatchOutcome::Handled);
            }
            if !project_trust_ok(&meta.name, meta.scope, trust_project, armed, app) {
                return Ok(DispatchOutcome::Handled);
            }
            let skill = forge_skills::Skill::load(&meta);
            for w in &skill.warnings {
                app.note(&format!("⚠ {w}"));
            }
            app.note(&format!("⚒ skill · {} ({})", meta.name, meta.scope.label()));
            if !skill.resources.is_empty() {
                app.note(&format!(
                    "↳ loaded methodology + {} resource(s)",
                    skill.resources.len()
                ));
            }
            let guidance = vec![skill.guidance()];
            if prompt.trim().is_empty() {
                // No task given: prime the methodology into the transcript (no model call) so it
                // shapes the next turn the user types.
                {
                    let mut s = session.lock().await;
                    s.prime_guidance(&guidance)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                app.note("↳ methodology primed — type your task");
                Ok(DispatchOutcome::Handled)
            } else {
                Ok(DispatchOutcome::RunTurn {
                    prompt,
                    guidance,
                    tier: meta.tier,
                })
            }
        }
        Resolved::MissingArgs { name, missing } => {
            let need = missing
                .iter()
                .map(|m| format!("<{m}>"))
                .collect::<Vec<_>>()
                .join(" ");
            app.note(&format!("/{name} requires {need}"));
            Ok(DispatchOutcome::Handled)
        }
        Resolved::Unknown(x) => {
            app.note(&format!("unknown command: /{x} — try /help"));
            Ok(DispatchOutcome::Handled)
        }
        // A `/`-line never resolves to Plain, but stay safe rather than silently submit it.
        Resolved::Plain(_) => {
            app.note("unknown command — try /help");
            Ok(DispatchOutcome::Handled)
        }
    }
}

/// First use of a *project*-scope command/skill is confirmed by re-running it (its name is
/// "armed" on the first attempt and runs on the second) — unless project scope is trusted. User-
/// scope and builtins are never gated. Returns true when the invocation may proceed.
fn project_trust_ok(
    name: &str,
    scope: forge_skills::Scope,
    trust_project: bool,
    armed: &mut std::collections::HashSet<String>,
    app: &mut forge_tui::App,
) -> bool {
    if scope != forge_skills::Scope::Project || trust_project || armed.contains(name) {
        return true;
    }
    armed.insert(name.to_string());
    app.note(&format!(
        "⚠ /{name} is a project command — it can steer the model. Run it again to confirm."
    ));
    false
}

/// Populate + open the session picker from the store (newest first). `query` pre-fills the filter.
fn open_sessions_picker(app: &mut forge_tui::App, query: &str) -> Result<()> {
    let store = open_store()?;
    let list = store.list_sessions().context("listing sessions")?;
    if list.is_empty() {
        app.note("no past sessions yet");
        return Ok(());
    }
    let rows = list
        .into_iter()
        .take(50)
        .map(|s| {
            let id8: String = s.id.chars().take(8).collect();
            let preview: String = s.preview.unwrap_or_default().chars().take(60).collect();
            forge_tui::PickerRow {
                title: format!(
                    "{id8}  ${:>7.4}  {:>3} msgs  {}",
                    s.total_cost_usd,
                    s.message_count,
                    fmt_age(s.created_at)
                ),
                subtitle: preview,
                id: s.id,
            }
        })
        .collect();
    app.picker
        .open_with(forge_tui::PickerKind::Sessions, "resume a session", rows);
    app.picker.query = query.to_string();
    app.picker.clamp();
    Ok(())
}

/// Read the session's checkpoints (one per turn, newest first) and open the rewind picker.
async fn open_checkpoint_picker(
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
    heading: &str,
) -> Result<()> {
    let rows = {
        let s = session.lock().await;
        checkpoint_rows(&s.checkpoints().map_err(|e| anyhow::anyhow!("{e}"))?)
    };
    if rows.is_empty() {
        app.note("nothing to undo yet");
    } else {
        app.picker
            .open_with(forge_tui::PickerKind::Checkpoints, heading, rows);
    }
    Ok(())
}

/// One picker row per checkpoint, reading as a message list: the prompt preview is the title,
/// with the turn index + age as the subtitle.
fn checkpoint_rows(cps: &[forge_store::CheckpointRow]) -> Vec<forge_tui::PickerRow> {
    cps.iter()
        .map(|c| forge_tui::PickerRow {
            id: c.seq.to_string(),
            title: c
                .label
                .clone()
                .unwrap_or_else(|| format!("turn @ {}", c.seq)),
            subtitle: format!("#{} · {}", c.seq, fmt_age(c.created_at)),
        })
        .collect()
}

/// Build the top-level provider list for the `/models` browser, with a stats heading.
fn models_provider_view(
    cat: &forge_mesh::ModelCatalog,
    pricing: &forge_mesh::pricing::Pricing,
    benched: &forge_types::ModelHealth,
) -> (String, Vec<forge_tui::PickerRow>) {
    let s = cat.stats(pricing);
    let heading = format!(
        "⊞ models — {} total · {} frontier · {} free · {} subscription · {} providers",
        s.total, s.frontier, s.free, s.subscription, s.providers
    );
    let rows = cat
        .by_provider(pricing)
        .into_iter()
        .map(|g| {
            let benched_n = g
                .models
                .iter()
                .filter(|m| benched.is_benched(&m.id))
                .count();
            let mut parts = vec![format!("{} models", g.total())];
            if g.frontier() > 0 {
                parts.push(format!("{} frontier", g.frontier()));
            }
            if g.free() > 0 {
                parts.push(format!("{} free", g.free()));
            }
            if g.paid() > 0 {
                parts.push(format!("{} paid", g.paid()));
            }
            if benched_n > 0 {
                parts.push(format!("{benched_n} benched"));
            }
            forge_tui::PickerRow {
                id: g.provider.clone(),
                title: g.provider.clone(),
                subtitle: parts.join(" · "),
            }
        })
        .collect();
    (heading, rows)
}

/// Build the drill-in model list for one provider (Enter on a provider row).
fn models_for_provider(
    cat: &forge_mesh::ModelCatalog,
    pricing: &forge_mesh::pricing::Pricing,
    benched: &forge_types::ModelHealth,
    provider: &str,
) -> (String, Vec<forge_tui::PickerRow>) {
    let rows: Vec<forge_tui::PickerRow> = cat
        .by_provider(pricing)
        .into_iter()
        .find(|g| g.provider == provider)
        .map(|g| {
            g.models
                .iter()
                .map(|m| {
                    let name = if m.name.is_empty() {
                        "(default model)".to_string()
                    } else {
                        m.name.clone()
                    };
                    let mut badges: Vec<String> = Vec::new();
                    if m.subscription {
                        badges.push("subscription".into());
                    }
                    if m.frontier {
                        badges.push("frontier".into());
                    }
                    if m.free {
                        badges.push("free".into());
                    }
                    if m.cost > f64::EPSILON {
                        badges.push(format!("paid ~${:.4}/turn", m.cost));
                    } else if m.paid {
                        badges.push("paid".into()); // metered gateway model, price unknown
                    }
                    if benched.is_benched(&m.id) {
                        badges.push("benched".into());
                    }
                    forge_tui::PickerRow {
                        id: m.id.clone(),
                        title: name,
                        subtitle: badges.join(" · "),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let heading = format!("⊞ {provider} — {} model(s)  ·  esc: back", rows.len());
    (heading, rows)
}

/// Open the `/models` browser at the top-level provider list (also the Esc target from a drill-in).
async fn open_models_root(
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
) -> Result<()> {
    let benched = open_store()?.current_benched().unwrap_or_default();
    let view = {
        let s = session.lock().await;
        s.catalog()
            .map(|c| models_provider_view(c, s.pricing(), &benched))
    };
    match view {
        Some((heading, rows)) if !rows.is_empty() => {
            app.models_drilled = None;
            app.picker
                .open_with(forge_tui::PickerKind::Models, &heading, rows);
        }
        Some(_) => app.note(
            "no models discovered — set a provider key (`forge auth <provider>`) or run ollama",
        ),
        None => app.note("model discovery is off (mock/offline) — nothing to browse"),
    }
    Ok(())
}

/// Drill the `/models` browser into one provider's models.
async fn open_models_provider(
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
    provider: &str,
) -> Result<()> {
    let benched = open_store()?.current_benched().unwrap_or_default();
    let view = {
        let s = session.lock().await;
        s.catalog()
            .map(|c| models_for_provider(c, s.pricing(), &benched, provider))
    };
    if let Some((heading, rows)) = view {
        app.models_drilled = Some(provider.to_string());
        app.picker
            .open_with(forge_tui::PickerKind::Models, &heading, rows);
    }
    Ok(())
}

/// Act on the picker's selected row: resume the chosen session, or rewind to the chosen
/// checkpoint — then redraw the surviving transcript into scrollback.
async fn picker_accept(
    kind: forge_tui::PickerKind,
    row: &forge_tui::PickerRow,
    session: &Arc<tokio::sync::Mutex<Session>>,
    tui: &mut forge_tui::Tui,
    app: &mut forge_tui::App,
) -> Result<()> {
    match kind {
        forge_tui::PickerKind::Sessions => {
            let history = {
                let mut s = session.lock().await;
                s.reset_resumed(&row.id)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                s.history()
            };
            tui.clear_screen();
            app.note(&format!(
                "● resumed {}",
                row.id.chars().take(8).collect::<String>()
            ));
            app.replay_history(&history);
        }
        forge_tui::PickerKind::Checkpoints => {
            let seq: i64 = row.id.parse().unwrap_or(0);
            let (history, outcome) = {
                let mut s = session.lock().await;
                let outcome = s.rewind_to(seq).map_err(|e| anyhow::anyhow!("{e}"))?;
                (s.history(), outcome)
            };
            tui.clear_screen();
            app.note("● rewound to that point");
            app.replay_history(&history);
            note_restore(app, &outcome.restore);
            // Put the rewound-to message back in the input box so it can be edited/resubmitted.
            if let Some(prompt) = outcome.rewound_prompt {
                app.input = prompt;
            }
        }
        forge_tui::PickerKind::Tempers => {
            if let Some(mode) = forge_types::PermissionMode::from_label(&row.id) {
                let label = {
                    let mut s = session.lock().await;
                    s.set_temper(mode).label()
                };
                app.set_temper(label);
                app.note(&format!("◆ mode → {label}"));
            }
        }
        // Assay's choice is handled in the render loop (it spawns a background task), never here.
        forge_tui::PickerKind::AssayChoice => {}
        // The models browser drills/steps within the render loop; Enter never resolves here.
        forge_tui::PickerKind::Models => {}
    }
    Ok(())
}

/// Surface what an undo/restore did to the user's files.
fn note_restore(app: &mut forge_tui::App, report: &forge_core::snapshot::RestoreReport) {
    if !report.restored.is_empty() {
        app.note(&format!("↺ restored {} file(s)", report.restored.len()));
    }
    for w in &report.warnings {
        app.note(&format!(
            "⚠ {w} changed since Forge wrote it — overwrote your edit"
        ));
    }
}

/// A short relative age like "3m ago" / "2h ago" / "5d ago" from an epoch-second timestamp.
fn fmt_age(created_at: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (now - created_at).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_catalog_assets_imports_then_skips_existing() {
        // A Codex-style prompt: plain markdown, no frontmatter (name = file stem, description =
        // first body line). The lenient command reader must accept it and we must copy it.
        let root = std::env::temp_dir().join(format!("forge-imp-{}", forge_types::new_id()));
        let src = root.join("prompts");
        let cmd_dst = root.join("out/commands");
        let skill_dst = root.join("out/skills");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("refactor.md"),
            "Refactor the selected code cleanly.\n",
        )
        .unwrap();

        let sources = forge_skills::Sources {
            commands: vec![forge_skills::ScopedDir {
                scope: forge_skills::Scope::User,
                path: src.clone(),
            }],
            skills: vec![],
        };
        let cat = forge_skills::Catalog::load(&sources);

        let first = copy_catalog_assets(&cat, &cmd_dst, &skill_dst);
        assert_eq!(first.copied_commands, 1, "the prompt was imported");
        assert_eq!(first.copied_skills, 0);
        assert!(cmd_dst.join("refactor.md").exists());

        // Re-running keeps the existing file instead of overwriting it.
        let second = copy_catalog_assets(&cat, &cmd_dst, &skill_dst);
        assert_eq!(second.copied_commands, 0);
        assert_eq!(second.skipped_commands, 1, "already present → skipped");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loop_stops_on_sentinel_or_iteration_cap() {
        // Keeps looping while the model hasn't signalled done and we're under the cap.
        assert!(loop_stop_reason(Some("still working on it"), 1).is_none());
        // Stops the moment the completion token appears.
        assert!(loop_stop_reason(Some("all green now\nLOOP_COMPLETE"), 3).is_some());
        // Stops at the hard iteration cap even without the token.
        assert!(loop_stop_reason(Some("more to do"), LOOP_MAX_ITERS).is_some());
        // No assistant text yet → not complete, keep going (under cap).
        assert!(loop_stop_reason(None, 1).is_none());
    }

    #[test]
    fn interactive_logs_go_to_a_file_never_the_tui() {
        // The crash: genai logged a 429 body to stderr, shredding the inline TUI. Interactive
        // runs must route logs to a file; only pipes/CI write to stderr.
        assert_eq!(log_target(true), LogTarget::File);
        assert_eq!(log_target(false), LogTarget::Stderr);
    }

    fn models_catalog() -> forge_mesh::ModelCatalog {
        forge_mesh::ModelCatalog::new(vec![
            "anthropic::claude-opus-4-8".into(),
            "groq::llama-3.1-8b-instant".into(),
            "groq::llama-3.3-70b-versatile".into(),
            "claude-cli::".into(),
        ])
    }

    #[test]
    fn models_provider_view_heading_has_counts_and_rows_per_provider() {
        let cat = models_catalog();
        let pricing = forge_mesh::pricing::Pricing::default();
        let (heading, rows) = models_provider_view(&cat, &pricing, &Default::default());
        assert!(heading.contains("4 total"), "heading counts: {heading}");
        assert!(heading.contains("2 frontier") && heading.contains("1 subscription"));
        // groq has 2 models → it's the first (richest) provider row.
        assert_eq!(rows[0].id, "groq");
        assert!(rows[0].subtitle.contains("2 models"));
        // every provider row is a header (no `::` in id) so the browser knows it can drill.
        assert!(rows.iter().all(|r| !r.id.contains("::")));
    }

    #[test]
    fn models_for_provider_lists_models_with_badges() {
        let cat = models_catalog();
        let pricing = forge_mesh::pricing::Pricing::default();
        let (heading, rows) = models_for_provider(&cat, &pricing, &Default::default(), "groq");
        assert!(heading.contains("groq") && heading.contains("esc: back"));
        assert_eq!(rows.len(), 2);
        // model rows carry the full id (so Enter on them is a no-op, not a drill) + badges.
        assert!(rows.iter().all(|r| r.id.contains("::")));
        let frontier = rows.iter().find(|r| r.id.contains("70b")).unwrap();
        assert!(frontier.subtitle.contains("frontier") && frontier.subtitle.contains("free"));
        // the bare subscription bridge shows "(default model)" as its name + a subscription badge.
        let (_, sub) = models_for_provider(&cat, &pricing, &Default::default(), "claude-cli");
        assert_eq!(sub[0].title, "(default model)");
        assert!(sub[0].subtitle.contains("subscription"));
    }

    #[test]
    fn onboarding_only_when_nothing_is_configured() {
        // Fresh machine: no key, no bridge, no config → onboard.
        assert!(needs_onboarding(false, false, false));
        // Any one signal of prior setup suppresses it.
        assert!(!needs_onboarding(true, false, false)); // has a key
        assert!(!needs_onboarding(false, true, false)); // a bridge is installed
        assert!(!needs_onboarding(false, false, true)); // a saved config exists
    }

    #[test]
    fn bridge_plans_cover_both_clis_with_stored_slugs() {
        let claude = bridge_plans(forge_provider::CliKind::ClaudeCode);
        assert!(claude.iter().any(|(_, slug)| *slug == "max-20x"));
        let codex = bridge_plans(forge_provider::CliKind::Codex);
        assert!(codex.iter().any(|(_, slug)| *slug == "plus"));
        // Every plan has a non-empty human label + slug.
        for (label, slug) in claude.iter().chain(codex) {
            assert!(!label.is_empty() && !slug.is_empty());
        }
    }

    #[test]
    fn bundle_source_collects_source_and_skips_build_dirs() {
        let dir = std::env::temp_dir().join(format!("forge-bundle-{}", forge_types::new_id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("target/junk.rs"), "GENERATED").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored ext").unwrap();

        let out = bundle_source(&dir, 100_000);
        assert!(out.contains("fn main()"), "source included: {out}");
        assert!(out.contains("FILE:"), "file headers present");
        assert!(!out.contains("GENERATED"), "target/ skipped");
        assert!(!out.contains("ignored ext"), "non-source ext skipped");
        std::fs::remove_dir_all(&dir).ok();
    }

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
