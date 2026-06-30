use anyhow::{Context, Result};

use crate::*;

/// `forge import <source> [--project]` — copy another AI CLI's commands/skills/agents into a
/// Forge scope, reusing the CC-compatible readers to validate before copying. Claude imports
/// commands + skills + agents; Codex imports its prompts as commands.
/// The `prepare-commit-msg` hook Forge installs. Strips Claude/Codex/Anthropic co-author lines,
/// then adds Forge as co-author — model-aware: if Forge wrote the active model to
/// `$GIT_DIR/forge-model` (it does, each turn), the model rides in the trailer.
const FORGE_COMMIT_HOOK: &str = r#"#!/bin/sh
# Installed by Forge — rewrites commit co-author attribution.
# Strips Claude/Codex/Anthropic co-author lines; adds Forge (with the active model) as co-author.
COMMIT_MSG_FILE="$1"
GIT_DIR=$(git rev-parse --git-dir 2>/dev/null || echo .git)
MODEL=$(cat "$GIT_DIR/forge-model" 2>/dev/null)
filtered=$(grep -Ev '^Co-Authored-By:.*([Cc]laude|[Cc]odex|[Aa]nthrop)' "$COMMIT_MSG_FILE") || filtered=$(cat "$COMMIT_MSG_FILE")
if [ -n "$MODEL" ]; then
  printf '%s\n\nCo-Authored-By: Forge (%s) <forge@adulari.dev>\n' "$filtered" "$MODEL" > "$COMMIT_MSG_FILE"
else
  printf '%s\n\nCo-Authored-By: Forge <forge@adulari.dev>\n' "$filtered" > "$COMMIT_MSG_FILE"
fi
"#;

/// Walk up from `cwd` to find the enclosing `.git` directory, if any.
fn find_git_dir() -> Option<std::path::PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.join(".git"));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Write the model-aware `prepare-commit-msg` hook into `git_dir`, making it executable.
fn install_commit_hook(git_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let hook_path = git_dir.join("hooks").join("prepare-commit-msg");
    if let Some(hooks_dir) = hook_path.parent() {
        std::fs::create_dir_all(hooks_dir).context("creating .git/hooks directory")?;
    }
    std::fs::write(&hook_path, FORGE_COMMIT_HOOK)
        .with_context(|| format!("writing {}", hook_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }
    Ok(hook_path)
}

/// Auto-install the commit hook when `[git] coauthor = true` and we're inside a git repo. Best-
/// effort and idempotent (the hook is just overwritten), so a fresh clone gets attribution without
/// the user remembering to run `forge git setup`. Silent on any failure — it's a convenience.
pub(crate) fn maybe_install_git_hook(config: &forge_config::Config) {
    if !config.git.coauthor {
        return;
    }
    if let Some(git_dir) = find_git_dir() {
        let _ = install_commit_hook(&git_dir);
    }
}

/// Record the active model where the commit hook can read it (`$GIT_DIR/forge-model`), so a commit
/// the agent makes this turn is attributed to the model that actually did the work. Best-effort.
pub(crate) fn write_active_model(model: &str) {
    if let Some(git_dir) = find_git_dir() {
        let _ = std::fs::write(git_dir.join("forge-model"), model);
    }
}

pub(crate) fn git_cmd(cmd: GitCmd) -> Result<()> {
    match cmd {
        GitCmd::Setup { force } => {
            let config = forge_config::load().context("loading forge config")?;
            if !force && !config.git.coauthor {
                anyhow::bail!(
                    "git.coauthor is not enabled in .forge/config.toml\n\
                     Add `[git]\ncoauthor = true` to enable, or run `forge git setup --force`."
                );
            }
            let git_dir = find_git_dir().context("not inside a git repository")?;
            let hook_path = install_commit_hook(&git_dir)?;
            println!("✓ installed {}", hook_path.display());
            println!("  strips Claude/Codex co-author lines; adds Co-Authored-By: Forge (model)");
            Ok(())
        }
    }
}
