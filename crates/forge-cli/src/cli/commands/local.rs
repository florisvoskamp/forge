use crate::*;
use anyhow::{Context, Result};

pub(crate) fn auth(provider: &str, remove: bool) -> Result<()> {
    let known_provider = forge_config::known_key_providers().any(|p| p == provider);
    let known_search = forge_config::known_search_providers().any(|p| p == provider);
    // `artificialanalysis` is the benchmark Data API key (ADR-0011), not a model/search provider,
    // but it stores/resolves via the same keyring entry name.
    let known_data = provider == "artificialanalysis";
    if !known_provider && !known_search && !known_data {
        let mut known: Vec<_> = forge_config::known_key_providers().collect();
        known.extend(forge_config::known_search_providers());
        known.push("artificialanalysis");
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
    forge_config::store_api_key(provider, key)
        .with_context(|| format!("storing {provider} key"))?;
    println!("stored {provider} key (OS keyring, or encrypted file if no keyring is available)");
    Ok(())
}

/// A human label + free/paid hint for a key-based provider, shown in `forge init`.
pub(crate) fn provider_label(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "Anthropic (Claude API) — paid",
        "openai" => "OpenAI (GPT API) — paid",
        "gemini" => "Google Gemini — free tier + paid",
        "xai" => "xAI (Grok) — paid",
        "deepseek" => "DeepSeek — paid",
        "openrouter" => "OpenRouter (gateway, many models) — paid + some :free",
        "groq" => "Groq — free tier (fast)",
        "opencode_go" => "OpenCode Zen — paid credit (curated coding models)",
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
pub(crate) fn bridge_plans(
    kind: forge_provider::CliKind,
) -> &'static [(&'static str, &'static str)] {
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
        forge_provider::CliKind::Antigravity => &[
            ("Free", "free"),
            ("Pro", "pro"),
            ("Ultra", "ultra"),
            ("API credits / unsure", "unknown"),
        ],
    }
}

/// Whether the user looks un-onboarded: no provider key, no installed bridge, and no saved
/// config. Pure so it's testable; the caller adds the tty check before auto-launching `init`.
pub(crate) fn needs_onboarding(has_any_key: bool, any_bridge: bool, config_exists: bool) -> bool {
    !has_any_key && !any_bridge && !config_exists
}

/// Read one trimmed line from stdin with a prompt (no echo suppression — same as `auth`).
/// Opt-in: if `[local] autostart` is set, ensure the configured local model's Ollama server is up
/// before the chat starts. Best-effort and non-fatal — a failure just means the mesh won't have the
/// local model this session.
pub(crate) fn maybe_autostart_local() {
    let cfg = forge_config::load().unwrap_or_default();
    if !cfg.local.autostart || !local::ollama_installed() {
        return;
    }
    if local::ollama_start_serve() {
        if let Some(tag) = &cfg.local.model {
            if !local::ollama_installed_models().iter().any(|m| m == tag) {
                println!("⚒ local: pulling {tag} (first run)…");
                local::ollama_pull(tag);
            }
            println!("⚒ local model ready: ollama::{tag}");
        }
    }
}

/// The animated `forge local` menu (no-arg on a terminal): pick a model to install/start, or view
/// status. Loops until the user closes it; each action prints, then waits for Enter before the
/// menu redraws (it owns its own alternate screen).
pub(crate) async fn local_menu() -> Result<()> {
    enum Act {
        Model(String),
        Status,
        Close,
    }
    let scores = local_bench_scores().await;
    loop {
        let specs = local::detect_specs();
        let cands = local::discover_ranked(&specs, scores.as_ref()).await;
        let installed = if local::ollama_installed() {
            local::ollama_installed_models()
        } else {
            Vec::new()
        };
        let mut items: Vec<forge_tui::SelectItem> = Vec::new();
        let mut acts: Vec<Act> = Vec::new();
        for c in &cands {
            let have = installed.iter().any(|t| t == &c.ollama_tag);
            let bench = if c.benchmarked {
                format!("AA {:.0}", c.score)
            } else {
                "—".to_string()
            };
            items.push(forge_tui::SelectItem {
                label: c.label.clone(),
                hint: format!(
                    "{} · ~{:.0} GB · bench {bench}{}",
                    c.ollama_tag,
                    c.min_memory_gb,
                    if have {
                        " · installed → start"
                    } else {
                        " → install"
                    }
                ),
                preselected: false,
            });
            acts.push(Act::Model(c.ollama_tag.clone()));
        }
        items.push(forge_tui::SelectItem {
            label: "Status".into(),
            hint: "runtime + installed models + autostart".into(),
            preselected: false,
        });
        acts.push(Act::Status);
        items.push(forge_tui::SelectItem {
            label: "Close".into(),
            hint: String::new(),
            preselected: false,
        });
        acts.push(Act::Close);

        let title = format!(
            "forge local — {:.0} GB usable · {} · GPU: {} · ranked by Artificial Analysis",
            specs.model_memory_gb(),
            specs.os,
            specs
                .gpu
                .as_ref()
                .map(|g| g.name.as_str())
                .unwrap_or("none")
        );
        let Some(idx) = forge_tui::select_one(&title, &items)? else {
            return Ok(());
        };
        match &acts[idx] {
            Act::Close => return Ok(()),
            Act::Status => {
                local_status();
                let _ = prompt_line("\n  press Enter to continue…");
            }
            Act::Model(tag) => {
                let have = local::ollama_installed_models().iter().any(|t| t == tag);
                let res = if have {
                    local_start(Some(tag))
                } else {
                    local_install(Some(tag))
                };
                if let Err(e) = res {
                    println!("⚠ {e}");
                }
                let _ = prompt_line("\n  press Enter to continue…");
            }
        }
    }
}

