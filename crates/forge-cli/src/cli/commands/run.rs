use anyhow::{Context, Result};
use std::io::IsTerminal;
use std::sync::Arc;

use forge_core::Session;
use forge_tools::ToolRegistry;
use forge_tui::{HeadlessPresenter, Presenter, TuiPresenter};

use crate::*;

mod atfiles;
pub(crate) use atfiles::*;
mod copy;
pub(crate) use copy::*;
mod pickers;
pub(crate) use pickers::*;
mod dispatch;
pub(crate) use dispatch::*;

/// Keep the command palette in sync with the `/command` token at the cursor (input end): open +
/// filter when one is present anywhere on the line, close when not (`//` escape yields no token).
/// Fill in missing bridge-provider percentages on the usage overlay from the store's
/// `subscription_usage` table (set via rate_limit_event during Forge turns). Used as a
/// fallback when the statusline cache file is stale or missing.
/// Populate the overlay's subscription utilisation %s, preferring the STORE's fractions (seeded
/// from the rate-limit caches at startup AND refreshed live on every CLI-bridge turn via
/// rate_limit_event) over the raw caches. This is the real staleness fix: a fresh Forge claude/
/// codex turn updates the store, so the overlay reflects it instead of the frozen statusline cache.
/// The "Xh ago" note is shown only when the claude reading is still the seeded cache value (i.e. no
/// live turn refreshed it this session) — when a turn has, the value is current and unmarked.
pub(crate) fn fill_subscription_pcts(
    overlay: &mut forge_tui::UsageOverlay,
    fracs: &std::collections::HashMap<String, std::collections::HashMap<String, f64>>,
    bstats: &bridge_stats::BridgeStats,
) {
    let store = |p: &str, w: &str| fracs.get(p).and_then(|m| m.get(w)).copied();
    // Cache as the base; override with the store only when it carries a genuinely DIFFERENT (live,
    // turn-recorded) value, so we never show a store reading staler than the cache. Returns the %
    // and whether it came from a live override.
    let pick = |cache: Option<f64>, st: Option<f64>| -> (Option<f64>, bool) {
        match (st, cache) {
            (Some(s), Some(c)) => {
                let sp = s * 100.0;
                if (sp - c).abs() > 1e-6 {
                    (Some(sp), true)
                } else {
                    (Some(c), false)
                }
            }
            (Some(s), None) => (Some(s * 100.0), true),
            (None, c) => (c, false),
        }
    };
    let (c5, _) = pick(bstats.claude_5h_pct, store("claude-cli", "five_hour"));
    let (cw, cw_live) = pick(bstats.claude_weekly_pct, store("claude-cli", "weekly"));
    overlay.claude_5h_pct = c5;
    overlay.claude_weekly_pct = cw;
    let (x5, _) = pick(bstats.codex_5h_pct, store("codex-cli", "five_hour"));
    let (xw, _) = pick(bstats.codex_weekly_pct, store("codex-cli", "weekly"));
    overlay.codex_5h_pct = x5;
    overlay.codex_weekly_pct = xw;
    // A live turn refreshed the weekly reading → it's current; otherwise surface the cache age.
    overlay.claude_rl_age_secs = if cw_live {
        None
    } else {
        bstats.claude_rl_age_secs
    };
}

pub(crate) fn sync_palette_to_slash_token(app: &mut forge_tui::App) {
    let cur = app.input_cursor.min(app.input.len());
    // Cursor-anchored: drive the palette only from a `/command` token the cursor sits *within*.
    // `slash_token_at` otherwise falls back to the last token on the line, which kept the palette
    // open after a trailing space (so it never closed once you started typing args). Requiring the
    // cursor to be inside the token closes it the moment the cursor moves past the command name.
    let tok = forge_tui::slash_token_at(&app.input, cur).filter(|t| cur >= t.start && cur <= t.end);
    match tok {
        Some(tok) if app.palette.open => {
            app.palette.query = tok.name;
            app.palette.clamp();
        }
        Some(tok) => app.palette.open_with(&tok.name),
        None => app.palette.close(),
    }
}

