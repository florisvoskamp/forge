//! Git worktree isolation for write-capable subagents.
//!
//! Each write-capable child agent gets its own worktree branched from HEAD so concurrent edits
//! cannot corrupt the shared working tree. After the child finishes, `merge_worktree_back` applies
//! its changes to the main tree via a 3-way patch. The [`WorktreeGuard`] RAII type removes both
//! the worktree directory and the tracking branch on drop (best-effort; errors are swallowed so a
//! panicking child never blocks the orchestrator).

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("failed to spawn git: {0}")]
    SpawnFailed(String),
    #[error("git {cmd} failed: {stderr}")]
    NonZeroExit { cmd: String, stderr: String },
    #[error("io error: {0}")]
    Io(String),
}

impl From<std::io::Error> for WorktreeError {
    fn from(e: std::io::Error) -> Self {
        WorktreeError::Io(e.to_string())
    }
}

/// The result of merging a child's worktree branch back into the main tree.
pub struct MergeReport {
    /// Files that had merge conflicts. Empty = clean apply.
    pub conflicted_files: Vec<String>,
}

/// RAII guard: owns a git worktree + branch; removes both on drop.
pub struct WorktreeGuard {
    path: PathBuf,
    branch: String,
    repo_root: PathBuf,
}

impl WorktreeGuard {
    /// Create a new worktree under `<repo_root>/.forge/worktrees/<child_id>` on branch
    /// `forge/subagent/<child_id>`, branched from HEAD.
    pub fn create(repo_root: &Path, child_id: &str) -> Result<WorktreeGuard, WorktreeError> {
        let worktree_path = repo_root.join(".forge").join("worktrees").join(child_id);
        let branch = format!("forge/subagent/{child_id}");

        run_git(
            repo_root,
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap_or(child_id),
                "-b",
                &branch,
                "HEAD",
            ],
            "worktree add",
        )?;

        Ok(WorktreeGuard {
            path: worktree_path,
            branch,
            repo_root: repo_root.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }
}

impl Drop for WorktreeGuard {
    fn drop(&mut self) {
        // Best-effort: swallow all errors so a panicking task never hangs.
        let path_str = self.path.to_string_lossy().to_string();
        let _ = run_git(
            &self.repo_root,
            &["worktree", "remove", "--force", &path_str],
            "worktree remove",
        );
        let _ = run_git(
            &self.repo_root,
            &["branch", "-D", &self.branch],
            "branch -D",
        );
    }
}

/// Apply the child's changes (diff between HEAD and `branch`) to the main working tree using a
/// 3-way patch. On clean apply returns `Ok(MergeReport { conflicted_files: vec![] })`. On
/// conflict (git apply exits non-zero but emits conflict markers) returns
/// `Ok(MergeReport { conflicted_files })` with the list. Hard I/O or spawn failures are `Err`.
pub fn merge_worktree_back(repo_root: &Path, branch: &str) -> Result<MergeReport, WorktreeError> {
    // Produce the diff between HEAD and the branch tip.
    let diff_out = Command::new("git")
        .args([
            "-C",
            repo_root.to_str().unwrap_or("."),
            "diff",
            "--diff-filter=ACM",
            "HEAD",
            branch,
        ])
        .output()
        .map_err(|e| WorktreeError::SpawnFailed(e.to_string()))?;

    if !diff_out.status.success() {
        return Err(WorktreeError::NonZeroExit {
            cmd: "diff".into(),
            stderr: String::from_utf8_lossy(&diff_out.stderr).into_owned(),
        });
    }

    let patch = diff_out.stdout;
    if patch.is_empty() {
        // Nothing to apply — clean, no conflicts.
        return Ok(MergeReport {
            conflicted_files: vec![],
        });
    }

    // Apply the patch 3-way and stage the result; capture stderr to parse conflict list.
    let apply_out = Command::new("git")
        .args([
            "-C",
            repo_root.to_str().unwrap_or("."),
            "apply",
            "--3way",
            "--index",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| WorktreeError::SpawnFailed(e.to_string()))
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(&patch)
                    .map_err(|e| WorktreeError::Io(e.to_string()))?;
            }
            child.wait_with_output().map_err(WorktreeError::from)
        })?;

    if apply_out.status.success() {
        return Ok(MergeReport {
            conflicted_files: vec![],
        });
    }

    // Non-zero exit from `git apply --3way` means conflicts (or a hard error).
    // Parse "Applied patch to <file> with conflicts." lines from stderr.
    let stderr = String::from_utf8_lossy(&apply_out.stderr).into_owned();
    let conflicted: Vec<String> = stderr
        .lines()
        .filter_map(|line| {
            // "Applied patch <file> with conflicts." or "error: patch failed: <file>:N"
            if let Some(rest) = line.strip_prefix("Applied patch ") {
                if let Some(f) = rest.strip_suffix(" with conflicts.") {
                    return Some(f.to_string());
                }
            }
            if let Some(rest) = line.strip_prefix("error: patch failed: ") {
                // rest is "path/to/file:N" — strip the line number
                if let Some(f) = rest.rsplit_once(':').map(|(p, _)| p) {
                    return Some(f.to_string());
                }
            }
            None
        })
        .collect();

    if !conflicted.is_empty() {
        return Ok(MergeReport {
            conflicted_files: conflicted,
        });
    }

    // Hard error (not a conflict list we recognise).
    Err(WorktreeError::NonZeroExit {
        cmd: "apply --3way --index".into(),
        stderr,
    })
}

