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
    /// Show the auto-discovered model catalog and the mesh's best pick per tier.
    Models {
        /// Actively ping every discovered model and persist the result: clear healthy ones,
        /// bench the ones that rate-limit / fail auth (so the mesh routes around them).
        #[arg(long)]
        probe: bool,
    },
    /// Internal: run Forge's tool registry as an MCP server on stdio (spawned by the CLI
    /// bridge so claude/codex use Forge's tools under Forge's permission gate). Not for direct use.
    #[command(hide = true)]
    McpServe,
    /// Store a provider API key securely in the OS keyring (reads the key from stdin).
    Auth {
        /// Provider: anthropic, openai, gemini, xai, deepseek, or openrouter.
        provider: String,
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
        Command::Models { probe } => models(probe).await,
        Command::Auth { provider } => auth(&provider),
        Command::McpServe => mcp_serve::run().await,
    }
}

fn auth(provider: &str) -> Result<()> {
    if !forge_config::known_key_providers().any(|p| p == provider) {
        let known: Vec<_> = forge_config::known_key_providers().collect();
        anyhow::bail!(
            "unknown provider '{provider}' — key-based providers are: {}",
            known.join(", ")
        );
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
async fn discover_catalog() -> forge_mesh::ModelCatalog {
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
    forge_mesh::ModelCatalog::new(models)
}

/// `forge models [--probe]`: discover the usable models + show the mesh's capability-ranked pick
/// per tier. With `--probe`, also ping each model and persist health (the user-driven rescan).
async fn models(probe: bool) -> Result<()> {
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();
    let cat = discover_catalog().await;
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

    println!("discovered {} usable models:", cat.models().len());
    let benched = store.current_benched().unwrap_or_default();
    for m in cat.models() {
        let mark = if benched.is_benched(m) {
            "  (benched)"
        } else {
            ""
        };
        println!("  {m}{mark}");
    }
    let pricing = forge_mesh::pricing::Pricing::from_config(&config);
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

async fn build_session_with(
    presenter: Box<dyn Presenter>,
    mock: bool,
    mode: Option<Mode>,
    resume: Option<String>,
    pin: Option<String>,
) -> Result<Session> {
    // Make any keyring-stored provider keys visible to the provider client.
    forge_config::inject_provider_keys();

    let mut config = forge_config::load().context("loading configuration")?;
    if let Some(m) = mode {
        config.permission_mode = m.into();
    }

    let store = Arc::new(open_store()?);
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
        Some(discover_catalog().await)
    } else {
        None
    };
    let (provider, router) = build_provider_and_router(&config, mock, pin, catalog);
    let tools = ToolRegistry::with_core_tools();

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

async fn chat(
    mock: bool,
    mode: Option<Mode>,
    resume: Option<String>,
    plain: bool,
    pin: Option<String>,
) -> Result<()> {
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
    let mut busy = false;
    // Each turn gets a monotonic generation; the abort handle lets Esc interrupt it (RFC
    // session-management). The current gen gates the done-signal so an aborted turn's late
    // signal is ignored once a new turn has started.
    let mut turn_gen: u64 = 0;
    let mut turn_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut pending: Option<std::sync::mpsc::Sender<bool>> = None;
    let mut pending_question: Option<std::sync::mpsc::Sender<String>> = None;
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
                        if let Some(name) = app.palette.selected_name() {
                            app.input = format!("/{name}");
                            app.palette.query = name.to_string();
                            app.palette.clamp();
                        }
                    }
                    KeyKind::Enter => {
                        let line = app
                            .palette
                            .selected_name()
                            .map(|n| format!("/{n}"))
                            .unwrap_or_else(|| app.input.clone());
                        app.palette.close();
                        app.input.clear();
                        if dispatch_command(&line, &session, &mut tui, &mut app, busy).await? {
                            quit = true;
                            break;
                        }
                    }
                    KeyKind::Char(c) => {
                        app.input.push(c);
                        if app.input.starts_with("//") {
                            app.palette.close(); // `//` escapes to a literal prompt
                        } else {
                            app.palette.query = app.input[1..].to_string();
                            app.palette.clamp();
                        }
                    }
                    KeyKind::Backspace => {
                        app.input.pop();
                        if app.input.starts_with('/') {
                            app.palette.query = app.input[1..].to_string();
                            app.palette.clamp();
                        } else {
                            app.palette.close();
                        }
                    }
                    KeyKind::CycleTemper => {}
                }
                continue;
            }

            // The session/checkpoint picker is modal too: arrows navigate, typing filters, Enter
            // acts on the selection (resume / rewind), Esc cancels.
            if app.picker.open {
                match key {
                    KeyKind::Esc => app.picker.close(),
                    KeyKind::Up => app.picker.move_up(),
                    KeyKind::Down => app.picker.move_down(),
                    KeyKind::Enter => {
                        let chosen = app.picker.selected_row().cloned();
                        let kind = app.picker.kind;
                        app.picker.close();
                        if let (Some(row), Some(kind)) = (chosen, kind) {
                            picker_accept(kind, &row, &session, &mut tui, &mut app).await?;
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
                    KeyKind::Tab | KeyKind::CycleTemper => {}
                }
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
                            if dispatch_command(&line, &session, &mut tui, &mut app, busy).await? {
                                quit = true;
                                break;
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
                        // Typing `/` as the first character opens the command palette.
                        if app.input.starts_with('/') && !app.input.starts_with("//") {
                            app.palette.open_with(&app.input[1..]);
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

/// Execute a slash command (RFC session-management-and-commands, PR1). Returns `Ok(true)` to
/// quit. Session-mutating commands (`/new`, `/resume`, `/clear`) are gated while a turn is in
/// flight (the turn task holds the session `Mutex`). All session access is `lock().await` — no
/// blocking on the render-loop thread (the #45 invariant).
async fn dispatch_command(
    line: &str,
    session: &Arc<tokio::sync::Mutex<Session>>,
    tui: &mut forge_tui::Tui,
    app: &mut forge_tui::App,
    busy: bool,
) -> Result<bool> {
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
    );
    if busy && mutates {
        app.note("⚠ finish or Esc the current turn first");
        return Ok(false);
    }
    match action {
        CommandAction::Help => app.palette.open_with(""),
        CommandAction::Quit => return Ok(true),
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
        // `/resume [prefix]` and `/sessions` both open the interactive picker; a prefix pre-fills
        // its filter. Resolving + swapping the session happens on Enter (picker_accept).
        CommandAction::Resume(prefix) => open_sessions_picker(app, &prefix)?,
        CommandAction::ListSessions => open_sessions_picker(app, "")?,
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
        CommandAction::Unknown(x) => app.note(&format!("unknown command: /{x} — try /help")),
    }
    Ok(false)
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
    fn interactive_logs_go_to_a_file_never_the_tui() {
        // The crash: genai logged a 429 body to stderr, shredding the inline TUI. Interactive
        // runs must route logs to a file; only pipes/CI write to stderr.
        assert_eq!(log_target(true), LogTarget::File);
        assert_eq!(log_target(false), LogTarget::Stderr);
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
