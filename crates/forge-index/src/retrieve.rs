//! Budgeted retrieval: turn a prompt into a ranked, token-bounded set of relevant symbols to
//! inject into the turn. Structural + lexical (embeddings fold in via `merge_semantic`): extract
//! candidate identifiers from the prompt, look them up by name, and pack them until the token
//! budget is hit. The top hits can be injected as full source **bodies** (the big token-saving
//! lever — the model reads the function from context instead of spending a whole-file `read_file`);
//! the rest are signature lines. Cheap, deterministic, degrades to "nothing" on an empty index.

use std::collections::HashSet;
use std::path::Path;

use crate::{Lattice, LatticeError, NodeHit};

/// Body-injection settings for [`retrieve`]. When present, up to [`BodyOpts::max_hits`] of the
/// top-ranked symbols are injected as full source bodies (each capped at [`BodyOpts::max_tokens`]).
#[derive(Debug, Clone, Copy)]
pub struct BodyOpts {
    pub max_tokens: usize,
    pub max_hits: usize,
}

/// One packed unit of injected context — either a one-line signature or a full source body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievedSnippet {
    pub rel_path: String,
    pub line: i64,
    /// For a signature snippet: the signature line. For a body snippet: the full fenced block.
    pub text: String,
    /// True when `text` is a complete, multi-line fenced source body (rendered verbatim).
    pub is_body: bool,
}

/// The result of [`Lattice::retrieve`] — what gets injected as a system message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InjectedContext {
    pub nodes: Vec<NodeHit>,
    pub snippets: Vec<RetrievedSnippet>,
    pub est_tokens: usize,
}

impl InjectedContext {
    pub fn is_empty(&self) -> bool {
        self.snippets.is_empty()
    }

    /// The system-message body injected ahead of the turn. Signature snippets render as a compact
    /// bulleted list; body snippets render as a labelled fenced block so the model reads the source
    /// directly (and needn't `read_file` the whole containing file).
    pub fn render(&self) -> String {
        let mut s = String::from("Relevant code (Lattice):\n");
        for snip in &self.snippets {
            if snip.is_body {
                s.push('\n');
                s.push_str(&snip.text);
                s.push('\n');
            } else {
                s.push_str(&format!(
                    "- {} ({}:{})\n",
                    snip.text, snip.rel_path, snip.line
                ));
            }
        }
        s
    }
}

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "this", "that", "with", "from", "into", "add", "use", "new", "get", "set",
    "all", "any", "but", "not", "you", "are", "can", "how", "why", "what", "where", "when", "fix",
    "make", "thread", "through", "field", "function", "method", "file", "code", "test", "tests",
];

/// Identifier-ish tokens worth looking up: length ≥ 3, not a stopword. CamelCase / snake_case
/// tokens are kept whole (a real symbol name); split words also count so plain prose still hits.
fn identifiers(prompt: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<String>, seen: &mut HashSet<String>| {
        if cur.len() >= 3
            && !STOPWORDS.contains(&cur.to_lowercase().as_str())
            && seen.insert(cur.clone())
        {
            out.push(std::mem::take(cur));
        } else {
            cur.clear();
        }
    };
    for c in prompt.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            flush(&mut cur, &mut out, &mut seen);
        }
    }
    flush(&mut cur, &mut out, &mut seen);
    out.truncate(12);
    out
}

fn render_line(n: &NodeHit) -> String {
    match (&n.signature, &n.qualname) {
        (Some(sig), _) => format!("{} {sig}", n.kind),
        (None, Some(q)) => format!("{} {q}", n.kind),
        (None, None) => format!("{} {}", n.kind, n.name),
    }
}

/// ~4 chars per token, floored at 1.
fn est_tokens(s: &str) -> usize {
    (s.len() / 4).max(1)
}

/// Read a symbol's source body by byte span, snapping to char boundaries and clamping to the file
/// length so a stale/oversize span can never panic. `None` on any read/range failure (caller then
/// falls back to the signature line).
fn read_body(repo_root: &str, rel_path: &str, start: i64, end: i64) -> Option<String> {
    if end <= start || start < 0 {
        return None;
    }
    let full = std::fs::read_to_string(Path::new(repo_root).join(rel_path)).ok()?;
    let mut s = (start as usize).min(full.len());
    let mut e = (end as usize).min(full.len());
    while s < full.len() && !full.is_char_boundary(s) {
        s += 1;
    }
    while e < full.len() && !full.is_char_boundary(e) {
        e += 1;
    }
    if s >= e {
        return None;
    }
    Some(full[s..e].to_string())
}

/// Build a fenced body block: `path:line — kind name` header then the source in a code fence.
fn body_block(n: &NodeHit, body: &str) -> String {
    let lang = n.rel_path.rsplit('.').next().unwrap_or("");
    format!(
        "{}:{} — {} {}\n```{lang}\n{}\n```",
        n.rel_path,
        n.line,
        n.kind,
        n.name,
        body.trim_end()
    )
}

