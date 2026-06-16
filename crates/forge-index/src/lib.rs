//! Lattice — Forge's native code-intelligence subsystem (docs/features/code-intelligence.md).
//! Multi-language tree-sitter extraction (via tags queries, `extract.rs`) persisted into the
//! shared SQLite store, incremental by file content hash. Provides structural `query`, reverse
//! dependents (`impact`), and call-chain `path`, plus a budgeted `retrieve` used by the agent
//! loop to auto-inject relevant context.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use forge_store::{
    LatticeEdgeRow, LatticeFileRow, LatticeNodeRow, LatticeRefRow, Store, StoreError,
};
use sha2::{Digest, Sha256};

mod extract;
mod retrieve;
mod watch;

pub use extract::{extract, lang_for_path, supported_languages, Def, Parsed, Ref};
pub use retrieve::{InjectedContext, RetrievedSnippet};
pub use watch::{spawn_watcher, LatticeWatcher};

#[derive(Debug, thiserror::Error)]
pub enum LatticeError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("io: {0}")]
    Io(String),
}

/// The code-intelligence graph for one repository root, backed by the shared [`Store`].
pub struct Lattice {
    store: Arc<Store>,
    /// Canonical root path, used to namespace symbols and compute repo-relative paths.
    repo_root: String,
}

/// What an `update` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateStats {
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub symbols: usize,
}

/// A symbol returned from a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeHit {
    pub name: String,
    pub kind: String,
    pub qualname: Option<String>,
    pub signature: Option<String>,
    pub rel_path: String,
    pub line: i64,
}

/// Reverse-dependency closure for `impact`: the symbol(s) named, everything that references them
/// (transitively), and the files involved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlastRadius {
    pub roots: Vec<NodeHit>,
    pub dependents: Vec<NodeHit>,
    pub files: Vec<String>,
    pub total_sites: usize,
}

/// Git provenance for a symbol — who last changed the line it's defined on, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub name: String,
    pub rel_path: String,
    pub line: i64,
    pub author: String,
    pub date: String,
    pub commit: String,
    pub subject: String,
}

/// A scoped subgraph for one symbol — what the interactive `/lattice` view renders: the matching
/// definitions, their reverse-dependents (blast radius), and git provenance for the exact match.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LatticeView {
    pub query: String,
    pub roots: Vec<NodeHit>,
    pub dependents: Vec<NodeHit>,
    pub why: Option<Provenance>,
}

/// Index-wide counts for `forge lattice status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatus {
    pub files: i64,
    pub nodes: i64,
    pub edges: i64,
    pub refs: i64,
}

impl Lattice {
    /// Open the Lattice for `repo_root` (canonicalized so identity is stable regardless of how
    /// the path was spelled).
    pub fn new(store: Arc<Store>, repo_root: &Path) -> Self {
        let repo_root = std::fs::canonicalize(repo_root)
            .unwrap_or_else(|_| repo_root.to_path_buf())
            .to_string_lossy()
            .into_owned();
        Self { store, repo_root }
    }

