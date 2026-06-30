use anyhow::{Context, Result};

// reqwest and serde_json are direct deps of forge-cli (Cargo.toml).

use crate::*;

/// `forge commands` — list discovered slash commands + skills with scope and collision markers.
pub(crate) fn commands_cmd() -> Result<()> {
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

/// `forge skill from-session <id> [--name <slug>] [--scope user|project]`
///
/// Loads the persisted transcript of `session_id`, calls a cheap model to synthesise a
/// generalised SKILL.md methodology, and writes it to the appropriate skills directory.
pub(crate) async fn skill_cmd(sub: SkillCmd) -> Result<()> {
    match sub {
        SkillCmd::Install { url } => skills_install(&url).await,
        SkillCmd::FromSession {
            session_id,
            name,
            scope,
        } => skill_from_session(&session_id, name.as_deref(), scope).await,
        SkillCmd::Export { dest, scope } => skills_export(&dest, scope),
        SkillCmd::Import { src, scope } => skills_import(&src, scope),
        SkillCmd::Normalize { project } => skills_normalize(project),
        SkillCmd::Update { name } => {
            crate::cli::commands::marketplace::update_installed(name.as_deref()).await
        }
    }
}

/// `forge skill install <owner/repo[@ref]>` — fetch `.md` files from a GitHub repo (or URL) and
/// install them into the user skills directory. Tries `<repo>/skills/` first; falls back to root.
pub(crate) async fn skills_install(source: &str) -> Result<()> {
    let (owner, repo, ref_opt) = parse_github_source(source)?;

    let client = reqwest::Client::builder()
        .user_agent("forge-cli")
        .build()
        .context("building HTTP client")?;

    let ref_query = ref_opt
        .as_deref()
        .map(|r| format!("?ref={r}"))
        .unwrap_or_default();

    // Try <repo>/skills/ first, fall back to repo root.
    let listing_url = {
        let skills_url =
            format!("https://api.github.com/repos/{owner}/{repo}/contents/skills{ref_query}");
        let resp = client
            .get(&skills_url)
            .header("Accept", "application/vnd.github.v3+json")
            .send()
            .await;
        if resp.is_ok_and(|r| r.status().is_success()) {
            skills_url
        } else {
            format!("https://api.github.com/repos/{owner}/{repo}/contents{ref_query}")
        }
    };

    let resp = client
        .get(&listing_url)
        .header("Accept", "application/vnd.github.v3+json")
        .send()
        .await
        .context("fetching GitHub directory listing")?;

    if !resp.status().is_success() {
        let status = resp.status();
        anyhow::bail!("GitHub API returned {status} for {listing_url}");
    }

    let listing: serde_json::Value = resp.json().await.context("parsing directory listing")?;
    let items = listing
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("unexpected GitHub API response (not a JSON array)"))?;

    let target_dir = forge_config::config_dir()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve user config directory"))?
        .join("skills");
    std::fs::create_dir_all(&target_dir).context("creating skills directory")?;

    let mut installed = 0usize;
    for item in items {
        let name = item["name"].as_str().unwrap_or_default();
        let file_type = item["type"].as_str().unwrap_or_default();

        if file_type != "file" || !name.ends_with(".md") {
            continue;
        }

        let download_url = match item["download_url"].as_str() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => {
                eprintln!("skipping {name}: no download_url (private repo?)");
                continue;
            }
        };

        let content = client
            .get(&download_url)
            .send()
            .await
            .with_context(|| format!("downloading {name}"))?
            .text()
            .await
            .with_context(|| format!("reading {name}"))?;

        // Normalize ~/.claude/ path references → ~/.config/forge/.
        let content = content.replace("~/.claude/", "~/.config/forge/");

        let dest = target_dir.join(name);
        std::fs::write(&dest, content).with_context(|| format!("writing {}", dest.display()))?;
        println!("installed: {}", dest.display());
        installed += 1;
    }

    if installed == 0 {
        println!("no .md skill files found in {owner}/{repo}");
    } else {
        println!(
            "✓ {installed} skill file(s) installed into {}",
            target_dir.display()
        );
    }
    Ok(())
}

