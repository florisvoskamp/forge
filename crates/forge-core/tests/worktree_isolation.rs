//! Integration tests for git-worktree isolation of write-capable subagents.
//!
//! These tests operate on real git repos in temp dirs and call `git` directly, so they skip
//! gracefully when git is not on PATH.

use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_git(repo: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

fn init_repo() -> PathBuf {
    // Unique per (process, call) so parallel tests never collide on the same temp dir — a
    // subsec_nanos-only name races under macOS's parallel test runner and `git init` then fails.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("forge-wt-integ-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init"]).unwrap();
    run_git(&dir, &["config", "user.email", "test@forge.local"]).unwrap();
    run_git(&dir, &["config", "user.name", "Forge Test"]).unwrap();
    // Byte-exact line endings so `git apply` doesn't add CRLF on Windows.
    run_git(&dir, &["config", "core.autocrlf", "false"]).unwrap();
    // Initial commit.
    std::fs::write(dir.join("README"), "root\n").unwrap();
    run_git(&dir, &["add", "README"]).unwrap();
    run_git(&dir, &["commit", "-m", "init", "--no-gpg-sign"]).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// WorktreeGuard::create gives a valid worktree; main tree is untouched until merge.
#[test]
fn worktree_is_isolated_until_merge() {
    if !git_available() {
        return;
    }
    let repo = init_repo();

    let guard = forge_core::worktree::WorktreeGuard::create(&repo, "test-child-1").unwrap();
    let wt = guard.path().to_path_buf();
    assert!(wt.exists(), "worktree directory must be created");

    // Write a file in the worktree and commit it there.
    let new_file_wt = wt.join("output.txt");
    std::fs::write(&new_file_wt, "written by child\n").unwrap();
    run_git(&wt, &["config", "user.email", "test@forge.local"]).unwrap();
    run_git(&wt, &["config", "user.name", "Forge Test"]).unwrap();
    run_git(&wt, &["add", "output.txt"]).unwrap();
    run_git(&wt, &["commit", "-m", "child write", "--no-gpg-sign"]).unwrap();

    // The file must NOT appear in the main tree yet.
    let main_file = repo.join("output.txt");
    assert!(
        !main_file.exists(),
        "file must not appear in main tree before merge"
    );

    // Merge back: should be clean.
    let branch = guard.branch().to_string();
    let report = forge_core::worktree::merge_worktree_back(&repo, &branch).unwrap();
    assert!(
        report.conflicted_files.is_empty(),
        "non-conflicting child write must merge cleanly"
    );

    // Now the file should be visible in the main tree.
    assert!(
        main_file.exists(),
        "file must appear in main tree after merge"
    );
    assert_eq!(
        std::fs::read_to_string(&main_file).unwrap(),
        "written by child\n"
    );

    // Drop guard: worktree dir + branch removed.
    drop(guard);
    assert!(!wt.exists(), "worktree dir removed after guard drop");

    std::fs::remove_dir_all(&repo).ok();
}

/// A read-only child (no Write/Shell tools) does NOT get a worktree: is_write_capable returns
/// false for the default SUBAGENT_TOOLS set, so no WorktreeGuard would be allocated.
#[test]
fn read_only_agent_is_not_write_capable() {
    let registry = forge_tools::ToolRegistry::with_core_tools();

    // Default subagent toolset (empty tools list → read-only investigator).
    let read_only = forge_core::subagent::ResolvedAgent {
        name: "general".into(),
        task: "find things".into(),
        system_prompt: "s".into(),
        tools: Vec::new(),
        tier: None,
    };
    assert!(
        !forge_core::subagent::is_write_capable(&read_only, &registry),
        "default subagent tools are read-only"
    );

    // Agent with write_file is write-capable.
    let writer = forge_core::subagent::ResolvedAgent {
        name: "writer".into(),
        task: "write things".into(),
        system_prompt: "s".into(),
        tools: vec!["write_file".into()],
        tier: None,
    };
    assert!(
        forge_core::subagent::is_write_capable(&writer, &registry),
        "agent with write_file is write-capable"
    );
}

/// is_git_repo returns true for a real repo and false for a plain dir.
#[test]
fn is_git_repo_detection() {
    if !git_available() {
        return;
    }
    let repo = init_repo();
    assert!(forge_core::worktree::is_git_repo(&repo));

    let plain = std::env::temp_dir().join(format!(
        "forge-plain-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&plain).unwrap();
    assert!(!forge_core::worktree::is_git_repo(&plain));

    std::fs::remove_dir_all(&repo).ok();
    std::fs::remove_dir_all(&plain).ok();
}
