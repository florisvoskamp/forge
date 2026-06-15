//! Named subagent types loaded from `.forge/agents/<name>.md` (RFC subagent-orchestration,
//! Phase 2). Each file is a small front-matter block followed by the agent's system prompt:
//!
//! ```text
//! ---
//! name: reviewer
//! description: Reviews a code change for bugs and risk.
//! tools: [read_file, list_dir, search]   # optional; omit → default read-only set
//! tier: standard                          # optional; omit → mesh-routed per task
//! ---
//! You are a meticulous code reviewer. ...
//! ```
//!
//! Parsing is dependency-free (a tiny front-matter reader, not full YAML) so a malformed file
//! degrades to being skipped rather than failing the whole load.

use std::collections::HashMap;
use std::path::Path;

use forge_types::TaskTier;

/// A reusable subagent type. The `system_prompt` is the file body; `tools`/`tier` are optional
/// overrides resolved by the orchestrator (empty `tools` → the default read-only set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub tier: Option<TaskTier>,
    pub system_prompt: String,
}

/// Load every `*.md` agent definition in `dir`, keyed by `name` (falling back to the file stem).
/// A missing directory or an unparseable file is skipped, never an error — agent types are
/// optional convenience config.
pub fn load_agents(dir: &Path) -> HashMap<String, AgentDef> {
    let mut out = HashMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("agent")
            .to_string();
        if let Some(def) = parse_agent(&text, &stem) {
            out.insert(def.name.clone(), def);
        }
    }
    out
}

/// Parse one agent file's text. `default_name` is used when the front matter omits `name`.
/// Returns `None` if there is no front-matter block.
pub fn parse_agent(text: &str, default_name: &str) -> Option<AgentDef> {
    let rest = text.strip_prefix("---")?;
    // Split on the closing fence; everything after it is the system prompt body.
    let (front, body) = split_front_matter(rest)?;

    let mut name = default_name.to_string();
    let mut description = String::new();
    let mut tools = Vec::new();
    let mut tier = None;

    for line in front.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "name" if !value.is_empty() => name = unquote(value),
            "description" => description = unquote(value),
            "tools" => tools = parse_list(value),
            "tier" => tier = parse_tier(value),
            _ => {}
        }
    }

    Some(AgentDef {
        name,
        description,
        tools,
        tier,
        system_prompt: body.trim().to_string(),
    })
}

/// Split `---`-prefixed-stripped text into (front_matter, body) on the next `---` line.
fn split_front_matter(after_open: &str) -> Option<(&str, &str)> {
    // The content after the opening fence; find a line that is exactly `---`.
    let mut idx = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']).trim() == "---" {
            let front = &after_open[..idx];
            let body = &after_open[idx + line.len()..];
            return Some((front, body));
        }
        idx += line.len();
    }
    None
}

fn unquote(s: &str) -> String {
    s.trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string()
}

/// Parse `[a, b, c]` or a bare comma list into items.
fn parse_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(unquote)
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_tier(value: &str) -> Option<TaskTier> {
    match unquote(value).to_lowercase().as_str() {
        "trivial" => Some(TaskTier::Trivial),
        "standard" => Some(TaskTier::Standard),
        "complex" => Some(TaskTier::Complex),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_front_matter_and_body() {
        let text = "---\nname: reviewer\ndescription: Reviews a change.\ntools: [read_file, search]\ntier: standard\n---\nYou are a reviewer.\nBe terse.";
        let def = parse_agent(text, "fallback").unwrap();
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "Reviews a change.");
        assert_eq!(def.tools, vec!["read_file", "search"]);
        assert_eq!(def.tier, Some(TaskTier::Standard));
        assert_eq!(def.system_prompt, "You are a reviewer.\nBe terse.");
    }

    #[test]
    fn omitted_fields_default_sensibly() {
        let text = "---\ndescription: just a body\n---\nDo the thing.";
        let def = parse_agent(text, "myagent").unwrap();
        assert_eq!(def.name, "myagent"); // falls back to file stem
        assert!(def.tools.is_empty()); // → default read-only set, decided in core
        assert_eq!(def.tier, None); // → mesh-routed
        assert_eq!(def.system_prompt, "Do the thing.");
    }

    #[test]
    fn no_front_matter_is_none() {
        assert!(parse_agent("just a plain file", "x").is_none());
    }
}
