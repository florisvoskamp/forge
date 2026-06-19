//! Diagnostic: for each bench task prompt, print exactly what Lattice retrieval would inject under
//! the Improved (body) condition — which symbols, body vs signature, per-snippet token cost, and
//! the total. Used to root-cause why a given task's injection helps or hurts.
//!
//! Run: `cargo run -p xtasks -- probe-retrieve`

use std::sync::Arc;

use forge_index::{BodyOpts, Lattice};
use forge_store::Store;

use crate::tasks::TASKS;

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
    for t in TASKS {
        let ctx = lattice.retrieve(t.prompt, 3000, bodies)?;
        println!("\n=== {} ===  est_tokens={} snippets={}", t.id, ctx.est_tokens, ctx.snippets.len());
        for s in &ctx.snippets {
            let kind = if s.is_body { "BODY" } else { "sig " };
            let cost = (s.text.len() / 4).max(1);
            let head = s.text.lines().next().unwrap_or("");
            println!("  [{kind}] ~{cost:>4} tok  {}:{}  {}", s.rel_path, s.line, head);
        }
    }
    Ok(())
}
