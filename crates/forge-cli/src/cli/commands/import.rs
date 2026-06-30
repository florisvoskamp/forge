use anyhow::{Context, Result};

use crate::*;

/// Tally of an import run: copied vs. already-present, for commands and skills.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportCounts {
    pub(crate) copied_commands: usize,
    pub(crate) skipped_commands: usize,
    pub(crate) copied_skills: usize,
    pub(crate) skipped_skills: usize,
    pub(crate) copied_agents: usize,
    pub(crate) skipped_agents: usize,
}

pub(crate) fn import_cmd(source: ImportSource) -> Result<()> {
    // Cursor and Aider have non-standard directory/format layouts — handled separately.
    // `--scope` wins over the legacy `--project` boolean for every source (see `Scope::to_project`).
    match &source {
        ImportSource::Cursor { scope, project } => {
            return import_cursor(Scope::to_project(*scope, *project))
        }
        ImportSource::Aider { scope, project } => {
            return import_aider(Scope::to_project(*scope, *project))
        }
        _ => {}
    }

    let (label, project, home, commands_sub, skills_sub, agents_sub) = match source {
        ImportSource::Claude { scope, project } => (
            "claude",
            Scope::to_project(scope, project),
            forge_config::claude_dir().context("no home directory — cannot locate ~/.claude")?,
            "commands",
            Some("skills"),
            Some("agents"),
        ),
        ImportSource::Codex { scope, project } => (
            "codex",
            Scope::to_project(scope, project),
            forge_config::codex_dir().context("no home directory — cannot locate ~/.codex")?,
            "prompts",
            None,
            None,
        ),
        ImportSource::Cursor { .. } | ImportSource::Aider { .. } => unreachable!(),
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

    // Claude's standing instructions (`./CLAUDE.md`) → Forge's `./.forge/AGENTS.md`, so a migrating
    // user keeps their agent guidance, not just commands/skills. Project scope only: Forge injects
    // `./.forge/AGENTS.md` / `./AGENTS.md` per turn — there's no user-global agent-memory location.
    let mut imported_memory = false;
    if label == "claude" && project {
        let src = std::path::PathBuf::from("./CLAUDE.md");
        let dst = std::path::PathBuf::from("./.forge/AGENTS.md");
        if src.is_file() && !dst.exists() {
            std::fs::create_dir_all("./.forge").ok();
            imported_memory = std::fs::copy(&src, &dst).is_ok();
        }
    }

    let scope = if project {
        "./.forge"
    } else {
        "the user config"
    };
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
    if imported_memory {
        println!("✓ imported CLAUDE.md → ./.forge/AGENTS.md (standing instructions)");
    }
    for w in cat.warnings() {
        eprintln!("skipped (malformed): {w}");
    }

    // Coverage uplift: also transfer settings.json permission rules + hooks (CC-compatible) and
    // fold in the MCP servers, instead of silently dropping them. Best-effort; each piece reports
    // what transferred vs. what was skipped.
    if label == "claude" {
        import_claude_settings(&home, project);
    }
    import_tool_mcp_servers(label, project);
    Ok(())
}

/// Translate a Claude-Code `settings.json` (user `~/.claude/settings.json` + project `.claude/`)
/// into Forge: permission allow/ask/deny rules → `[[permissions.rules]]`, and hooks → a
/// CC-compatible `settings.json` Forge loads natively (item: CC-compatible hooks). Prints a summary.
fn import_claude_settings(claude_home: &std::path::Path, project: bool) {
    // Gather every CC settings file that exists, in increasing precedence.
    let mut sources = vec![claude_home.join("settings.json")];
    sources.push(std::path::PathBuf::from("./.claude/settings.json"));
    sources.push(std::path::PathBuf::from("./.claude/settings.local.json"));

    let mut values = Vec::new();
    for p in &sources {
        if let Ok(text) = std::fs::read_to_string(p) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                values.push(v);
            }
        }
    }
    if values.is_empty() {
        return;
    }

    // Target files: project scope writes `./.forge/...`; user scope writes the user config dir.
    let (settings_dst, config_dst) = if project {
        (
            std::path::PathBuf::from("./.forge/settings.json"),
            std::path::PathBuf::from("./.forge/config.toml"),
        )
    } else {
        match forge_config::config_dir() {
            Some(dir) => (dir.join("settings.json"), dir.join("config.toml")),
            None => return,
        }
    };

    // Hooks are NOT imported by default (see `count_cc_hooks` doc comment for why) — only
    // permission rules transfer automatically.
    let hooks_n = count_cc_hooks(&values);
    let perms_n = transfer_permissions(&values, &config_dst);

    if hooks_n > 0 {
        println!(
            "• found {hooks_n} hook(s) in settings.json — NOT imported. Claude Code hooks \
             commonly assume they're running inside Claude Code itself (e.g. injecting text \
             into an LLM's system prompt, or talking to Claude-Code-only tooling) and can \
             misbehave or print confusing raw output when Forge runs them as its own session \
             hooks. Review them and add any that genuinely apply to {} manually \
             (docs/features/hooks.md).",
            settings_dst.display()
        );
    }
    if perms_n > 0 {
        println!(
            "✓ imported {perms_n} permission rule(s) from settings.json → {}",
            config_dst.display()
        );
    }
    if hooks_n == 0 && perms_n == 0 {
        println!("• no hooks or permission rules found in settings.json to import");
    }
}