    /// Incrementally (re)index every supported source file under the root. Files whose content
    /// hash is unchanged since the last run are skipped without re-parsing.
    pub fn update(&self) -> Result<UpdateStats, LatticeError> {
        let mut stats = UpdateStats::default();
        let root = Path::new(&self.repo_root).to_path_buf();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if path.is_dir() {
                    if is_skippable_dir(&name) {
                        continue;
                    }
                    stack.push(path);
                } else if lang_for_path(&name).is_some() {
                    self.index_file(&path, &mut stats)?;
                }
            }
        }
        Ok(stats)
    }

    /// (Re)index a single file (e.g. after the agent edits it). No-op for unsupported files.
    pub fn reindex_path(&self, path: &Path) -> Result<(), LatticeError> {
        if path.to_str().and_then(lang_for_path).is_none() {
            return Ok(());
        }
        let mut stats = UpdateStats::default();
        self.index_file(path, &mut stats)
    }

    fn index_file(&self, path: &Path, stats: &mut UpdateStats) -> Result<(), LatticeError> {
        let rel = self.rel_path(path);
        let src = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => return Ok(()), // unreadable (e.g. non-UTF8) — skip, don't fail the whole run
        };
        let hash = sha_hex(src.as_bytes());
        if self
            .store
            .lattice_file_hash(&self.repo_root, &rel)?
            .as_deref()
            == Some(hash.as_str())
        {
            stats.files_skipped += 1;
            return Ok(());
        }

        let lang = lang_for_path(&rel).unwrap_or("unsupported");
        let file_id = sha_hex(format!("{}\0{}", self.repo_root, rel).as_bytes());
        let parsed = extract(&rel, &src);

        let mut node_ids: Vec<String> = Vec::with_capacity(parsed.defs.len());
        let mut nodes = Vec::with_capacity(parsed.defs.len());
        for d in &parsed.defs {
            let id = sha_hex(
                format!(
                    "{}\0{}\0{}\0{}\0{}",
                    self.repo_root, rel, d.kind, d.qualname, d.line_start
                )
                .as_bytes(),
            );
            node_ids.push(id.clone());
            nodes.push(LatticeNodeRow {
                id,
                file_id: file_id.clone(),
                kind: d.kind.clone(),
                name: d.name.clone(),
                qualname: Some(d.qualname.clone()),
                signature: d.signature.clone(),
                span_start: d.span_start as i64,
                span_end: d.span_end as i64,
                line_start: d.line_start as i64,
            });
        }

        // `contains` edges: enclosing definition → nested definition (struct→method, class→…).
        let mut edges = Vec::new();
        for (i, d) in parsed.defs.iter().enumerate() {
            if let Some(p) = d.parent {
                let id = sha_hex(format!("{}\0contains\0{}", node_ids[p], node_ids[i]).as_bytes());
                edges.push(LatticeEdgeRow {
                    id,
                    src_id: node_ids[p].clone(),
                    dst_id: node_ids[i].clone(),
                    kind: "contains".to_string(),
                    unresolved_name: None,
                });
            }
        }

        // References: each call/use site inside a definition → a name-keyed `lattice_ref` row.
        let mut refs = Vec::new();
        for (i, r) in parsed.refs.iter().enumerate() {
            let Some(from) = r.from else { continue };
            let id = sha_hex(
                format!(
                    "{}\0{}\0{}\0{}\0{i}",
                    node_ids[from], r.name, r.kind, r.line
                )
                .as_bytes(),
            );
            refs.push(LatticeRefRow {
                id,
                src_id: node_ids[from].clone(),
                name: r.name.clone(),
                kind: r.kind.clone(),
                line: r.line as i64,
            });
        }

        let file = LatticeFileRow {
            id: file_id,
            repo_root: self.repo_root.clone(),
            rel_path: rel,
            lang: lang.to_string(),
            content_hash: hash,
            parse_status: "ok".to_string(),
        };
        self.store
            .replace_lattice_file(&file, &nodes, &edges, &refs)?;
        stats.files_indexed += 1;
        stats.symbols += nodes.len();
        Ok(())
    }

    /// Symbols whose name matches `query` (case-insensitive), best-first.
    pub fn query(&self, query: &str, limit: usize) -> Result<Vec<NodeHit>, LatticeError> {
        let rows = self.store.lattice_nodes_by_name(query, limit)?;
        self.rows_to_hits(rows)
    }

    /// Reverse-dependency closure: who references `symbol`, transitively, up to `max_depth` hops.
    pub fn impact(&self, symbol: &str, max_depth: usize) -> Result<BlastRadius, LatticeError> {
        let roots = self.rows_to_hits(self.store.lattice_nodes_by_name(symbol, 32)?)?;
        let roots: Vec<NodeHit> = roots.into_iter().filter(|h| h.name == symbol).collect();

        let mut seen: HashSet<String> = HashSet::from([symbol.to_string()]);
        let mut frontier = vec![symbol.to_string()];
        let mut dependents: Vec<NodeHit> = Vec::new();
        let mut files: HashSet<String> = roots.iter().map(|h| h.rel_path.clone()).collect();

        for _ in 0..max_depth.max(1) {
            let mut next = Vec::new();
            for name in &frontier {
                for hit in self.rows_to_hits(self.store.lattice_callers_by_name(name, 200)?)? {
                    if seen.insert(hit.name.clone()) {
                        next.push(hit.name.clone());
                    }
                    files.insert(hit.rel_path.clone());
                    dependents.push(hit);
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }

        // De-dup dependents by (name, rel_path, line) — a symbol can be reached via many paths.
        dependents.sort_by(|a, b| {
            (a.rel_path.as_str(), a.line, a.name.as_str()).cmp(&(
                b.rel_path.as_str(),
                b.line,
                b.name.as_str(),
            ))
        });
        dependents.dedup();
        let total_sites = dependents.len();
        let mut files: Vec<String> = files.into_iter().collect();
        files.sort();
        Ok(BlastRadius {
            roots,
            dependents,
            files,
            total_sites,
        })
    }

    /// A shortest call/reference chain of symbol *names* from `a` to `b` (BFS over forward
    /// references), or `None` if `b` isn't reachable from `a` within `max_depth` hops.
    pub fn path(
        &self,
        a: &str,
        b: &str,
        max_depth: usize,
    ) -> Result<Option<Vec<String>>, LatticeError> {
        if a == b {
            return Ok(Some(vec![a.to_string()]));
        }
        let mut seen: HashSet<String> = HashSet::from([a.to_string()]);
        let mut queue: VecDeque<Vec<String>> = VecDeque::from([vec![a.to_string()]]);
        while let Some(chain) = queue.pop_front() {
            if chain.len() > max_depth.max(1) {
                continue;
            }
            let last = chain.last().unwrap();
            for callee in self.store.lattice_callees_of_name(last)? {
                if callee == b {
                    let mut found = chain.clone();
                    found.push(callee);
                    return Ok(Some(found));
                }
                if seen.insert(callee.clone()) {
                    let mut next = chain.clone();
                    next.push(callee);
                    queue.push_back(next);
                }
            }
        }
        Ok(None)
    }

    /// Git provenance for a symbol: resolve its definition's file+line, `git blame` that line for
    /// the last commit that touched it, and report author/date/commit/subject. `Ok(None)` when the
    /// symbol isn't indexed, the tree isn't under git, or git is unavailable (never errors the turn).
    pub fn why(&self, symbol: &str) -> Result<Option<Provenance>, LatticeError> {
        let Some(hit) = self
            .query(symbol, 8)?
            .into_iter()
            .find(|h| h.name == symbol)
        else {
            return Ok(None);
        };
        let sha = match git_blame_sha(&self.repo_root, &hit.rel_path, hit.line) {
            Some(s) => s,
            None => return Ok(None),
        };
        let Some(meta) = git_show_meta(&self.repo_root, &sha) else {
            return Ok(None);
        };
        Ok(Some(Provenance {
            name: hit.name,
            rel_path: hit.rel_path,
            line: hit.line,
            author: meta.0,
            date: meta.1,
            commit: meta.2,
            subject: meta.3,
        }))
    }

    /// Build the scoped subgraph for `symbol` (the interactive view): matching definitions, and —
    /// when there's an exact-name match — its reverse-dependents and git provenance.
    pub fn view(&self, symbol: &str) -> Result<LatticeView, LatticeError> {
        let roots = self.query(symbol, 12)?;
        let has_exact = roots.iter().any(|h| h.name == symbol);
        let (dependents, why) = if has_exact {
            (self.impact(symbol, 3)?.dependents, self.why(symbol)?)
        } else {
            (Vec::new(), None)
        };
        Ok(LatticeView {
            query: symbol.to_string(),
            roots,
            dependents,
            why,
        })
    }

    /// Retrieve a budgeted set of relevant code for `prompt` — the auto-injection payload.
    pub fn retrieve(
        &self,
        prompt: &str,
        token_budget: usize,
    ) -> Result<InjectedContext, LatticeError> {
        retrieve::retrieve(self, prompt, token_budget)
    }

    pub fn status(&self) -> Result<IndexStatus, LatticeError> {
        let (files, nodes, edges) = self.store.lattice_counts()?;
        let refs = self.store.lattice_ref_count()?;
        Ok(IndexStatus {
            files,
            nodes,
            edges,
            refs,
        })
    }

    fn rows_to_hits(&self, rows: Vec<LatticeNodeRow>) -> Result<Vec<NodeHit>, LatticeError> {
        let mut hits = Vec::with_capacity(rows.len());
        for r in rows {
            let rel_path = self
                .store
                .lattice_file_path(&r.file_id)?
                .unwrap_or_default();
            hits.push(NodeHit {
                name: r.name,
                kind: r.kind,
                qualname: r.qualname,
                signature: r.signature,
                rel_path,
                line: r.line_start,
            });
        }
        Ok(hits)
    }

    fn rel_path(&self, path: &Path) -> String {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        canon
            .strip_prefix(&self.repo_root)
            .unwrap_or(&canon)
            .to_string_lossy()
            .replace('\\', "/")
    }
}

/// Directories never worth indexing — build output, VCS, dependencies, and dotdirs.
fn is_skippable_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | "graphify-out"
            | "vendor"
            | "dist"
            | "build"
            | "__pycache__"
    ) || name.starts_with('.')
}

