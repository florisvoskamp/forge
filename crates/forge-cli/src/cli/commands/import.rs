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
    match &source {
        ImportSource::Cursor { project } => return import_cursor(*project),
        ImportSource::Aider { project } => return import_aider(*project),
        _ => {}
    }

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
    Ok(())
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