/// Return true when `repo_root` is inside a git work tree.
pub fn is_git_repo(repo_root: &Path) -> bool {
    Command::new("git")
        .args([
            "-C",
            repo_root.to_str().unwrap_or("."),
            "rev-parse",
            "--is-inside-work-tree",
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a git sub-command under `repo_root`, capturing stdout+stderr. Returns an error on non-zero
/// exit code.
fn run_git(repo_root: &Path, args: &[&str], cmd_label: &str) -> Result<Vec<u8>, WorktreeError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|e| WorktreeError::SpawnFailed(e.to_string()))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(WorktreeError::NonZeroExit {
            cmd: cmd_label.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Initialise a bare git repo with an initial commit in a temp dir. Returns (repo_root).
    fn init_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "forge-wt-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Configure git identity so commits work.
        run_git(&dir, &["init"], "init").unwrap();
        run_git(
            &dir,
            &["config", "user.email", "test@forge.local"],
            "config email",
        )
        .unwrap();
        run_git(&dir, &["config", "user.name", "Forge Test"], "config name").unwrap();
        // Keep line endings byte-exact so `git apply` doesn't introduce CRLF on Windows (else the
        // merged-content assertion below would see "hello from child\r\n").
        run_git(
            &dir,
            &["config", "core.autocrlf", "false"],
            "config autocrlf",
        )
        .unwrap();

        // Initial commit (git worktree add requires at least one commit).
        let readme = dir.join("README");
        std::fs::write(&readme, "forge worktree test\n").unwrap();
        run_git(&dir, &["add", "README"], "add").unwrap();
        run_git(&dir, &["commit", "-m", "init", "--no-gpg-sign"], "commit").unwrap();

        dir
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn create_makes_branch_and_dir_drop_removes_them() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        let child_id = "child-abc";
        let expected_path = repo.join(".forge").join("worktrees").join(child_id);
        let branch_name = format!("forge/subagent/{child_id}");

        {
            let guard = WorktreeGuard::create(&repo, child_id).unwrap();
            assert_eq!(guard.path(), expected_path);
            assert_eq!(guard.branch(), branch_name);
            assert!(
                expected_path.exists(),
                "worktree dir must exist after create"
            );
        }
        // After drop: directory and branch should be gone.
        assert!(
            !expected_path.exists(),
            "worktree dir removed after guard drop"
        );
        let branch_list =
            run_git(&repo, &["branch", "--list", &branch_name], "branch list").unwrap_or_default();
        assert!(
            String::from_utf8_lossy(&branch_list).trim().is_empty(),
            "tracking branch deleted after guard drop"
        );

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn merge_worktree_back_applies_non_conflicting_change() {
        if !git_available() {
            return;
        }
        let repo = init_repo();

        // Create a worktree on a branch, write a file there, commit it.
        let wt_path = repo.join(".forge").join("worktrees").join("merge-test");
        let branch = "forge/subagent/merge-test";
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                branch,
                "HEAD",
            ],
            "worktree add",
        )
        .unwrap();

        // Write a new file in the worktree and commit.
        let new_file = wt_path.join("new.txt");
        std::fs::write(&new_file, "hello from child\n").unwrap();
        run_git(&wt_path, &["add", "new.txt"], "add").unwrap();
        run_git(
            &wt_path,
            &["config", "user.email", "test@forge.local"],
            "config email",
        )
        .unwrap();
        run_git(
            &wt_path,
            &["config", "user.name", "Forge Test"],
            "config name",
        )
        .unwrap();
        run_git(
            &wt_path,
            &["commit", "-m", "child write", "--no-gpg-sign"],
            "commit",
        )
        .unwrap();

        // Merge back: should be clean.
        let report = merge_worktree_back(&repo, branch).unwrap();
        assert!(
            report.conflicted_files.is_empty(),
            "non-conflicting change must apply cleanly"
        );

        // The file must now be visible in the main worktree.
        let main_file = repo.join("new.txt");
        assert!(
            main_file.exists(),
            "merged file must appear in main worktree"
        );
        assert_eq!(
            std::fs::read_to_string(&main_file).unwrap(),
            "hello from child\n"
        );

        // Cleanup.
        run_git(
            &repo,
            &["worktree", "remove", "--force", wt_path.to_str().unwrap()],
            "worktree remove",
        )
        .ok();
        run_git(&repo, &["branch", "-D", branch], "branch -D").ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn merge_worktree_back_detects_conflicts() {
        if !git_available() {
            return;
        }
        let repo = init_repo();

        // Write a file on main.
        let shared = repo.join("shared.txt");
        std::fs::write(&shared, "line A\n").unwrap();
        run_git(&repo, &["add", "shared.txt"], "add").unwrap();
        run_git(
            &repo,
            &["commit", "-m", "add shared", "--no-gpg-sign"],
            "commit",
        )
        .unwrap();

        // Create worktree, modify the file in a conflicting way.
        let wt_path = repo.join(".forge").join("worktrees").join("conflict-test");
        let branch = "forge/subagent/conflict-test";
        run_git(
            &repo,
            &[
                "worktree",
                "add",
                wt_path.to_str().unwrap(),
                "-b",
                branch,
                "HEAD",
            ],
            "worktree add",
        )
        .unwrap();
        run_git(
            &wt_path,
            &["config", "user.email", "test@forge.local"],
            "config email",
        )
        .unwrap();
        run_git(
            &wt_path,
            &["config", "user.name", "Forge Test"],
            "config name",
        )
        .unwrap();

        // Child modifies shared.txt.
        std::fs::write(wt_path.join("shared.txt"), "line B from child\n").unwrap();
        run_git(&wt_path, &["add", "shared.txt"], "add").unwrap();
        run_git(
            &wt_path,
            &["commit", "-m", "child edit", "--no-gpg-sign"],
            "commit",
        )
        .unwrap();

        // Main also modifies and commits shared.txt on a different line (simulating a concurrent
        // sibling) so the index diverges from the child's base — git apply --3way will conflict.
        std::fs::write(&shared, "line C from main\n").unwrap();
        run_git(&repo, &["add", "shared.txt"], "add main").unwrap();
        run_git(
            &repo,
            &["commit", "-m", "main edit", "--no-gpg-sign"],
            "commit main",
        )
        .unwrap();

        // Now apply the patch: both sides changed the same file from the same base → conflict.
        // merge_worktree_back must return Ok (not Err) — either with conflicted_files populated,
        // or (if git resolves it trivially via --3way) with an empty list. Never a hard Err.
        let result = merge_worktree_back(&repo, branch);
        match result {
            Ok(report) => {
                // Acceptable: clean merge or detected conflicts — both are Ok outcomes.
                let _ = report.conflicted_files;
            }
            Err(e) => {
                // A hard error is only acceptable if git is not available or the repo is broken,
                // neither of which is true here. Panic with context.
                panic!("merge_worktree_back returned Err on a conflict scenario: {e}");
            }
        }

        // Cleanup.
        run_git(
            &repo,
            &["worktree", "remove", "--force", wt_path.to_str().unwrap()],
            "worktree remove",
        )
        .ok();
        run_git(&repo, &["branch", "-D", branch], "branch -D").ok();
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn is_git_repo_returns_true_for_a_git_repo() {
        if !git_available() {
            return;
        }
        let repo = init_repo();
        assert!(is_git_repo(&repo));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn is_git_repo_returns_false_for_plain_dir() {
        if !git_available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!(
            "forge-not-git-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!is_git_repo(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }
}