/// The commit sha that last touched `line` of `rel_path`, via `git blame --porcelain`. `None` if
/// git fails (not a repo, git missing, path untracked).
fn git_blame_sha(repo_root: &str, rel_path: &str, line: i64) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["blame", "-L"])
        .arg(format!("{line},{line}"))
        .args(["--porcelain", "--"])
        .arg(rel_path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_blame_sha(&String::from_utf8_lossy(&out.stdout))
}

/// The first token of `git blame --porcelain` output is the commit sha.
fn parse_blame_sha(porcelain: &str) -> Option<String> {
    let sha = porcelain.split_whitespace().next()?;
    (sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit())).then(|| sha.to_string())
}

/// `(author, date, short-sha, subject)` for a commit via `git show`. `None` on git failure.
fn git_show_meta(repo_root: &str, sha: &str) -> Option<(String, String, String, String)> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "show",
            "-s",
            "--date=short",
            "--format=%an%x09%ad%x09%h%x09%s",
            sha,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_show_meta(&String::from_utf8_lossy(&out.stdout))
}

/// Parse the tab-separated `git show` line into `(author, date, short-sha, subject)`.
fn parse_show_meta(line: &str) -> Option<(String, String, String, String)> {
    let line = line.trim();
    let mut parts = line.splitn(4, '\t');
    let author = parts.next()?.to_string();
    let date = parts.next()?.to_string();
    let commit = parts.next()?.to_string();
    let subject = parts.next().unwrap_or("").to_string();
    Some((author, date, commit, subject))
}

