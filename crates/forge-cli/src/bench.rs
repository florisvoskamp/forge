//! SWE-bench prediction harness.
//!
//! Runs Forge's real coding harness against SWE-bench instances (real GitHub issue fixes) and emits
//! a standard `predictions.jsonl`. We generate the patches; **scoring is delegated** to the official
//! `swebench` Docker evaluator — reimplementing its hermetic per-instance test environment is out of
//! scope and not our value-add. See `docs/benchmarks/swe-bench.md` for the end-to-end flow.
//!
//! Per instance: clone the repo at `base_commit`, run one headless Forge turn on the
//! `problem_statement` (Bypass mode — no prompts), then capture the working-tree diff as the
//! `model_patch`. Sequential by design (each instance sets the process CWD).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::Mode;

/// Which agent harness solves each instance. The point of the comparison: run the SAME instances
/// through Forge vs another CLI agent (each with ITS OWN harness, on the same task + repo state),
/// score both with the official evaluator, and compare resolved rates. The external agents run
/// fully autonomous (they must edit + run commands unattended) in the freshly-reset clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Agent {
    /// Forge's own harness (in-process).
    Forge,
    /// Anthropic's Claude Code CLI (`claude -p --dangerously-skip-permissions`).
    ClaudeCode,
    /// OpenAI's Codex CLI (`codex exec --full-auto`).
    Codex,
}

impl Agent {
    fn label(self) -> &'static str {
        match self {
            Agent::Forge => "forge",
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        }
    }
}

/// One SWE-bench task. Datasets carry more fields (test patches, FAIL_TO_PASS, …) used only by the
/// evaluator; the prediction step needs just these four.
#[derive(Debug, Clone, Deserialize)]
pub struct SweInstance {
    pub instance_id: String,
    /// `owner/name` on GitHub.
    pub repo: String,
    pub base_commit: String,
    pub problem_statement: String,
}

/// A prediction row in the schema the official `swebench` evaluator consumes.
#[derive(Debug, Serialize, PartialEq)]
pub struct Prediction {
    pub instance_id: String,
    pub model_name_or_path: String,
    pub model_patch: String,
}

/// Parse a SWE-bench dataset: either JSONL (one object per line) or a top-level JSON array. Lines
/// that fail to parse are surfaced with their position so a malformed dataset is easy to fix.
pub fn load_instances(path: &Path) -> Result<Vec<SweInstance>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let trimmed = raw.trim_start();
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).context("parsing JSON-array dataset");
    }
    raw.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str::<SweInstance>(l)
                .with_context(|| format!("parsing dataset line {}", i + 1))
        })
        .collect()
}