/// Count the hook command entries present across the CC settings sources — for the import
/// summary only, nothing is written. Hooks are deliberately NOT auto-imported (unlike commands/
/// skills/agents/permissions): a CC hook script is written against Claude Code's specific
/// behavior — many exist purely to inject text into an LLM's SYSTEM PROMPT (mode trackers,
/// project-context primers) or to talk to Claude-Code-only tooling, and assume nobody but an
/// LLM ever sees their raw stdout. Forge has its own session lifecycle and currently renders a
/// CC-compatible hook's stdout directly as a visible chat note — so blindly importing someone's
/// personal Claude Code hook set silently turned every new Forge session into a wall of garbled,
/// context-injection-style text (found via a real user report; the hooks were never meant to be
/// shown to a human). Permissions stay auto-imported below — `allow`/`deny`/`ask` tool rules are
/// data, not arbitrary code, so they carry no equivalent execution-context mismatch risk.
fn count_cc_hooks(values: &[serde_json::Value]) -> usize {
    use serde_json::Value;
    let mut merged: serde_json::Map<String, Value> = serde_json::Map::new();
    for v in values {
        let Some(hooks) = v.get("hooks").and_then(|h| h.as_object()) else {
            continue;
        };
        for (event, groups) in hooks {
            let Some(groups) = groups.as_array() else {
                continue;
            };
            let entry = merged
                .entry(event.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(arr) = entry.as_array_mut() {
                arr.extend(groups.iter().cloned());
            }
        }
    }
    if merged.is_empty() {
        return 0;
    }
    forge_config::cc_hooks_from_settings(&Value::Object(merged)).len()
}

/// Translate CC `permissions.{allow,ask,deny}` entries into Forge `[[permissions.rules]]` blocks,
/// appended to the target `config.toml`. Returns the number of rules written.
fn transfer_permissions(values: &[serde_json::Value], config_dst: &std::path::Path) -> usize {
    let mut blocks = String::new();
    let mut count = 0usize;
    for v in values {
        let Some(perms) = v.get("permissions").and_then(|p| p.as_object()) else {
            continue;
        };
        for (kind, decision) in [("deny", "deny"), ("ask", "ask"), ("allow", "allow")] {
            let Some(arr) = perms.get(kind).and_then(|a| a.as_array()) else {
                continue;
            };
            for item in arr {
                let Some(s) = item.as_str() else { continue };
                let (cc_tool, pattern) = parse_cc_permission(s);
                let tool = forge_config::forge_tool_from_cc(&cc_tool);
                let pat = pattern.unwrap_or_else(|| "*".to_string());
                blocks.push_str(&format!(
                    "\n[[permissions.rules]]\ntool = {}\n{decision} = {}\nreason = \"imported from Claude Code settings.json\"\n",
                    toml_str(tool),
                    toml_str(&pat),
                ));
                count += 1;
            }
        }
    }
    if count == 0 {
        return 0;
    }
    if let Some(parent) = config_dst.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // Append (create if absent) so existing config.toml settings are preserved.
    use std::io::Write;
    let header = if config_dst.exists() {
        String::new()
    } else {
        "# Forge config (imported permission rules from Claude Code)\n".to_string()
    };
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config_dst)
    {
        Ok(mut f) => {
            let _ = write!(f, "{header}{blocks}");
            count
        }
        Err(_) => 0,
    }
}

