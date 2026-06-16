//! The `lattice` tool — lets the model query Forge's code-intelligence index directly
//! (code-intelligence.md). `ReadOnly`, so it never prompts. Three operations: `query` (find
//! symbols by name), `impact` (reverse-dependents — what references X), and `path` (a call/
//! reference chain between two symbols). Backed by the same `Arc<Lattice>` the turn auto-injects
//! from, so what the model can ask matches what gets injected.

use std::sync::Arc;

use async_trait::async_trait;
use forge_index::Lattice;
use forge_types::SideEffect;
use serde_json::{json, Value};

use crate::{str_arg, Tool, ToolError};

pub struct LatticeTool {
    lattice: Arc<Lattice>,
}

impl LatticeTool {
    pub fn new(lattice: Arc<Lattice>) -> Self {
        Self { lattice }
    }
}

#[async_trait]
impl Tool for LatticeTool {
    fn name(&self) -> &str {
        "lattice"
    }

    fn description(&self) -> &str {
        "Query the code-intelligence index of this repository. ops: \
         \"query\" (find definitions by name — args: name), \
         \"impact\" (what references a symbol, transitively — args: name), \
         \"path\" (a reference chain between two symbols — args: from, to), \
         \"why\" (who last changed a symbol's definition + the commit — args: name). \
         Use it instead of grepping when you need structure: callers, blast radius, how two \
         symbols connect, or decision provenance. Returns file:line locations. Read-only."
    }

    fn side_effect(&self) -> SideEffect {
        SideEffect::ReadOnly
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "op": { "type": "string", "enum": ["query", "impact", "path", "why"] },
                "name": { "type": "string", "description": "symbol name for query/impact/why" },
                "from": { "type": "string", "description": "source symbol for path" },
                "to": { "type": "string", "description": "target symbol for path" }
            },
            "required": ["op"]
        })
    }

    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let op = str_arg(args, "op")?;
        let map = |e: forge_index::LatticeError| ToolError::Failed(e.to_string());
        match op {
            "query" => {
                let name = str_arg(args, "name")?;
                let hits = self.lattice.query(name, 20).map_err(map)?;
                if hits.is_empty() {
                    return Ok(format!("no symbols match '{name}'"));
                }
                let mut out = String::new();
                for h in hits {
                    let sig = h.signature.unwrap_or_else(|| h.name.clone());
                    out.push_str(&format!(
                        "{:<10} {}:{}  {sig}\n",
                        h.kind, h.rel_path, h.line
                    ));
                }
                Ok(out)
            }
            "impact" => {
                let name = str_arg(args, "name")?;
                let blast = self.lattice.impact(name, 4).map_err(map)?;
                if blast.roots.is_empty() {
                    return Ok(format!("no symbol named '{name}'"));
                }
                let mut out = format!(
                    "{} references across {} file(s):\n",
                    blast.total_sites,
                    blast.files.len()
                );
                for d in blast.dependents {
                    out.push_str(&format!(
                        "← {} {} {}:{}\n",
                        d.kind, d.name, d.rel_path, d.line
                    ));
                }
                Ok(out)
            }
            "path" => {
                let from = str_arg(args, "from")?;
                let to = str_arg(args, "to")?;
                match self.lattice.path(from, to, 8).map_err(map)? {
                    Some(chain) => Ok(chain.join(" -> ")),
                    None => Ok(format!("no reference path from '{from}' to '{to}'")),
                }
            }
            "why" => {
                let name = str_arg(args, "name")?;
                match self.lattice.why(name).map_err(map)? {
                    Some(p) => Ok(format!(
                        "{} ({}:{}) — {} · {} · {} · {}",
                        p.name, p.rel_path, p.line, p.author, p.date, p.commit, p.subject
                    )),
                    None => Ok(format!(
                        "no provenance for '{name}' (unknown symbol or not under git)"
                    )),
                }
            }
            other => Err(ToolError::BadArgs(format!(
                "unknown op '{other}' (expected query|impact|path|why)"
            ))),
        }
    }
}
