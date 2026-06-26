//! `forge doctor` — diagnose a user's environment in one command: config, providers/keys, CLI
//! bridges, the local Ollama runtime, git, and the terminal — each with an actionable fix. The
//! single biggest lever for onboarding + support (and the first thing to paste into a bug report).
//!
//! Doctor tests *function*, not just *presence*: a key being set, a binary being on PATH, or a
//! port being open does NOT mean a turn can run. So beyond the local/static checks it does two
//! bounded LIVE probes — a keyed provider's `list_models` (free) and a CLI-bridge round-trip
//! ($0 on a subscription) — each behind a timeout. These catch the real "doctor says fine but
//! Forge is unusable" cases: a keyed provider that's unreachable (→ keyless fallback churn) and a
//! bridge that's on PATH but can't actually launch (the Windows `cmd /S /C` shim path).

use crate::local;

/// One diagnostic line's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Warn,
    Fail,
    Info,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "⚠",
            Status::Fail => "✗",
            Status::Info => "·",
        }
    }
}

struct Check {
    status: Status,
    label: String,
    detail: String,
    /// An actionable next step, shown when not `Ok`.
    fix: Option<String>,
}

impl Check {
    fn print(&self) {
        println!(
            "  {} {:<22} {}",
            self.status.glyph(),
            self.label,
            self.detail
        );
        if self.status != Status::Ok && self.status != Status::Info {
            if let Some(fix) = &self.fix {
                println!("      → {fix}");
            }
        }
    }
}

fn check(status: Status, label: &str, detail: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        status,
        label: label.to_string(),
        detail: detail.into(),
        fix: fix.map(str::to_string),
    }
}

/// Run all diagnostics and print a report. Returns the number of hard failures (for the exit code).
pub async fn run() -> anyhow::Result<usize> {
    println!("⚒ forge doctor — {}\n", env!("CARGO_PKG_VERSION"));

    let mut sections: Vec<(&str, Vec<Check>)> = Vec::new();
    sections.push(("Config", config_checks()));
    let (mut provider_v, has_usable_provider) = provider_checks();
    // Live, timeout-bounded: prove keyed providers are actually reachable (not just key-present).
    provider_v.extend(provider_reachability_checks().await);
    sections.push(("Providers", provider_v));
    // Live, timeout-bounded: prove a detected bridge can actually launch + answer (not just on PATH).
    let bridge_live = bridge_roundtrip_checks().await;
    if !bridge_live.is_empty() {
        sections.push(("Bridge liveness", bridge_live));
    }
    sections.push(("Local LLM (Ollama)", ollama_checks()));
    sections.push(("Environment", environment_checks()));

    let mut fails = 0;
    let mut warns = 0;
    for (title, checks) in &sections {
        println!("{title}");
        for c in checks {
            c.print();
            match c.status {
                Status::Fail => fails += 1,
                Status::Warn => warns += 1,
                _ => {}
            }
        }
        println!();
    }

    // The one gate that actually blocks usage: a routable provider must exist.
    if !has_usable_provider {
        fails += 1;
        println!("✗ No usable model provider configured — Forge can't route a turn.");
        println!(
            "  Run `forge setup` (add an API key, a CLI-bridge subscription, or a local model).\n"
        );
    }

    if fails == 0 && warns == 0 {
        println!("All good — Forge is ready. ⚒");
    } else {
        println!(
            "{fails} failure(s), {warns} warning(s). Address the ✗ items above; ⚠ are optional.",
        );
    }
    Ok(fails)
}

