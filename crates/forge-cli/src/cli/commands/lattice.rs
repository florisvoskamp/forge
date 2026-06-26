use anyhow::{Context, Result};

use crate::*;

/// `forge lattice <op>` — build / query / inspect the code-intelligence graph.
pub(crate) async fn lattice_cmd(op: LatticeOp) -> Result<()> {
    let config = forge_config::load().context("loading configuration")?;
    if !config.lattice.enabled {
        println!("lattice is disabled (set [lattice] enabled = true)");
        return Ok(());
    }
    let store = std::sync::Arc::new(open_store()?);
    let cwd = std::env::current_dir()?;
    match op {
        LatticeOp::Embed => {
            let emb = &config.lattice.embeddings;
            if !emb.enabled {
                println!("embeddings are off (set [lattice.embeddings] enabled = true)");
                return Ok(());
            }
            let lat = forge_index::Lattice::new(store, &cwd);
            lat.update().map_err(|e| anyhow::anyhow!("{e}"))?;
            let Some((embedder, label)) = forge_provider::select_embedder(emb) else {
                println!(
                    "no embedding backend available — set [lattice.embeddings] backend + a provider key, or run ollama"
                );
                return Ok(());
            };
            let n = lat
                .embed_pending(embedder.as_ref(), 64)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "⌬ embedded {n} node(s) via {label}; {} total",
                lat.embedding_count().map_err(|e| anyhow::anyhow!("{e}"))?
            );
        }
        LatticeOp::Update { path } => {
            let root = path.map(std::path::PathBuf::from).unwrap_or(cwd);
            let lat = forge_index::Lattice::new(store, &root);
            let stats = lat.update().map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "⌬ lattice updated — {} file(s) indexed, {} skipped, {} symbol(s)",
                stats.files_indexed, stats.files_skipped, stats.symbols
            );
        }
        LatticeOp::Query { query } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            let hits = lat.query(&query, 20).map_err(|e| anyhow::anyhow!("{e}"))?;
            if hits.is_empty() {
                println!("no symbols match '{query}' — run `forge lattice update` first?");
            } else {
                for h in hits {
                    let sig = h.signature.unwrap_or_else(|| h.name.clone());
                    println!("{:<8} {}:{}  {}", h.kind, h.rel_path, h.line, sig);
                }
            }
        }
        LatticeOp::Impact { symbol, scope } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            let blast = lat
                .impact_in_scope(&symbol, 4, scope.as_deref())
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let scope_note = scope
                .as_deref()
                .map(|s| format!(" (scoped to {s})"))
                .unwrap_or_default();
            if blast.roots.is_empty() {
                if scope.is_some() {
                    println!("no symbol named '{symbol}'{scope_note} — wrong --scope, or run `forge lattice update`?");
                } else {
                    println!("no symbol named '{symbol}' — run `forge lattice update` first?");
                }
            } else if blast.dependents.is_empty() {
                println!("⌬ {symbol}{scope_note}: no known references (leaf, or callers not yet indexed)");
            } else {
                println!(
                    "⌬ impact · {symbol}{scope_note} — {} site(s) across {} file(s)",
                    blast.total_sites,
                    blast.files.len()
                );
                for d in &blast.dependents {
                    println!("  ← {:<8} {} {}:{}", d.kind, d.name, d.rel_path, d.line);
                }
                if scope.is_some() {
                    println!(
                        "  ⓘ name-based, confined to the scope. References to a same-named item \
                         OUTSIDE the scope are excluded; within it, confirm a hit is the right \
                         definition before treating it as a real blocker."
                    );
                } else {
                    println!(
                        "  ⓘ name-based: matches ANY symbol named '{symbol}' ({} definition(s) carry \
                         this name). References to a same-named item in an unrelated module/crate are \
                         included — narrow with `--scope <path>`, and confirm a hit is the right \
                         definition (grep/read it) before treating a cross-module reference as a real \
                         blocker.",
                        blast.roots.len()
                    );
                }
            }
        }
        LatticeOp::Path { from, to } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            match lat
                .path(&from, &to, 8)
                .map_err(|e| anyhow::anyhow!("{e}"))?
            {
                Some(chain) => println!("⌬ path · {}", chain.join(" → ")),
                None => println!("no reference path from '{from}' to '{to}' within 8 hops"),
            }
        }
        LatticeOp::Why { symbol } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            match lat.why(&symbol).map_err(|e| anyhow::anyhow!("{e}"))? {
                Some(p) => println!(
                    "⌬ why · {} ({}:{})\n  {} · {} · {} · {}",
                    p.name, p.rel_path, p.line, p.author, p.date, p.commit, p.subject
                ),
                None => println!(
                    "no provenance for '{symbol}' — unknown symbol, or the tree isn't under git"
                ),
            }
        }
        LatticeOp::Status => {
            let lat = forge_index::Lattice::new(store, &cwd);
            let s = lat.status().map_err(|e| anyhow::anyhow!("{e}"))?;
            let embedded = lat.embedding_count().map_err(|e| anyhow::anyhow!("{e}"))?;
            let emb = if config.lattice.embeddings.enabled {
                format!("{embedded} embedded")
            } else {
                "embeddings off".to_string()
            };
            println!(
                "⌬ lattice — {} file(s), {} symbol(s), {} edge(s), {} ref(s) · {} languages · {emb}",
                s.files,
                s.nodes,
                s.edges,
                s.refs,
                forge_index::supported_languages().len()
            );
        }
        LatticeOp::Map { budget } => {
            let lat = forge_index::Lattice::new(store, &cwd);
            let budget = budget.unwrap_or(2000);
            let map = lat.map(budget).map_err(|e| anyhow::anyhow!("{e}"))?;
            print!("{map}");
        }
    }
    Ok(())
}
