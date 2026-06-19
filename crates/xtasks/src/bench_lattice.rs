//! Token-reduction benchmark for Lattice context injection.
//!
//! Measures the TOTAL agent-loop token consumption (summed input+output across every provider
//! call) needed to answer a fixed set of repo-specific questions under three conditions:
//!   - Off:      no Lattice injection (the model must grep/read files to answer)
//!   - Current:  signature-only injection (today's default)
//!   - Improved: signature + body injection (the candidate)
//!
//! The questions are answerable from this repo's own source, so without injection the model
//! spends tool calls (read_file/grep) that dump large outputs into the transcript; good injection
//! replaces that exploration with a few precise snippets. Lower total input tokens = better.
//!
//! Run: `cargo run -p xtasks -- bench-lattice`
//! Env: FORGE_BENCH_MODEL (default openrouter::google/gemini-2.5-flash)
//!      FORGE_BENCH_REPS  (default 2)
//!      FORGE_BENCH_CONDS (default "off,current,improved")

use std::sync::Arc;

use forge_config::Config;
use forge_core::Session;
use forge_index::Lattice;
use forge_mesh::HeuristicRouter;
use forge_provider::{DispatchProvider, Provider};
use forge_store::Store;
use forge_tui::HeadlessPresenter;

const DEFAULT_MODEL: &str = "openrouter::google/gemini-2.5-flash";
const DEFAULT_REPS: usize = 3;
const MAX_STEPS: usize = 6;

use crate::tasks::TASKS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Condition {
    Off,
    Current,
    Improved,
}

impl Condition {
    fn label(self) -> &'static str {
        match self {
            Condition::Off => "Off",
            Condition::Current => "Current",
            Condition::Improved => "Improved",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "off" => Some(Condition::Off),
            "current" => Some(Condition::Current),
            "improved" => Some(Condition::Improved),
            _ => None,
        }
    }
    fn apply(self, cfg: &mut Config) {
        match self {
            Condition::Off => {
                cfg.lattice.inject = false;
            }
            Condition::Current => {
                cfg.lattice.inject = true;
                cfg.lattice.inject_bodies = false;
            }
            Condition::Improved => {
                cfg.lattice.inject = true;
                cfg.lattice.inject_bodies = true;
            }
        }
    }
}

#[derive(Debug)]
struct Row {
    cond: Condition,
    task: &'static str,
    input: u64,
    output: u64,
    steps: u64,
}

#[allow(clippy::field_reassign_with_default)]
fn bench_config(model: &str, cond: Condition) -> Config {
    let mut cfg = Config::default();
    // Read-only posture: the tasks are questions, so the model only needs read tools; this avoids
    // any write/shell permission prompt blocking a headless run.
    cfg.permission_mode = forge_types::PermissionMode::Plan;
    // Hold routing constant: pin the model via the router, no failover, no budget cutoff.
    cfg.mesh.failover = false;
    cfg.mesh.max_steps = MAX_STEPS;
    cfg.mesh.daily_budget_usd = None;
    cfg.mesh.monthly_cap_usd = None;
    // Lattice: structural only (deterministic — no embedder API variance), no background watcher.
    cfg.lattice.enabled = true;
    cfg.lattice.watch = false;
    cfg.lattice.embeddings.enabled = false;
    cond.apply(&mut cfg);
    let _ = model;
    cfg
}