fn config_checks() -> Vec<Check> {
    let mut out = Vec::new();
    match forge_config::load() {
        Ok(_) => out.push(check(Status::Ok, "config", "loads cleanly", None)),
        Err(e) => out.push(check(
            Status::Fail,
            "config",
            format!("failed to load: {e}"),
            Some("fix the syntax in your config.toml (see `forge doctor` detail above)"),
        )),
    }
    let user = forge_config::config_dir().map(|d| d.join("config.toml"));
    let user_exists = user.as_ref().is_some_and(|p| p.exists());
    out.push(check(
        if user_exists {
            Status::Ok
        } else {
            Status::Info
        },
        "user config",
        match &user {
            Some(p) if user_exists => p.display().to_string(),
            Some(p) => format!("{} (not created yet)", p.display()),
            None => "no config dir resolved".to_string(),
        },
        None,
    ));
    if std::path::Path::new("./.forge/config.toml").exists() {
        out.push(check(
            Status::Info,
            "project config",
            "./.forge/config.toml",
            None,
        ));
    }
    // Data dir writable (the session store lives here).
    match forge_config::data_dir() {
        Some(d) => {
            let writable = std::fs::create_dir_all(&d).is_ok();
            out.push(check(
                if writable { Status::Ok } else { Status::Fail },
                "data dir",
                d.display().to_string(),
                (!writable).then_some("ensure the data directory is writable"),
            ));
        }
        None => out.push(check(
            Status::Warn,
            "data dir",
            "could not resolve a data directory",
            Some("set $XDG_DATA_HOME or $HOME"),
        )),
    }
    out
}

/// Provider checks + whether at least one routable provider exists.
fn provider_checks() -> (Vec<Check>, bool) {
    let mut out = Vec::new();
    let mut usable = false;

    // API keys (env or keyring).
    let mut any_key = false;
    for p in forge_config::known_key_providers() {
        if forge_config::has_api_key(p) {
            any_key = true;
            usable = true;
            out.push(check(Status::Ok, &format!("{p} key"), "configured", None));
        }
    }
    if !any_key {
        out.push(check(
            Status::Info,
            "API keys",
            "none configured",
            Some("`forge auth <provider>` or `/config` to add one (optional if you use bridges/local)"),
        ));
    }

    // Subscription CLI bridges.
    for k in forge_provider::CliKind::all() {
        let avail = k.available();
        if avail {
            usable = true;
        }
        out.push(check(
            if avail { Status::Ok } else { Status::Info },
            &format!("{} bridge", k.prefix()),
            if avail { "installed" } else { "not installed" },
            (!avail).then_some(match k {
                forge_provider::CliKind::ClaudeCode => {
                    "install Claude Code + run `claude` once to log in (optional)"
                }
                forge_provider::CliKind::Codex => "install Codex + run `codex login` (optional)",
                forge_provider::CliKind::Antigravity => {
                    "install Antigravity + run `agy` once to log in (optional)"
                }
            }),
        ));
    }

    // A local model counts as a usable provider too.
    if local::ollama_installed() && !local::ollama_installed_models().is_empty() {
        usable = true;
    }
    (out, usable)
}

fn ollama_checks() -> Vec<Check> {
    let mut out = Vec::new();
    match local::ollama_version() {
        Some(v) => out.push(check(Status::Ok, "ollama", v, None)),
        None => {
            out.push(check(
                Status::Info,
                "ollama",
                "not installed",
                Some("`forge local install` to run models locally (optional)"),
            ));
            return out;
        }
    }
    out.push(check(
        if local::ollama_serving() {
            Status::Ok
        } else {
            Status::Info
        },
        "server",
        if local::ollama_serving() {
            "running (localhost:11434)"
        } else {
            "stopped"
        },
        (!local::ollama_serving()).then_some("`forge local start` to run the server + model"),
    ));
    let models = local::ollama_installed_models();
    out.push(check(
        if models.is_empty() {
            Status::Info
        } else {
            Status::Ok
        },
        "models",
        if models.is_empty() {
            "none pulled".to_string()
        } else {
            models.join(", ")
        },
        models
            .is_empty()
            .then_some("`forge local install` to pull a model"),
    ));
    out
}