fn sha_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    // 128 bits of hex is plenty to avoid collisions across one repo's symbols.
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    struct Tmp {
        root: std::path::PathBuf,
    }
    impl Tmp {
        fn new() -> Tmp {
            let n = N.fetch_add(1, Ordering::SeqCst);
            let root =
                std::env::temp_dir().join(format!("forge-lattice-{}-{n}", std::process::id()));
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::create_dir_all(root.join("target/debug")).unwrap();
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

    fn lattice(root: &Path) -> Lattice {
        let store = Arc::new(Store::open_in_memory().unwrap());
        Lattice::new(store, root)
    }

    #[test]
    fn indexes_rust_files_and_queries_symbols() {
        let t = Tmp::new();
        t.write(
            "src/lib.rs",
            "pub struct Session { id: String }\nimpl Session { pub fn run_turn(&self) {} }\n",
        );
        // A file under target/ must be ignored.
        t.write("target/debug/built.rs", "pub fn should_not_index() {}");
        let lat = lattice(&t.root);

        let stats = lat.update().unwrap();
        assert_eq!(stats.files_indexed, 1, "only src/lib.rs, not target/");
        assert!(stats.symbols >= 2, "struct + method: {stats:?}");

        let hits = lat.query("run_turn", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel_path, "src/lib.rs");
        assert!(lat.query("should_not_index", 10).unwrap().is_empty());
    }

    #[test]
    fn indexes_multiple_languages() {
        let t = Tmp::new();
        t.write("src/app.py", "def greet(n):\n    return n\n");
        t.write("src/main.go", "package main\nfunc Run() {}\n");
        t.write("src/Widget.java", "class Widget { void render() {} }\n");
        let lat = lattice(&t.root);
        let stats = lat.update().unwrap();
        assert_eq!(stats.files_indexed, 3, "py + go + java: {stats:?}");
        assert_eq!(lat.query("greet", 10).unwrap().len(), 1);
        assert_eq!(lat.query("Run", 10).unwrap().len(), 1);
        assert_eq!(lat.query("Widget", 10).unwrap().len(), 1);
    }

    #[test]
    fn impact_finds_callers_across_files() {
        let t = Tmp::new();
        t.write("src/a.rs", "pub fn target() {}\n");
        t.write(
            "src/b.rs",
            "use crate::a::target;\npub fn caller() { target(); }\n",
        );
        let lat = lattice(&t.root);
        lat.update().unwrap();
        let blast = lat.impact("target", 3).unwrap();
        assert!(
            blast.dependents.iter().any(|d| d.name == "caller"),
            "caller references target: {blast:?}"
        );
    }

    #[test]
    fn reindex_is_incremental_on_unchanged_hash() {
        let t = Tmp::new();
        t.write("src/a.rs", "pub fn alpha() {}");
        let lat = lattice(&t.root);

        let first = lat.update().unwrap();
        assert_eq!(first.files_indexed, 1);
        assert_eq!(first.files_skipped, 0);

        let second = lat.update().unwrap();
        assert_eq!(second.files_indexed, 0);
        assert_eq!(second.files_skipped, 1);

        t.write("src/a.rs", "pub fn beta() {}");
        let third = lat.update().unwrap();
        assert_eq!(third.files_indexed, 1);
        assert_eq!(lat.query("beta", 10).unwrap().len(), 1);
        assert!(
            lat.query("alpha", 10).unwrap().is_empty(),
            "stale symbol removed"
        );
    }

    #[test]
    fn view_bundles_roots_and_dependents() {
        let t = Tmp::new();
        t.write("src/a.rs", "pub fn target() {}\n");
        t.write(
            "src/b.rs",
            "use crate::a::target;\npub fn caller() { target(); }\n",
        );
        let lat = lattice(&t.root);
        lat.update().unwrap();
        let view = lat.view("target").unwrap();
        assert!(view.roots.iter().any(|h| h.name == "target"));
        assert!(
            view.dependents.iter().any(|d| d.name == "caller"),
            "blast radius includes the caller: {view:?}"
        );
    }

    #[test]
    fn parses_git_provenance_output() {
        let porcelain =
            "9b64263a1f2e3d4c5b6a7890 12 12 1\nauthor Floris\nsummary did a thing\n\tcode line\n";
        assert_eq!(
            parse_blame_sha(porcelain).as_deref(),
            Some("9b64263a1f2e3d4c5b6a7890")
        );
        assert_eq!(parse_blame_sha("\n\n"), None);

        let show = "Floris\t2026-06-16\tab3b2ef\tfeat: add the watcher\n";
        let (author, date, commit, subject) = parse_show_meta(show).unwrap();
        assert_eq!(author, "Floris");
        assert_eq!(date, "2026-06-16");
        assert_eq!(commit, "ab3b2ef");
        assert_eq!(subject, "feat: add the watcher");
    }

    #[test]
    fn status_reports_counts() {
        let t = Tmp::new();
        t.write("src/lib.rs", "pub fn one() {}\npub fn two() {}");
        let lat = lattice(&t.root);
        lat.update().unwrap();
        let s = lat.status().unwrap();
        assert_eq!(s.files, 1);
        assert_eq!(s.nodes, 2);
    }
}
