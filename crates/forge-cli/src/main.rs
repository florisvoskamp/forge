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

/// Build a session around a caller-provided presenter, wiring all subsystems.
fn build_session_with(
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
    let provider: Arc<dyn Provider> = if mock {
        Arc::new(MockProvider)
    } else {
        // Routes API models to genai and `claude-cli::`/`codex-cli::` to the subscription CLI
        // bridge. `harness` mode runs the bridge's tools through Forge's MCP server (RFC Phase 2).
        let harness = config.mesh.bridge_mode == forge_config::BridgeMode::Harness;
        Arc::new(DispatchProvider::new(harness))
    };
    let heuristic = HeuristicRouter::new(config.clone()).with_pin(pin);
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
fn build_session(
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
    build_session_with(presenter, mock, mode, resume, pin)
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
    let mut session = build_session(mock, mode, tui, resume, pin)?;
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
    )?;
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

/// Sends the turn-complete signal on drop — so `busy` is released even if the turn task
/// panics. Without this, a panic would skip the send and freeze the UI on the spinner.
struct DoneGuard(std::sync::mpsc::Sender<()>);
impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.send(());
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
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let session = build_session_with(Box::new(ChannelPresenter::new(tx)), mock, mode, resume, pin)?;
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(session));

    let mut tui = Tui::new().context("initializing TUI")?;
    // The welcome banner is a one-time print into scrollback (not a render branch).
    tui.insert_lines(banner_lines(tui.width()));
    let mut app = App::default();
    let mut busy = false;
    let mut pending: Option<std::sync::mpsc::Sender<bool>> = None;
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
            if let Some(reply) = pending.take() {
                // Answering a permission prompt.
                let yes = matches!(
                    key,
                    KeyKind::Char('y') | KeyKind::Char('Y') | KeyKind::Enter
                );
                let _ = reply.send(yes);
                app.prompt = None;
            } else if busy {
                if matches!(key, KeyKind::Esc) {
                    quit = true;
                    break;
                }
            } else {
                match handle_key(&mut app.input, key) {
                    InputOutcome::Submit(line) => {
                        app.submit_user(&line);
                        app.done = false;
                        app.tick = 0;
                        busy = true;
                        busy_since = Instant::now();
                        let s = session.clone();
                        let dt = done_tx.clone();
                        tokio::spawn(async move {
                            // DoneGuard fires on the way out — normal return OR panic unwind —
                            // so a panicking turn can never leave the UI stuck "working".
                            let _done = DoneGuard(dt);
                            let mut sess = s.lock().await;
                            if let Err(e) = sess.run_turn(&line).await {
                                sess.notify_error(&format!("turn failed: {e}"));
                            }
                        });
                    }
                    InputOutcome::Quit => {
                        quit = true;
                        break;
                    }
                    InputOutcome::Editing => {}
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
            }
        }

        if busy && done_rx.try_recv().is_ok() {
            busy = false;
            dirty = true;
        }
        if busy {
            let t = (busy_since.elapsed().as_millis() / 60) as usize;
            if t != app.tick {
                app.tick = t;
                dirty = true;
            }
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