fn environment_checks() -> Vec<Check> {
    use std::io::IsTerminal;
    let mut out = Vec::new();

    // git
    let git = binary_on_path("git");
    out.push(check(
        if git { Status::Ok } else { Status::Warn },
        "git",
        if git { "on PATH" } else { "not found" },
        (!git).then_some("install git — some features (provenance, /init) use it"),
    ));
    if git {
        let in_repo = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        out.push(check(
            Status::Info,
            "git repo",
            if in_repo {
                "inside a work tree"
            } else {
                "not in a git repo (cwd)"
            },
            None,
        ));
    }

    // terminal — resolve the old "(?)". On Unix an interactive stdout with no usable TERM is the
    // class of box where the full-screen TUI misbehaves, so flag it. On Windows TERM is a Unix
    // concept and is normally UNSET — crossterm drives the console via the Console API regardless —
    // so an interactive Windows console is simply OK (warning there is a false positive).
    let tty = std::io::stdout().is_terminal();
    let term = std::env::var("TERM").ok().filter(|t| !t.is_empty());
    let term_usable = term.as_deref().is_some_and(|t| t != "dumb");
    // Gold-standard viability: actually enter+exit raw mode, exactly what the TUI does on launch.
    // More authoritative than the TERM heuristic — a box where this fails genuinely can't run the
    // full-screen UI, while one where it succeeds can (even with an odd TERM). Only meaningful on an
    // interactive stdout, so it's gated on `tty`.
    let raw_probe = tty.then(raw_mode_probe);
    let (status, detail, fix) = if !tty {
        (Status::Info, "non-interactive (piped/CI)".to_string(), None)
    } else if let Some(Err(e)) = &raw_probe {
        (
            Status::Warn,
            format!("interactive but raw-mode probe failed ({e}) — the full-screen TUI won't work here"),
            Some("use a different terminal emulator; ensure stdin+stdout are a real tty and TERM is set"),
        )
    } else if cfg!(windows) {
        (
            Status::Ok,
            "interactive (Windows console, raw-mode OK)".to_string(),
            None,
        )
    } else if term_usable {
        (
            Status::Ok,
            format!("interactive ({}, raw-mode OK)", term.unwrap()),
            None,
        )
    } else {
        (
            Status::Warn,
            format!(
                "interactive but TERM={} — the TUI may not render correctly",
                term.as_deref().unwrap_or("(unset)")
            ),
            Some("export TERM=xterm-256color (add it to your shell profile)"),
        )
    };
    out.push(check(status, "terminal", detail, fix));

    // WSL: surface it explicitly — it's the platform behind most "hangs / won't open" reports, and
    // knowing it's WSL focuses the fix (TERM, a responsive keyring, PATH'd Windows .cmd shims).
    if is_wsl() {
        out.push(check(Status::Info, "platform", "WSL detected", None));
    }
    out
}

/// Enter then exit raw mode — the exact terminal capability the full-screen TUI needs. Returns the
/// error string if entering fails (a box that can't support the UI). Always attempts to restore
/// cooked mode so `forge doctor` never leaves the terminal in raw mode.
fn raw_mode_probe() -> Result<(), String> {
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
    enable_raw_mode().map_err(|e| e.to_string())?;
    disable_raw_mode().map_err(|e| e.to_string())
}

/// Best-effort WSL detection: the kernel release string carries "microsoft" under WSL1/2.
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.to_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

/// Truncate a provider/error string to one tidy line for the report.
fn short(s: &str) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() > 90 {
        format!("{}…", line.chars().take(89).collect::<String>())
    } else {
        line.to_string()
    }
}