/// Artificial Analysis benchmark scores for ranking local models (cache-first; `None` if disabled
/// or unavailable). Seeds the coverage check with the static catalog's tags.
pub(crate) async fn local_bench_scores() -> Option<forge_mesh::BenchmarkScores> {
    let cfg = forge_config::load().unwrap_or_default();
    let ids: Vec<String> = local::CATALOG
        .iter()
        .map(|m| format!("ollama::{}", m.ollama_tag))
        .collect();
    benchmarks::ensure(&cfg, &ids, false).await
}

/// `forge local [subcommand]`: detect specs, install/run a local model via Ollama, list, status.
/// No subcommand on a terminal → the animated interactive menu; otherwise (piped) → `detect`.
pub(crate) async fn local_cmd(sub: Option<LocalCmd>) -> Result<()> {
    let Some(sub) = sub else {
        use std::io::IsTerminal;
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            return local_menu().await;
        }
        print_specs_and_recommendation().await;
        return Ok(());
    };
    match sub {
        LocalCmd::Detect => {
            print_specs_and_recommendation().await;
            Ok(())
        }
        LocalCmd::Install { key } => local_install(key.as_deref()),
        LocalCmd::List => {
            if !local::ollama_installed() {
                println!("Ollama is not installed. Run `forge local install` to set it up.");
                return Ok(());
            }
            let models = local::ollama_installed_models();
            if models.is_empty() {
                println!("No local models pulled yet. Run `forge local install`.");
            } else {
                println!("Local models ({}):", models.len());
                for m in models {
                    println!("  • {m}");
                }
            }
            Ok(())
        }
        LocalCmd::Start { key } => local_start(key.as_deref()),
        LocalCmd::Status => {
            local_status();
            Ok(())
        }
    }
}

/// Print the detected specs + the ranked recommendation list.
pub(crate) async fn print_specs_and_recommendation() {
    let specs = local::detect_specs();
    let gpu = match &specs.gpu {
        Some(g) => match g.vram_gb {
            Some(v) => format!("{} ({v:.0} GB VRAM)", g.name),
            None => g.name.clone(),
        },
        None => "none detected".to_string(),
    };
    println!("⚒ This machine");
    println!(
        "  RAM {:.0} GB · {} cores · {} · GPU: {gpu}",
        specs.total_ram_gb, specs.cpu_cores, specs.os
    );
    println!(
        "  model memory budget: ~{:.0} GB\n",
        specs.model_memory_gb()
    );

    let scores = local_bench_scores().await;
    let cands = local::discover_ranked(&specs, scores.as_ref()).await;
    if cands.is_empty() {
        println!("No model fits this machine's memory (the smallest needs ~4 GB).");
        return;
    }
    let benched = cands.iter().filter(|c| c.benchmarked).count();
    println!(
        "Models that fit, ranked by Artificial Analysis benchmark score ({benched}/{} rated):",
        cands.len()
    );
    for (i, c) in cands.iter().enumerate() {
        let rec = if i == 0 { "  ‹recommended›" } else { "" };
        let bench = if c.benchmarked {
            format!("AA {:.0}", c.score)
        } else {
            "unrated".to_string()
        };
        println!(
            "  {} {:<26} [{}]  {} · ~{:.0} GB · {bench}{rec}",
            if i == 0 { "▸" } else { " " },
            c.label,
            c.ollama_tag,
            c.family,
            c.min_memory_gb,
        );
        if !c.blurb.is_empty() {
            println!("      {}", c.blurb);
        }
    }
    println!(
        "\nInstall with `forge local install` (recommended) or `forge local install <tag-or-key>`."
    );
}

