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

/// Per-instance resource accounting, written to a `<out>.metrics.jsonl` sidecar alongside the
/// predictions. This is what powers the headline comparison: not just *resolved rate* but
/// **tokens-per-success** and cost/wall — so "Forge bridging model X" can be shown to use fewer
/// tokens (and equal-or-better resolve rate) than running model X's own CLI directly.
///
/// Token/cost capture is reliable for the in-process Forge agent (read from its own usage DB,
/// which records bridge usage too). For an external CLI it is best-effort: parsed from the CLI's
/// machine output where available (claude `--output-format json`), else left at 0 and flagged
/// `metrics_complete = false` so the report can exclude it from token claims rather than lie.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstanceMetric {
    pub instance_id: String,
    pub agent: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    pub wall_secs: f64,
    /// Whether a non-empty patch was produced (a *submitted* attempt; not the same as *resolved*,
    /// which only the official evaluator decides).
    pub patched: bool,
    /// False when token/cost numbers could not be captured (external CLI without machine output).
    pub metrics_complete: bool,
}

/// Outcome of running one instance: the patch plus the resources it took.
struct RunOutcome {
    patch: String,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    wall_secs: f64,
    metrics_complete: bool,
}

/// Serialize per-instance metrics as JSONL (one object per line) for the `<out>.metrics.jsonl`
/// sidecar.
pub fn metrics_to_jsonl(metrics: &[InstanceMetric]) -> String {
    let mut out = metrics
        .iter()
        .map(|m| serde_json::to_string(m).expect("InstanceMetric serializes"))
        .collect::<Vec<_>>()
        .join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// `predictions.jsonl` → `predictions.metrics.jsonl` (insert `.metrics` before the extension).
fn metrics_path(out: &Path) -> PathBuf {
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("preds");
    let ext = out.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    out.with_file_name(format!("{stem}.metrics.{ext}"))
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
    // Absolutize `root` first: the per-instance turn changes the process CWD, and `git clone` is
    // invoked with `current_dir(root)` while passing the target path — if both stay relative the
    // target resolves to `root/root/<id>` (double-nested) and the later checkout, run from the
    // original CWD, fails with `os error 2`. An absolute `dir` removes that ambiguity entirely.
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)
        .with_context(|| format!("resolving workdir {}", root.display()))?;
    let dir = root.join(&inst.instance_id);
    if !dir.join(".git").exists() {
        let url = format!("https://github.com/{}.git", inst.repo);
        run_git(
            &root,
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
    // STAGE everything first (including NEW files), then diff the index against HEAD. A plain
    // `git diff` ignores untracked files, so a solution that ADDS a file (very common in SWE-bench —
    // new modules, regression tests) produced an EMPTY patch and was scored unresolved even though
    // the agent did the work. `git add -A` + `git diff --cached` captures additions, modifications,
    // and deletions alike. Excludes keep Forge's own dir / pycache out of the patch.
    let pathspec = [
        "--",
        ".",
        ":(exclude).forge/**",
        ":(exclude)**/__pycache__/**",
        ":(exclude)**/*.pyc",
    ];
    let mut add = vec!["add", "-A"];
    add.extend_from_slice(&pathspec);
    run_git(dir, &add)?;
    let mut diff = vec!["diff", "--cached"];
    diff.extend_from_slice(&pathspec);
    run_git(dir, &diff)
}

/// Insert `.seed<k>` before the extension of `out` (e.g. `preds.jsonl` → `preds.seed2.jsonl`), so
/// multi-attempt runs write one predictions file per seed for pass@k scoring.
fn seed_path(out: &Path, seed: usize) -> PathBuf {
    let stem = out.file_stem().and_then(|s| s.to_str()).unwrap_or("preds");
    let ext = out.extension().and_then(|s| s.to_str()).unwrap_or("jsonl");
    out.with_file_name(format!("{stem}.seed{seed}.{ext}"))
}

/// Generate predictions for (up to `limit`) instances, optionally over `attempts` seeds (for
/// pass@k / best-of-k). With one attempt, writes `out`; with several, writes `out.seed1.jsonl`,
/// `out.seed2.jsonl`, … (each scored separately, then aggregated with `forge bench passk`). Repos
/// are prepared under `workdir` (clones reused across runs + seeds).
#[allow(clippy::too_many_arguments)]
pub async fn run_swe(
    dataset: PathBuf,
    out: PathBuf,
    limit: Option<usize>,
    model: Option<String>,
    workdir: PathBuf,
    agent: Agent,
    timeout_secs: u64,
    attempts: usize,
) -> Result<()> {
    let mut instances = load_instances(&dataset)?;
    if let Some(n) = limit {
        instances.truncate(n);
    }
    if instances.is_empty() {
        anyhow::bail!("no instances in {}", dataset.display());
    }
    let attempts = attempts.max(1);
    for seed in 1..=attempts {
        let seed_out = if attempts == 1 {
            out.clone()
        } else {
            seed_path(&out, seed)
        };
        if attempts > 1 {
            eprintln!("=== attempt {seed}/{attempts} ===");
        }
        run_one_sweep(
            &instances,
            model.clone(),
            &workdir,
            agent,
            timeout_secs,
            &seed_out,
        )
        .await?;
    }
    Ok(())
}

/// One full sweep over `instances`, writing predictions to `out`. A failed instance records an empty
/// patch (counts as unresolved) rather than aborting the sweep.
async fn run_one_sweep(
    instances: &[SweInstance],
    model: Option<String>,
    workdir: &Path,
    agent: Agent,
    timeout_secs: u64,
    out: &Path,
) -> Result<()> {
    eprintln!(
        "SWE-bench [{}]: {} instance(s) → {}; repos under {}",
        agent.label(),
        instances.len(),
        out.display(),
        workdir.display()
    );

    let orig_cwd = std::env::current_dir().context("reading current dir")?;
    let mut preds = Vec::with_capacity(instances.len());
    let mut metrics = Vec::with_capacity(instances.len());
    let total = instances.len();
    for (i, inst) in instances.iter().enumerate() {
        eprintln!("[{}/{}] {} ({})", i + 1, total, inst.instance_id, inst.repo);
        let outcome = match prepare_and_run(inst, workdir, model.clone(), agent, timeout_secs).await
        {
            Ok(o) => {
                eprintln!(
                    "  ✓ patch: {} lines · {} tok ({} in / {} out) · {:.1}s{}",
                    o.patch.lines().count(),
                    o.input_tokens + o.output_tokens,
                    o.input_tokens,
                    o.output_tokens,
                    o.wall_secs,
                    if o.metrics_complete {
                        ""
                    } else {
                        " · tokens n/a"
                    },
                );
                o
            }
            Err(e) => {
                eprintln!("  ✗ skipped: {e:#}");
                RunOutcome {
                    patch: String::new(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cost_usd: 0.0,
                    wall_secs: 0.0,
                    metrics_complete: false,
                }
            }
        };
        // Always restore CWD so the next instance (and the final write) resolve correctly.
        let _ = std::env::set_current_dir(&orig_cwd);
        let patched = !outcome.patch.is_empty();
        metrics.push(InstanceMetric {
            instance_id: inst.instance_id.clone(),
            agent: agent.label().to_string(),
            input_tokens: outcome.input_tokens,
            output_tokens: outcome.output_tokens,
            total_tokens: outcome.input_tokens + outcome.output_tokens,
            cost_usd: outcome.cost_usd,
            wall_secs: outcome.wall_secs,
            patched,
            metrics_complete: outcome.metrics_complete,
        });
        preds.push(Prediction {
            instance_id: inst.instance_id.clone(),
            model_name_or_path: agent.label().to_string(),
            model_patch: outcome.patch,
        });
    }

    std::fs::write(out, predictions_to_jsonl(&preds))
        .with_context(|| format!("writing {}", out.display()))?;
    let metrics_out = metrics_path(out);
    std::fs::write(&metrics_out, metrics_to_jsonl(&metrics))
        .with_context(|| format!("writing {}", metrics_out.display()))?;
    let nonempty = preds.iter().filter(|p| !p.model_patch.is_empty()).count();
    eprintln!(
        "wrote {} prediction(s) ({} with a patch) to {}; metrics → {}",
        preds.len(),
        nonempty,
        out.display(),
        metrics_out.display(),
    );
    eprintln!("score with the official evaluator — see docs/benchmarks/swe-bench.md");
    Ok(())
}

/// Aggregate pass@k from several swebench evaluation reports (the `*.json` written by
/// `run_evaluation`, one per seed). pass@k = an instance counts as solved if ANY seed resolved it;
/// also prints each seed's own resolved count so variance is visible.
pub fn passk(reports: &[PathBuf]) -> Result<()> {
    use std::collections::BTreeSet;
    if reports.is_empty() {
        anyhow::bail!("pass@k needs at least one report (the *.json from run_evaluation)");
    }
    let mut union: BTreeSet<String> = BTreeSet::new();
    let mut submitted = 0usize;
    for (i, path) in reports.iter().enumerate() {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading report {}", path.display()))?;
        let v: serde_json::Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        let resolved: Vec<String> = v
            .get("resolved_ids")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        submitted = submitted.max(
            v.get("submitted_instances")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as usize,
        );
        eprintln!(
            "  seed {}: {} resolved  ({})",
            i + 1,
            resolved.len(),
            path.display()
        );
        union.extend(resolved);
    }
    let k = reports.len();
    let denom = submitted.max(union.len()).max(1);
    eprintln!(
        "pass@{k}: {} / {} resolved by at least one seed  ({:.0}%)",
        union.len(),
        denom,
        union.len() as f64 / denom as f64 * 100.0
    );
    Ok(())
}

/// Load a `<out>.metrics.jsonl` sidecar (one [`InstanceMetric`] per line).
pub fn load_metrics(path: &Path) -> Result<Vec<InstanceMetric>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str::<InstanceMetric>(l)
                .with_context(|| format!("parsing metrics line {}", i + 1))
        })
        .collect()
}