pub async fn run() -> anyhow::Result<()> {
    let model = std::env::var("FORGE_BENCH_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let reps: usize = std::env::var("FORGE_BENCH_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_REPS);
    let conds: Vec<Condition> = std::env::var("FORGE_BENCH_CONDS")
        .unwrap_or_else(|_| "off,current,improved".to_string())
        .split(',')
        .filter_map(Condition::parse)
        .collect();

    // genai reads provider keys from the environment; the CLI does this at startup. Mirror it so
    // the harness can reach OpenRouter/etc. via the keyring-stored key.
    forge_config::inject_provider_keys();

    let repo_root = std::env::current_dir()?;
    eprintln!("[bench] model={model} reps={reps} conds={conds:?}");
    eprintln!("[bench] indexing {} ...", repo_root.display());

    // Build the Lattice index once and share it across every run (the index is read-only here).
    let lat_store = Arc::new(Store::open_in_memory()?);
    let lattice = Arc::new(Lattice::new(lat_store, &repo_root));
    let stats = lattice.update()?;
    eprintln!("[bench] indexed: {stats:?}");

    let provider: Arc<dyn Provider> = Arc::new(DispatchProvider::new(false));

    let mut rows: Vec<Row> = Vec::new();
    for &cond in &conds {
        for task in TASKS {
            for rep in 0..reps {
                let session_store = Arc::new(Store::open_in_memory()?);
                let cfg = bench_config(&model, cond);
                let router =
                    Arc::new(HeuristicRouter::new(cfg.clone()).with_pin(Some(model.clone())));
                let mut session = Session::start(
                    session_store.clone(),
                    provider.clone(),
                    router,
                    forge_tools::ToolRegistry::with_core_tools(),
                    Box::new(HeadlessPresenter::default()),
                    cfg,
                    repo_root.to_str().unwrap(),
                )?;
                if cond != Condition::Off {
                    session.set_lattice(Some(lattice.clone()));
                }

                let sid = session.session_id().to_string();
                if let Err(e) = session.run_turn(task.prompt).await {
                    eprintln!("[bench] {cond:?}/{}/{rep} turn error: {e}", task.id);
                }
                let (input, output) = session_store.session_tokens(&sid)?;
                let steps = session_store.session_step_count(&sid)?;
                eprintln!(
                    "[bench] {:<9} {:<22} rep{rep}  in={input:<7} out={output:<6} steps={steps}",
                    cond.label(),
                    task.id
                );
                rows.push(Row {
                    cond,
                    task: task.id,
                    input,
                    output,
                    steps,
                });
            }
        }
    }

    print_report(&rows, &conds);
    Ok(())
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

/// Median is the honest per-task aggregate: a single live-model run can blow up into a 4–6-step
/// exploration (a 5–10× token outlier), which drags an arithmetic mean toward whichever condition
/// got unlucky. The median across reps reflects the typical run.
fn median(v: &[u64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s: Vec<u64> = v.to_vec();
    s.sort_unstable();
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2] as f64
    } else {
        (s[n / 2 - 1] + s[n / 2]) as f64 / 2.0
    }
}

/// Median total (input+output) tokens for one task under one condition, across reps.
fn task_median(rows: &[Row], task: &str, cond: Condition) -> f64 {
    median(
        &rows
            .iter()
            .filter(|r| r.task == task && r.cond == cond)
            .map(|r| r.input + r.output)
            .collect::<Vec<_>>(),
    )
}

fn print_report(rows: &[Row], conds: &[Condition]) {
    println!("\n## Per-task median (input+output) tokens\n");
    println!(
        "| task | {} |",
        conds
            .iter()
            .map(|c| c.label())
            .collect::<Vec<_>>()
            .join(" | ")
    );
    println!("|------|{}|", "-------|".repeat(conds.len()));
    for task in TASKS {
        let cells: Vec<String> = conds
            .iter()
            .map(|&c| format!("{:.0}", task_median(rows, task.id, c)))
            .collect();
        println!("| {} | {} |", task.id, cells.join(" | "));
    }

    println!("\n## Median steps per condition\n");
    for &c in conds {
        let steps = median(
            &rows
                .iter()
                .filter(|r| r.cond == c)
                .map(|r| r.steps)
                .collect::<Vec<_>>(),
        );
        println!("- {:<9}: {steps:.1} steps", c.label());
    }

    // Equal-weight aggregate: per-task % reduction, then averaged across tasks. Weighting each task
    // equally (rather than summing tokens) stops one giant-token task from dominating the headline —
    // the flaw that produced the earlier, unreliable "-60%" figure.
    let report_delta = |label: &str, base: Condition, cand: Condition| {
        if !conds.contains(&base) || !conds.contains(&cand) {
            return;
        }
        let deltas: Vec<f64> = TASKS
            .iter()
            .filter_map(|t| {
                let b = task_median(rows, t.id, base);
                let c = task_median(rows, t.id, cand);
                (b > 0.0).then_some((c - b) / b * 100.0)
            })
            .collect();
        let mut sorted = deltas.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = if sorted.is_empty() {
            0.0
        } else if sorted.len() % 2 == 1 {
            sorted[sorted.len() / 2]
        } else {
            (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
        };
        println!(
            "- {label}: mean {:+.1}% · median {:+.1}%  (per-task, equal weight)",
            mean(&deltas),
            med
        );
    };

    println!("\n## Reduction (equal-weight per-task)\n");
    report_delta("Current vs Off    ", Condition::Off, Condition::Current);
    report_delta("Improved vs Off   ", Condition::Off, Condition::Improved);
    report_delta(
        "Improved vs Current",
        Condition::Current,
        Condition::Improved,
    );
    println!("\n(target: Improved vs Current ≤ -30%)");
}
