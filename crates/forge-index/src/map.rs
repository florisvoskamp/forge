//! Repo-map: a compact, token-budgeted, importance-ranked overview of the repository's key
//! definitions grouped by file — the aider-style "map" that helps a model orient in an unfamiliar
//! codebase without reading every file. Symbols are selected by PageRank (already computed in
//! `Lattice::update`); within a file they are listed in source order (line ascending) so the
//! output reads like the file itself. The whole map is deterministic: same index → same output.

use std::collections::BTreeMap;

use crate::{Lattice, LatticeError};

/// ~4 chars/token — the same estimate used in `retrieve.rs`.
fn est_tokens(s: &str) -> usize {
    (s.len() / 4).max(1)
}

/// One display line for a symbol: the render format mirrors what `retrieve::render_line` produces
/// (kind + signature or qualname or name), but kept local so map.rs has no dep on retrieve's
/// private helpers.
fn render_sig(kind: &str, name: &str, sig: Option<&str>, qualname: Option<&str>) -> String {
    match (sig, qualname) {
        (Some(s), _) => format!("{kind} {s}"),
        (None, Some(q)) => format!("{kind} {q}"),
        (None, None) => format!("{kind} {name}"),
    }
}

/// Build a compact, token-budgeted repo-map from the Lattice index.
///
/// Algorithm:
/// 1. Fetch all nodes ranked by pagerank descending (higher = more important). The store query
///    returns them pre-sorted, so no resorting is needed for selection.
/// 2. Greedily pick nodes until the token budget is exhausted. A file-header line costs ~1 token
///    and is emitted the first time a file is encountered; each symbol line costs ~1-4 tokens.
///    Estimation uses the same 4-char/token heuristic as `retrieve.rs`.
/// 3. Group the selected nodes by `rel_path`, then within each file sort by line ascending so
///    the output reads in source order.
/// 4. Render: one `<path>:` header line per file, then its selected symbols indented by two spaces.
///
/// Ordering guarantees:
/// - Files appear in lexicographic path order (deterministic, readable).
/// - Symbols within a file appear in line order.
/// - Selection priority is pagerank descending (importance).
pub fn build_map(lat: &Lattice, token_budget: usize) -> Result<String, LatticeError> {
    // Fetch all nodes ranked by pagerank descending (the store handles the ORDER BY).
    // We pass usize::MAX so we get everything and apply the token budget ourselves.
    let all_nodes = lat.store_nodes_ranked(usize::MAX)?;

    if all_nodes.is_empty() {
        return Ok(String::from(
            "⌬ lattice map — index is empty (run `forge lattice update` first)\n",
        ));
    }

    // Group selected nodes by file path, preserving selection order within each path so we can
    // track which files were seen first (for header cost accounting).
    // Key: rel_path, Value: Vec<(line, rendered_sig)> — filled greedily, sorted later.
    let mut by_file: BTreeMap<String, Vec<(i64, String)>> = BTreeMap::new();
    let mut est = 0usize;

    for node in &all_nodes {
        if node.rel_path.is_empty() {
            continue;
        }

        let sig = render_sig(
            &node.kind,
            &node.name,
            node.signature.as_deref(),
            node.qualname.as_deref(),
        );
        let sym_line = format!("  {sig}");
        let sym_cost = est_tokens(&sym_line);

        // Account for the file header if this is the first symbol from this file.
        let header_cost = if by_file.contains_key(&node.rel_path) {
            0
        } else {
            let header = format!("{}:", node.rel_path);
            est_tokens(&header)
        };

        let total_cost = header_cost + sym_cost;
        if est + total_cost > token_budget {
            // Budget exhausted — stop selecting. Because nodes are ordered by pagerank desc,
            // everything remaining is less important. We allow the very first symbol if the
            // budget is tiny (avoid a completely empty map).
            if est == 0 {
                // Emit at least one entry so the caller gets something useful.
                by_file
                    .entry(node.rel_path.clone())
                    .or_default()
                    .push((node.line, sym_line));
                // est not updated — we're breaking immediately after, so the write is a no-op.
            }
            break;
        }

        est += total_cost;
        by_file
            .entry(node.rel_path.clone())
            .or_default()
            .push((node.line, sym_line));
    }

    // Render: BTreeMap iterates keys in sorted (lexicographic) order → deterministic file order.
    let mut out = String::new();
    for (path, mut syms) in by_file {
        // Sort within each file by source line for readability.
        syms.sort_by_key(|(line, _)| *line);
        out.push_str(&path);
        out.push(':');
        out.push('\n');
        for (_, sym) in syms {
            out.push_str(&sym);
            out.push('\n');
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lattice;
    use forge_store::Store;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    static N: AtomicUsize = AtomicUsize::new(0);

    struct Tmp {
        root: std::path::PathBuf,
    }
    impl Tmp {
        fn new() -> Tmp {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let root = std::env::temp_dir().join(format!("forge-map-{}-{n}", std::process::id()));
            std::fs::create_dir_all(root.join("src")).unwrap();
            Tmp { root }
        }
        fn write(&self, rel: &str, content: &str) {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn lattice(root: &std::path::Path) -> Lattice {
        let store = Arc::new(Store::open_in_memory().unwrap());
        Lattice::new(store, root)
    }

    /// Higher-pagerank symbols appear in the map before lower-pagerank ones when the budget is
    /// tight enough to exclude all symbols. We build a small graph: `hub` is called by three
    /// callers (high pagerank); `leaf` is never referenced (low pagerank). With a budget that
    /// fits only one symbol, the map must contain `hub` and not `leaf`.
    #[test]
    fn build_map_higher_pagerank_selected_first() {
        let t = Tmp::new();
        t.write(
            "src/lib.rs",
            r#"
pub fn hub() {}
pub fn leaf() {}
pub fn caller_a() { hub(); }
pub fn caller_b() { hub(); }
pub fn caller_c() { hub(); }
"#,
        );
        let lat = lattice(&t.root);
        lat.update().unwrap();

        // Very tight budget: enough for the header + one short symbol line (~3–5 tokens each).
        // "src/lib.rs:" is ~3 tokens, "  function hub" is ~4 tokens → 8 tokens fits one.
        let map = build_map(&lat, 8).unwrap();
        assert!(
            map.contains("hub"),
            "hub (high pagerank) must appear in a tight-budget map:\n{map}"
        );
        assert!(
            !map.contains("leaf"),
            "leaf (low pagerank) must be excluded when budget is tight:\n{map}"
        );
    }

    /// With a generous budget all symbols appear, and the output is deterministic.
    #[test]
    fn build_map_within_budget_and_deterministic() {
        let t = Tmp::new();
        t.write("src/a.rs", "pub fn alpha() {}\npub fn beta() {}\n");
        t.write("src/b.rs", "pub fn gamma() {}\n");
        let lat = lattice(&t.root);
        lat.update().unwrap();

        let map1 = build_map(&lat, 2000).unwrap();
        let map2 = build_map(&lat, 2000).unwrap();
        assert_eq!(map1, map2, "map must be deterministic");

        // All three symbols must appear.
        assert!(map1.contains("alpha"), "alpha missing:\n{map1}");
        assert!(map1.contains("beta"), "beta missing:\n{map1}");
        assert!(map1.contains("gamma"), "gamma missing:\n{map1}");
    }

    /// The estimated token count of the output must not exceed the budget (allowing the last item
    /// that pushed it over — the greedy break is after the budget check).
    #[test]
    fn build_map_respects_token_budget() {
        let t = Tmp::new();
        // 20 symbols in one file — generous enough that a tight budget must cut most of them.
        let src: String = (0..20)
            .map(|i| format!("pub fn sym_{i:02}() {{}}\n"))
            .collect();
        t.write("src/lib.rs", &src);
        let lat = lattice(&t.root);
        lat.update().unwrap();

        let budget = 30usize;
        let map = build_map(&lat, budget).unwrap();

        // est_tokens uses 4-char/token; the map should be bounded reasonably close to budget.
        // We allow 1 extra item past the cutoff (the "last item" grace).
        let estimated = est_tokens(&map);
        // The output est must be within (budget + max one extra item ~10 tokens).
        assert!(
            estimated <= budget + 10,
            "map est_tokens={estimated} greatly exceeds budget={budget}:\n{map}"
        );
    }

    /// Within a file, symbols must appear in line order regardless of pagerank selection order.
    #[test]
    fn build_map_symbols_in_line_order_within_file() {
        let t = Tmp::new();
        // Define symbols in reverse alphabetical order so the line numbers won't match
        // alphabetical order — we can verify line-order sorting.
        t.write(
            "src/lib.rs",
            "pub fn zzz() {}\npub fn mmm() {}\npub fn aaa() {}\n",
        );
        let lat = lattice(&t.root);
        lat.update().unwrap();

        let map = build_map(&lat, 2000).unwrap();
        let zzz = map.find("zzz").unwrap_or(usize::MAX);
        let mmm = map.find("mmm").unwrap_or(usize::MAX);
        let aaa = map.find("aaa").unwrap_or(usize::MAX);
        assert!(
            zzz < mmm && mmm < aaa,
            "symbols must appear in line order (zzz:1, mmm:2, aaa:3):\n{map}"
        );
    }

    /// Empty index returns a human-readable message, not an error or panic.
    #[test]
    fn build_map_empty_index_graceful() {
        let t = Tmp::new();
        let lat = lattice(&t.root);
        let map = build_map(&lat, 2000).unwrap();
        assert!(
            map.contains("empty") || map.contains("update"),
            "empty index should hint at running update:\n{map}"
        );
    }
}