/// Read the set of resolved `instance_id`s from one official `swebench` evaluation report
/// (the `*.json` from `run_evaluation`, which carries `resolved_ids`).
fn resolved_ids_from_report(path: &Path) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading report {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(v.get("resolved_ids")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default())
}

/// One agent's aggregated numbers for the comparison table.
struct AgentSummary {
    agent: String,
    instances: usize,
    patched: usize,
    resolved: usize,
    total_tokens: u64,
    total_cost: f64,
    total_wall: f64,
    complete: usize,
}

/// Aggregate one agent's per-instance metric rows against the official `resolved` set into an
/// [`AgentSummary`]. Pure (no I/O) so the headline comparison's arithmetic is unit-testable: an
/// instance counts as `resolved` iff the scorer put its id in `resolved`; token/cost totals only
/// include rows whose capture was `metrics_complete` (so a partial capture can't understate
/// tokens-per-success and flatter Forge). Assumes `rows` is non-empty (one file = one agent).
fn summarize_agent(
    rows: &[InstanceMetric],
    resolved: &std::collections::BTreeSet<String>,
) -> AgentSummary {
    let mut s = AgentSummary {
        agent: rows[0].agent.clone(),
        instances: rows.len(),
        patched: 0,
        resolved: 0,
        total_tokens: 0,
        total_cost: 0.0,
        total_wall: 0.0,
        complete: 0,
    };
    for r in rows {
        if r.patched {
            s.patched += 1;
        }
        if resolved.contains(&r.instance_id) {
            s.resolved += 1;
        }
        if r.metrics_complete {
            s.complete += 1;
            s.total_tokens += r.total_tokens;
            s.total_cost += r.cost_usd;
        }
        s.total_wall += r.wall_secs;
    }
    s
}