/// Ensure Ollama is installed (offering to install it), then pull the chosen (or recommended)
/// model. `name` is a raw Ollama tag (`qwen2.5-coder:14b`), a catalog key (`qwen2.5-coder-14b`),
/// or `None` for the recommended pick.
pub(crate) fn local_install(name: Option<&str>) -> Result<()> {
    let specs = local::detect_specs();
    // Resolve to (display label, ollama tag).
    let (label, tag): (String, String) = match name {
        Some(n) if n.contains(':') => (n.to_string(), n.to_string()), // raw tag
        Some(k) => {
            let m = local::model_by_key(k)
                .with_context(|| format!("unknown model '{k}' — see `forge local detect`"))?;
            (m.label.to_string(), m.ollama_tag.to_string())
        }
        None => {
            let m = *local::recommend(&specs)
                .first()
                .context("no local model fits this machine (needs ≥4 GB)")?;
            (m.label.to_string(), m.ollama_tag.to_string())
        }
    };

    if !local::ollama_installed() {
        println!("Ollama (the local-model runtime) is not installed.");
        match local::ollama_install_command(&specs) {
            Some((cmd, args)) => {
                let shown = std::iter::once(cmd.to_string())
                    .chain(args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" ");
                let yes = prompt_line(&format!("Install it now with `{shown}`? [Y/n]: "))?;
                if yes.is_empty()
                    || yes.eq_ignore_ascii_case("y")
                    || yes.eq_ignore_ascii_case("yes")
                {
                    if !local::run_install(cmd, &args) {
                        anyhow::bail!("Ollama install failed — install it manually from https://ollama.com/download, then re-run.");
                    }
                } else {
                    println!("Skipped. Install Ollama from https://ollama.com/download, then re-run `forge local install`.");
                    return Ok(());
                }
            }
            None => {
                println!("Install Ollama from https://ollama.com/download, then re-run `forge local install`.");
                return Ok(());
            }
        }
    }

    println!("Pulling {label} ({tag})…");
    if !local::ollama_pull(&tag) {
        anyhow::bail!(
            "`ollama pull {tag}` failed. The tag may not exist in your Ollama version — check `ollama list` / upgrade Ollama, or pick another model with `forge local detect`."
        );
    }
    println!("✓ {label} is ready. It's available in the mesh as `ollama::{tag}`.");
    println!("  Start it with `forge local start {tag}`, or enable `[local] autostart` in config.");
    Ok(())
}

/// Ensure the Ollama server is up and the chosen model is available.
pub(crate) fn local_start(key: Option<&str>) -> Result<()> {
    if !local::ollama_installed() {
        anyhow::bail!("Ollama is not installed. Run `forge local install` first.");
    }
    let cfg = forge_config::load().unwrap_or_default();
    // Choose the model: raw tag as-is; catalog key → its tag; else configured tag; else recommended.
    let tag: String = match key {
        Some(n) if n.contains(':') => n.to_string(),
        Some(k) => local::model_by_key(k)
            .map(|m| m.ollama_tag.to_string())
            .with_context(|| format!("unknown model '{k}'"))?,
        None => cfg
            .local
            .model
            .clone()
            .or_else(|| {
                let specs = local::detect_specs();
                local::recommend(&specs)
                    .first()
                    .map(|m| m.ollama_tag.to_string())
            })
            .context("no model configured and none fits — run `forge local install`")?,
    };
    print!("Starting Ollama… ");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    if !local::ollama_start_serve() {
        anyhow::bail!("could not start `ollama serve` (is it already running on another port?)");
    }
    println!("up.");
    if !local::ollama_installed_models().iter().any(|m| m == &tag) {
        println!("Model {tag} not pulled yet — pulling…");
        if !local::ollama_pull(&tag) {
            anyhow::bail!("`ollama pull {tag}` failed.");
        }
    }
    println!("✓ Local model ready: `ollama::{tag}` (mesh will route to it).");
    Ok(())
}