pub(crate) async fn build_session_with(
    presenter: Box<dyn Presenter>,
    mock: bool,
    mode: Option<Mode>,
    resume: Option<String>,
    pin: Option<String>,
    suppress_mcp_announce: bool,
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
    let mut mcp_config = config.mcp.clone();
    // Self-MCP: inject a sub-Forge MCP agent server so forge_chat / forge_assay are available
    // as native tools. Skipped if already declared (prevents duplicate "forge" prefix).
    if config.self_mcp && !mcp_config.servers.iter().any(|s| s.name == "forge") {
        let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("forge"));
        mcp_config.servers.insert(
            0,
            forge_config::McpServerConfig {
                name: "forge".to_string(),
                transport: forge_config::McpTransport::Stdio {
                    command: exe.to_string_lossy().into_owned(),
                    args: vec!["mcp".to_string(), "agent".to_string()],
                    env: std::collections::HashMap::new(),
                },
                auth: None,
                enabled: true,
            },
        );
    }
    let config_has_mcp = mcp_config.active_servers().next().is_some();
    let lattice_enabled = config.lattice.enabled;
    let config_lattice_watch = config.lattice.watch;
    let config_default_effort = config.mesh.default_effort.clone();

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

    // Normalize legacy underscore-prefix aliases (codex_cli:: → codex-cli::) so that
    // `--model codex_cli::gpt-5.4-mini` works identically to the canonical hyphen form.
    let pin = pin.map(|p| forge_provider::normalize_model_id(&p).into_owned());

    // Auto-discovery: build a live model catalog so the mesh routes to the best usable model
    // (docs/features/auto-discovery-mesh.md). Skipped for the offline mock and when disabled.
    //
    // Cache-first: if a catalog from the last 24 h exists on disk, use it instantly and kick off
    // a background refresh so the NEXT startup is also fast. On first run (or stale cache) we
    // do the full network discovery (bounded at 15 s) and save it for next time.
    let catalog = if !mock && config.mesh.auto_discover {
        if let Some(cached) = load_cached_catalog() {
            // Fast path — instant startup. Refresh in background for the next run.
            let cfg = config.clone();
            tokio::spawn(async move {
                let fresh = discover_catalog(&cfg).await;
                save_catalog(&fresh);
            });
            Some(cached)
        } else {
            // First run or stale cache — block on discovery, then persist the result.
            const DISCOVERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);
            match tokio::time::timeout(DISCOVERY_BUDGET, discover_catalog(&config)).await {
                Ok(cat) => {
                    save_catalog(&cat);
                    Some(cat)
                }
                Err(_) => {
                    presenter.emit(forge_tui::PresenterEvent::Warning(format!(
                        "model auto-discovery exceeded {}s — using built-in defaults for now; run \
                         `forge models` to refresh once your network/providers respond",
                        DISCOVERY_BUDGET.as_secs()
                    )));
                    None
                }
            }
        }
    } else {
        None
    };

    // Validate the pinned model against the catalog so unknown ids fail fast with a clear message
    // rather than a confusing provider "Resolver error" at the first API call.
    if let (Some(id), Some(cat)) = (pin.as_deref(), catalog.as_ref()) {
        if !cat.models().contains(&id.to_string()) {
            let provider_prefix = id.split("::").next().unwrap_or(id);
            let suggestions: Vec<&str> = cat
                .models()
                .iter()
                .filter(|m| m.starts_with(provider_prefix))
                .map(String::as_str)
                .take(5)
                .collect();
            let hint = if suggestions.is_empty() {
                format!("no '{provider_prefix}' models in catalog — run `forge models` to see what's available")
            } else {
                format!("try: {}", suggestions.join(", "))
            };
            presenter.emit(forge_tui::PresenterEvent::Warning(format!(
                "unknown model '{id}' — {hint}"
            )));
        }
    }

    let ctx_windows = crate::open_store()
        .ok()
        .and_then(|s| s.all_model_contexts().ok())
        .unwrap_or_default();
    let (provider, router) =
        build_provider_and_router(&config, mock, pin, catalog.clone(), ctx_windows);

    // Build the code-intelligence index up front so it can be shared between the model-facing
    // `lattice` tool and the turn's auto-injection (code-intelligence.md). Cheap to construct; it
    // reads whatever `forge lattice update` last persisted.
    let lattice = (!mock && lattice_enabled).then(|| {
        let root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        Arc::new(forge_index::Lattice::new(store_for_lattice, &root))
    });
    let mut tools = ToolRegistry::with_core_tools();
    // Opt-in OS sandbox: replace the default shell tool with one that confines filesystem writes
    // to the workspace via Landlock (Linux; no-op elsewhere / on unsupported kernels).
    if config.shell.sandbox {
        let writable = config
            .shell
            .sandbox_writable
            .iter()
            .map(std::path::PathBuf::from)
            .collect();
        tools.register(Box::new(forge_tools::ShellTool {
            policy: forge_tools::SandboxPolicy {
                enabled: true,
                writable,
            },
        }));
    }
    if let Some(lat) = &lattice {
        tools.register(Box::new(forge_tools::LatticeTool::new(Arc::clone(lat))));
        // Auto-index (and auto-embed when enabled) in the background so the graph is fresh without
        // a manual `forge lattice update` — "automatic under the hood". Incremental + non-blocking;
        // the watcher keeps it fresh thereafter. Errors are swallowed (best-effort, additive).
        let lat_bg = Arc::clone(lat);
        let embeddings = config.lattice.embeddings.clone();
        tokio::spawn(async move {
            // `Lattice::update()` is fully synchronous and CPU-bound (walks the repo, tree-sitter
            // parses every file, writes SQLite). Running it inside a plain async task occupies a
            // tokio *worker* thread for its whole duration — on a low-core machine (runtime sized
            // to `num_cpus`) that starves the executor and the first turn's `route_hinted` never
            // gets scheduled, so `forge run` hangs right after `● session`. Offload to the blocking
            // pool so worker threads stay free. (`spawn_blocking` JoinError on panic → treat as
            // "not updated" rather than propagating.)
            let lat_update = Arc::clone(&lat_bg);
            let updated = tokio::task::spawn_blocking(move || lat_update.update().is_ok())
                .await
                .unwrap_or(false);
            if updated {
                if let Some((embedder, _)) = forge_provider::select_embedder(&embeddings) {
                    let _ = lat_bg.embed_pending(embedder.as_ref(), 64).await;
                }
            }
        });
    }

    let lsp_config = config.lsp.clone();
    let mut session = match resume {
        Some(ref prefix) => {
            let full = resolve_session(&store, prefix)?;
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
    // Seed the effort pin from config if set (`mesh.default_effort`).
    if let Some(ref s) = config_default_effort {
        if let Some(e) = forge_types::EffortLevel::parse(s) {
            session.set_effort(Some(e));
        }
    }
    // Share the index with the session so turns auto-inject relevant code and agent edits reindex
    // in-turn (code-intelligence.md). Empty index → nothing injected (additive guarantee).
    // Also start the background watcher so external editor edits reindex automatically.
    if let Some(lat) = &lattice {
        if config_lattice_watch {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            // Scope the recursive watch to the nearest PROJECT ROOT, and refuse to watch all of
            // $HOME (pathological: pulls in .cargo / cloned .git trees / caches → thousands of
            // inotify watches + a slow initial walk). `None` ⇒ no sensible root → skip the watcher.
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            match forge_index::resolve_watch_root(&cwd, home.as_deref()) {
                None => session.notify_error(
                    "watch & reindex skipped: launched in the home directory with no project root \
                     — open a project folder (one with a .git) to enable auto-reindex",
                ),
                Some(root) => {
                    // Build the watcher on a detached thread and DELIVER it to the session through a
                    // channel, so NOTHING about watcher setup gates TUI startup — not a recursive
                    // inotify registration (which blocks uninterruptibly on WSL2's 9p DrvFs and used
                    // to hang `forge chat`), nor the polling backend's synchronous initial tree scan
                    // (slow over a remote/9p link). On a non-native fs spawn_watcher transparently
                    // uses polling so auto-reindex still works there. The session holds the receiver,
                    // so the watcher is owned per-session and dropped when the session ends (no leak
                    // across repeated build_session calls — bench/replay); the thread exits after the
                    // send. A setup error is non-fatal and intentionally silent (no caveat).
                    let lat2 = Arc::clone(lat);
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        if let Ok(watcher) = forge_index::spawn_watcher(
                            lat2,
                            &root,
                            std::time::Duration::from_millis(400),
                        ) {
                            let _ = tx.send(watcher);
                        }
                    });
                    session.set_lattice_watcher(Some(rx));
                }
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
    // whole listing once on a fresh session (resume suppresses it — the transcript separator
    // already orients the user, and the MCP panel is always reachable via `/mcp`).
    if !mock && config_has_mcp {
        // Connect MCP servers in the BACKGROUND so a slow/unreachable server can't delay TUI startup
        // by up to connect_timeout (20s default per server) — the same non-blocking pattern
        // `mcp-serve` uses. `connecting()` marks every active server `Reconnecting` and advertises
        // the MCP meta-tools immediately (so `is_empty()` is false and the tool surface is ready),
        // then a detached task connects them; each flips to connected/failed in the `/mcp` panel as
        // it resolves, and the first `mcp_call` lazily waits on its own server. No startup op should
        // gate the UI (cf. the 9p watcher hang).
        let manager = std::sync::Arc::new(forge_mcp::McpManager::connecting(&mcp_config));
        let bg = std::sync::Arc::clone(&manager);
        tokio::spawn(async move { bg.connect_active().await });
        session.set_mcp(Some(manager));
        if resume.is_none() && !suppress_mcp_announce {
            session.announce_mcp();
        }
    }
    if lsp_config.enabled {
        session.set_lsp(Some(std::sync::Arc::new(
            forge_lsp::LspRegistry::from_config(&lsp_config),
        )));
    }
    Ok(session)
}

/// Build a session with the default surface (TUI on a tty, else plain).
pub(crate) async fn build_session(
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
    build_session_with(presenter, mock, mode, resume, pin, false).await
}

pub(crate) async fn run(
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

pub(crate) async fn nl_cmd(query: String, mode: Option<Mode>) -> Result<()> {
    if query.trim().is_empty() {
        anyhow::bail!(
            "empty query — usage: forge nl \"what changed performance-wise since last week\""
        );
    }
    // Gather shell context so the model can run the right commands.
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());
    let git_ctx = {
        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        let log = std::process::Command::new("git")
            .args(["log", "--oneline", "-8"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        match (branch, log) {
            (Some(b), Some(l)) if !l.is_empty() => {
                format!("\n- Git branch: {b}\n- Recent commits:\n{l}")
            }
            (Some(b), _) => format!("\n- Git branch: {b}"),
            _ => String::new(),
        }
    };
    let platform = std::env::consts::OS;
    let guidance = format!(
        "You are a shell expert. The user asks a natural-language question about their system \
or codebase. Determine which shell commands answer it, run them with the shell tool, then \
synthesize a clear, direct answer. Do not explain what you are about to do — just run \
commands and explain the output. Be concise.\n\
\n\
Environment:\n\
- Working directory: {cwd}\n\
- Platform: {platform}{git_ctx}"
    );
    let mut session = build_session(false, mode, false, None, None).await?;
    session
        .run_turn_with(&query, &[guidance], None)
        .await
        .context("nl query")?;
    Ok(())
}

/// On a fresh machine (no keys, no bridge, no config) offer the `forge init` wizard before the
/// first chat. Skipped for `--mock`, non-interactive shells, and once anything is configured.
/// Declining writes an (empty) config so we don't nag on every launch.
pub(crate) fn maybe_first_run_setup(mock: bool) -> Result<()> {
    if mock || !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(());
    }
    let has_any_key = forge_config::known_key_providers().any(forge_config::has_api_key);
    let any_bridge = forge_provider::CliKind::all().iter().any(|k| k.available());
    if !needs_onboarding(has_any_key, any_bridge, forge_config::user_config_exists()) {
        return Ok(());
    }
    println!("⚒ Welcome to Forge — no providers are configured yet.");
    let yes = prompt_line("Run guided setup now? [Y/n]: ")?;
    if yes.is_empty() || yes.eq_ignore_ascii_case("y") || yes.eq_ignore_ascii_case("yes") {
        setup()?;
    } else {
        // Mark onboarded so we don't ask again; the user can re-run `forge setup` anytime.
        let _ = forge_config::write_subscriptions(&std::collections::HashMap::new());
        println!("Skipped. Run `forge setup` anytime, or `forge auth <provider>` to add a key.");
    }
    Ok(())
}

/// Probe Claude's CURRENT rate limits (both windows, via the `claude --debug` headers) and record
/// them into the session store. Best-effort; the caller gates it on staleness. This is the live
/// claude-usage source — it replaces the helm-wiped statusline cache.
pub(crate) async fn refresh_claude_quota(session: &std::sync::Arc<tokio::sync::Mutex<Session>>) {
    let limits = tokio::task::spawn_blocking(bridge_stats::probe_claude_limits)
        .await
        .unwrap_or_default();
    if !limits.is_empty() {
        let s = session.lock().await;
        for (w, f) in limits {
            s.seed_subscription_quota("claude-cli", &w, Some(f * 100.0));
        }
    }
}

/// Whether the stored claude quota is older than `max_age` seconds (or absent) — gates the probe.
pub(crate) async fn claude_quota_is_stale(
    session: &std::sync::Arc<tokio::sync::Mutex<Session>>,
    max_age: i64,
) -> bool {
    session
        .lock()
        .await
        .claude_quota_age_secs()
        .is_none_or(|a| a > max_age)
}

pub(crate) async fn chat(
    mock: bool,
    mode: Option<Mode>,
    resume_mode: ResumeMode,
    plain: bool,
    fullscreen: bool,
    pin: Option<String>,
) -> Result<()> {
    maybe_first_run_setup(mock)?;
    maybe_autostart_local();
    // Default to the interactive (animated) TUI on a real terminal.
    if !plain && std::io::stdout().is_terminal() {
        // Update check happens in background inside run_chat_tui (via the UiMsg channel) so it
        // never delays TUI startup. The check has a 3s network timeout — blocking here would
        // freeze the terminal for up to 3s once per day.
        return run_chat_tui(mock, mode, resume_mode, fullscreen, pin).await;
    }
    // Plain path: blocking update check is fine (no TUI to corrupt).
    update_check::maybe_notify(&forge_config::load().unwrap_or_default()).await;

    // Plain line mode: read prompts from stdin.
    // Picker is already ruled out by resolve_resume_mode for headless/plain.
    let resume_id = match resume_mode {
        ResumeMode::Id(id) => Some(id),
        ResumeMode::Fresh | ResumeMode::Picker => None,
    };
    let mut session = build_session_with(
        Box::new(HeadlessPresenter::default()),
        mock,
        mode,
        resume_id,
        pin,
        false,
    )
    .await?;
    if std::io::stdin().is_terminal() {
        println!("forge chat — type a task and press enter; /quit to exit");
    }
    {
        let sid = session.session_id().to_string();
        let hooks = session.hooks().to_vec();
        forge_core::hooks::run_session_hooks(&hooks, forge_config::HookEvent::SessionStart, &sid)
            .await;
    }
    while let Some(line) = session.read_line() {
        match chat_action(&line) {
            ChatAction::Quit => break,
            ChatAction::Skip => continue,
            ChatAction::Run(task) => {
                let hooks = session.hooks().to_vec();
                let task = match forge_core::hooks::run_prompt_hooks(&hooks, &task).await {
                    Ok(t) => t,
                    Err(reason) => {
                        eprintln!("⎇ prompt blocked by hook: {reason}");
                        continue;
                    }
                };
                session
                    .run_turn(&task)
                    .await
                    .context("running agent turn")?;
            }
        }
    }
    {
        let sid = session.session_id().to_string();
        let hooks = session.hooks().to_vec();
        forge_core::hooks::run_session_hooks(&hooks, forge_config::HookEvent::SessionEnd, &sid)
            .await;
    }
    Ok(())
}

/// Sends the turn-complete signal (carrying the turn's generation) on drop — so `busy` is released
/// even if the turn task panics or is aborted. The loop only acts on a signal whose generation
/// matches the current turn, so an interrupted turn's late signal can't end a *later* turn.
pub(crate) struct DoneGuard(pub(crate) std::sync::mpsc::Sender<u64>, pub(crate) u64);

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.0.send(self.1);
    }
}

/// Animated TUI chat loop: renders at ~16fps, runs each turn on a task so a spinner
/// ticks (and streamed tokens flow) while the model works.
/// Emit pre-styled out-of-band lines to the conversation, respecting the viewport mode: inline →
/// the terminal's native scrollback; full-screen → the app's transcript log (since there's no
/// native scrollback in alternate-screen mode).
pub(crate) fn emit_scrollback(
    tui: &mut forge_tui::Tui,
    app: &mut forge_tui::App,
    lines: Vec<forge_tui::ScrollbackLine<'static>>,
) {
    if tui.is_fullscreen() {
        app.push_scrollback(lines);
    } else {
        tui.insert_lines(lines);
    }
}

/// Like [`emit_scrollback`] but for plain (unstyled) multi-line text.
pub(crate) fn emit_text(tui: &mut forge_tui::Tui, app: &mut forge_tui::App, text: &str) {
    if tui.is_fullscreen() {
        app.push_scrollback_text(text);
    } else {
        tui.print_text(text);
    }
}

/// Every editable setting as `/config` editor rows, grouped: "Providers & Keys" (API keys, keyring)
/// first, then the discovered scalar settings (friendly labels, control kind, default, source).
pub(crate) fn config_editor_rows() -> Vec<forge_tui::SettingRow> {
    let mut rows: Vec<forge_tui::SettingRow> = forge_config::known_key_providers()
        .map(|p| forge_tui::SettingRow {
            path: format!("key.{p}"),
            group: "Providers & Keys".to_string(),
            label: format!("{} API key", provider_label(p)),
            help: Some(format!(
                "API key for {p}, stored in the OS keyring. Enter to set; empty to remove."
            )),
            kind: forge_tui::RowKind::Secret,
            value: if forge_config::has_api_key(p) {
                "● set".to_string()
            } else {
                "○ not set".to_string()
            },
            default: String::new(),
            modified: forge_config::has_api_key(p),
            source: "keyring".to_string(),
        })
        .collect();
    rows.extend(forge_config::config_descriptors().into_iter().map(|d| {
        let kind = match d.kind {
            forge_config::SettingKind::Bool => forge_tui::RowKind::Bool,
            forge_config::SettingKind::Int => forge_tui::RowKind::Int,
            forge_config::SettingKind::Float => forge_tui::RowKind::Float,
            forge_config::SettingKind::Text => forge_tui::RowKind::Text,
            forge_config::SettingKind::Enum(opts) => {
                forge_tui::RowKind::Enum(opts.into_iter().map(str::to_string).collect())
            }
        };
        forge_tui::SettingRow {
            path: d.path,
            group: d.group,
            label: d.label,
            help: d.help,
            kind,
            value: d.value.display(),
            default: d.default.display(),
            modified: d.modified,
            source: d.source.to_string(),
        }
    }));
    rows
}

pub(crate) async fn run_chat_tui(
    mock: bool,
    mode: Option<Mode>,
    resume_mode: ResumeMode,
    fullscreen: bool,
    pin: Option<String>,
) -> Result<()> {
    use forge_tui::{
        banner_lines, handle_key, App, ChannelPresenter, InputOutcome, KeyKind, Tui, UiMsg,
    };
    use std::time::{Duration, Instant};

    let (tx, rx) = std::sync::mpsc::channel::<UiMsg>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<u64>();

    // Load config once — shared between update check, session build, and TUI config below.
    let tui_config = forge_config::load().unwrap_or_default();
    // Fire the update check in the background so it never blocks TUI startup.
    // The notification arrives as a Warning in the TUI instead of blocking on a 3s HTTP call.
    update_check::maybe_notify_background(&tui_config, tx.clone());

    // For Picker mode we start a fresh session; the picker fires on the first frame.
    let open_picker_on_start = matches!(resume_mode, ResumeMode::Picker);
    let resume_id = match &resume_mode {
        ResumeMode::Id(id) => Some(id.clone()),
        ResumeMode::Fresh | ResumeMode::Picker => None,
    };
    let tx_mcp = tx.clone(); // clone before tx is moved into ChannelPresenter
    let session = build_session_with(
        Box::new(ChannelPresenter::new(tx)),
        mock,
        mode,
        resume_id,
        pin,
        true, // suppress initial "reconnecting" announce; re-announce fires after connect_active
    )
    .await?;
    // Grab the MCP connect-done receiver before moving the session into the Arc. When the
    // background connect_active() completes, re-announce so the TUI shows connected/failed
    // state rather than the "reconnecting" placeholder from the initial announce.
    let mcp_done_rx = session.mcp_connect_done();
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(session));
    if let Some(mut rx) = mcp_done_rx {
        let s = session.clone();
        let tx2 = tx_mcp;
        tokio::spawn(async move {
            // Wait until connect_active() signals done (or 30s watchdog).
            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    if *rx.borrow() {
                        break;
                    }
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
            let status = s.lock().await.mcp_status();
            if !status.is_empty() {
                let _ = tx2.send(UiMsg::Event(forge_tui::PresenterEvent::McpStatus(status)));
            }
        });
    }

    // Seed the mesh subscription quota at startup so routing + the overlays reflect usage from
    // outside Forge. Codex comes from its rollout files (fresh); claude's stale cache is only a
    // weak fallback — the background probe below fetches claude's CURRENT 5h+weekly utilisation
    // (via the `claude --debug` rate-limit headers) so the store is live within a few seconds.
    {
        // bridge_stats::fetch recursively scans ~/.claude/projects/**/*.jsonl — on a slow FS (WSL
        // /mnt, a huge history) that can stall the first frame. Run it in a background task;
        // the quota overlay refreshes on its own cadence so the numbers fill in within seconds.
        tokio::spawn({
            let s = session.clone();
            async move {
                if let Ok(bstats) = tokio::task::spawn_blocking(bridge_stats::fetch).await {
                    let sess = s.lock().await;
                    sess.seed_subscription_quota("codex-cli", "five_hour", bstats.codex_5h_pct);
                    sess.seed_subscription_quota("codex-cli", "weekly", bstats.codex_weekly_pct);
                    sess.seed_subscription_quota("claude-cli", "five_hour", bstats.claude_5h_pct);
                    sess.seed_subscription_quota("claude-cli", "weekly", bstats.claude_weekly_pct);
                }
            }
        });
    }
    if claude_quota_is_stale(&session, 300).await {
        tokio::spawn({
            let s = session.clone();
            async move { refresh_claude_quota(&s).await }
        });
    }

    // Mouse capture (full-screen wheel scroll) is opt-in: it disables native click-drag text
    // selection, so it stays off unless the user enables `[tui] mouse_capture`.
    let mouse_capture = tui_config.tui.mouse_capture;
    let mut tui = Tui::new(fullscreen, mouse_capture).context("initializing TUI")?;
    let mut app = App::default();
    app.fullscreen = fullscreen;
    app.transcript_follow = true;
    // Welcome banner only on a fresh session — resumes show the transcript separator instead. In
    // full-screen mode there's no native scrollback, so banner lines go into the transcript log.
    if matches!(resume_mode, ResumeMode::Fresh) {
        let banner = banner_lines(tui.width());
        if fullscreen {
            app.push_scrollback(banner);
        } else {
            tui.insert_lines(banner);
        }
    }
    {
        let s = session.lock().await;
        app.temper = s.temper().label().to_string();
        app.effort = s.pinned_effort();
    }

    // Populate the command palette from the skill catalog the session already loaded in
    // build_session_with — avoids a second disk scan of all skill/command dirs.
    let catalog: Arc<forge_skills::Catalog> = {
        let s = session.lock().await;
        // Reuse the Arc the session holds; fall back to a fresh load only if missing.
        s.skills().cloned().unwrap_or_else(|| {
            Arc::new(forge_skills::Catalog::load(&forge_config::command_sources()))
        })
    };
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
    // Git attribution: auto-install the model-aware commit hook when enabled, and remember the
    // flag so each turn's routed model is written where the hook can stamp it.
    let git_coauthor = tui_config.git.coauthor;
    if git_coauthor {
        maybe_install_git_hook(&tui_config);
    }
    {
        let (hooks, sid) = {
            let s = session.lock().await;
            (s.hooks().to_vec(), s.session_id().to_string())
        };
        forge_core::hooks::run_session_hooks(&hooks, forge_config::HookEvent::SessionStart, &sid)
            .await;
    }

    // On a resumed session (`--continue` / `--resume <id>`): render the FULL prior transcript into
    // scrollback (the user sees the entire original conversation, even the parts compaction folded
    // away from the model's view), then a separator marking where new input begins.
    let mut offer_resume_choice = false;
    {
        let s = session.lock().await;
        let items = s.replay_items_full();
        if !items.is_empty() {
            let sid8: String = s.session_id().chars().take(8).collect();
            let n = items.len();
            app.replay_history(&items);
            app.push_resume_separator(&format!("— resumed session {sid8} ({n} entries) —"));
            // Restore the on-screen view (activity panel, viewer, scroll) saved on the last turn,
            // so resume reopens exactly where the user left off.
            if let Some(json) = s.view_snapshot() {
                app.restore_view_json(&json);
            }
            // If this session was compacted, the model only sees a summary. Offer the choice.
            offer_resume_choice = s.was_compacted();
        }
    }

    // For bare `--resume` (Picker mode): open the session picker on the first frame so the user
    // can choose which session to reattach to. Otherwise, if we resumed a previously-compacted
    // session, ask whether to continue compacted or reload the full history into the model's view.
    if open_picker_on_start {
        open_sessions_picker(&mut app, "")?;
    } else if offer_resume_choice {
        open_resume_choice_picker(&mut app);
    }

    // Project-scope commands/skills can steer the model; their first use this session is gated
    // unless trusted. Re-running a gated command confirms it (its name lands here).
    let mut armed_project: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut busy = false;
    // Each turn gets a monotonic generation; the abort handle lets Esc interrupt it (RFC
    // session-management). The current gen gates the done-signal so an aborted turn's late
    // signal is ignored once a new turn has started.
    let mut turn_gen: u64 = 0;
    // Generation of the last auto-compact turn; prevents re-firing before a new user turn updates
    // context_tokens (compact's own Cost event still reflects the old full-context size).
    let mut last_auto_compact_gen: u64 = 0;
    let mut turn_handle: Option<tokio::task::JoinHandle<()>> = None;
    // `/loop` state: when set, each completed turn of this generation is re-run until the model
    // signals completion or the iteration cap is hit.
    let mut loop_state: Option<LoopState> = None;
    let mut pending: Option<(String, std::sync::mpsc::Sender<bool>)> = None;
    let mut pending_question: Option<std::sync::mpsc::Sender<String>> = None;
    // Lens filter set by `/assay --only`/`--skip`; consumed when the AssayChoice picker resolves.
    let mut assay_lenses: Vec<forge_types::FindingCategory> = Vec::new();
    // Scope set by `/assay --diff/--branch/--since/<path>`; consumed when picker resolves.
    let mut assay_scope: forge_types::AssayScope = forge_types::AssayScope::Repo;
    // Baseline for the spinner: deriving the tick from elapsed time keeps the animation
    // speed independent of the loop frequency (one frame per 60ms, exactly as before).
    let mut busy_since = Instant::now();
    // Fixed epoch for idle animations (effort slider rainbow, etc.): unlike busy_since this
    // never resets, so idle animations always have a monotonically increasing tick.
    let anim_epoch = Instant::now();
    // Receivers for overlay background loads (mesh/usage open instantly; data fills in async).
    let mut mesh_load_rx: Option<tokio::sync::oneshot::Receiver<Option<forge_tui::MeshOverlay>>> =
        None;
    let mut usage_load_rx: Option<tokio::sync::oneshot::Receiver<bridge_stats::BridgeStats>> = None;
    // Remote control (`/remote`): when `Some`, a browser can drive the session. The handle owns
    // the server task + the snapshot channel + the input queue; we broadcast a snapshot each
    // dirty frame and drain inputs to inject them like local keystrokes.
    let mut remote: Option<remote::RemoteControl> = None;
    // Only redraw when state actually changed: idle frames cost nothing and the whole
    // conversation isn't rebuilt 16×/sec for no reason.
    let mut dirty = true;
    let mut quit = false;
    // Drives the input-cursor blink. The cursor stays solid while the user is actively typing and
    // only begins a calm blink after a short idle gap (like Claude Code) — measured from the last
    // input event, so it never flickers mid-keystroke.
    let mut last_input_at = std::time::Instant::now();
    // Last model written to `$GIT_DIR/forge-model` for commit attribution (only when coauthor on).
    let mut last_model_written = String::new();
    let mut prompt_history: Vec<String> = Vec::new();
    let mut history_pos: Option<usize> = None;
    let mut history_draft = String::new();
    // Prompts typed while a turn is running, queued to run one-per-turn after it finishes
    // (like Claude Code / aider). Drained in the done-handler below; cleared on interrupt.
    let mut queued_prompts: Vec<String> = Vec::new();
    // One long-lived clipboard for mouse-selection copies (see `copy_selection`). Created once so
    // arboard keeps the X11/Wayland selection alive and never logs a "dropped" warning to the TUI.
    let mut clipboard: Option<arboard::Clipboard> = arboard::Clipboard::new().ok();

    struct ObserverState {
        session_id: String,
        store: std::sync::Arc<forge_store::Store>,
        last_event_id: i64,
        last_poll: std::time::Instant,
    }
    let mut observer: Option<ObserverState> = None;

    while !quit {
        if let Some(obs) = &mut observer {
            if obs.last_poll.elapsed() >= std::time::Duration::from_millis(50) {
                obs.last_poll = std::time::Instant::now();
                if let Ok(events) = obs
                    .store
                    .live_events_after(&obs.session_id, obs.last_event_id)
                {
                    for (id, json) in events {
                        obs.last_event_id = id;
                        if let Ok(ev) =
                            serde_json::from_str::<crate::live_observer::LiveEvent>(&json)
                        {
                            if let Some(pe) = crate::live_observer::live_event_to_presenter(ev) {
                                app.apply(pe);
                                dirty = true;
                            }
                        }
                    }
                }
            }
        }

        // While the in-loop activity viewer is open during a running turn, tick the elapsed-time
        // counter at 1 Hz (it shows whole seconds) and redraw only when it changes, instead of
        // forcing a full repaint every 16 ms.
        if app.viewer.is_some() && busy {
            let new_elapsed = busy_since.elapsed().as_secs();
            if new_elapsed != app.turn_elapsed_secs {
                dirty = true;
            }
        }
        if dirty {
            app.busy = busy;
            if busy {
                app.turn_elapsed_secs = busy_since.elapsed().as_secs();
            }
            tui.draw(&app);
            dirty = false;
        }

        // Drain *all* buffered keystrokes this iteration. Reading one per frame throttled
        while let Some(ev) = tui.poll_event().context("reading input")? {
            dirty = true;
            // Any input counts as activity: hold the cursor solid and restart the idle timer, so
            // the blink only resumes once typing pauses.
            last_input_at = std::time::Instant::now();
            app.cursor_hidden = false;

            if observer.is_some() {
                match ev {
                    forge_tui::InputEvent::Focus(gained) => {
                        app.unfocused = !gained;
                        if gained {
                            app.cursor_hidden = false;
                        }
                    }
                    forge_tui::InputEvent::Scroll { up } => {
                        const STEP: usize = 3;
                        if app.viewer.is_some() {
                            let key = if up { KeyKind::Up } else { KeyKind::Down };
                            for _ in 0..STEP {
                                app.viewer_key(key);
                            }
                        } else if app.fullscreen {
                            if up {
                                app.transcript_scroll_up(STEP);
                            } else {
                                let body = tui.height().saturating_sub(8).max(1);
                                let (_, max_scroll) = app.transcript_metrics(tui.width(), body);
                                app.transcript_scroll_down(STEP, max_scroll);
                            }
                        }
                    }
                    forge_tui::InputEvent::Key(key) => {
                        if matches!(key, KeyKind::Esc) {
                            observer = None;
                            tui.clear_screen();
                            app.clear_transcript();
                            app.input.clear();
                            let _ = open_sessions_picker(&mut app, "");
                            dirty = true;
                        } else if app.fullscreen
                            && matches!(key, KeyKind::PageUp | KeyKind::PageDown)
                        {
                            let body = tui.height().saturating_sub(8).max(1);
                            if matches!(key, KeyKind::PageUp) {
                                app.transcript_scroll_up(body as usize);
                            } else {
                                let (_, max_scroll) = app.transcript_metrics(tui.width(), body);
                                app.transcript_scroll_down(body as usize, max_scroll);
                            }
                            dirty = true;
                        } else if app.fullscreen && matches!(key, KeyKind::JumpBottom) {
                            app.transcript_to_bottom();
                            dirty = true;
                        }
                    }
                    _ => {}
                }
                continue;
            }

            let key = match ev {
                forge_tui::InputEvent::Paste(s) => {
                    // Pasting an image: terminals deliver an empty/whitespace bracketed-paste for
                    // image clipboard content, so on an empty payload probe the OS clipboard for an
                    // image and drop it in as an attachment block. Otherwise it's a normal text paste.
                    if s.trim().is_empty() {
                        if let Some((att, label)) = crate::image_input::clipboard_image() {
                            app.attach_image(att, &label);
                            app.note(&format!("📎 attached image ({label})"));
                            continue;
                        }
                    }
                    app.handle_paste(s);
                    continue;
                }
                forge_tui::InputEvent::Focus(gained) => {
                    // Window focus changed: dim/hollow the input cursor while another window is in
                    // front, restore the solid block on return. Reset the blink phase on regain so
                    // the cursor reappears immediately rather than mid-"off" frame.
                    app.unfocused = !gained;
                    if gained {
                        app.cursor_hidden = false;
                    }
                    continue;
                }
                forge_tui::InputEvent::Scroll { up } => {
                    // Mouse wheel (full-screen only): scroll the open activity viewer, else the
                    // main transcript. A few rows per notch feels natural.
                    const STEP: usize = 3;
                    if app.viewer.is_some() {
                        let key = if up { KeyKind::Up } else { KeyKind::Down };
                        for _ in 0..STEP {
                            app.viewer_key(key);
                        }
                    } else if app.fullscreen {
                        if up {
                            app.transcript_scroll_up(STEP);
                        } else {
                            let body = tui.height().saturating_sub(8).max(1);
                            let (_, max_scroll) = app.transcript_metrics(tui.width(), body);
                            app.transcript_scroll_down(STEP, max_scroll);
                        }
                    }
                    continue;
                }
                forge_tui::InputEvent::Mouse { kind, col, row } => {
                    // Full-screen mouse: drag to select text (copied on release), click the floating
                    // jump-to-bottom bar. Only meaningful in the transcript (not the activity viewer).
                    use forge_tui::MouseKind;
                    if app.fullscreen && app.viewer.is_none() {
                        match kind {
                            MouseKind::Down => {
                                if app.jump_bar_hit(col, row) {
                                    app.transcript_to_bottom();
                                } else {
                                    app.clear_selection();
                                    app.selection_begin(col, row);
                                }
                            }
                            MouseKind::Drag => app.selection_extend(col, row),
                            MouseKind::Up => {
                                if let Some(text) = app.selection_text() {
                                    copy_selection(&mut clipboard, &text);
                                }
                            }
                        }
                    }
                    continue;
                }
                forge_tui::InputEvent::Key(k) => k,
            };

            // The in-loop activity viewer (full-screen mode) is modal while open: it owns every key
            // (scroll / switch entry / Esc to close). Rendered through the main terminal, so there's
            // no nested alternate screen to collide with the chat.
            if app.viewer_key(key) {
                dirty = true;
                continue;
            }

            // The `/config` editor is modal while open: it owns every key (filter / navigate / edit
            // / Tab scope / Esc). The editor returns an action; the shell performs the validated
            // write and refreshes the rows.
            if app.config_editor.open {
                match app.config_editor.handle_key(key) {
                    forge_tui::ConfigAction::Save { path, value } => {
                        let result = if let Some(provider) = path.strip_prefix("key.") {
                            // Secret: store/remove the API key in the OS keyring (never config.toml).
                            if value.trim().is_empty() {
                                forge_config::remove_api_key(provider)
                                    .map(|_| ())
                                    .map_err(|e| e.to_string())
                            } else {
                                forge_config::store_api_key(provider, value.trim())
                                    .map_err(|e| e.to_string())
                            }
                        } else {
                            let scope = if app.config_editor.project_scope {
                                forge_config::ConfigScope::Project
                            } else {
                                forge_config::ConfigScope::User
                            };
                            forge_config::set_config_value(scope, &path, &value)
                                .map_err(|e| e.to_string())
                        };
                        match result {
                            Ok(()) => {
                                app.config_editor.rows = config_editor_rows();
                                app.config_editor.status = Some(format!("✓ saved {path}"));
                            }
                            Err(e) => app.config_editor.status = Some(format!("✗ {e}")),
                        }
                    }
                    forge_tui::ConfigAction::Reset { path } => {
                        let scope = if app.config_editor.project_scope {
                            forge_config::ConfigScope::Project
                        } else {
                            forge_config::ConfigScope::User
                        };
                        match forge_config::reset_config_value(scope, &path) {
                            Ok(()) => {
                                app.config_editor.rows = config_editor_rows();
                                app.config_editor.status =
                                    Some(format!("✓ reset {path} to default"));
                            }
                            Err(e) => app.config_editor.status = Some(format!("✗ {e}")),
                        }
                    }
                    forge_tui::ConfigAction::Reload => {
                        app.config_editor.rows = config_editor_rows();
                    }
                    forge_tui::ConfigAction::Close | forge_tui::ConfigAction::None => {}
                }
                dirty = true;
                continue;
            }

            // Effort slider is modal while open: ←/→ adjust level, Esc/Enter/Ctrl+R close.
            if app.effort_slider {
                match key {
                    KeyKind::Left => {
                        app.effort_slider_left();
                        if let Some(level) = app.effort {
                            session.lock().await.set_effort(Some(level));
                        }
                    }
                    KeyKind::Right => {
                        app.effort_slider_right();
                        if let Some(level) = app.effort {
                            session.lock().await.set_effort(Some(level));
                        }
                    }
                    KeyKind::Esc | KeyKind::Enter | KeyKind::ToggleEffortSlider => {
                        app.effort_slider = false;
                    }
                    _ => {}
                }
                dirty = true;
                continue;
            }

            // Ctrl+R: toggle the effort slider when nothing else is modal.
            if matches!(key, KeyKind::ToggleEffortSlider) {
                app.toggle_effort_slider();
                dirty = true;
                continue;
            }

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
                            if let Some(tok) = forge_tui::slash_token_at(
                                &app.input,
                                app.input_cursor.min(app.input.len()),
                            ) {
                                app.input
                                    .replace_range(tok.start..tok.end, &format!("/{name}"));
                                app.input_cursor = app.input.len();
                            } else {
                                app.input = format!("/{name}");
                                app.input_cursor = app.input.len();
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
                                if let Some(tok) = forge_tui::slash_token_at(
                                    &app.input,
                                    app.input_cursor.min(app.input.len()),
                                ) {
                                    app.input
                                        .replace_range(tok.start..tok.end, &format!("/{name}"));
                                    app.input_cursor = app.input.len();
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
                            &mut assay_scope,
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
                            DispatchOutcome::PendingMesh(rx) => {
                                mesh_load_rx = Some(rx);
                            }
                            DispatchOutcome::PendingUsage(rx) => {
                                usage_load_rx = Some(rx);
                            }
                            DispatchOutcome::ToggleRemote { exposure } => {
                                toggle_remote(&mut remote, &mut app, &mut tui, exposure).await?;
                            }
                            DispatchOutcome::CopyToClipboard(text) => {
                                let chars = text.chars().count();
                                copy_selection(&mut clipboard, &text);
                                app.note(&format!(
                                    "✓ copied response to clipboard ({chars} chars)"
                                ));
                            }
                        }
                    }
                    KeyKind::CycleTemper
                    | KeyKind::ToggleSubagentDetail
                    | KeyKind::ToggleEffortSlider => {}
                    // Any other editing key mutates the input at the *cursor* (not blindly at the
                    // end) and then re-syncs the palette to the slash-token the cursor now sits in.
                    // That keeps the text cursor moving while the palette is open, and closes the
                    // palette once the cursor leaves the command name (e.g. a space into the args).
                    _ => {
                        let _ = forge_tui::handle_key(&mut app.input, &mut app.input_cursor, key);
                        sync_palette_to_slash_token(&mut app);
                    }
                }
                continue;
            }

            // Usage overlay captures all keys; Esc closes it.
            if app.usage_overlay.open {
                if matches!(key, KeyKind::Esc) {
                    app.usage_overlay.open = false;
                    dirty = true;
                }
                continue;
            }

            // Mesh inspector overlay captures all keys; Esc closes, ↑/↓ scroll the candidate list.
            if app.mesh_overlay.open {
                match key {
                    KeyKind::Esc => {
                        app.mesh_overlay.open = false;
                        app.mesh_overlay.scroll = 0;
                        dirty = true;
                    }
                    KeyKind::Down => {
                        app.mesh_overlay.scroll = app.mesh_overlay.scroll.saturating_add(1);
                        dirty = true;
                    }
                    KeyKind::Up => {
                        app.mesh_overlay.scroll = app.mesh_overlay.scroll.saturating_sub(1);
                        dirty = true;
                    }
                    _ => {}
                }
                continue;
            }

            // The @path file-path picker is modal while open.
            if app.at_picker.open {
                match key {
                    KeyKind::Esc => app.at_picker.close(),
                    KeyKind::Up => app.at_picker.move_up(),
                    KeyKind::Down => app.at_picker.move_down(),
                    KeyKind::Tab | KeyKind::Enter => {
                        if let Some(path) = app.at_picker.selected_path() {
                            if let Some(tok) = forge_tui::at_token_at(
                                &app.input,
                                app.input_cursor.min(app.input.len()),
                            ) {
                                // Insert `@path ` (trailing space so the user can keep typing).
                                app.input
                                    .replace_range(tok.start..tok.end, &format!("@{path} "));
                                app.input_cursor = app.input.len();
                            } else {
                                app.input = format!("@{path} ");
                                app.input_cursor = app.input.len();
                            }
                        }
                        app.at_picker.close();
                    }
                    KeyKind::Char(c) => {
                        app.input.push(c);
                        sync_at_picker_to_at_token(&mut app);
                    }
                    KeyKind::Backspace => {
                        app.input.pop();
                        sync_at_picker_to_at_token(&mut app);
                    }
                    KeyKind::CycleTemper
                    | KeyKind::ToggleSubagentDetail
                    | KeyKind::ToggleEffortSlider => {}
                    _ => {}
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
                            app.models_pin_mode = false;
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
                        // Exception: in pin-mode (bare `/model`) a leaf model row closes the picker
                        // and pins the selected model.
                        if kind == Some(forge_tui::PickerKind::Models) {
                            if let Some(row) = chosen {
                                if app.models_drilled.is_none() && !row.id.contains("::") {
                                    // Provider-level row → drill in.
                                    open_models_provider(&session, &mut app, &row.id).await?;
                                } else if row.id.contains("::") && app.models_pin_mode {
                                    // Leaf model row in pin-mode → pin it and close.
                                    let model_id =
                                        forge_provider::normalize_model_id(&row.id).into_owned();
                                    session.lock().await.pin_model(Some(model_id.clone()));
                                    app.models_pin_mode = false;
                                    app.models_drilled = None;
                                    app.picker.close();
                                    app.note(&format!(
                                        "⊕ model pinned: {model_id} (clears with /model)"
                                    ));
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
                                let scope = std::mem::replace(
                                    &mut assay_scope,
                                    forge_types::AssayScope::Repo,
                                );
                                turn_handle = spawn_assay(
                                    row.id == "cleanup",
                                    lenses,
                                    scope,
                                    &session,
                                    &done_tx,
                                    turn_gen,
                                    &mut app,
                                    &mut busy,
                                    &mut busy_since,
                                )
                                .await?;
                            } else if kind == forge_tui::PickerKind::CopyBlocks {
                                // Enter copies the selected candidate (full response or a block) to
                                // the clipboard. Row id is the index into copy_candidates.
                                if let Some((_, text)) = row
                                    .id
                                    .parse::<usize>()
                                    .ok()
                                    .and_then(|i| app.copy_candidates.get(i).cloned())
                                {
                                    let chars = text.chars().count();
                                    copy_selection(&mut clipboard, &text);
                                    app.note(&format!("✓ copied to clipboard ({chars} chars)"));
                                }
                                app.copy_candidates.clear();
                            } else if kind == forge_tui::PickerKind::Sessions
                                && row.id.starts_with("observe:")
                            {
                                let session_id = row.id.trim_start_matches("observe:").to_string();
                                let obs_store = std::sync::Arc::new(crate::open_store()?);
                                let start_event_id =
                                    find_starting_event_id(&obs_store, &session_id);
                                let (items, view) = {
                                    let mut s = session.lock().await;
                                    s.reset_resumed(&session_id)
                                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                                    (s.replay_items_full(), s.view_snapshot())
                                };
                                tui.clear_screen();
                                app.clear_transcript();
                                app.note(&format!(
                                    "⚡ resumed live session {}",
                                    session_id.chars().take(8).collect::<String>()
                                ));
                                app.replay_history(&items);
                                if let Some(json) = view {
                                    app.restore_view_json(&json);
                                }
                                app.input =
                                    "⚡ Observing live MCP session — press Esc to stop".to_string();
                                observer = Some(ObserverState {
                                    session_id,
                                    store: obs_store,
                                    last_event_id: start_event_id,
                                    last_poll: std::time::Instant::now(),
                                });
                            } else {
                                picker_accept(kind, &row, &session, &mut tui, &mut app).await?;
                            }
                        }
                    }
                    // `w` in the copy picker writes the selected candidate to a file instead of the
                    // clipboard (useful over SSH). Other chars filter the list as usual.
                    KeyKind::Char(c)
                        if app.picker.kind == Some(forge_tui::PickerKind::CopyBlocks)
                            && (c == 'w' || c == 'W') =>
                    {
                        let pick = app
                            .picker
                            .selected_row()
                            .and_then(|r| r.id.parse::<usize>().ok())
                            .and_then(|i| app.copy_candidates.get(i).cloned());
                        app.picker.close();
                        app.copy_candidates.clear();
                        if let Some((lang, text)) = pick {
                            match write_copy_to_file(&text, &lang) {
                                Ok(path) => app.note(&format!("✓ wrote to {}", path.display())),
                                Err(e) => app.note(&format!("write failed: {e}")),
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
                    KeyKind::Tab
                    | KeyKind::CycleTemper
                    | KeyKind::ToggleSubagentDetail
                    | KeyKind::ToggleEffortSlider => {}
                    _ => {}
                }
                continue;
            }

            // Full-screen mode: PageUp/PageDown scroll the transcript region. The render re-clamps
            // the offset to the visible area, so an over-scroll is harmless; here we approximate the
            // page (and the follow-resume threshold) from the terminal height.
            if app.fullscreen && matches!(key, KeyKind::PageUp | KeyKind::PageDown) {
                let body = tui.height().saturating_sub(8).max(1);
                if matches!(key, KeyKind::PageUp) {
                    app.transcript_scroll_up(body as usize);
                } else {
                    let (_, max_scroll) = app.transcript_metrics(tui.width(), body);
                    app.transcript_scroll_down(body as usize, max_scroll);
                }
                dirty = true;
                continue;
            }

            // Ctrl+End jumps the transcript to the tail and resumes following (mirrors clicking the
            // floating jump-to-bottom bar).
            if app.fullscreen && matches!(key, KeyKind::JumpBottom) {
                app.transcript_to_bottom();
                dirty = true;
                continue;
            }

            // Ctrl+O toggles focus on the sticky activity panel (main chat + subagents + critics).
            // When focused, ↑↓ move the selection and Enter opens the full-screen transcript viewer.
            if matches!(key, KeyKind::ToggleSubagentDetail) {
                if app.has_activity() {
                    app.activity_focused = !app.activity_focused;
                    if app.activity_focused {
                        app.activity_idx =
                            app.activity_idx.min(app.activity_len().saturating_sub(1));
                    }
                }
                dirty = true;
                continue;
            }

            // While the activity panel has focus: ↑↓ move the selection (wrapping), Enter opens the
            // selected entry's full-screen transcript viewer, Esc unfocuses. Handled before the
            // global Esc so Esc steps out of the panel instead of quitting.
            if app.activity_focused {
                match key {
                    KeyKind::Up => {
                        let n = app.activity_len();
                        if n > 0 {
                            app.activity_idx = (app.activity_idx + n - 1) % n;
                        }
                    }
                    KeyKind::Down => {
                        let n = app.activity_len();
                        if n > 0 {
                            app.activity_idx = (app.activity_idx + 1) % n;
                        }
                    }
                    KeyKind::Enter => {
                        let idx = app.activity_idx;
                        if app.fullscreen {
                            // Full-screen: open the in-loop viewer (same terminal, no nested
                            // alt-screen). The main render loop keeps draining events, so the
                            // selected entry auto-updates while open.
                            app.open_viewer(idx);
                            app.activity_focused = false;
                        } else {
                            // Inline: the live region is tiny, so take over a separate alternate
                            // screen for the viewer and drain events in its refresh closure.
                            tui.run_fullscreen(|| {
                                forge_tui::run_transcript_viewer(idx, || {
                                    while let Ok(msg) = rx.try_recv() {
                                        match msg {
                                            UiMsg::Event(e) => app.apply(e),
                                            UiMsg::Permission { reply, .. } => {
                                                let _ = reply.send(false);
                                            }
                                            UiMsg::Question { reply, .. } => {
                                                let _ =
                                                    reply.send(forge_tui::NO_ANSWER.to_string());
                                            }
                                        }
                                    }
                                    app.activity_views()
                                })
                            })?;
                        }
                    }
                    KeyKind::Esc => {
                        app.activity_focused = false;
                    }
                    _ => {}
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
                    if !queued_prompts.is_empty() {
                        queued_prompts.clear(); // interrupting drops the queued prompts too
                        app.set_queued(&queued_prompts);
                    }
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
            if let Some((tool, reply)) = pending.take() {
                // Answering a permission prompt.
                let always = matches!(key, KeyKind::Char('a') | KeyKind::Char('A'));
                let yes = always
                    || matches!(
                        key,
                        KeyKind::Char('y') | KeyKind::Char('Y') | KeyKind::Enter
                    );
                let _ = reply.send(yes);
                app.prompt = None;
                if always {
                    if let Err(e) = forge_config::append_allow_rule(&tool) {
                        app.note(&format!("⚠ could not save allow rule: {e}"));
                    } else {
                        app.note(&format!("✓ {tool} added to .forge/config.toml allow rules"));
                    }
                }
            } else if app.awaiting_question() {
                // Answering an AskUserQuestion (the turn task is blocked in `ask()`): the input
                // line collects a number or free-text answer; submit resolves + replies.
                match handle_key(&mut app.input, &mut app.input_cursor, key) {
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
                // Mid-turn: let the user keep typing and QUEUE submitted prompts to run after the
                // current turn finishes (Claude Code / aider style). Only plain text editing +
                // Enter is honored here; palette, commands, history and temper-cycling wait until
                // the turn is idle. A `/command` is held back (it needs the idle session).
                let outcome = if app.try_delete_paste_block(key) {
                    InputOutcome::Editing
                } else {
                    handle_key(&mut app.input, &mut app.input_cursor, key)
                };
                if let InputOutcome::Submit(raw_line) = outcome {
                    let (line, _imgs) = app.resolve_paste_blocks(raw_line);
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        // nothing to queue
                    } else if trimmed.starts_with('/') && !trimmed.starts_with("//") {
                        app.note("⏳ commands run when the turn is idle — finish or Esc first");
                    } else {
                        queued_prompts.push(line.clone());
                        app.set_queued(&queued_prompts);
                        app.note(&format!(
                            "⏳ queued ({} pending) — runs after this turn",
                            queued_prompts.len()
                        ));
                    }
                }
                dirty = true;
            } else if matches!(key, KeyKind::Char('f') | KeyKind::Char('F'))
                && app.pending_shell_fix.is_some()
            {
                // F: populate input with the pending shell fix command for the user to review.
                if let Some(fix) = app.pending_shell_fix.take() {
                    app.input = fix;
                }
            } else if matches!(key, KeyKind::CycleTemper) {
                // SHIFT+TAB: cycle the operating temper (idle only — never mid-turn).
                let new = {
                    let mut sess = session.lock().await;
                    sess.cycle_temper()
                };
                app.set_temper(new.label());
                // Remember the chosen temper as the default for the next session (best-effort).
                let _ = forge_config::write_permission_mode(new);
            } else if matches!(key, KeyKind::Up) {
                // Arrow-up: browse to the previous prompt history entry.
                if history_pos.is_none() {
                    history_draft = app.input.clone();
                }
                if let Some(p) = history_pos {
                    if p > 0 {
                        history_pos = Some(p - 1);
                    }
                } else if !prompt_history.is_empty() {
                    history_pos = Some(prompt_history.len() - 1);
                }
                if let Some(p) = history_pos {
                    app.input = prompt_history[p].clone();
                    app.input_cursor = app.input.len();
                }
                dirty = true;
            } else if matches!(key, KeyKind::Down) {
                // Arrow-down: browse to the next entry, or restore the draft past the end.
                if let Some(p) = history_pos {
                    if p + 1 < prompt_history.len() {
                        history_pos = Some(p + 1);
                        app.input = prompt_history[p + 1].clone();
                        app.input_cursor = app.input.len();
                    } else {
                        history_pos = None;
                        app.input = history_draft.clone();
                        app.input_cursor = app.input.len();
                    }
                }
                dirty = true;
            } else {
                let pre_edit_len = app.input.len();
                let outcome = if app.try_delete_paste_block(key) {
                    InputOutcome::Editing
                } else {
                    handle_key(&mut app.input, &mut app.input_cursor, key)
                };
                match outcome {
                    InputOutcome::Submit(raw_line) => {
                        let (line, submit_images) = app.resolve_paste_blocks(raw_line);
                        history_pos = None;
                        if !line.trim().is_empty() && prompt_history.last() != Some(&line) {
                            prompt_history.push(line.clone());
                        }
                        // `//foo` escapes to a literal prompt `/foo`; a bare `/cmd` typed without
                        // the palette still dispatches as a command; everything else is a prompt.
                        if let Some(rest) = line.strip_prefix("//") {
                            let hooks = session.lock().await.hooks().to_vec();
                            let escaped = format!("/{rest}");
                            match forge_core::hooks::run_prompt_hooks(&hooks, &escaped).await {
                                Err(reason) => {
                                    app.note(&format!("⎇ prompt blocked by hook: {reason}"));
                                }
                                Ok(prompt) => {
                                    turn_gen += 1;
                                    turn_handle = Some(spawn_turn(
                                        &prompt,
                                        &session,
                                        &done_tx,
                                        turn_gen,
                                        &mut app,
                                        &mut busy,
                                        &mut busy_since,
                                    ));
                                }
                            }
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
                                &mut assay_scope,
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
                                DispatchOutcome::PendingMesh(rx) => {
                                    mesh_load_rx = Some(rx);
                                }
                                DispatchOutcome::PendingUsage(rx) => {
                                    usage_load_rx = Some(rx);
                                }
                                DispatchOutcome::ToggleRemote { exposure } => {
                                    toggle_remote(&mut remote, &mut app, &mut tui, exposure)
                                        .await?;
                                }
                                DispatchOutcome::CopyToClipboard(text) => {
                                    let chars = text.chars().count();
                                    copy_selection(&mut clipboard, &text);
                                    app.note(&format!(
                                        "✓ copied response to clipboard ({chars} chars)"
                                    ));
                                }
                            }
                        } else {
                            let hooks = session.lock().await.hooks().to_vec();
                            match forge_core::hooks::run_prompt_hooks(&hooks, &line).await {
                                Err(reason) => {
                                    app.note(&format!("⎇ prompt blocked by hook: {reason}"));
                                }
                                Ok(prompt) => {
                                    // Attach any images pasted/added into this prompt as vision
                                    // input for the turn about to run.
                                    if !submit_images.is_empty() {
                                        session.lock().await.attach_images(submit_images);
                                    }
                                    // Expand `@path` mentions: read those files and ride their
                                    // contents along as turn guidance, leaving the echoed line clean.
                                    let (file_blocks, included, skipped) = expand_at_files(&prompt);
                                    if !included.is_empty() {
                                        app.note(&format!("📎 included {}", included.join(", ")));
                                    }
                                    for s in &skipped {
                                        app.note(&format!("⚠ skipped {s}"));
                                    }
                                    turn_gen += 1;
                                    turn_handle = Some(if file_blocks.is_empty() {
                                        spawn_turn(
                                            &prompt,
                                            &session,
                                            &done_tx,
                                            turn_gen,
                                            &mut app,
                                            &mut busy,
                                            &mut busy_since,
                                        )
                                    } else {
                                        spawn_turn_with(
                                            prompt.clone(),
                                            file_blocks,
                                            None,
                                            &session,
                                            &done_tx,
                                            turn_gen,
                                            &mut app,
                                            &mut busy,
                                            &mut busy_since,
                                        )
                                    });
                                }
                            }
                        }
                    }
                    InputOutcome::Quit => {
                        quit = true;
                        break;
                    }
                    InputOutcome::Editing => {
                        if app.input.len() != pre_edit_len {
                            history_pos = None;
                        }
                        // `/command` anywhere on the line opens the palette; `@path` opens the
                        // file picker. They are mutually exclusive — slash wins at cursor.
                        if let Some(tok) = forge_tui::slash_token_at(
                            &app.input,
                            app.input_cursor.min(app.input.len()),
                        ) {
                            app.at_picker.close();
                            app.palette.open_with(&tok.name);
                        } else {
                            app.palette.close();
                            sync_at_picker_to_at_token(&mut app);
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
                    app.prompt = Some(format!("allow {tool} ({side_effect:?}) [y/n/a=always]"));
                    pending = Some((tool, reply));
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

        // Keep the commit hook's model file current with whichever model ran the latest turn, so a
        // commit the agent makes is attributed to the model that actually did the work.
        if git_coauthor {
            if let Some(model) = app.routing.as_ref().map(|r| r.model.clone()) {
                if !model.is_empty() && model != last_model_written {
                    write_active_model(&model);
                    last_model_written = model;
                }
            }
        }

        // Drain remote-control inputs (a browser sent a prompt / answer / interrupt) and inject
        // them exactly like local keystrokes. We process the whole queue each iteration so a
        // chatty phone can't fall behind. Each input marks `dirty` (the statusline/preview may
        // change) and may spawn a turn / answer a prompt.
        if let Some(rc) = remote.as_mut() {
            while let Ok(input) = rc.input_rx.try_recv() {
                dirty = true;
                match input {
                    remote::RemoteInput::Prompt { text } => {
                        if busy {
                            // A turn is running — don't queue a second; mirror the local guard.
                            app.note("⚠ finish or Esc the current turn first (remote)");
                        } else if let Some(rest) = text.strip_prefix("//") {
                            let hooks = session.lock().await.hooks().to_vec();
                            let escaped = format!("/{rest}");
                            if let Ok(prompt) =
                                forge_core::hooks::run_prompt_hooks(&hooks, &escaped).await
                            {
                                turn_gen += 1;
                                turn_handle = Some(spawn_turn(
                                    &prompt,
                                    &session,
                                    &done_tx,
                                    turn_gen,
                                    &mut app,
                                    &mut busy,
                                    &mut busy_since,
                                ));
                            }
                        } else if text.starts_with('/') {
                            match dispatch_command(
                                &text,
                                &session,
                                &mut tui,
                                &mut app,
                                &catalog,
                                &mut armed_project,
                                trust_project,
                                busy,
                                &mut assay_lenses,
                                &mut assay_scope,
                            )
                            .await?
                            {
                                DispatchOutcome::Quit => {
                                    quit = true;
                                    break;
                                }
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
                                _ => {} // handled in-loop (toggle, note, …)
                            }
                        } else {
                            let hooks = session.lock().await.hooks().to_vec();
                            if let Ok(prompt) =
                                forge_core::hooks::run_prompt_hooks(&hooks, &text).await
                            {
                                turn_gen += 1;
                                turn_handle = Some(spawn_turn(
                                    &prompt,
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
                    remote::RemoteInput::Allow { yes } => {
                        if let Some((tool, reply)) = pending.take() {
                            let _ = reply.send(yes);
                            app.prompt = None;
                            if yes {
                                app.note(&format!("✓ remote allowed {tool}"));
                            } else {
                                app.note(&format!("✗ remote denied {tool}"));
                            }
                        }
                    }
                    remote::RemoteInput::Answer { text } => {
                        if app.awaiting_question() {
                            if let Some(ans) = app.resolve_question(&text) {
                                if let Some(tx) = pending_question.take() {
                                    let _ = tx.send(ans);
                                }
                            } else {
                                app.note("⚠ remote answer was invalid — re-asking");
                            }
                        }
                    }
                    remote::RemoteInput::Interrupt => {
                        if busy {
                            if let Some(h) = turn_handle.take() {
                                h.abort();
                            }
                            turn_gen += 1;
                            busy = false;
                            loop_state = None;
                            pending = None;
                            pending_question = None;
                            app.prompt = None;
                            app.clear_question();
                            app.apply(forge_tui::PresenterEvent::AssistantDone);
                            app.note("⏹ remote interrupted — stopped responding");
                        }
                    }
                }
            }
        }
        if quit {
            break;
        }

        // Clear busy only on the *current* turn's done-signal; a stale signal from an interrupted
        // (aborted) turn carries an older generation and is ignored.
        while let Ok(g) = done_rx.try_recv() {
            if busy && g == turn_gen {
                busy = false;
                turn_handle = None;
                dirty = true;
                // Persist the on-screen view (activity panel, viewer, scroll) as of this completed
                // turn so a later resume restores it exactly. Skipped when there's nothing to save.
                if let Some(json) = app.view_snapshot_json() {
                    session.lock().await.save_view_snapshot(&json);
                }
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
                // Drain a queued prompt (typed while this turn was running): run it as the next
                // turn, ahead of auto-compaction (the queued turn auto-compacts itself if needed).
                if turn_handle.is_none() && !queued_prompts.is_empty() {
                    let next = queued_prompts.remove(0);
                    app.set_queued(&queued_prompts);
                    if prompt_history.last() != Some(&next) {
                        prompt_history.push(next.clone());
                    }
                    turn_gen += 1;
                    turn_handle = Some(spawn_turn(
                        &next,
                        &session,
                        &done_tx,
                        turn_gen,
                        &mut app,
                        &mut busy,
                        &mut busy_since,
                    ));
                }
                // Auto-compact: when no new turn was spawned (not a loop iteration) and the
                // context gauge is above AUTO_COMPACT_THRESHOLD, quietly run /compact so the
                // user doesn't need to do it manually (context-compaction.md).
                // Guard: only fire once per user turn — compact's own Cost event still carries
                // the old full-context size, so context_tokens won't drop until the next real
                // turn. Without the gen guard this would re-fire on every compact completion.
                if turn_handle.is_none() && turn_gen > last_auto_compact_gen {
                    if let Some(lim) = app.context_limit {
                        let fill = app.context_tokens as f64 / lim as f64;
                        if fill > AUTO_COMPACT_THRESHOLD {
                            app.note(&format!(
                                "⚒ context {:.0}% full — auto-compacting",
                                fill * 100.0
                            ));
                            turn_gen += 1;
                            last_auto_compact_gen = turn_gen;
                            turn_handle = Some(spawn_compact(
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
            }
        }
        if busy {
            let t = (busy_since.elapsed().as_millis() / 60) as usize;
            if t != app.tick {
                app.tick = t;
                dirty = true;
            }
        }
        // Animate the effort slider's rainbow/pulse at XHigh even while idle.
        if app.effort_slider {
            let t = (anim_epoch.elapsed().as_millis() / 80) as usize;
            if t != app.tick {
                app.tick = t;
                dirty = true;
            }
        }
        // Blink the input cursor only when focused AND idle: solid for the first ~600ms after the
        // last keystroke, then a calm ~600ms square wave. Typing resets `last_input_at`, so the
        // block never flickers while you write. Unfocused → static hollow, so leave it alone.
        if !app.unfocused {
            let idle = last_input_at.elapsed().as_millis();
            let phase_off = idle >= 600 && ((idle - 600) / 600) % 2 == 1;
            if phase_off != app.cursor_hidden {
                app.cursor_hidden = phase_off;
                dirty = true;
            }
        }
        // Animate the command palette's / picker's / at-path picker's ease-in reveal while open.
        if app.palette.open && app.palette.anim < 1.0 {
            app.palette.tick_anim();
            dirty = true;
        }
        if app.at_picker.open && app.at_picker.anim < 1.0 {
            app.at_picker.tick_anim();
            dirty = true;
        }
        if app.picker.open && app.picker.anim < 1.0 {
            app.picker.tick_anim();
            dirty = true;
        }
        if app.mesh_overlay.open && app.mesh_overlay.anim_tick < app.mesh_overlay.settle_tick() {
            // Animate only until the reveal settles, then stop redrawing (no infinite spinner).
            app.mesh_overlay.anim_tick += 1;
            dirty = true;
        }
        if app.usage_overlay.open {
            app.usage_overlay.anim_tick = app.usage_overlay.anim_tick.wrapping_add(1);
            dirty = true;
            // Auto-refresh data every ~3 s (180 ticks × 16 ms).
            if app.usage_overlay.anim_tick % 180 == 1 {
                let (
                    (
                        month_usd,
                        by_model_5h,
                        by_model,
                        by_model_week,
                        (daily_cap, monthly_cap, weekly_cap),
                    ),
                    (session_in, session_out, session_usd),
                ) = {
                    let s = session.lock().await;
                    (
                        (
                            s.spend_this_month_usd(),
                            s.spend_by_model_5h(),
                            s.spend_by_model_today(),
                            s.spend_by_model_week(),
                            s.budget_caps(),
                        ),
                        s.session_usage_db(),
                    )
                };
                app.usage_overlay.month_usd = month_usd;
                app.usage_overlay.session_usd = session_usd;
                app.usage_overlay.session_in = session_in;
                app.usage_overlay.session_out = session_out;
                app.usage_overlay.by_model_5h = by_model_5h;
                app.usage_overlay.by_model = by_model;
                app.usage_overlay.by_model_week = by_model_week;
                app.usage_overlay.daily_cap = daily_cap;
                app.usage_overlay.weekly_cap = weekly_cap;
                app.usage_overlay.monthly_cap = monthly_cap;
                // bridge_stats scan can take seconds on large histories — fire it in the
                // background and let the existing usage_load_rx receiver fill in the
                // claude quota fields without stalling the event loop.
                if usage_load_rx.is_none() {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.send(bridge_stats::fetch());
                    });
                    usage_load_rx = Some(rx);
                }
            }
        }

        // Poll mesh background load (opened with loading=true; result populates when ready).
        if let Some(rx) = &mut mesh_load_rx {
            match rx.try_recv() {
                Ok(Some(overlay)) => {
                    let tick = app.mesh_overlay.anim_tick;
                    app.mesh_overlay = overlay;
                    app.mesh_overlay.anim_tick = tick;
                    mesh_load_rx = None;
                    dirty = true;
                }
                Ok(None) => {
                    app.mesh_overlay.open = false;
                    mesh_load_rx = None;
                    emit_text(
                        &mut tui,
                        &mut app,
                        "mesh: auto-discovery routing is off (no model catalog) — nothing to inspect",
                    );
                    dirty = true;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    app.mesh_overlay.open = false;
                    mesh_load_rx = None;
                    dirty = true;
                }
            }
        }
        // Poll usage background load (bridge stats; session data was already populated on open).
        if let Some(rx) = &mut usage_load_rx {
            match rx.try_recv() {
                Ok(bstats) => {
                    let fracs = session.lock().await.bridge_fractions();
                    app.usage_overlay.claude_5h_in = bstats.claude_5h_in;
                    app.usage_overlay.claude_5h_out = bstats.claude_5h_out;
                    app.usage_overlay.claude_weekly_in = bstats.claude_weekly_in;
                    app.usage_overlay.claude_weekly_out = bstats.claude_weekly_out;
                    fill_subscription_pcts(&mut app.usage_overlay, &fracs, &bstats);
                    app.usage_overlay.loading = false;
                    usage_load_rx = None;
                    dirty = true;
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    app.usage_overlay.loading = false;
                    usage_load_rx = None;
                    dirty = true;
                }
            }
        }

        // Push any finalized lines into native scrollback (above the pinned live region). While
        // remote control is on, also fold them into the transcript ring buffer so the phone's
        // snapshot mirrors the conversation tail, then broadcast the snapshot.
        if remote.is_some() {
            let flushed = app.drain_flush_remote();
            if !flushed.is_empty() {
                tui.insert_lines(flushed);
                dirty = true;
            }
            if dirty || busy {
                let view = app.remote_snapshot();
                let snap = remote::Snapshot {
                    busy: view.busy,
                    done: view.done,
                    temper: view.temper,
                    tier: view.tier,
                    model: view.model,
                    cost_usd: view.cost_usd,
                    context_tokens: view.context_tokens,
                    context_limit: view.context_limit,
                    streaming: view.streaming,
                    transcript: view.transcript,
                    permission_prompt: view.permission_prompt,
                    question: view.question,
                    closed: false,
                };
                if let Some(rc) = remote.as_ref() {
                    let _ = rc.snapshot_tx.send(snap);
                }
            }
        } else {
            let flushed = app.drain_flush();
            if !flushed.is_empty() {
                tui.insert_lines(flushed);
                dirty = true;
            }
        }
        // Adaptive frame pacing. When the user is actively interacting (a key/paste was handled
        // this iteration) and no turn is streaming, loop back quickly so typing/selection in the
        // palette, picker, and approve prompts feels immediate instead of capped at ~60fps. Idle or
        // mid-stream → a full ~16ms frame keeps CPU low and the spinner smooth.
        let snappy = dirty && !busy;
        tokio::time::sleep(Duration::from_millis(if snappy { 3 } else { 16 })).await;
    }
    {
        let (hooks, sid) = {
            let s = session.lock().await;
            // Save the final view on clean exit so resuming this session restores the screen.
            if let Some(json) = app.view_snapshot_json() {
                s.save_view_snapshot(&json);
            }
            (s.hooks().to_vec(), s.session_id().to_string())
        };
        forge_core::hooks::run_session_hooks(&hooks, forge_config::HookEvent::SessionEnd, &sid)
            .await;
    }
    Ok(())
}

/// `/loop` runtime state: the generation of the in-flight loop turn and how many iterations have
/// run, so completion can be detected and capped.
pub(crate) struct LoopState {
    gen: u64,
    iter: usize,
}

/// Iteration cap so a loop that never signals completion can't run forever.
pub(crate) const LOOP_MAX_ITERS: usize = 25;

/// Context-fill fraction above which a turn-end auto-compact fires (context-compaction.md).
pub(crate) const AUTO_COMPACT_THRESHOLD: f64 = 0.80;

/// The token the model is told to emit when the looped task is fully complete.
pub(crate) const LOOP_DONE_SENTINEL: &str = "LOOP_COMPLETE";

/// Guidance injected on every loop turn: make progress, and signal completion explicitly.
pub(crate) const LOOP_GUIDANCE: &str = "You are running in an autonomous loop. Make concrete progress on the \
task each turn. When — and ONLY when — the task is fully complete, end your final message with \
the token LOOP_COMPLETE on its own line. While work remains, keep going and do NOT emit that token.";

/// Decide whether a loop should stop after a turn. Returns `Some(reason)` to stop (shown to the
/// user), or `None` to run another iteration. Pure so it's unit-testable.
pub(crate) fn loop_stop_reason(last_assistant: Option<&str>, iter: usize) -> Option<&'static str> {
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
pub(crate) fn spawn_turn(
    prompt: &str,
    session: &Arc<tokio::sync::Mutex<Session>>,
    done_tx: &std::sync::mpsc::Sender<u64>,
    gen: u64,
    app: &mut forge_tui::App,
    busy: &mut bool,
    busy_since: &mut std::time::Instant,
) -> tokio::task::JoinHandle<()> {
    app.on_turn_start();
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
pub(crate) fn spawn_turn_with(
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
    app.on_turn_start();
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
pub(crate) fn spawn_compact(
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
        if let Err(e) = sess.compact(false).await {
            sess.notify_error(&format!("compact failed: {e}"));
        }
    })
}

/// Start or stop remote control in response to `/remote`. On: bind the server (LAN-reachable by
/// default, loopback with `--local`, or piped through a public tunnel with `--anywhere`), print
/// the connect URL + a scan-to-connect QR code into scrollback, and light the statusline
/// indicator. Off: drop the handle (stops the server + tunnel, frees the port) and clear the
/// indicator. Idempotent: `/remote` toggles, so running it again turns it off.
pub(crate) async fn toggle_remote(
    remote: &mut Option<remote::RemoteControl>,
    app: &mut forge_tui::App,
    _tui: &mut forge_tui::Tui,
    exposure: remote::Exposure,
) -> Result<()> {
    if let Some(rc) = remote.take() {
        // Turning it off: the handle's Drop aborts the server task + tunnel and sends a `closed`
        // snapshot so any connected browser stops reconnecting.
        app.remote_active = false;
        app.note("◉ remote control off — browser disconnected");
        drop(rc);
        return Ok(());
    }
    let anywhere = exposure == remote::Exposure::Anywhere;
    if anywhere {
        app.note("◉ remote control — opening a public tunnel (this can take a few seconds)…");
    }
    let started = match exposure {
        remote::Exposure::Anywhere => remote::start_anywhere().await,
        other => remote::start(other),
    };
    match started {
        Ok(rc) => {
            app.remote_active = true;
            let where_ = match exposure {
                remote::Exposure::Lan => "LAN".to_string(),
                remote::Exposure::Local => "loopback".to_string(),
                remote::Exposure::Anywhere => {
                    format!("public tunnel via {}", rc.tunnel.unwrap_or("tunnel"))
                }
            };
            app.note(&format!(
                "◉ remote control on — listening on {} ({where_})",
                rc.url.addr,
            ));
            if anywhere {
                // A public URL is reachable from the whole internet; the path token is the only
                // gate. Make that explicit so the user knows what they've opened.
                app.note(
                    "  ⚠ anyone with the link can drive this session — the token is the only gate",
                );
            }
            app.note(&format!("  connect: {}", rc.url.url));
            if let Some(qr) = remote::qr_lines(&rc.url.url) {
                app.print_lines(qr);
            }
            *remote = Some(rc);
        }
        Err(e) => {
            app.note(&format!("⚠ could not start remote control: {e}"));
        }
    }
    Ok(())
}

/// First use of a *project*-scope command/skill is confirmed by re-running it (its name is
/// "armed" on the first attempt and runs on the second) — unless project scope is trusted. User-
/// scope and builtins are never gated. Returns true when the invocation may proceed.
pub(crate) fn project_trust_ok(
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
/// A clean, single-line title for a session row, derived from its first user prompt: newlines and
/// runs of whitespace collapse to single spaces, leading `/command` noise is kept, and the result
/// is trimmed to a readable length. Falls back to a placeholder when the session has no prompt.
pub(crate) fn session_title(preview: Option<&str>) -> String {
    let raw = preview.unwrap_or("").trim();
    if raw.is_empty() {
        return "(no prompt yet)".to_string();
    }
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let max = 64;
    if collapsed.chars().count() > max {
        format!("{}…", collapsed.chars().take(max - 1).collect::<String>())
    } else {
        collapsed
    }
}

/// Surface what an undo/restore did to the user's files.
pub(crate) fn note_restore(app: &mut forge_tui::App, report: &forge_core::snapshot::RestoreReport) {
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
pub(crate) fn fmt_age(created_at: i64) -> String {
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

fn find_starting_event_id(store: &forge_store::Store, session_id: &str) -> i64 {
    if let Ok(events) = store.live_events_after(session_id, 0) {
        for (id, json) in events.iter().rev() {
            if let Ok(ev) = serde_json::from_str::<crate::live_observer::LiveEvent>(json) {
                if matches!(ev, crate::live_observer::LiveEvent::AssistantDone) {
                    return *id;
                }
            }
        }
    }
    0
}