/// The tokens-per-success cell: total tokens (across all attempts) per resolved instance — the
/// efficiency number, lower is better. Only an honest number when there ARE eval results, at least
/// one instance resolved, AND every row's token capture was complete; otherwise it's `incomplete`
/// (some capture missing → would understate) or `n/a` (no evals / nothing resolved). Pure so the
/// honesty conditions are locked down by tests — this is the headline efficiency claim.
fn tok_per_success_cell(s: &AgentSummary, have_evals: bool) -> String {
    if have_evals && s.resolved > 0 && s.complete == s.instances {
        format!("{}", s.total_tokens / s.resolved as u64)
    } else if s.complete < s.instances {
        "incomplete".to_string()
    } else {
        "n/a".to_string()
    }
}

/// The headline comparison: join per-instance metrics with the official eval's `resolved_ids` and
/// print, per agent, **both** the resolve rate AND tokens-per-success (+ cost/wall). This is how
/// "Forge bridging model X beats running model X's own CLI" is shown — same instances, same scorer,
/// fewer tokens per solved task. `metrics` files come from `bench swe`; `evals` are the official
/// `run_evaluation` `*.json` reports (their resolved-id sets are unioned, then intersected with each
/// agent's instances, so one combined report or per-agent reports both work).
pub fn report(metrics: &[PathBuf], evals: &[PathBuf]) -> Result<()> {
    use std::collections::BTreeSet;
    if metrics.is_empty() {
        anyhow::bail!("report needs at least one --metrics <file.metrics.jsonl>");
    }
    let mut resolved: BTreeSet<String> = BTreeSet::new();
    for e in evals {
        resolved.extend(resolved_ids_from_report(e)?);
    }
    let have_evals = !evals.is_empty();

    let mut summaries = Vec::new();
    for m in metrics {
        let rows = load_metrics(m)?;
        if rows.is_empty() {
            continue;
        }
        summaries.push(summarize_agent(&rows, &resolved));
    }
    if summaries.is_empty() {
        anyhow::bail!("no metrics rows found in the given files");
    }

    println!(
        "{:<14} {:>5} {:>8} {:>9} {:>13} {:>11} {:>9}",
        "agent", "n", "patched", "resolved", "tok/success", "mean cost", "mean s"
    );
    for s in &summaries {
        let resolved_str = if have_evals {
            format!("{} ({:.0}%)", s.resolved, pct(s.resolved, s.instances))
        } else {
            "n/a".to_string()
        };
        let tok_per_success = tok_per_success_cell(s, have_evals);
        let mean_cost = if s.complete > 0 {
            format!("${:.4}", s.total_cost / s.complete as f64)
        } else {
            "n/a".to_string()
        };
        println!(
            "{:<14} {:>5} {:>8} {:>9} {:>13} {:>11} {:>9.1}",
            s.agent,
            s.instances,
            s.patched,
            resolved_str,
            tok_per_success,
            mean_cost,
            s.total_wall / s.instances as f64,
        );
    }
    if !have_evals {
        eprintln!(
            "\nnote: no --eval reports given → resolve rate + tok/success omitted. Score predictions\nwith the official evaluator, then re-run with --eval <report.json>."
        );
    }
    Ok(())
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 / d as f64 * 100.0
    }
}