/// Print local-runtime status: install, serving, models, and the autostart config.
pub(crate) fn local_status() {
    let cfg = forge_config::load().unwrap_or_default();
    match local::ollama_version() {
        Some(v) => println!("Ollama: installed ({v})"),
        None => {
            println!("Ollama: not installed — run `forge local install`");
            return;
        }
    }
    println!(
        "Server:  {}",
        if local::ollama_serving() {
            "running (localhost:11434)"
        } else {
            "stopped — `forge local start`"
        }
    );
    let models = local::ollama_installed_models();
    println!(
        "Models:  {}",
        if models.is_empty() {
            "none".to_string()
        } else {
            models.join(", ")
        }
    );
    println!(
        "Autostart: {}{}",
        if cfg.local.autostart { "on" } else { "off" },
        cfg.local
            .model
            .as_deref()
            .map(|m| format!(" · model {m}"))
            .unwrap_or_default()
    );
}

pub(crate) fn prompt_line(prompt: &str) -> Result<String> {
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
pub(crate) fn init() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("`forge init` is interactive — run it in a terminal");
    }
    let cfg = forge_config::load().unwrap_or_default();
    let outcome =
        forge_tui::init_wizard::run(wizard_input(cfg.permission_mode, cfg.mesh.credit_mode))
            .context("running the setup wizard")?;
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

/// `forge setup`: the full guided flow — the provider/plan wizard ([`init`]), then an optional
/// local-LLM step. Used by `forge setup`, `forge init`, and the first-run prompt.
pub(crate) fn setup() -> Result<()> {
    init()?;
    offer_local_setup();
    Ok(())
}

/// Interactive local-LLM step of `forge setup`: detect the machine, recommend a Gemma model that
/// fits, and offer to install it (and auto-start it). Best-effort — any failure prints and the
/// flow continues. Skipped on a machine too small for the smallest model.
pub(crate) fn offer_local_setup() {
    let specs = local::detect_specs();
    let picks = local::recommend(&specs);
    let Some(&rec) = picks.first() else {
        return; // nothing fits — don't pester the user
    };
    println!("\n⚒ Local LLM (optional)");
    println!(
        "  This machine (~{:.0} GB usable) can run {} [{}].",
        specs.model_memory_gb(),
        rec.label,
        rec.ollama_tag
    );
    let ans = match prompt_line("  Install it now via Ollama? [Y/n]: ") {
        Ok(a) => a,
        Err(_) => return,
    };
    if !(ans.is_empty() || ans.eq_ignore_ascii_case("y") || ans.eq_ignore_ascii_case("yes")) {
        println!("  Skipped. Run `forge local install` anytime.");
        return;
    }
    if let Err(e) = local_install(Some(rec.key)) {
        println!("  ⚠ {e}");
        return;
    }
    // Offer auto-start so the model is ready whenever Forge runs.
    if let Ok(a) = prompt_line("  Auto-start this model when Forge runs? [y/N]: ") {
        if a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes") {
            let _ = forge_config::set_config_value(
                forge_config::ConfigScope::User,
                "local.autostart",
                "true",
            );
            let _ = forge_config::set_config_value(
                forge_config::ConfigScope::User,
                "local.model",
                rec.ollama_tag,
            );
            println!("  ✓ Auto-start enabled ({}).", rec.ollama_tag);
        }
    }
}

/// Build the config-wizard inputs from what Forge knows: key-based model providers, search-API
/// providers (for `web_search`), and every INSTALLED CLI bridge (with its subscription plans).
/// Shared by `forge init` and the in-chat `/config` command.
pub(crate) fn wizard_input(
    current_permission: forge_types::PermissionMode,
    current_credit_mode: forge_types::CreditMode,
) -> forge_tui::WizardInput {
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
        current_permission,
        current_credit_mode,
    }
}

/// Persist a wizard outcome: keys → OS keyring (ADR-0007), plans + settings → user config; then
/// inject keys into this process's env so a running session picks them up immediately.
/// Returns the config path. Shared by `forge init` and `/config`.
pub(crate) fn apply_wizard_outcome(
    outcome: &forge_tui::WizardOutcome,
) -> Result<std::path::PathBuf> {
    for (provider, key) in &outcome.keys {
        forge_config::store_api_key(provider, key)
            .with_context(|| format!("storing {provider} key"))?;
    }
    let path = forge_config::write_subscriptions(&outcome.plans).context("writing config")?;
    forge_config::write_settings(outcome.permission, outcome.credit_mode)
        .context("writing settings")?;
    forge_config::inject_provider_keys();
    forge_config::inject_search_keys();
    Ok(path)
}
