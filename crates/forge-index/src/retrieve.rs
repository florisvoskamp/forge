//! Budgeted retrieval: turn a prompt into a ranked, token-bounded set of relevant symbols to
//! inject into the turn. Structural + lexical only (embeddings are a later PR): extract candidate
//! identifiers from the prompt, look them up by name, and pack signatures until the token budget
//! is hit. Cheap, deterministic, and degrades to "nothing" on an empty index.

use std::collections::HashSet;

use crate::{Lattice, LatticeError, NodeHit};

/// One packed line of injected context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievedSnippet {
    pub rel_path: String,
    pub line: i64,
    pub text: String,
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

    /// The system-message body injected ahead of the turn.
    pub fn render(&self) -> String {
        let mut s = String::from("Relevant code (Lattice):\n");
        for snip in &self.snippets {
            s.push_str(&format!(
                "- {} ({}:{})\n",
                snip.text, snip.rel_path, snip.line
            ));
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

pub fn retrieve(
    lat: &Lattice,
    prompt: &str,
    token_budget: usize,
) -> Result<InjectedContext, LatticeError> {
    let mut seen: HashSet<(String, i64, String)> = HashSet::new();
    let mut candidates: Vec<NodeHit> = Vec::new();
    for ident in identifiers(prompt) {
        for hit in lat.query(&ident, 5)? {
            // Prefer exact name matches as the strongest signal.
            let exact = hit.name.eq_ignore_ascii_case(&ident);
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
    for n in candidates {
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
        });
        nodes.push(n);
    }

    Ok(InjectedContext {
        nodes,
        snippets,
        est_tokens: est,
    })
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
}
