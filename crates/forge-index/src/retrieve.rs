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

/// Max signature lines injected per turn. Beyond a handful, extra fuzzy matches are noise that the
/// model re-reads on every step without benefit. Bodies (exact hits) are not counted here.
const MAX_SIG_SNIPPETS: usize = 8;

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "this", "that", "with", "from", "into", "add", "use", "new", "get", "set",
    "all", "any", "but", "not", "you", "are", "can", "how", "why", "what", "where", "when", "fix",
    "make", "thread", "through", "field", "function", "method", "file", "code", "test", "tests",
    "answer", "stop", "concisely", "please", "every", "given",
];

/// A token that *looks like a code symbol* — snake_case (`inject_budget`) or multi-word mixed-case
/// (`BudgetStatus`, `PermissionMode`). Requires an underscore or an **internal** capital: a single
/// leading capital is just Titlecase prose ("Answer", "Usage", "Store") and must not qualify, or
/// sentence-start words leak in and drag a wall of unrelated symbols into the injection. Genuine
/// single-word types ("Usage") are recovered via backtick quoting instead (see [`backticked`]).
fn looks_like_symbol(t: &str) -> bool {
    let internal_upper = t.chars().enumerate().any(|(i, c)| i > 0 && c.is_ascii_uppercase());
    let has_lower = t.chars().any(|c| c.is_ascii_lowercase());
    // `_` → snake_case. internal-upper + lower → camelCase/PascalCase-multiword. The lowercase
    // requirement excludes all-caps acronyms ("SQL", "INSERT", "JSON") which are prose, not symbols
    // — and which otherwise exact-match unrelated helpers ("insert") and inject a fat wrong body.
    t.contains('_') || (internal_upper && has_lower)
}

/// Identifiers the prompt wrapped in backticks (`` `Usage` ``) — an explicit "this is code" signal,
/// so they count as symbol-shaped even when Titlecase. Returns the lengthy-enough tokens inside any
/// backtick span.
fn backticked(prompt: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    let mut inside = false;
    for part in prompt.split('`') {
        if inside {
            for tok in part.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
                if tok.len() >= 3 {
                    set.insert(tok.to_string());
                }
            }
        }
        inside = !inside;
    }
    set
}

/// Query terms extracted from a prompt, plus whether they came from the high-confidence
/// symbol-shaped path. `strong=false` means we fell back to plain prose words (no symbol-shaped
/// token in the prompt) — a low-confidence signal where injecting a fat source *body* is risky
/// (a prose word like "insert" can exact-match an unrelated helper), so the caller injects only
/// signature lines in that case.
struct Query {
    terms: Vec<String>,
    strong: bool,
}

/// Identifier-ish tokens worth looking up: length ≥ 3, not a stopword. **Symbol-shaped** tokens
/// (snake_case / CamelCase) are strongly preferred — if the prompt names any, we query *only* those
/// and ignore the surrounding prose, because a fuzzy prose match (e.g. "forge", "value") returns a
/// wall of irrelevant symbols that costs tokens on every agent step without answering anything.
/// Only when the prompt contains no symbol-shaped token do we fall back to plain words so generic
/// prose questions still hit something.
fn extract_query(prompt: &str) -> Query {
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

    let quoted = backticked(prompt);
    let strong: Vec<String> = out
        .iter()
        .filter(|t| looks_like_symbol(t) || quoted.contains(*t))
        .cloned()
        .collect();
    let is_strong = !strong.is_empty();
    let mut terms = if is_strong { strong } else { out };
    terms.truncate(12);
    Query {
        terms,
        strong: is_strong,
    }
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
    let Query {
        terms: idents,
        strong,
    } = extract_query(prompt);
    // Bodies are high-cost and re-sent every agent step — only inject them when the query is
    // confident (symbol-shaped). On a prose fallback, signatures only.
    let bodies = if strong { bodies } else { None };
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
    let mut sigs_left = MAX_SIG_SNIPPETS;
    for n in candidates {
        // A body is only worth its (per-step) cost for a *confident* hit — a symbol whose name
        // exactly matches a queried identifier. Injecting a fuzzy match's body (e.g. `ForgeMcp` for
        // the word "forge") spends hundreds of tokens on noise the model must then ignore.
        let exact = idents.iter().any(|id| n.name.eq_ignore_ascii_case(id));
        if bodies_left > 0 && exact {
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
        // On a low-confidence prose query, a fuzzy signature match is worse than nothing: it points
        // the model at an unrelated symbol and *adds* exploration. Inject only exact-name hits then
        // (possibly nothing — degrading to the no-injection baseline rather than misleading).
        if !strong && !exact {
            continue;
        }
        // Signature lines are cheap individually but a long tail of fuzzy matches is pure tax,
        // re-sent on every agent step. Cap them so injection stays lean.
        if sigs_left == 0 {
            continue;
        }
        let text = render_line(&n);
        let cost = est_tokens(&text);
        if est + cost > token_budget {
            break;
        }
        est += cost;
        sigs_left -= 1;
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
        // No symbol-shaped token here → prose fallback, so plain words survive.
        let q = extract_query("add a depth field to session and thread it through start");
        assert!(!q.strong);
        assert!(q.terms.contains(&"depth".to_string()));
        assert!(q.terms.contains(&"start".to_string()));
        assert!(!q.terms.iter().any(|i| i == "and" || i == "the" || i == "field"));
    }

    #[test]
    fn symbol_shaped_tokens_win_over_prose() {
        // A snake_case / CamelCase token makes the query "strong" and suppresses prose noise.
        let q = extract_query("what does inject_budget return for the BudgetStatus value");
        assert!(q.strong);
        assert!(q.terms.contains(&"inject_budget".to_string()));
        assert!(q.terms.contains(&"BudgetStatus".to_string()));
        assert!(!q.terms.iter().any(|t| t == "does" || t == "return"));
    }

    #[test]
    fn all_caps_acronyms_are_not_symbols() {
        // "SQL"/"INSERT" are prose acronyms, not symbol names — must not trigger the strong path.
        assert!(!looks_like_symbol("SQL"));
        assert!(!looks_like_symbol("INSERT"));
        assert!(!looks_like_symbol("Store")); // leading-cap Titlecase is prose
        assert!(looks_like_symbol("BudgetStatus"));
        assert!(looks_like_symbol("inject_budget"));
    }

    #[test]
    fn backticked_titlecase_counts_as_symbol() {
        let q = extract_query("list fields of the `Usage` struct");
        assert!(q.strong);
        assert!(q.terms.contains(&"Usage".to_string()));
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
