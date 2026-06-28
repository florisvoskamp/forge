use anyhow::Result;

use crate::cli::args::MemoryCmd;
use crate::*;

/// Scope key for the current project (its absolute path) or the global store.
fn scope(global: bool) -> String {
    if global {
        return "global".to_string();
    }
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "global".to_string())
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// `forge memory [list|add|search|rm|clear]` — inspect + curate the durable facts Forge remembers.
pub(crate) fn memory_cmd(cmd: Option<MemoryCmd>, global: bool) -> Result<()> {
    let store = open_store()?;
    let scope = scope(global);
    let where_ = if global { "global" } else { "project" };
    match cmd {
        None => {
            let mems = store.list_memories(&scope)?;
            if mems.is_empty() {
                println!("no {where_} memories yet");
                return Ok(());
            }
            println!("{} {where_} memories:", mems.len());
            for m in mems {
                println!(
                    "  {}  [{}] {}  ({:.2})",
                    short(&m.id),
                    m.kind,
                    m.text,
                    m.salience
                );
            }
        }
        Some(MemoryCmd::Add { text, kind }) => {
            let text = text.join(" ");
            let id = store.add_memory(&scope, &kind, &text, "manual")?;
            println!("remembered ({}) [{}]: {}", short(&id), kind, text);
        }
        Some(MemoryCmd::Search { query }) => {
            let hits = store.search_memories(&scope, &query.join(" "), 20)?;
            if hits.is_empty() {
                println!("no matches");
                return Ok(());
            }
            for m in hits {
                println!("  {}  [{}] {}", short(&m.id), m.kind, m.text);
            }
        }
        Some(MemoryCmd::Rm { id }) => {
            // Accept the short id prefix printed by `forge memory`.
            let full = store
                .list_memories(&scope)?
                .into_iter()
                .find(|m| m.id.starts_with(&id))
                .map(|m| m.id);
            match full {
                Some(full) if store.delete_memory(&full)? => println!("removed {}", short(&full)),
                _ => println!("no memory matching '{id}' in the {where_} scope"),
            }
        }
        Some(MemoryCmd::Clear) => {
            let n = store.clear_memories(&scope)?;
            println!("cleared {n} {where_} memories");
        }
    }
    Ok(())
}
