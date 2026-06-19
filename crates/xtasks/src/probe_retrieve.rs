//! Diagnostic: for each bench task prompt, print exactly what Lattice retrieval would inject under
//! the Improved (body) condition — which symbols, body vs signature, per-snippet token cost, and
//! the total. Used to root-cause why a given task's injection helps or hurts.
//!
//! Run: `cargo run -p xtasks -- probe-retrieve`

use std::sync::Arc;

use forge_index::{BodyOpts, Lattice};
use forge_store::Store;

const TASKS: &[(&str, &str)] = &[
    ("T1-usage-fields", "List every field of the `Usage` struct in the forge-types crate. Answer concisely, then stop."),
    ("T2-inject-budget", "In forge-core, what value does the `inject_budget` function return when the BudgetStatus is the most constrained variant, given a base of 1500? Answer with the number and stop."),
    ("T3-record-usage", "Which method on the Store type in forge-store records per-message token usage, and which SQL table does it INSERT into? Answer in one line and stop."),
    ("T4-retrieve-identifiers", "In forge-index, the retrieval code extracts candidate identifiers from a prompt. What minimum length must a token be to qualify, and name one stopword it drops? Answer in one line and stop."),
    ("T5-permission-modes", "Name the variants of the PermissionMode enum in forge-types. Answer with just the variant names and stop."),
];

pub fn run() -> anyhow::Result<()> {
    let repo_root = std::env::current_dir()?;
    let store = Arc::new(Store::open_in_memory()?);
    let lattice = Lattice::new(store, &repo_root);
    let stats = lattice.update()?;
    eprintln!("[probe] indexed: {stats:?}");

    let bodies = Some(BodyOpts {
        max_tokens: 800,
        max_hits: 3,
    });
    for (id, prompt) in TASKS {
        let ctx = lattice.retrieve(prompt, 3000, bodies)?;
        println!("\n=== {id} ===  est_tokens={} snippets={}", ctx.est_tokens, ctx.snippets.len());
        for s in &ctx.snippets {
            let kind = if s.is_body { "BODY" } else { "sig " };
            let cost = (s.text.len() / 4).max(1);
            let head = s.text.lines().next().unwrap_or("");
            println!("  [{kind}] ~{cost:>4} tok  {}:{}  {}", s.rel_path, s.line, head);
        }
    }
    Ok(())
}