/// Prepare one instance's repo, run a single headless turn in it, and return the diff plus the
/// resources it took. Sets the process CWD to the repo (the caller restores it).
async fn prepare_and_run(
    inst: &SweInstance,
    workdir: &Path,
    model: Option<String>,
    agent: Agent,
    timeout_secs: u64,
) -> Result<RunOutcome> {
    let dir = prepare_repo(inst, workdir)?;
    std::env::set_current_dir(&dir).context("entering instance repo")?;
    let started = std::time::Instant::now();
    let (input_tokens, output_tokens, cost_usd, metrics_complete) = match agent {
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
            // Reliable even for bridge providers — read from Forge's own usage DB for THIS session.
            let (inp, out, cost) = session.session_usage_db();
            (inp, out, cost, true)
        }
        Agent::ClaudeCode | Agent::Codex => {
            let usage = run_external_agent(
                agent,
                &inst.problem_statement,
                &dir,
                model.as_deref(),
                timeout_secs,
            )
            .await?;
            (usage.0, usage.1, usage.2, usage.3)
        }
    };
    let wall_secs = started.elapsed().as_secs_f64();
    let patch = extract_patch(&dir)?;
    Ok(RunOutcome {
        patch,
        input_tokens,
        output_tokens,
        cost_usd,
        wall_secs,
        metrics_complete,
    })
}