/// LIVE: for each KEYED provider, can we actually list its models within a timeout? A keyed
/// provider whose discovery times out silently drops out of routing and the mesh falls back to a
/// keyless default (the "groq for everything" churn) — a key-PRESENCE check can't see this.
async fn provider_reachability_checks() -> Vec<Check> {
    const REACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
    let mut out = Vec::new();
    for p in forge_config::known_key_providers() {
        if !forge_config::has_api_key(p) {
            continue;
        }
        let res = tokio::time::timeout(REACH_TIMEOUT, forge_provider::list_models(p)).await;
        let c = match res {
            Ok(Ok(list)) if !list.is_empty() => check(
                Status::Ok,
                &format!("{p} reachable"),
                format!("{} models", list.len()),
                None,
            ),
            // Reachable but empty listing — chat may still work; not actionable, so Info.
            Ok(Ok(_)) => check(
                Status::Info,
                &format!("{p} reachable"),
                "responded, but listed no models",
                None,
            ),
            // An Err from list_models is NOT a reliable usability signal: several adapters (Gemini,
            // Groq, the cerebras custom endpoint) have no listing endpoint or key it differently
            // than chat, so they error here while chat works fine. Surface it as Info, not a
            // failure — the robust "this provider is dead" signal is the TIMEOUT branch below, and
            // real credential validation would need a paid chat call doctor won't make by default.
            Ok(Err(_)) => check(
                Status::Info,
                &format!("{p} reachable"),
                "model listing unavailable — chat unaffected",
                None,
            ),
            // The churn cause: keyed but unreachable. Its models won't route.
            Err(_) => check(
                Status::Fail,
                &format!("{p} reachable"),
                format!("discovery timed out (> {}s)", REACH_TIMEOUT.as_secs()),
                Some(
                    "provider/network unreachable — its models won't route this session; the mesh \
                     falls back to another provider",
                ),
            ),
        };
        out.push(c);
    }
    out
}

/// LIVE: for each AVAILABLE CLI bridge, actually launch it with a tiny prompt and confirm it
/// answers — exercising the real launch path (the Windows `cmd /S /C` shim, auth, the streamed
/// handshake). "On PATH" is not "works": a bridge can resolve on PATH yet fail every turn at
/// launch. $0 on a subscription bridge. Bounded by a timeout so a hung CLI can't wedge doctor.
async fn bridge_roundtrip_checks() -> Vec<Check> {
    const BRIDGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    use forge_provider::Provider as _;
    let mut out = Vec::new();
    for k in forge_provider::CliKind::all() {
        if !k.available() {
            continue;
        }
        // harness=false → a plain CLI turn (no Forge-tool MCP bridge): the cheapest probe that
        // still exercises the binary launch + auth + a streamed reply.
        let provider = forge_provider::CliProvider::new(k)
            .with_harness(false)
            .with_timeout(BRIDGE_TIMEOUT);
        let model = k.default_model_id();
        let msgs = [forge_types::Message::user("Reply with the single word: ok")];
        let mut sink = |_ev: forge_provider::StreamEvent| {};
        let fut = provider.complete(&model, &msgs, &[], &mut sink);
        let label = format!("{} turn", k.prefix());
        let fix = match k {
            forge_provider::CliKind::ClaudeCode => {
                "run `claude` once to log in; if it's a Windows .cmd shim, confirm it launches"
            }
            forge_provider::CliKind::Codex => "run `codex login`; confirm the binary launches",
            forge_provider::CliKind::Antigravity => {
                "run `agy` once to log in; confirm the binary launches"
            }
        };
        let c = match tokio::time::timeout(BRIDGE_TIMEOUT + std::time::Duration::from_secs(2), fut)
            .await
        {
            Ok(Ok(resp)) if !resp.content.trim().is_empty() => {
                check(Status::Ok, &label, "launches + answers", None)
            }
            Ok(Ok(_)) => check(
                Status::Warn,
                &label,
                "launched but returned no text",
                Some(fix),
            ),
            Ok(Err(e)) => check(
                Status::Fail,
                &label,
                format!("launch failed: {}", short(&e.to_string())),
                Some(fix),
            ),
            Err(_) => check(
                Status::Fail,
                &label,
                "timed out — bridge did not respond",
                Some(fix),
            ),
        };
        out.push(c);
    }
    out
}

fn binary_on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}