pub fn retrieve(
    lat: &Lattice,
    prompt: &str,
    token_budget: usize,
    bodies: Option<BodyOpts>,
) -> Result<InjectedContext, LatticeError> {
    let idents = identifiers(prompt);
    // Prompt-adaptive ceiling: don't spend the full budget padding context for a prompt that names
    // one symbol. Each named symbol "earns" up to ~one body's worth of budget, clamped to the
    // configured ceiling. A prompt with no identifiers still gets a small floor.
    let per_ident = bodies.map(|b| b.max_tokens).unwrap_or(400).max(400);
    let token_budget = token_budget.min((idents.len().max(1) * per_ident).max(per_ident));

    let mut seen: HashSet<(String, i64, String)> = HashSet::new();
    let mut candidates: Vec<NodeHit> = Vec::new();
    for ident in &idents {
        for hit in lat.query(ident, 5)? {
            // Prefer exact name matches as the strongest signal.
            let exact = hit.name.eq_ignore_ascii_case(ident);
            if seen.insert((hit.rel_path.clone(), hit.line, hit.name.clone())) {
                if exact {
                    candidates.insert(0, hit);
                } else {
                    candidates.push(hit);
                }
            }
        }
    }

    let mut est = 0usize;
    let mut snippets = Vec::new();
    let mut nodes = Vec::new();
    let mut bodies_left = bodies.map(|b| b.max_hits).unwrap_or(0);
    for n in candidates {
        // Try a body for the top hits: a precise function body injected here saves the model a
        // whole-file `read_file` later. Falls back to the signature line if the body is missing,
        // too large, or wouldn't fit the budget.
        if bodies_left > 0 {
            if let Some(opts) = bodies {
                if let Some(body) =
                    read_body(lat.repo_root(), &n.rel_path, n.span_start, n.span_end)
                {
                    let block = body_block(&n, &body);
                    let cost = est_tokens(&block);
                    if cost <= opts.max_tokens && est + cost <= token_budget {
                        est += cost;
                        snippets.push(RetrievedSnippet {
                            rel_path: n.rel_path.clone(),
                            line: n.line,
                            text: block,
                            is_body: true,
                        });
                        nodes.push(n);
                        bodies_left -= 1;
                        continue;
                    }
                }
            }
        }
        let text = render_line(&n);
        let cost = est_tokens(&text);
        if est + cost > token_budget {
            break;
        }
        est += cost;
        snippets.push(RetrievedSnippet {
            rel_path: n.rel_path.clone(),
            line: n.line,
            text,
            is_body: false,
        });
        nodes.push(n);
    }

    Ok(InjectedContext {
        nodes,
        snippets,
        est_tokens: est,
    })
}

/// Fold semantic (embedding-ranked) hits into an existing structural context: append any not
/// already present, in rank order, until the token budget is hit. Dedups by (path, line, name).
pub fn merge_semantic(
    mut ctx: InjectedContext,
    semantic: Vec<NodeHit>,
    token_budget: usize,
) -> InjectedContext {
    let mut seen: HashSet<(String, i64, String)> = ctx
        .nodes
        .iter()
        .map(|n| (n.rel_path.clone(), n.line, n.name.clone()))
        .collect();
    for n in semantic {
        if !seen.insert((n.rel_path.clone(), n.line, n.name.clone())) {
            continue;
        }
        let text = render_line(&n);
        let cost = est_tokens(&text);
        if ctx.est_tokens + cost > token_budget {
            continue;
        }
        ctx.est_tokens += cost;
        ctx.snippets.push(RetrievedSnippet {
            rel_path: n.rel_path.clone(),
            line: n.line,
            text,
            is_body: false,
        });
        ctx.nodes.push(n);
    }
    ctx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_keep_symbols_drop_stopwords() {
        let ids = identifiers("add a depth field to Session and thread it through start");
        assert!(ids.contains(&"Session".to_string()));
        assert!(ids.contains(&"depth".to_string()));
        assert!(ids.contains(&"start".to_string()));
        assert!(!ids.iter().any(|i| i == "and" || i == "the" || i == "field"));
    }

    #[test]
    fn body_block_fences_source_with_header() {
        let n = NodeHit {
            name: "foo".into(),
            kind: "function".into(),
            qualname: None,
            signature: None,
            rel_path: "src/a.rs".into(),
            line: 10,
            span_start: 0,
            span_end: 0,
        };
        let b = body_block(&n, "fn foo() {}\n");
        assert!(b.starts_with("src/a.rs:10 — function foo"));
        assert!(b.contains("```rs\nfn foo() {}\n```"));
    }
}