/// Serialize predictions as JSONL (one object per line) — the input format the evaluator expects.
pub fn predictions_to_jsonl(preds: &[Prediction]) -> String {
    let mut out = preds
        .iter()
        .map(|p| serde_json::to_string(p).expect("Prediction serializes"))
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Clone (or reuse) the instance's repo under `root/<instance_id>` and hard-reset it to a clean
/// `base_commit`, so each run starts from the exact pre-fix state.
fn prepare_repo(inst: &SweInstance, root: &Path) -> Result<PathBuf> {
    let dir = root.join(&inst.instance_id);
    if !dir.join(".git").exists() {
        std::fs::create_dir_all(root)?;
        let url = format!("https://github.com/{}.git", inst.repo);
        run_git(
            root,
            &["clone", "--quiet", &url, dir.to_string_lossy().as_ref()],
        )
        .with_context(|| format!("cloning {}", inst.repo))?;
    }
    // The base_commit may not be on the default branch's fetched history — best-effort fetch it.
    let _ = run_git(&dir, &["fetch", "--quiet", "origin", &inst.base_commit]);
    run_git(&dir, &["checkout", "--quiet", "-f", &inst.base_commit])
        .with_context(|| format!("checking out {}", inst.base_commit))?;
    run_git(&dir, &["reset", "--hard", "--quiet", &inst.base_commit])?;
    run_git(&dir, &["clean", "-fdq"])?;
    Ok(dir)
}

/// The model's patch: the diff of TRACKED files after the agent's edits. We deliberately do NOT
/// `git add -A` first — that swept in untracked junk an agent run leaves in the repo (`__pycache__`
/// from running python, Forge's own `.forge/forge.db` store, etc.), which bloated and invalidated
/// the patch. SWE-bench gold patches edit existing source/test files, so a tracked-file diff is
/// what the evaluator expects; brand-new untracked files (rare) are intentionally excluded as the
/// safe default. As a belt-and-braces guard, junk paths are excluded via pathspec too.
fn extract_patch(dir: &Path) -> Result<String> {
    run_git(
        dir,
        &[
            "diff",
            "--",
            ".",
            ":(exclude).forge/**",
            ":(exclude)**/__pycache__/**",
            ":(exclude)**/*.pyc",
        ],
    )
}

/// Generate predictions for (up to `limit`) instances from `dataset`, writing `predictions.jsonl`
/// to `out`. Repos are prepared under `workdir` (clones are reused across runs). A failed instance
/// records an empty patch (counts as unresolved) rather than aborting the whole sweep.
#[allow(clippy::too_many_arguments)]
pub async fn run_swe(
    dataset: PathBuf,
    out: PathBuf,
    limit: Option<usize>,
    model: Option<String>,
    workdir: PathBuf,
    agent: Agent,
    timeout_secs: u64,
) -> Result<()> {
    let mut instances = load_instances(&dataset)?;
    if let Some(n) = limit {
        instances.truncate(n);
    }
    if instances.is_empty() {
        anyhow::bail!("no instances in {}", dataset.display());
    }
    eprintln!(
        "SWE-bench [{}]: {} instance(s); repos under {}",
        agent.label(),
        instances.len(),
        workdir.display()
    );

    let orig_cwd = std::env::current_dir().context("reading current dir")?;
    let mut preds = Vec::with_capacity(instances.len());
    let total = instances.len();
    for (i, inst) in instances.iter().enumerate() {
        eprintln!("[{}/{}] {} ({})", i + 1, total, inst.instance_id, inst.repo);
        let patch = match prepare_and_run(inst, &workdir, model.clone(), agent, timeout_secs).await
        {
            Ok(p) => {
                let lines = p.lines().count();
                eprintln!("  ✓ patch: {} lines", lines);
                p
            }
            Err(e) => {
                eprintln!("  ✗ skipped: {e:#}");
                String::new()
            }
        };
        // Always restore CWD so the next instance (and the final write) resolve correctly.
        let _ = std::env::set_current_dir(&orig_cwd);
        preds.push(Prediction {
            instance_id: inst.instance_id.clone(),
            model_name_or_path: agent.label().to_string(),
            model_patch: patch,
        });
    }

    std::fs::write(&out, predictions_to_jsonl(&preds))
        .with_context(|| format!("writing {}", out.display()))?;
    let nonempty = preds.iter().filter(|p| !p.model_patch.is_empty()).count();
    eprintln!(
        "wrote {} prediction(s) ({} with a patch) to {}",
        preds.len(),
        nonempty,
        out.display()
    );
    eprintln!("score with the official evaluator — see docs/benchmarks/swe-bench.md");
    Ok(())
}

/// Prepare one instance's repo, run a single headless Forge turn in it, and return the diff. Sets
/// the process CWD to the repo (the caller restores it).
async fn prepare_and_run(
    inst: &SweInstance,
    workdir: &Path,
    model: Option<String>,
    agent: Agent,
    timeout_secs: u64,
) -> Result<String> {
    let dir = prepare_repo(inst, workdir)?;
    std::env::set_current_dir(&dir).context("entering instance repo")?;
    match agent {
        Agent::Forge => {
            // Bypass mode: a benchmark turn runs unattended, so no permission prompts. The agent
            // edits the freshly-reset working tree; we read the diff back out afterwards.
            let mut session = crate::build_session(false, Some(Mode::Bypass), false, None, model)
                .await
                .context("building session")?;
            session
                .run_turn(&inst.problem_statement)
                .await
                .context("running the agent turn")?;
        }
        Agent::ClaudeCode | Agent::Codex => {
            run_external_agent(
                agent,
                &inst.problem_statement,
                &dir,
                model.as_deref(),
                timeout_secs,
            )
            .await?;
        }
    }
    extract_patch(&dir)
}

/// Run an external agent CLI (Claude Code / Codex) as its OWN autonomous agent in `dir`, feeding the
/// task on stdin. Both must run fully unattended (edit files + run commands without prompts) so they
/// can actually solve the instance — the clone is disposable, so the broad autonomy is contained.
async fn run_external_agent(
    agent: Agent,
    problem: &str,
    dir: &Path,
    model: Option<&str>,
    timeout_secs: u64,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let (bin, mut args): (&str, Vec<String>) = match agent {
        // `-p` reads the prompt from stdin; skip-permissions so edits + shell run unattended.
        Agent::ClaudeCode => (
            "claude",
            vec!["-p".into(), "--dangerously-skip-permissions".into()],
        ),
        // `exec` is codex's non-interactive mode; `--full-auto` = workspace-write + never-ask.
        Agent::Codex => (
            "codex",
            vec![
                "exec".into(),
                "--skip-git-repo-check".into(),
                "--full-auto".into(),
            ],
        ),
        Agent::Forge => unreachable!("forge takes the in-process path"),
    };
    if let Some(m) = model {
        args.push("--model".into());
        args.push(m.to_string());
    }

    let mut child = tokio::process::Command::new(bin)
        .args(&args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{bin}` — is it installed and on PATH?"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(problem.as_bytes()).await.ok();
        stdin.shutdown().await.ok();
    }

    let status =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child.wait()).await;
    match status {
        Ok(Ok(st)) if st.success() => Ok(()),
        // A non-zero exit is common (the agent may "fail" yet still have edited files) — don't abort
        // the instance; the diff (possibly empty) is captured by the caller either way.
        Ok(Ok(st)) => {
            eprintln!("  (note: {bin} exited {st})");
            Ok(())
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("waiting on {bin}: {e}")),
        Err(_) => {
            let _ = child.start_kill();
            anyhow::bail!("{bin} timed out after {timeout_secs}s")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_instances_parses_jsonl_and_array() {
        let dir = std::env::temp_dir().join(format!("forge-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join("d.jsonl");
        std::fs::write(
            &jsonl,
            "{\"instance_id\":\"a__b-1\",\"repo\":\"a/b\",\"base_commit\":\"abc\",\"problem_statement\":\"fix it\"}\n\n{\"instance_id\":\"a__b-2\",\"repo\":\"a/b\",\"base_commit\":\"def\",\"problem_statement\":\"and this\"}\n",
        )
        .unwrap();
        let got = load_instances(&jsonl).unwrap();
        assert_eq!(got.len(), 2, "blank lines skipped");
        assert_eq!(got[0].instance_id, "a__b-1");
        assert_eq!(got[1].base_commit, "def");

        let arr = dir.join("d.json");
        std::fs::write(
            &arr,
            "[{\"instance_id\":\"x-1\",\"repo\":\"x/y\",\"base_commit\":\"c\",\"problem_statement\":\"p\"}]",
        )
        .unwrap();
        assert_eq!(load_instances(&arr).unwrap().len(), 1);
    }

    #[test]
    fn predictions_render_as_one_json_object_per_line() {
        let preds = vec![
            Prediction {
                instance_id: "a-1".into(),
                model_name_or_path: "forge".into(),
                model_patch: "diff --git a b".into(),
            },
            Prediction {
                instance_id: "a-2".into(),
                model_name_or_path: "forge".into(),
                model_patch: String::new(),
            },
        ];
        let jsonl = predictions_to_jsonl(&preds);
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v.get("instance_id").is_some());
            assert_eq!(v["model_name_or_path"], "forge");
            assert!(v.get("model_patch").is_some());
        }
        assert!(jsonl.ends_with('\n'));
    }
}
