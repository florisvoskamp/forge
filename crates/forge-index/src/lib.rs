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

mod embed;
mod extract;
mod map;
mod retrieve;
mod watch;

pub use embed::{parse_ollama_embeddings, Embedder, OllamaEmbedder};
pub use extract::{extract, lang_for_path, supported_languages, Def, Parsed, Ref};
pub use map::build_map;
pub use retrieve::{BodyOpts, InjectedContext, RetrievedSnippet};
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
#[derive(Debug, Clone, PartialEq)]
pub struct NodeHit {
    pub name: String,
    pub kind: String,
    pub qualname: Option<String>,
    pub signature: Option<String>,
    pub rel_path: String,
    pub line: i64,
    /// Byte offsets of the symbol's source span in its file (from tree-sitter). Used to slice the
    /// body for body-injection retrieval without re-parsing. `(0, 0)` when unknown.
    pub span_start: i64,
    pub span_end: i64,
    /// PageRank importance score: higher = referenced by more symbols. Used as a secondary
    /// sort key (tie-break) within each retrieval group. `0.0` until `recompute_pagerank` runs.
    pub pagerank: f64,
}

/// Reverse-dependency closure for `impact`: the symbol(s) named, everything that references them
/// (transitively), and the files involved.
#[derive(Debug, Clone, Default, PartialEq)]
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
#[derive(Debug, Clone, Default, PartialEq)]
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
        if stats.files_indexed > 0 {
            // Recompute PageRank whenever the graph changed. Best-effort: a failure here is
            // non-fatal — retrieval simply uses the previous (or zero) scores.
            let _ = self.recompute_pagerank();
        }
        Ok(stats)
    }

    /// Compute PageRank over the lattice reference graph and persist scores for every node.
    ///
    /// Algorithm: iterative power method, damping factor 0.85, up to 20 iterations or until
    /// the L1 norm of the update is < 1e-6 (convergence). The graph is built from `lattice_ref`
    /// (name-keyed edges) by joining to `lattice_node` to resolve names to node ids; nodes with
    /// no outgoing edges (dangling nodes) distribute their rank uniformly. Scores are normalized
    /// to sum to 1.0 before persisting so they're comparable across index sizes.
    pub fn recompute_pagerank(&self) -> Result<(), LatticeError> {
        use std::collections::HashMap;

        // Load all nodes and build an id→index map.
        let node_pairs = self.store.lattice_node_ids_and_names()?;
        if node_pairs.is_empty() {
            return Ok(());
        }
        let n = node_pairs.len();
        let id_to_idx: HashMap<&str, usize> = node_pairs
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.as_str(), i))
            .collect();
        // name → list of node indices (multiple nodes can share a name across files).
        let mut name_to_idxs: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, (_, name)) in node_pairs.iter().enumerate() {
            name_to_idxs.entry(name.as_str()).or_default().push(i);
        }

        // Build adjacency: out_edges[src_idx] = list of dst_idx (resolved from lattice_ref).
        let ref_edges = self.store.lattice_ref_edges()?;
        let mut out_edges: Vec<Vec<usize>> = vec![vec![]; n];
        for (src_id, dst_name) in &ref_edges {
            let Some(&src_idx) = id_to_idx.get(src_id.as_str()) else {
                continue;
            };
            if let Some(targets) = name_to_idxs.get(dst_name.as_str()) {
                for &dst_idx in targets {
                    if dst_idx != src_idx {
                        out_edges[src_idx].push(dst_idx);
                    }
                }
            }
        }

        const DAMPING: f64 = 0.85;
        const MAX_ITER: usize = 20;
        const CONVERGENCE: f64 = 1e-6;

        let uniform = 1.0 / n as f64;
        let mut rank = vec![uniform; n];
        let mut next = vec![0.0f64; n];

        for _ in 0..MAX_ITER {
            // Dangling rank: nodes with no out-edges contribute uniformly.
            let dangling: f64 = rank
                .iter()
                .enumerate()
                .filter(|(i, _)| out_edges[*i].is_empty())
                .map(|(_, r)| r)
                .sum::<f64>();

            for v in next.iter_mut() {
                *v = (1.0 - DAMPING) * uniform + DAMPING * dangling * uniform;
            }
            for (src, targets) in out_edges.iter().enumerate() {
                if targets.is_empty() {
                    continue;
                }
                let share = DAMPING * rank[src] / targets.len() as f64;
                for &dst in targets {
                    next[dst] += share;
                }
            }

            // Check convergence (L1 norm of delta).
            let delta: f64 = rank.iter().zip(&next).map(|(a, b)| (a - b).abs()).sum();
            rank.copy_from_slice(&next);
            for v in next.iter_mut() {
                *v = 0.0;
            }
            if delta < CONVERGENCE {
                break;
            }
        }

        // Normalize so scores sum to 1.0 (keeps values comparable across index sizes).
        let total: f64 = rank.iter().sum();
        if total > 0.0 {
            for r in rank.iter_mut() {
                *r /= total;
            }
        }

        let scores: Vec<(String, f64)> = node_pairs
            .iter()
            .enumerate()
            .map(|(i, (id, _))| (id.clone(), rank[i]))
            .collect();
        self.store.set_lattice_pageranks(&scores)?;
        Ok(())
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
                pagerank: 0.0,
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

    /// Store a node's embedding vector (semantic retrieval, code-intelligence.md §5.6). The vector
    /// is produced by an [`Embedder`]; this just persists it.
    pub fn set_embedding(&self, node_id: &str, vector: &[f32]) -> Result<(), LatticeError> {
        self.store.put_lattice_embedding(node_id, vector)?;
        Ok(())
    }

    /// How many nodes currently carry an embedding.
    pub fn embedding_count(&self) -> Result<i64, LatticeError> {
        Ok(self.store.lattice_embedding_count()?)
    }

    /// Compute + store embeddings for every node that lacks one, in batches (incremental: already-
    /// embedded nodes are skipped, so it's cheap to call after each `update`). Returns the count
    /// embedded. Off-path unless a backend is configured.
    pub async fn embed_pending(
        &self,
        embedder: &dyn Embedder,
        batch: usize,
    ) -> Result<usize, LatticeError> {
        let batch = batch.max(1);
        let mut total = 0;
        loop {
            let nodes = self.store.lattice_nodes_without_embedding(batch)?;
            if nodes.is_empty() {
                break;
            }
            let texts: Vec<String> = nodes.iter().map(embed_text).collect();
            let vecs = embedder.embed(&texts).await?;
            for (n, v) in nodes.iter().zip(vecs) {
                self.store.put_lattice_embedding(&n.id, &v)?;
            }
            let n = nodes.len();
            total += n;
            if n < batch {
                break;
            }
        }
        Ok(total)
    }

    /// Hybrid retrieval: the structural/lexical [`retrieve`](retrieve::retrieve) result, augmented
    /// with semantic neighbours of the prompt (embed the prompt → cosine-rank stored vectors) when
    /// embeddings exist. Degrades to pure structural when none are stored or the embedder errors.
    pub async fn retrieve_hybrid(
        &self,
        prompt: &str,
        token_budget: usize,
        bodies: Option<retrieve::BodyOpts>,
        embedder: &dyn Embedder,
    ) -> Result<InjectedContext, LatticeError> {
        let ctx = retrieve::retrieve(self, prompt, token_budget, bodies)?;
        if self.store.lattice_embedding_count()? == 0 {
            return Ok(ctx);
        }
        match embedder.embed(&[prompt.to_string()]).await {
            Ok(vecs) => match vecs.first() {
                Some(qv) => {
                    let semantic = self.rank_by_vector(qv, 8)?;
                    Ok(retrieve::merge_semantic(ctx, semantic, token_budget))
                }
                None => Ok(ctx),
            },
            // A backend hiccup must never break the turn — fall back to structural.
            Err(_) => Ok(ctx),
        }
    }

    /// Rank indexed nodes by cosine similarity to a query embedding, best first (semantic
    /// retrieval). Returns the top `limit` nodes that still exist. Empty when no embeddings are
    /// stored — the caller then falls back to structural/lexical retrieval (graceful degrade).
    pub fn rank_by_vector(
        &self,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<NodeHit>, LatticeError> {
        let mut scored: Vec<(f32, String)> = self
            .store
            .lattice_embeddings()?
            .into_iter()
            .map(|(id, v)| (cosine(query, &v), id))
            .collect();
        // Highest similarity first; NaN (zero-norm) sorts last.
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Greater));
        scored.truncate(limit);
        let mut rows = Vec::new();
        for (_, id) in scored {
            if let Some(row) = self.store.lattice_node_by_id(&id)? {
                rows.push(row);
            }
        }
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

    /// Canonicalized repo root this index covers (used to resolve a node's file for body slicing).
    pub fn repo_root(&self) -> &str {
        &self.repo_root
    }

    /// Retrieve a budgeted set of relevant code for `prompt` — the auto-injection payload.
    /// `bodies` enables source-body injection for the top hits (the big token-saving lever); pass
    /// `None` for signature-only (legacy) retrieval.
    pub fn retrieve(
        &self,
        prompt: &str,
        token_budget: usize,
        bodies: Option<retrieve::BodyOpts>,
    ) -> Result<InjectedContext, LatticeError> {
        retrieve::retrieve(self, prompt, token_budget, bodies)
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

    /// All nodes ranked by pagerank descending, capped at `limit`. Used by [`Lattice::map`] to
    /// select the most important symbols for the repo-map without needing to re-sort client-side.
    /// Pass `usize::MAX` to retrieve everything (the map applies its own token-budget cutoff).
    pub(crate) fn store_nodes_ranked(&self, limit: usize) -> Result<Vec<NodeHit>, LatticeError> {
        let rows = self.store.lattice_nodes_ranked(limit)?;
        self.rows_to_hits(rows)
    }

    /// Build a compact, token-budgeted repo-map: the most important definitions across the repo,
    /// grouped by file, ordered by source line within each file. Selection is by pagerank
    /// descending so high-centrality symbols (called by many others) appear first. The output is
    /// deterministic and safe to cache — same index + budget → same string.
    ///
    /// Returns a plain-text block ready to print or inject. Empty index → a hint to run update.
    pub fn map(&self, token_budget: usize) -> Result<String, LatticeError> {
        map::build_map(self, token_budget)
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
                span_start: r.span_start,
                span_end: r.span_end,
                pagerank: r.pagerank,
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

/// The text embedded for a node — its signature if known, else qualname, else `kind name`.
fn embed_text(n: &LatticeNodeRow) -> String {
    match (&n.signature, &n.qualname) {
        (Some(s), _) if !s.is_empty() => s.clone(),
        (_, Some(q)) if !q.is_empty() => format!("{} {q}", n.kind),
        _ => format!("{} {}", n.kind, n.name),
    }
}

/// Cosine similarity of two vectors in `[-1, 1]`; `0.0` if lengths differ or either is zero-norm
/// (so an all-zero or mismatched vector ranks last rather than NaN-poisoning the sort).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
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

    struct FakeEmbedder;
    #[async_trait::async_trait]
    impl Embedder for FakeEmbedder {
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, LatticeError> {
            // Deterministic 3-dim vector from byte sums per position — enough to exercise the
            // storage + ranking pipeline without a real model.
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = [0f32; 3];
                    for (i, b) in t.bytes().enumerate() {
                        v[i % 3] += b as f32;
                    }
                    v.to_vec()
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn embed_pending_embeds_every_node_then_is_a_noop() {
        let t = Tmp::new();
        t.write("src/lib.rs", "pub fn one() {}\npub fn two() {}\n");
        let lat = lattice(&t.root);
        lat.update().unwrap();
        let n = lat.embed_pending(&FakeEmbedder, 50).await.unwrap();
        assert!(n >= 2, "embedded both functions: {n}");
        assert_eq!(lat.embedding_count().unwrap(), n as i64);
        // Incremental: nothing left to embed on a second pass.
        assert_eq!(lat.embed_pending(&FakeEmbedder, 50).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn retrieve_hybrid_degrades_without_embeddings_then_augments() {
        let t = Tmp::new();
        t.write("src/lib.rs", "pub fn parse_tokens() {}\n");
        let lat = lattice(&t.root);
        lat.update().unwrap();

        // No embeddings yet → identical to structural retrieve.
        let structural = lat.retrieve("parse_tokens", 500, None).unwrap();
        let hybrid = lat
            .retrieve_hybrid("parse_tokens", 500, None, &FakeEmbedder)
            .await
            .unwrap();
        assert_eq!(
            hybrid.snippets, structural.snippets,
            "degrades to structural"
        );

        // After embedding, hybrid still returns the structural hit (and never fewer).
        lat.embed_pending(&FakeEmbedder, 50).await.unwrap();
        let hybrid2 = lat
            .retrieve_hybrid("parse_tokens", 500, None, &FakeEmbedder)
            .await
            .unwrap();
        assert!(hybrid2.snippets.len() >= structural.snippets.len());
        assert!(hybrid2
            .snippets
            .iter()
            .any(|s| s.text.contains("parse_tokens")));
    }

    #[test]
    fn cosine_similarity_basics() {
        assert!(
            (cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6,
            "identical = 1"
        );
        assert!(
            cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6,
            "orthogonal = 0"
        );
        assert!(
            (cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6,
            "opposite = -1"
        );
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "length mismatch = 0");
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "zero-norm = 0");
    }

    #[test]
    fn rank_by_vector_orders_nodes_by_cosine() {
        let t = Tmp::new();
        t.write(
            "src/lib.rs",
            "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n",
        );
        let store = Arc::new(Store::open_in_memory().unwrap());
        let lat = Lattice::new(Arc::clone(&store), &t.root);
        lat.update().unwrap();

        // Assign distinct unit vectors per node, then query near `beta`'s.
        for n in store.lattice_nodes_by_name("", 100).unwrap() {
            let v = match n.name.as_str() {
                "alpha" => [1.0, 0.0, 0.0],
                "beta" => [0.0, 1.0, 0.0],
                _ => [0.0, 0.0, 1.0],
            };
            lat.set_embedding(&n.id, &v).unwrap();
        }
        assert_eq!(lat.embedding_count().unwrap(), 3);

        let ranked = lat.rank_by_vector(&[0.1, 0.9, 0.0], 2).unwrap();
        assert_eq!(ranked.len(), 2, "top-2 only");
        assert_eq!(ranked[0].name, "beta", "nearest vector ranks first");

        // Graceful degrade: a fresh index with no embeddings returns nothing.
        let t2 = Tmp::new();
        t2.write("src/x.rs", "pub fn z() {}\n");
        let empty = lattice(&t2.root);
        empty.update().unwrap();
        assert!(empty.rank_by_vector(&[1.0, 0.0], 5).unwrap().is_empty());
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

    /// PageRank assigns a higher score to a symbol referenced by many others than to an
    /// unreferenced leaf. We build a small graph: `hub` is called by `a`, `b`, and `c`
    /// (three separate symbols). `leaf` is never referenced. After `recompute_pagerank`,
    /// hub's score must exceed leaf's.
    #[test]
    fn pagerank_hub_scores_higher_than_leaf() {
        let t = Tmp::new();
        // hub is called by three callers; leaf is never referenced.
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
        lat.update().unwrap(); // recompute_pagerank is called inside update when files_indexed > 0

        let hub_hits = lat.query("hub", 1).unwrap();
        let leaf_hits = lat.query("leaf", 1).unwrap();
        assert_eq!(hub_hits.len(), 1, "hub should be indexed");
        assert_eq!(leaf_hits.len(), 1, "leaf should be indexed");
        assert!(
            hub_hits[0].pagerank > leaf_hits[0].pagerank,
            "hub (referenced 3×) must outrank leaf (0 references): hub={} leaf={}",
            hub_hits[0].pagerank,
            leaf_hits[0].pagerank
        );
    }

    /// Among two equal-name-match candidates, the one with higher pagerank is ordered first
    /// in retrieve output. We index two files each defining `parse_target` (snake_case →
    /// symbol-shaped so the retrieve strong-path fires), give one a higher pagerank via direct
    /// store write, then verify retrieve orders it first.
    #[test]
    fn retrieve_orders_higher_pagerank_first_among_equal_name_matches() {
        use crate::retrieve;
        let t = Tmp::new();
        // Two separate files each define a symbol-shaped function name.
        t.write("src/a.rs", "pub fn parse_target() {}");
        t.write("src/b.rs", "pub fn parse_target() {}");
        let store = std::sync::Arc::new(forge_store::Store::open_in_memory().unwrap());
        let lat = Lattice::new(std::sync::Arc::clone(&store), &t.root);
        lat.update().unwrap();

        // Find the two `parse_target` nodes and assign distinct pagerank scores directly.
        let nodes = store.lattice_nodes_by_name("parse_target", 10).unwrap();
        assert_eq!(
            nodes.len(),
            2,
            "both parse_target definitions must be indexed"
        );
        let high_id = nodes[0].id.clone();
        let low_id = nodes[1].id.clone();
        store
            .set_lattice_pageranks(&[(high_id.clone(), 0.9), (low_id.clone(), 0.1)])
            .unwrap();

        // `parse_target` is snake_case → symbol-shaped → strong path fires → snippets injected.
        let ctx = retrieve::retrieve(&lat, "parse_target", 2000, None).unwrap();
        assert_eq!(ctx.snippets.len(), 2, "both definitions must be retrieved");

        // Identify which file the high-pagerank node lives in. It must be first.
        let high_node = store.lattice_node_by_id(&high_id).unwrap().unwrap();
        let high_path = store
            .lattice_file_path(&high_node.file_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            ctx.snippets[0].rel_path, high_path,
            "higher-pagerank parse_target must be ordered first"
        );
    }
}