/// Run an external agent CLI (Claude Code / Codex) as its OWN autonomous agent in `dir`, feeding the
/// task on stdin. Both must run fully unattended (edit files + run commands without prompts) so they
/// can actually solve the instance — the clone is disposable, so the broad autonomy is contained.
/// Returns `(input_tokens, output_tokens, cost_usd, metrics_complete)` parsed from the CLI's machine
/// output where available (claude `--output-format json`); `metrics_complete = false` when the CLI
/// gave no parseable usage, so the report won't make token claims it can't back up.
async fn run_external_agent(
    agent: Agent,
    problem: &str,
    dir: &Path,
    model: Option<&str>,
    timeout_secs: u64,
) -> Result<(u64, u64, f64, bool)> {
    use tokio::io::AsyncWriteExt;

    let (bin, mut args): (&str, Vec<String>) = match agent {
        // `-p` reads the prompt from stdin; skip-permissions so edits + shell run unattended.
        // `--output-format json` makes claude emit a final result object with `usage` + cost.
        Agent::ClaudeCode => (
            "claude",
            vec![
                "-p".into(),
                "--output-format".into(),
                "json".into(),
                "--dangerously-skip-permissions".into(),
            ],
        ),
        // `exec` is codex's non-interactive mode; `--full-auto` = workspace-write + never-ask.
        // `--json` emits a JSONL event stream we scan for a token-count event (best-effort).
        Agent::Codex => (
            "codex",
            vec![
                "exec".into(),
                "--json".into(),
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{bin}` — is it installed and on PATH?"))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(problem.as_bytes()).await.ok();
        stdin.shutdown().await.ok();
    }

    // Drain stdout concurrently on a separate task so the child can't block on a full pipe, while we
    // wait on the process with a borrow (so we can still `start_kill` it on timeout — unlike
    // `wait_with_output`, which consumes the child).
    let stdout_pipe = child.stdout.take();
    let reader = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        if let Some(mut so) = stdout_pipe {
            let _ = so.read_to_end(&mut buf).await;
        }
        buf
    });

    let waited =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), child.wait()).await;
    match waited {
        Ok(Ok(st)) => {
            if !st.success() {
                // A non-zero exit is common (the agent may "fail" yet still have edited files) —
                // don't abort the instance; the diff (possibly empty) is captured by the caller.
                eprintln!("  (note: {bin} exited {st})");
            }
            let buf = reader.await.unwrap_or_default();
            let stdout = String::from_utf8_lossy(&buf);
            Ok(parse_external_usage(agent, &stdout))
        }
        Ok(Err(e)) => {
            reader.abort();
            Err(anyhow::anyhow!("waiting on {bin}: {e}"))
        }
        Err(_) => {
            let _ = child.start_kill();
            reader.abort();
            anyhow::bail!("{bin} timed out after {timeout_secs}s")
        }
    }
}

/// Best-effort token/cost extraction from an external agent's machine output.
/// - claude (`--output-format json`): a single JSON object with `usage.{input_tokens,output_tokens,
///   cache_read_input_tokens}` and `total_cost_usd`.
/// - codex (`--json`): a JSONL event stream; we take the LAST object carrying token fields
///   (`input_tokens`/`output_tokens`, possibly nested under `usage`/`token_usage`/`info`).
///
/// Returns `(input, output, cost, complete)`; `complete = false` when nothing parsed.
fn parse_external_usage(agent: Agent, stdout: &str) -> (u64, u64, f64, bool) {
    fn u(v: &serde_json::Value, keys: &[&str]) -> Option<u64> {
        keys.iter().find_map(|k| v.get(k).and_then(|x| x.as_u64()))
    }
    // Pull token/cost out of a single JSON object that may nest usage under a few known keys.
    fn from_obj(v: &serde_json::Value) -> Option<(u64, u64, f64)> {
        let usage = ["usage", "token_usage", "info", "tokens"]
            .iter()
            .find_map(|k| v.get(k))
            .unwrap_or(v);
        let inp = u(usage, &["input_tokens", "prompt_tokens", "input"]);
        let out = u(usage, &["output_tokens", "completion_tokens", "output"]);
        let (inp, out) = (inp?, out?);
        let cache = u(usage, &["cache_read_input_tokens", "cached_input_tokens"]).unwrap_or(0);
        let cost = ["total_cost_usd", "cost_usd", "cost"]
            .iter()
            .find_map(|k| v.get(k).and_then(|x| x.as_f64()))
            .unwrap_or(0.0);
        Some((inp + cache, out, cost))
    }

    match agent {
        Agent::ClaudeCode => serde_json::from_str::<serde_json::Value>(stdout.trim())
            .ok()
            .and_then(|v| from_obj(&v))
            .map(|(i, o, c)| (i, o, c, true))
            .unwrap_or((0, 0, 0.0, false)),
        Agent::Codex => {
            // Last JSONL line that yields token numbers wins (codex prints a running/final tally).
            let last = stdout
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
                .filter_map(|v| from_obj(&v))
                .next_back();
            match last {
                Some((i, o, c)) => (i, o, c, true),
                None => (0, 0, 0.0, false),
            }
        }
        Agent::Forge => (0, 0, 0.0, false),
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

    #[test]
    fn metrics_roundtrip_jsonl() {
        let m = vec![
            InstanceMetric {
                instance_id: "a-1".into(),
                agent: "forge".into(),
                input_tokens: 1000,
                output_tokens: 200,
                total_tokens: 1200,
                cost_usd: 0.012,
                wall_secs: 4.5,
                patched: true,
                metrics_complete: true,
            },
            InstanceMetric {
                instance_id: "a-2".into(),
                agent: "forge".into(),
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cost_usd: 0.0,
                wall_secs: 0.0,
                patched: false,
                metrics_complete: false,
            },
        ];
        let jsonl = metrics_to_jsonl(&m);
        assert_eq!(jsonl.lines().count(), 2);
        assert!(jsonl.ends_with('\n'));
        let dir = std::env::temp_dir().join(format!("forge-bench-m-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("preds.metrics.jsonl");
        std::fs::write(&p, &jsonl).unwrap();
        assert_eq!(load_metrics(&p).unwrap(), m);
    }

    fn mk(id: &str, patched: bool, complete: bool, tokens: u64) -> InstanceMetric {
        InstanceMetric {
            instance_id: id.into(),
            agent: "forge".into(),
            input_tokens: tokens,
            output_tokens: 0,
            total_tokens: tokens,
            cost_usd: 0.01,
            wall_secs: 2.0,
            patched,
            metrics_complete: complete,
        }
    }

    #[test]
    fn summarize_agent_counts_resolved_patched_and_complete() {
        use std::collections::BTreeSet;
        let rows = vec![
            mk("a-1", true, true, 100),  // patched, complete, RESOLVED
            mk("a-2", true, true, 200),  // patched, complete, not resolved
            mk("a-3", false, false, 50), // not patched, INCOMPLETE, resolved
        ];
        let resolved: BTreeSet<String> = ["a-1", "a-3"].iter().map(|s| s.to_string()).collect();
        let s = summarize_agent(&rows, &resolved);
        assert_eq!(s.agent, "forge");
        assert_eq!(s.instances, 3);
        assert_eq!(s.patched, 2);
        assert_eq!(s.resolved, 2, "a-1 + a-3 are in the resolved set");
        assert_eq!(s.complete, 2, "a-3's capture was incomplete");
        // Only complete rows contribute tokens — a-3's 50 is excluded so tok/success can't be understated.
        assert_eq!(s.total_tokens, 300);
        assert_eq!(s.total_wall, 6.0);
    }

    #[test]
    fn tok_per_success_is_honest_only_with_complete_capture() {
        // All complete, evals present, 2 resolved, 300 tokens → 150 per success.
        let full = AgentSummary {
            agent: "forge".into(),
            instances: 2,
            patched: 2,
            resolved: 2,
            total_tokens: 300,
            total_cost: 0.02,
            total_wall: 4.0,
            complete: 2,
        };
        assert_eq!(tok_per_success_cell(&full, true), "150");
        // No eval reports → can't claim a success rate → n/a (even with complete capture).
        assert_eq!(tok_per_success_cell(&full, false), "n/a");
        // Resolved zero → dividing would be meaningless → n/a.
        let none_resolved = AgentSummary {
            resolved: 0,
            ..AgentSummary {
                agent: "forge".into(),
                instances: 2,
                patched: 0,
                resolved: 0,
                total_tokens: 300,
                total_cost: 0.0,
                total_wall: 0.0,
                complete: 2,
            }
        };
        assert_eq!(tok_per_success_cell(&none_resolved, true), "n/a");
        // Partial token capture (complete < instances) → refuse to print a flattering number.
        let partial = AgentSummary {
            instances: 3,
            complete: 2,
            resolved: 2,
            total_tokens: 300,
            ..full
        };
        assert_eq!(tok_per_success_cell(&partial, true), "incomplete");
    }

    #[test]
    fn metrics_path_inserts_before_extension() {
        assert_eq!(
            metrics_path(Path::new("predictions.jsonl")),
            PathBuf::from("predictions.metrics.jsonl")
        );
        assert_eq!(
            metrics_path(Path::new("/tmp/run/preds.seed1.jsonl")),
            PathBuf::from("/tmp/run/preds.seed1.metrics.jsonl")
        );
    }

    #[test]
    fn parse_claude_json_usage() {
        let out = r#"{"type":"result","is_error":false,"result":"done","total_cost_usd":0.0345,"usage":{"input_tokens":1200,"output_tokens":340,"cache_read_input_tokens":800}}"#;
        let (i, o, c, ok) = parse_external_usage(Agent::ClaudeCode, out);
        assert!(ok);
        assert_eq!(i, 2000, "input + cache_read folded in"); // 1200 + 800
        assert_eq!(o, 340);
        assert!((c - 0.0345).abs() < 1e-9);
    }

    #[test]
    fn parse_codex_jsonl_takes_last_token_event() {
        let out = "{\"type\":\"start\"}\n{\"token_usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n{\"token_usage\":{\"input_tokens\":900,\"output_tokens\":120}}\n";
        let (i, o, _c, ok) = parse_external_usage(Agent::Codex, out);
        assert!(ok);
        assert_eq!((i, o), (900, 120), "last tally wins");
    }

    #[test]
    fn parse_external_usage_incomplete_on_garbage() {
        let (_, _, _, ok) = parse_external_usage(Agent::ClaudeCode, "not json at all");
        assert!(
            !ok,
            "unparseable output → metrics_complete=false, not a lie"
        );
    }

    #[test]
    fn extract_patch_includes_new_untracked_files() {
        // The bug a live smoke caught: `git diff` ignores untracked files, so a solution that ADDS a
        // file (common in SWE-bench: new modules / tests) produced an EMPTY patch and was scored
        // unresolved even though the agent did the work. The patch must carry additions too.
        let dir = std::env::temp_dir().join(format!("forge-bench-patch-{}", forge_types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let g = |args: &[&str]| run_git(&dir, args).expect("git");
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(dir.join("tracked.txt"), "original\n").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-q", "-m", "base"]);
        // The "solution": add a brand-new file AND modify the tracked one.
        std::fs::write(dir.join("NEW.txt"), "added by the agent\n").unwrap();
        std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();

        let patch = extract_patch(&dir).expect("extract_patch");
        assert!(
            patch.contains("NEW.txt") && patch.contains("added by the agent"),
            "the NEW (untracked) file must be in the patch — this was the bug:\n{patch}"
        );
        assert!(
            patch.contains("tracked.txt"),
            "the modified file is also present"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