/// Parse a CC permission string `Tool(pattern)` → `(tool, Some(pattern))`; a bare `Tool` →
/// `(tool, None)`.
fn parse_cc_permission(s: &str) -> (String, Option<String>) {
    let s = s.trim();
    if let Some(open) = s.find('(') {
        if s.ends_with(')') {
            let tool = s[..open].trim().to_string();
            let pat = s[open + 1..s.len() - 1].trim().to_string();
            return (tool, (!pat.is_empty()).then_some(pat));
        }
    }
    (s.to_string(), None)
}

/// Quote a string as a TOML basic string (escaping `\` and `"`).
fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Fold the tool's MCP servers into `.forge/mcp.toml` (item: fold `forge mcp import` into
/// `forge import`). Non-interactive: imports every server discovered for this tool, storing secrets
/// in the OS keyring. `label` is `claude` or `codex`; other labels have no MCP sources here.
fn import_tool_mcp_servers(label: &str, _project: bool) {
    let prefix = match label {
        "claude" => "claude",
        "codex" => "codex",
        _ => return,
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let sources = forge_config::discover_import_sources(&cwd);
    let mut servers = Vec::new();
    let mut secrets = std::collections::HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for s in &sources {
        if !s.label.starts_with(prefix) {
            continue;
        }
        for srv in &s.servers {
            if seen.insert(srv.name.clone()) {
                for key in srv.keyring_keys() {
                    if let Some(val) = s.secrets.get(&key) {
                        secrets.insert(key, val.clone());
                    }
                }
                servers.push(srv.clone());
            }
        }
    }
    if servers.is_empty() {
        return;
    }
    let out = std::path::Path::new(".forge/mcp.toml");
    if let Err(e) = crate::cli::commands::mcp::finish_import(out, servers, secrets) {
        eprintln!("• MCP import skipped: {e}");
    }
}

/// Copy `*.md` files from `src` into `dst`, skipping any that already exist. Updates `counts`.
pub(crate) fn count_copy_md_files(
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
pub(crate) fn copy_catalog_assets(
    cat: &forge_skills::Catalog,
    cmd_dst: &std::path::Path,
    skill_dst: &std::path::Path,
) -> ImportCounts {
    let mut counts = ImportCounts::default();
    std::fs::create_dir_all(cmd_dst).ok();
    for cmd in cat.all_commands() {
        // Preserve the command's NAMESPACE (its source subdirectory). A subdir command `git/commit.md`
        // loads as the namespaced name `git:commit`; copying it by bare file name flattened it to
        // `commit.md`, dropping the namespace (so `/git:commit` became `/commit`) and risking a
        // collision with another `commit.md` from a different namespace. Rebuild the relative path
        // from the namespaced name (`git:commit` → `git/commit.md`) so the layout round-trips.
        let rel = format!(
            "{}.md",
            cmd.name.replace(':', std::path::MAIN_SEPARATOR_STR)
        );
        let dest = cmd_dst.join(rel);
        if dest.exists() {
            counts.skipped_commands += 1;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).ok();
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

/// Import Cursor AI rules (`~/.cursor/rules/*.mdc`) as Forge commands.
/// Each `.mdc` file is converted to a CC-compatible `.md` command: the YAML front-matter
/// `description:` is kept, while `globs` / `alwaysApply` are dropped.
pub(crate) fn import_cursor(project: bool) -> Result<()> {
    let rules_dir = forge_config::cursor_dir()
        .context("no home directory — cannot locate ~/.cursor")?
        .join("rules");
    if !rules_dir.exists() {
        println!("nothing to import: {} does not exist", rules_dir.display());
        return Ok(());
    }
    let cmd_dst = if project {
        std::path::PathBuf::from("./.forge/commands")
    } else {
        forge_config::config_dir()
            .context("no config directory on this platform")?
            .join("commands")
    };
    std::fs::create_dir_all(&cmd_dst).ok();

    let mut copied = 0usize;
    let mut skipped = 0usize;
    let Ok(entries) = std::fs::read_dir(&rules_dir) else {
        println!("nothing to import: cannot read {}", rules_dir.display());
        return Ok(());
    };
    for entry in entries.flatten() {
        let from = entry.path();
        if from.extension().and_then(|e| e.to_str()) != Some("mdc") {
            continue;
        }
        let stem = from
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("cursor-rule");
        let dest = cmd_dst.join(format!("{stem}.md"));
        if dest.exists() {
            skipped += 1;
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&from) else {
            continue;
        };
        let converted = convert_mdc_to_command_md(&raw, stem);
        if std::fs::write(&dest, converted).is_ok() {
            copied += 1;
        }
    }
    let scope = if project {
        "./.forge"
    } else {
        "the user config"
    };
    println!("✓ imported {copied} command(s) from cursor into {scope} ({skipped} already present, skipped)");
    Ok(())
}

/// Strip `.mdc` YAML front-matter down to just `description:`, preserving the body.
/// Unknown YAML fields (`globs`, `alwaysApply`, etc.) are dropped.
pub(crate) fn convert_mdc_to_command_md(raw: &str, fallback_name: &str) -> String {
    let (description, body) = if let Some(rest) = raw.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            let description = fm
                .lines()
                .find_map(|l| {
                    let l = l.trim();
                    l.strip_prefix("description:")
                        .map(|v| v.trim().trim_matches('"').trim_matches('\'').to_string())
                })
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| format!("Cursor rule: {fallback_name}"));
            let body = rest[end + 4..].trim_start_matches('\n').to_string();
            (description, body)
        } else {
            (format!("Cursor rule: {fallback_name}"), raw.to_string())
        }
    } else {
        (format!("Cursor rule: {fallback_name}"), raw.to_string())
    };
    format!("---\ndescription: \"{description}\"\n---\n{body}")
}

/// Import Aider convention files as Forge commands. Looks for `CONVENTIONS.md`,
/// `.aider.md`, and `.aider.conventions.md` in `$HOME` then `$PWD`.
pub(crate) fn import_aider(project: bool) -> Result<()> {
    let cmd_dst = if project {
        std::path::PathBuf::from("./.forge/commands")
    } else {
        forge_config::config_dir()
            .context("no config directory on this platform")?
            .join("commands")
    };
    std::fs::create_dir_all(&cmd_dst).ok();

    let search_dirs: Vec<std::path::PathBuf> =
        [forge_config::home_dir(), std::env::current_dir().ok()]
            .into_iter()
            .flatten()
            .collect();

    let candidates = ["CONVENTIONS.md", ".aider.md", ".aider.conventions.md"];
    let mut copied = 0usize;
    let mut skipped = 0usize;

    for dir in &search_dirs {
        for name in candidates {
            let from = dir.join(name);
            if !from.is_file() {
                continue;
            }
            let dest = cmd_dst.join(name);
            if dest.exists() {
                skipped += 1;
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&from) else {
                continue;
            };
            // Wrap the file as a CC-compatible command if it lacks front-matter.
            let content = if raw.starts_with("---") {
                raw
            } else {
                let stem = from
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("aider-conventions");
                format!("---\ndescription: \"Aider conventions ({stem})\"\n---\n{raw}")
            };
            if std::fs::write(&dest, content).is_ok() {
                copied += 1;
            }
        }
    }

    if copied == 0 && skipped == 0 {
        println!("nothing to import: no Aider convention files found (CONVENTIONS.md, .aider.md, .aider.conventions.md)");
        return Ok(());
    }
    let scope = if project {
        "./.forge"
    } else {
        "the user config"
    };
    println!("✓ imported {copied} command(s) from aider into {scope} ({skipped} already present, skipped)");
    Ok(())
}

/// Recursively copy a directory tree (used to import a skill's SKILL.md + its resource files).
pub(crate) fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
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

/// Recursively walk `dir` and normalize every `.md` file in-place using
/// [`forge_skills::normalize_skill_content`]. Returns the count of files actually changed.
pub(crate) fn normalize_md_dir(dir: &std::path::Path) -> usize {
    let mut count = 0;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            count += normalize_md_dir(&path);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let normalized = forge_skills::normalize_skill_content(&content);
                if normalized != content && std::fs::write(&path, &normalized).is_ok() {
                    count += 1;
                }
            }
        }
    }
    count
}