/// Parse a GitHub source string into `(owner, repo, Option<ref>)`.
///
/// Accepts:
/// - `owner/repo`
/// - `owner/repo@branch`
/// - `owner/repo.git` (`.git` stripped)
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo/tree/branch`
pub(crate) fn parse_github_source(source: &str) -> Result<(String, String, Option<String>)> {
    let s = source.trim();

    // Full GitHub URL.
    if s.starts_with("https://github.com/") || s.starts_with("http://github.com/") {
        let path = s
            .trim_start_matches("https://github.com/")
            .trim_start_matches("http://github.com/");
        let parts: Vec<&str> = path.splitn(4, '/').collect();
        let owner = parts
            .first()
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitHub URL owner: {s}"))?
            .to_string();
        let repo = parts
            .get(1)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow::anyhow!("cannot parse GitHub URL repo: {s}"))?
            .trim_end_matches(".git")
            .to_string();
        // https://github.com/owner/repo/tree/<ref>
        let ref_opt = if parts.get(2) == Some(&"tree") {
            parts.get(3).map(|r| r.to_string())
        } else {
            None
        };
        return Ok((owner, repo, ref_opt));
    }

    // Shorthand: [owner/repo] or [owner/repo@ref].
    if let Some((owner_repo, ref_str)) = s.split_once('@') {
        let (owner, repo) = owner_repo
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("expected owner/repo[@ref], got: {s}"))?;
        return Ok((
            owner.to_string(),
            repo.trim_end_matches(".git").to_string(),
            Some(ref_str.to_string()),
        ));
    }

    let (owner, repo) = s
        .split_once('/')
        .ok_or_else(|| anyhow::anyhow!("expected owner/repo[@ref] or a GitHub URL, got: {s}"))?;
    Ok((
        owner.to_string(),
        repo.trim_end_matches(".git").to_string(),
        None,
    ))
}

/// `forge skill normalize [--project]` — re-run path/binary normalization on all installed skills
/// and commands in-place. Replaces `~/.claude/` paths and `claude`/`codex` binary references with
/// their Forge equivalents. Safe to run multiple times; only touches files that actually changed.
pub(crate) fn skills_normalize(project: bool) -> Result<()> {
    use crate::cli::commands::import::normalize_md_dir;

    let (skills_dir, commands_dir) = if project {
        (
            std::path::PathBuf::from("./.forge/skills"),
            std::path::PathBuf::from("./.forge/commands"),
        )
    } else {
        let base =
            forge_config::config_dir().context("no user config directory on this platform")?;
        (base.join("skills"), base.join("commands"))
    };

    let mut count = 0;
    count += normalize_md_dir(&skills_dir);
    count += normalize_md_dir(&commands_dir);

    if count > 0 {
        println!("normalized {count} skill file(s) (replaced claude/codex paths and commands with forge equivalents)");
    } else {
        println!("nothing to normalize — all skills already use forge paths");
    }
    Ok(())
}

/// `forge skill import <dir> [--scope user|project]` — import a Forge bundle (a directory produced
/// by `forge skill export`, laid out as `commands/`, `skills/`, `agents/`) into the chosen scope.
/// The inverse of export; closes the round-trip so an exported library can be re-imported on another
/// machine. Reuses the same catalog-copy helpers as `forge import`: malformed files are validated
/// and skipped, and anything already present in the target scope is kept (never overwritten).
pub(crate) fn skills_import(src: &std::path::Path, scope: SkillScope) -> Result<()> {
    use crate::cli::commands::import::{copy_catalog_assets, count_copy_md_files};
    use forge_skills::{Scope, ScopedDir, Sources};

    if !src.exists() {
        anyhow::bail!("nothing to import: {} does not exist", src.display());
    }

    // Read the bundle's commands + skills through the real catalog readers (validates/skips junk).
    let sources = Sources {
        commands: vec![ScopedDir {
            scope: Scope::User,
            path: src.join("commands"),
        }],
        skills: vec![ScopedDir {
            scope: Scope::User,
            path: src.join("skills"),
        }],
    };
    let cat = forge_skills::Catalog::load(&sources);

    let (cmd_dst, skill_dst, agent_dst) = match scope {
        SkillScope::User => {
            let base =
                forge_config::config_dir().context("no user config directory on this platform")?;
            (
                base.join("commands"),
                base.join("skills"),
                base.join("agents"),
            )
        }
        SkillScope::Project => (
            std::path::PathBuf::from("./.forge/commands"),
            std::path::PathBuf::from("./.forge/skills"),
            std::path::PathBuf::from("./.forge/agents"),
        ),
    };

    let mut counts = copy_catalog_assets(&cat, &cmd_dst, &skill_dst);
    let agent_src = src.join("agents");
    if agent_src.is_dir() {
        count_copy_md_files(&agent_src, &agent_dst, &mut counts);
    }

    let scope_label = match scope {
        SkillScope::User => "the user config",
        SkillScope::Project => "./.forge",
    };
    println!(
        "✓ imported {} command(s) + {} skill(s) + {} agent(s) from {} into {scope_label} \
         ({} already present, skipped)",
        counts.copied_commands,
        counts.copied_skills,
        counts.copied_agents,
        src.display(),
        counts.skipped_commands + counts.skipped_skills + counts.skipped_agents,
    );
    for w in cat.warnings() {
        eprintln!("skipped (malformed): {w}");
    }
    Ok(())
}

/// `forge skill export <dir> [--scope user|project|all]` — copy this machine's discovered skills,
/// commands, and agents into `<dir>` (as `commands/`, `skills/`, `agents/`), the same layout
/// `forge import` reads. The inverse of import: validates via the real catalog readers (malformed
/// files are skipped + warned) and never overwrites a file already present in the destination.
pub(crate) fn skills_export(dest: &std::path::Path, scope: ExportScope) -> Result<()> {
    use crate::cli::commands::import::{copy_catalog_assets, count_copy_md_files, ImportCounts};
    use forge_skills::{Scope, ScopedDir, Sources};

    let want_user = matches!(scope, ExportScope::User | ExportScope::All);
    let want_project = matches!(scope, ExportScope::Project | ExportScope::All);

    let mut sources = Sources {
        commands: Vec::new(),
        skills: Vec::new(),
    };
    let mut agent_dirs: Vec<std::path::PathBuf> = Vec::new();
    if want_user {
        let base =
            forge_config::config_dir().context("no user config directory on this platform")?;
        sources.commands.push(ScopedDir {
            scope: Scope::User,
            path: base.join("commands"),
        });
        sources.skills.push(ScopedDir {
            scope: Scope::User,
            path: base.join("skills"),
        });
        agent_dirs.push(base.join("agents"));
    }
    if want_project {
        sources.commands.push(ScopedDir {
            scope: Scope::Project,
            path: std::path::PathBuf::from("./.forge/commands"),
        });
        sources.skills.push(ScopedDir {
            scope: Scope::Project,
            path: std::path::PathBuf::from("./.forge/skills"),
        });
        agent_dirs.push(std::path::PathBuf::from("./.forge/agents"));
    }

    // Commands + skills go through the same catalog-copy helper `forge import` uses (inverse direction).
    let cat = forge_skills::Catalog::load(&sources);
    let counts = copy_catalog_assets(&cat, &dest.join("commands"), &dest.join("skills"));

    // Agents are plain `.md` files (not catalog entries) — copy them directly.
    let mut agents = ImportCounts::default();
    let agent_dst = dest.join("agents");
    for d in &agent_dirs {
        if d.exists() {
            count_copy_md_files(d, &agent_dst, &mut agents);
        }
    }

    let copied = counts.copied_commands + counts.copied_skills + agents.copied_agents;
    println!(
        "exported to {} — {} command(s), {} skill(s), {} agent(s)",
        dest.display(),
        counts.copied_commands,
        counts.copied_skills,
        agents.copied_agents,
    );
    let skipped = counts.skipped_commands + counts.skipped_skills + agents.skipped_agents;
    if skipped > 0 {
        println!("  ({skipped} already present in the destination — kept, not overwritten)");
    }
    for w in cat.warnings() {
        eprintln!("skipped (malformed): {w}");
    }
    if copied == 0 && skipped == 0 {
        println!("nothing exported — no skills/commands/agents found in the selected scope");
    } else {
        println!(
            "import on another machine with: copy these into your Forge config dir, \
             or `forge import` a tool that reads this layout"
        );
    }
    Ok(())
}

pub(crate) async fn skill_from_session(
    session_prefix: &str,
    name_override: Option<&str>,
    scope: SkillScope,
) -> Result<()> {
    // --- Inject provider keys (same as assay / mesh) ---
    forge_config::inject_provider_keys();

    // --- Resolve session id ---
    let store = open_store()?;
    let session_id = resolve_session(&store, session_prefix)?;

    // --- Load transcript ---
    let messages = store
        .load_messages(&session_id)
        .context("loading session transcript")?;
    if messages.is_empty() {
        anyhow::bail!(
            "session {} has no messages — nothing to distil",
            &session_id[..session_id.len().min(8)]
        );
    }

    // --- Convert StoredMessage → TranscriptEntry ---
    let entries: Vec<forge_skills::TranscriptEntry> = messages
        .iter()
        .map(|m| forge_skills::TranscriptEntry {
            role: m.role.as_str().to_string(),
            content: m.content.clone(),
            tool_actions: m
                .tool_calls
                .iter()
                .map(|tc| format!("{} {}", tc.name, compact_tool_args(&tc.args.to_string())))
                .collect(),
        })
        .collect();

    // --- Derive slug (from first user prompt or --name override) ---
    let slug: String = match name_override {
        Some(n) => {
            let s = forge_skills::derive_slug(n);
            if s.is_empty() {
                anyhow::bail!("--name produced an empty slug; use alphanumeric characters");
            }
            s
        }
        None => {
            let first_user = entries
                .iter()
                .find(|e| e.role == "user" && !e.content.trim().is_empty())
                .map(|e| e.content.as_str())
                .unwrap_or("custom-skill");
            forge_skills::derive_slug(first_user)
        }
    };

    // --- Resolve target skills directory ---
    let skills_dir = match scope {
        SkillScope::User => {
            let dir = forge_config::config_dir()
                .ok_or_else(|| anyhow::anyhow!("cannot resolve user config directory"))?;
            dir.join("skills")
        }
        SkillScope::Project => std::path::PathBuf::from("./.forge/skills"),
    };

    // --- Check for existing skill before making the model call ---
    if skills_dir.join(&slug).exists() {
        anyhow::bail!(
            "skill '{}' already exists at {} — use --name to choose a different name",
            slug,
            skills_dir.join(&slug).display()
        );
    }

    // --- Discover models ---
    let config = forge_config::load().unwrap_or_default();
    let pricing = std::sync::Arc::new(forge_mesh::pricing::Pricing::from_config(&config));
    let store = std::sync::Arc::new(open_store()?);
    let cat = discover_catalog(&config).await;
    if cat.is_empty() {
        anyhow::bail!(
            "no models available — set a provider key (`forge auth <provider>`) or run ollama"
        );
    }
    let benched = store.current_benched().unwrap_or_default();
    let mut trivial_models: Vec<String> = cat
        .ranked_for(forge_types::TaskTier::Trivial, &pricing, 5)
        .into_iter()
        .filter(|m| !benched.is_benched(m))
        .collect();
    if trivial_models.is_empty() {
        if let Some(m) = config.model_for(forge_types::TaskTier::Trivial) {
            trivial_models.push(m.to_string());
        }
    }
    if trivial_models.is_empty() {
        anyhow::bail!("every model is rate-limited/benched — try `forge models --probe`");
    }
    let model = trivial_models.remove(0);

    // --- Build the distillation prompt and call the model ---
    let prompt_text = forge_skills::build_distillation_prompt(&entries);
    let messages_for_model = [
        forge_types::Message::system(
            "You are a Forge skill author. Follow the instructions exactly.",
        ),
        forge_types::Message::user(prompt_text),
    ];

    eprintln!(
        "forge skill: distilling session {} via {} ...",
        &session_id[..session_id.len().min(8)],
        model
    );

    let harness = config.mesh.bridge_mode == forge_config::BridgeMode::Harness;
    let provider = DispatchProvider::new(harness);
    let mut sink = |_: forge_provider::StreamEvent| { /* discard; no TUI in headless mode */ };
    let response = provider
        .complete(&model, &messages_for_model, &[], &mut sink)
        .await
        .map_err(|e| anyhow::anyhow!("model call failed: {e}"))?;

    // Record cost against the store (best-effort, like compact / diagnose)
    let _ = store.record_side_call_usage(&session_id, "skill/from-session", &response.usage);

    // --- Parse model output and assemble SKILL.md ---
    let (body, description) = forge_skills::parse_model_output(&response.content);
    if body.trim().is_empty() {
        anyhow::bail!("model returned an empty skill body — try again or use a different model");
    }
    let skill_md = forge_skills::assemble_skill_md(&slug, &description, &body);

    // --- Write to disk ---
    let written = forge_skills::write_skill(&skills_dir, &slug, &skill_md)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    println!("skill '{}' written to {}", slug, written.display());
    println!("invoke with: /skill {slug}");
    Ok(())
}

/// Compact a tool-call arguments JSON to at most 80 chars for the transcript summary.
pub(crate) fn compact_tool_args(args: &str) -> String {
    let s = args.trim();
    if s.chars().count() > 80 {
        s.chars().take(80).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}
