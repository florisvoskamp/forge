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
const DEFAULT_REPS: usize = 2;
const MAX_STEPS: usize = 6;

struct BenchTask {
    id: &'static str,
    prompt: &'static str,
}

/// Narrow, repo-specific questions whose answers live in one or two source files — exactly the
/// case where injecting the relevant symbol should save the model a file read.
const TASKS: &[BenchTask] = &[
    BenchTask {
        id: "T1-usage-fields",
        prompt: "List every field of the `Usage` struct in the forge-types crate. Answer concisely, then stop.",
    },
    BenchTask {
        id: "T2-inject-budget",
        prompt: "In forge-core, what value does the `inject_budget` function return when the BudgetStatus is the most constrained variant, given a base of 1500? Answer with the number and stop.",
    },
    BenchTask {
        id: "T3-record-usage",
        prompt: "Which method on the Store type in forge-store records per-message token usage, and which SQL table does it INSERT into? Answer in one line and stop.",
    },
    BenchTask {
        id: "T4-retrieve-identifiers",
        prompt: "In forge-index, the retrieval code extracts candidate identifiers from a prompt. What minimum length must a token be to qualify, and name one stopword it drops? Answer in one line and stop.",
    },
    BenchTask {
        id: "T5-permission-modes",
        prompt: "Name the variants of the PermissionMode enum in forge-types. Answer with just the variant names and stop.",
    },
];

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

fn mean(v: &[u64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().sum::<u64>() as f64 / v.len() as f64
}

fn cond_total(rows: &[Row], cond: Condition) -> f64 {
    // Total = input + output, the full token cost of completing the suite.
    mean(
        &rows
            .iter()
            .filter(|r| r.cond == cond)
            .map(|r| r.input + r.output)
            .collect::<Vec<_>>(),
    )
}

fn print_report(rows: &[Row], conds: &[Condition]) {
    println!("\n## Per-task mean (input+output) tokens\n");
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
            .map(|&c| {
                let m = mean(
                    &rows
                        .iter()
                        .filter(|r| r.task == task.id && r.cond == c)
                        .map(|r| r.input + r.output)
                        .collect::<Vec<_>>(),
                );
                format!("{m:.0}")
            })
            .collect();
        println!("| {} | {} |", task.id, cells.join(" | "));
    }

    println!("\n## Overall mean total tokens per task (and mean steps)\n");
    for &c in conds {
        let steps = mean(
            &rows
                .iter()
                .filter(|r| r.cond == c)
                .map(|r| r.steps)
                .collect::<Vec<_>>(),
        );
        println!(
            "- {:<9}: {:.0} tok  ({steps:.1} steps)",
            c.label(),
            cond_total(rows, c)
        );
    }

    // Reductions vs Off and vs Current.
    let off = conds
        .contains(&Condition::Off)
        .then(|| cond_total(rows, Condition::Off));
    let current = conds
        .contains(&Condition::Current)
        .then(|| cond_total(rows, Condition::Current));
    let improved = conds
        .contains(&Condition::Improved)
        .then(|| cond_total(rows, Condition::Improved));

    println!("\n## Reduction\n");
    if let (Some(off), Some(cur)) = (off, current) {
        if off > 0.0 {
            println!("- Current vs Off:  {:+.1}%", (cur - off) / off * 100.0);
        }
    }
    if let (Some(off), Some(imp)) = (off, improved) {
        if off > 0.0 {
            println!(
                "- Improved vs Off: {:+.1}%  (target ≤ -30%)",
                (imp - off) / off * 100.0
            );
        }
    }
    if let (Some(cur), Some(imp)) = (current, improved) {
        if cur > 0.0 {
            println!("- Improved vs Current: {:+.1}%", (imp - cur) / cur * 100.0);
        }
    }
}