#[cfg(test)]
mod settings_import_tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("forge-imp-{name}-{}", forge_types::new_id()))
    }

    #[test]
    fn parse_cc_permission_handles_tool_and_pattern() {
        assert_eq!(
            parse_cc_permission("Bash(npm run test:*)"),
            ("Bash".to_string(), Some("npm run test:*".to_string()))
        );
        assert_eq!(parse_cc_permission("Read"), ("Read".to_string(), None));
        assert_eq!(
            parse_cc_permission("WebFetch(domain:docs.rs)"),
            ("WebFetch".to_string(), Some("domain:docs.rs".to_string()))
        );
    }

    #[test]
    fn transfer_permissions_translates_cc_rules_into_toml() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{ "permissions": {
                   "allow": ["Bash(npm run lint)", "Read"],
                   "deny": ["Bash(rm -rf *)"]
                 } }"#,
        )
        .unwrap();
        let dir = tmp("perms");
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        let n = transfer_permissions(&[v], &cfg);
        assert_eq!(n, 3, "two allows + one deny");

        // The written TOML parses, and its [[permissions.rules]] carry the translated tool names +
        // patterns (CC `Bash` → Forge `shell`, `Read` → `read_file`, bare tool → pattern `*`).
        let text = std::fs::read_to_string(&cfg).unwrap();
        let root: toml::Value = toml::from_str(&text).expect("imported config.toml must be valid");
        let rules = root["permissions"]["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 3);
        assert!(rules.iter().any(|r| r["tool"].as_str() == Some("shell")
            && r.get("deny").and_then(|d| d.as_str()) == Some("rm -rf *")));
        assert!(rules.iter().any(|r| r["tool"].as_str() == Some("shell")
            && r.get("allow").and_then(|d| d.as_str()) == Some("npm run lint")));
        assert!(rules.iter().any(|r| r["tool"].as_str() == Some("read_file")
            && r.get("allow").and_then(|d| d.as_str()) == Some("*")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn count_cc_hooks_counts_without_writing_anything() {
        // Hooks must NOT be auto-imported (they assume Claude-Code-specific behavior and Forge
        // renders their raw stdout as a visible chat note — see count_cc_hooks's doc comment).
        // count_cc_hooks only reports how many exist; it must not touch the filesystem at all.
        let v: serde_json::Value = serde_json::from_str(
            r#"{ "hooks": {
                   "PreToolUse": [
                     { "matcher": "Bash",
                       "hooks": [ { "type": "command", "command": "./audit.sh" } ] }
                   ]
                 } }"#,
        )
        .unwrap();
        let dir = tmp("hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        let n = count_cc_hooks(&[v]);
        assert_eq!(n, 1, "one hook command counted");
        assert!(
            !settings.exists(),
            "count_cc_hooks must not write a settings.json"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn count_cc_hooks_sums_across_multiple_settings_sources() {
        let user: serde_json::Value = serde_json::from_str(
            r#"{ "hooks": { "PostToolUse": [ { "hooks": [ { "type": "command", "command": "x" } ] } ] } }"#,
        )
        .unwrap();
        let project: serde_json::Value = serde_json::from_str(
            r#"{ "hooks": { "PreToolUse": [ { "hooks": [ { "type": "command", "command": "y" } ] } ] } }"#,
        )
        .unwrap();
        assert_eq!(
            count_cc_hooks(&[user, project]),
            2,
            "hooks from every CC settings source are counted"
        );
    }

    #[test]
    fn count_cc_hooks_is_zero_with_no_hooks_key() {
        let v: serde_json::Value = serde_json::from_str(r#"{ "permissions": {} }"#).unwrap();
        assert_eq!(count_cc_hooks(&[v]), 0);
    }
}
