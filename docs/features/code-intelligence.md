# Feature: Lattice — built-in code intelligence & persistent semantic memory

> **Status (PR2 shipped):** the `forge-index` crate now extracts **10 languages** (Rust, Python,
> JavaScript, TypeScript+TSX, Go, Java, C, C++, Ruby) via tree-sitter **tags queries** — one
> uniform path, adding a language is one registry row (ADR-0010). Definitions → symbol nodes;
> references → a name-keyed `lattice_ref` table; `contains` from span-nesting. Persisted into the
> **shared** `forge-store` SQLite db, **incremental by SHA-256 content hash**. **The killer step
> is live:** `retrieve_context()` auto-injects budget-bounded relevant code into `run_turn`
> (after routing, before the first provider call), scaled by `BudgetStatus`; the agent's edits
> reindex the touched file in-turn; a model-callable `lattice` tool (ReadOnly) and a
> `ContextInjected` UI event are wired. A **`notify`-based background watcher** reindexes files
> on external editor edits (debounced) so retrieval stays fresh without a manual update. A git
> blame overlay answers `why <symbol>` (last author/date/commit/subject for the symbol's line).
> An in-chat **`/lattice <symbol>`** command renders the scoped subgraph (matching defs + blast
> radius + `why`) as a styled tree in the TUI. CLI: `forge lattice update|query|impact|path|why|status`.
> `LatticeConfig { enabled, inject, inject_token_budget, watch }`.
>
> **Verified:** the multi-language matrix compiles + links (10 grammars on tree-sitter core 0.24);
> tags extraction, cross-file `impact`, end-to-end injection, watcher auto-reindex, `why`
> provenance, and the `/lattice` view have tests (live-verified on this repo, incl. a pty TUI
> smoke). **Embeddings (§5.6): shipped (off by default)** — `lattice_embedding` storage, `cosine`,
> `rank_by_vector`, the `Embedder` trait + `OllamaEmbedder`, `embed_pending` (incremental), and
> `retrieve_hybrid` (blends semantic neighbours into structural retrieval, degrades on backend
> error); wired into `run_turn` when enabled, plus `forge lattice embed` and **automatic
> indexing + embedding in the background on session start** (no manual `forge lattice update`).
> Unit-tested with a fake embedder + hand-crafted vectors; real semantic quality needs a live
> backend (`ollama pull nomic-embed-text`). **Not yet built:** cross-repo identity; C# (its
> 0.24-compatible grammar ships no tags query). The sections below are the full design.

> A native, zero-setup code-intelligence subsystem for Forge: a pure-Rust, tree-sitter
> AST/dependency graph plus optional semantic retrieval, stored in SQLite **alongside**
> `forge-store`, kept always-fresh by an incremental file watcher and by every edit the
> agent makes — and, critically, **queried automatically inside the agent loop**
> (`Session::run_turn`, `crates/forge-core/src/lib.rs:170`) to retrieve and inject the most
> relevant symbols/files/call-chains for the current task. Spans a new `forge-index` crate
> (parsers, graph, retrieval, watcher), the `forge-store` SQLite schema (graph + embedding
> tables), `forge-core` (the retrieval/injection step + post-edit reindex), `forge-tools`
> (a `lattice` tool the model can call directly), `forge-cli` (a `forge lattice` command
> family), `forge-tui` (scoped-subgraph + blast-radius rendering), `forge-mesh` (the
> injection token budget is tied to the live budget), and `forge-config` (a `[lattice]` block).
>
> Status: design. **This feature consolidates and supersedes four Wave-4 roadmap items**
> (`docs/roadmap.md:114-122`): "persistent semantic code memory", "AI archaeology",
> "git-native context", and "cross-repo intelligence" — they become facets of one subsystem.
>
> **Why "Lattice".** A metal's crystal *lattice* is the repeating structural network that
> gives it its properties; "lattice" also literally means a graph/network. The code graph is
> Forge's lattice: the structure beneath the source. On-brand with Forge's metallurgy theme,
> and it reads naturally as a verb-object: `forge lattice query`, `forge lattice impact`.

## 1. Problem (JTBD)

> When I ask the agent to do something in a real codebase — "add a field to `Session`",
> "why does `inject_provider_keys` exist", "what breaks if I change `Router::route`" — I want
> it to **already know the structure of my code**: which symbols call which, what an edit will
> ripple into, who last touched a line and why, and which files are relevant to *this* task —
> **without me hand-picking files, without a separate tool, without re-indexing by hand, and
> without paying an LLM to grep.** I want that knowledge to be **built into Forge**, fast,
> persistent across sessions, and used **automatically** to feed the model the right context.

Today Forge's agent loop is **context-blind**. `Session::run_turn`
(`crates/forge-core/src/lib.rs:170`) builds the model request from exactly the transcript
(`self.transcript`) plus the tool *specs* (`tool_specs()`, `:157`). The only way the model
learns about the codebase is by *spending tokens* calling `read_file` / `search`
(`crates/forge-tools/src/core_tools.rs`) one file at a time, re-discovering structure on every
turn. There is no persistent model of the code, no notion of "relevant files for this task",
no impact analysis, and no history overlay. Every session starts from zero.

**The external baseline we are beating: graphify.** This repo is currently indexed by an
*external* tool, **graphify** (see `graphify-out/`: `graph.json`, `GRAPH_REPORT.md`, `wiki/`).
It does AST extraction into a knowledge graph (nodes = files/symbols, edges = calls/imports),
detects communities + "god nodes", generates a wiki, and exposes `graphify query/explain/path`
and a manual `graphify update .`. Forge's owner wires it into *his* Claude Code via **hooks**
that inject graph context on read/grep (you can see those hook reminders firing in this very
session). It is genuinely useful — and it exposes exactly the gaps Forge should close:

1. **External process + separate install** — not part of the harness; another thing to run.
2. **Context injection is via brittle hooks**, bolted onto tool calls, not native to the loop.
3. **Updates are a manual `graphify update`** re-extraction step; the graph goes stale silently
   (its own report says "Built from commit `ff89cc7f` … run `git rev-parse HEAD` to check if
   the graph is stale", `graphify-out/GRAPH_REPORT.md:13-14`).
4. **Structural-only** — no semantic/embedding retrieval; you must know the symbol name.
5. **No impact / blast-radius analysis** — it can't tell you what an edit breaks.
6. **The agent doesn't decide to use it** — a human or a hook injects context; the *model*
   never asks the index a question.

**Who's affected.** Every Forge user on a non-trivial codebase (better answers, fewer wasted
read/search round trips, lower cost); the **agent itself** (it gains structural awareness and a
`lattice` tool); and the Model Mesh (cheaper turns — retrieved context lets a *cheaper* tier
answer a question that would otherwise need a frontier model to go spelunking).

**Why this is on-brand and a differentiator.** Forge *already embeds SQLite* (`forge-store`,
ADR-0005) — the storage substrate exists. Forge is pure-Rust + Tokio with an "instant startup"
ethos; tree-sitter compiles in, queries are sub-millisecond, the footprint is tiny. Competitors
either don't have persistent code memory or bolt it on as a cloud service. Lattice is **native,
local, free, automatic, and always fresh** — graphify's value with none of its seams.

## 2. Scope (MoSCoW)

**Must have**
- A new **`forge-index` crate**: tree-sitter parsers compiled in (Rust first; the parser set is
  pluggable), an extractor producing **Nodes** (file / module / function / method / struct /
  enum / trait / impl / const) and **Edges** (defines / calls / imports / impls / references /
  contains), and a `LatticeStore` persisting them via `forge-store`'s SQLite connection.
- **Persistent graph in SQLite alongside `forge-store`** — new tables (`lattice_node`,
  `lattice_edge`, `lattice_file`, with `lattice_embedding` reserved) added to
  `crates/forge-store/src/schema.rs:4`. Survives across sessions; gets richer over time.
- **Incremental, content-hash freshness.** Per-file SHA-256 in `lattice_file`; on index/update
  only files whose hash changed are re-parsed and their nodes/edges replaced. No full re-extract.
- **The killer step — automatic retrieval + injection in the agent loop.** A new
  `retrieve_context()` runs inside `Session::run_turn` *after* routing and *before* the first
  provider call (`crates/forge-core/src/lib.rs:203`), querying the index for the symbols/files/
  call-chains most relevant to the prompt and injecting them as a budgeted system message.
  This **replaces graphify's hooks** with a native, ranked retrieval step the agent owns.
- **Post-edit reindex.** When the agent's `write_file`/`edit_file` tools succeed
  (`invoke_tool`, `crates/forge-core/src/lib.rs:292`), the touched file is re-parsed
  immediately so the index is fresh *within the same turn*.
- **Structural queries**: `query` (relevant nodes for a string), `path` (relationship between
  two symbols), `explain` (focused subgraph for a concept) — graphify parity, native.
- **Impact / blast-radius** (`impact <symbol>`): reverse-call + dependents closure — "what
  breaks if I change X". Graphify cannot do this. Refs are name-keyed (no cross-crate binding), so
  a symbol that exists in several crates mixes their references; narrow with
  `impact <symbol> --scope <path-prefix>` (e.g. `crates/forge-core`) to confine the blast radius to
  one crate/dir.
- **`forge lattice` command family**: `query`, `explain`, `path`, `impact`, `why`, `update`,
  `status` — added to the clap `Command` enum (`crates/forge-cli/src/main.rs:29`).
- **A `lattice` tool** (`forge-tools`) the model can call to ask the index directly — a
  `ReadOnly` side effect (never prompts), registered in `with_core_tools`
  (`crates/forge-tools/src/lib.rs:49`).
- **Graceful degradation**: unsupported languages, binary/vendored/generated files are skipped
  cleanly; an empty or absent index never breaks a turn (retrieval returns nothing, loop runs
  as today). No network, no API key required for the structural core.

**Should have**
- **Incremental file watcher** (debounced, `notify` crate) so the index stays fresh on
  *external* edits (your editor), not just agent edits — opt-in background task in `forge chat`.
- **History / archaeology overlay** (`why <symbol|file:line>`): join graph nodes with
  `git blame`/`git log` to answer "who last changed this, when, in which commit, with what
  message" — decision provenance. Consolidates the "AI archaeology" roadmap item.
- **Git-native auto-scoping**: a retrieval mode that biases toward files changed since the last
  commit / on the current branch (`git diff --name-only`), so "the current task" auto-focuses on
  in-flight work. Consolidates the "git-native context" roadmap item.
- **Optional semantic retrieval (embeddings), BYOK/local, lazy.** Hybrid structural+semantic
  ranking for natural-language queries. Embeddings are **off by default**, computed lazily, and
  use either a local model (e.g. a small ONNX/`fastembed` model) or a provider embeddings
  endpoint under explicit consent. Degrades to structural-only if disabled/unavailable.
- **TUI rendering** of a scoped subgraph and an impact set in the live region.

**Could have**
- **Cross-repo intelligence**: index multiple roots; a `repo_root` identity dimension on every
  node so symbols are addressable across repos and shared patterns are queryable. Consolidates
  the "cross-repo" roadmap item.
- **God-node / community detection** (graphify-style centrality + clustering) over the native
  graph, surfaced in `forge lattice status`.
- **A persisted, navigable wiki/index** generated from the graph (graphify's `wiki/` parity).
- **Embedding cache invalidation by content hash** so only changed symbols are re-embedded.

**Won't have (this iteration)**
- A language server / LSP replacement (we do not do type inference, completion, or rename).
  Lattice is a *retrieval and impact* index, not a compiler front-end.
- Full semantic understanding of every language — coverage grows by adding tree-sitter grammars.
- Cloud-hosted or shared-team index; everything is local to the user's machine in v1.
- Sending code to an embedding provider **without explicit opt-in** (privacy non-negotiable).
- Replacing `read_file`/`search` — Lattice *augments* them; the model can still read raw files.

### Non-goals
- Lattice does **not** change behaviour when the index is empty/absent: `run_turn` with no
  retrievable context runs exactly as today (additive, like every other Forge feature).
- It does **not** introduce a new provider or routing algorithm; it reuses `Provider`, `Router`,
  `Store`, `Tool`, `Presenter` exactly as they exist.
- It is **not** a plugin; the index is native to the core, like the permission broker.

## 3. Acceptance criteria (Given / When / Then)

```
# Build the index (zero setup, no API key, no external process)
Given a Rust workspace and a fresh Forge install (structural core only, embeddings off)
When I run `forge lattice update`
Then every supported source file is parsed by the compiled-in tree-sitter grammar
And lattice_node / lattice_edge / lattice_file rows are written to the SAME SQLite db as sessions
And lattice_file records each file's content hash + language + parse status
And no network call is made and no API key is required
And `forge lattice status` reports node/edge/file counts and "embeddings: disabled"

# Incremental freshness (content hash — only re-parse what changed)
Given an existing index built from the current tree
When one file changes and I run `forge lattice update`
Then only that file is re-parsed (its hash differs); all others are skipped (hash match)
And its old nodes/edges are replaced atomically; the rest of the graph is untouched
And the update completes in well under the full-build time

# The killer path — automatic retrieval + injection in a turn
Given an indexed repo and the prompt "add a `depth` field to Session and thread it through start"
When Session::run_turn executes (after routing, before the first provider call)
Then retrieve_context() queries the index and returns a ranked, budgeted set:
     the `Session` struct node, its definition span, its callers (start/resume/build), the file
And those are injected as a single system message ("Relevant code (Lattice): …") within the
     configured context-injection token budget
And the model answers using that context WITHOUT first spending turns on read_file/search
And a PresenterEvent::ContextInjected{symbols, files, tokens} is emitted so the UI can show it

# Post-edit reindex within the same turn
Given the agent calls edit_file on crates/forge-core/src/lib.rs and it succeeds
When invoke_tool returns the successful result
Then that file is re-parsed and its nodes/edges are updated before the next loop step
And a subsequent retrieve_context() in the same turn reflects the edit (e.g. the new field)

# Impact / blast-radius (graphify cannot do this)
Given an indexed repo
When I run `forge lattice impact "Router::route"`
Then Lattice computes the reverse-call + dependents closure (callers, transitive callers, the
     trait + its impls, and files containing them)
And prints the blast radius as a ranked set with a count ("12 sites across 4 files")
And the same set is available to the agent before it edits that symbol (proactive warning)

# Natural-language query degrades gracefully when embeddings are off
Given embeddings are disabled (default)
When I run `forge lattice query "where do we resolve API keys"`
Then Lattice answers with structural + lexical ranking (symbol/file name + identifier match),
     returning api_key()/inject_provider_keys()/env_var_for() in forge-config
And it never errors for lack of an embedding backend; it notes "(structural ranking; embeddings off)"

# Semantic query with consent (opt-in, BYOK/local)
Given `[lattice] embeddings = "local"` (or "provider") and the user has consented
When I run `forge lattice query "where do we resolve API keys"`
Then symbol/doc embeddings are computed lazily (cached by content hash) and hybrid-ranked with structure
And results improve for paraphrased queries; the consent + backend are shown in `lattice status`

# Privacy — never embed code without consent under BYOK
Given embeddings = "provider" but no explicit consent flag is set
When any path would send source text to an embedding endpoint
Then Lattice refuses, emits a one-time prompt/notice, and falls back to structural-only
And no code leaves the machine

# History / archaeology overlay
Given an indexed repo inside a git work tree
When I run `forge lattice why "inject_provider_keys"`
Then Lattice resolves the symbol's file+span, runs git blame/log over it, and reports the last
     author, date, commit hash, and subject (decision provenance)
And if the path is not under git, it says so and returns the structural node only

# Stale index after an external git operation (checkout/rebase/pull)
Given an index built at commit A and a `git checkout B` that changed 30 files
When the next `forge lattice query` / turn runs
Then Lattice detects drift (HEAD changed and/or file hashes differ) and re-parses only the
     changed files lazily before answering; it never serves results from deleted/old nodes
And `forge lattice status` shows the indexed HEAD vs the current HEAD

# Huge monorepo (bounded work, never blocks the turn)
Given a 50k-file monorepo
When retrieve_context() runs inside a turn
Then retrieval is bounded by a node-visit cap and a wall-clock budget (default ~15ms), returning
     the best-so-far ranked set; it never blocks the provider call
And `forge lattice update` honours include/exclude globs (vendored/generated dirs excluded)

# Multi-repo identity (no collisions)
Given two indexed repos that both define a `Config` struct
When I query across them (cross-repo enabled)
Then each node's SymbolId is namespaced by repo_root, so the two `Config`s are distinct
And results label which repo each hit comes from

# Empty / absent index never breaks a turn (additive guarantee)
Given a brand-new repo with no index yet
When Session::run_turn executes
Then retrieve_context() returns an empty set, no context is injected, and the turn proceeds
     exactly as it does today (no error, no behaviour change)
```

## 4. Impact analysis & insertion points

Additive and trait-respecting. The one genuinely new thing is the `forge-index` crate and the
single retrieval call wired into `run_turn`; everything else reuses existing seams.

| Layer | Insertion point | Change |
|-------|-----------------|--------|
| **new `forge-index`** | `crates/forge-index/` | The subsystem: `parser` (tree-sitter grammars, compiled in), `extract` (AST → Nodes/Edges), `graph` (`Lattice` query/path/impact over the in-SQLite graph), `retrieve` (ranking + budgeting), `watch` (debounced `notify` watcher), `embed` (optional, lazy, feature-gated), `git` (blame/log overlay + drift detection). Depends on `forge-types`, `forge-store`, `forge-config`. Leaf-ish: nothing in the workspace depends on it except `forge-core`, `forge-tools`, `forge-cli`. |
| `forge-store` | `crates/forge-store/src/schema.rs:4` (`SCHEMA`) | Add `lattice_node`, `lattice_edge`, `lattice_file`, `lattice_embedding` tables + indexes (§5.2). Same connection/mutex (`crates/forge-store/src/lib.rs:24`), same WAL pragmas (`:40`), same idempotent batch — graph and sessions share one db file. |
| `forge-store` | `crates/forge-store/src/lib.rs` | New methods: `upsert_file`, `replace_file_nodes`, `nodes_by_name`, `edges_from/to`, `node_by_id`, `file_hash`, `indexed_head`. Reuse the existing `lock()` (`:56`). (Or expose the `Connection` to `forge-index` via a thin accessor — see §5.7 wiring seam.) |
| `forge-core` | `Session` struct (`crates/forge-core/src/lib.rs:33`) | Add `lattice: Option<Lattice>` (None when disabled). Constructed in `build()` (`:111`) from the shared `Store` + config. |
| `forge-core` | `run_turn`, after routing/before the loop (`crates/forge-core/src/lib.rs:203`, between `tool_specs()` and the `for step` loop) | Insert `let injected = self.retrieve_context(prompt, &budget)?;` and, if non-empty, push a `Role::System` message and `emit(PresenterEvent::ContextInjected{…})`. The injection token budget is derived from the live `BudgetState` (§5.4). |
| `forge-core` | `invoke_tool` success path (`crates/forge-core/src/lib.rs:324-331`) | After a `write_file`/`edit_file` tool returns `ok`, call `self.lattice.reindex_path(path)` so the index is fresh within the turn. Behind `if let Some(lat) = &self.lattice`. |
| `forge-tools` | new `crates/forge-tools/src/lattice_tool.rs`; register in `with_core_tools` (`crates/forge-tools/src/lib.rs:49`) | A `LatticeTool` implementing `Tool` (`crates/forge-tools/src/lib.rs:28`), `side_effect()` → `ReadOnly` (never prompts). Holds an `Arc<Lattice>` injected at registry build (same wiring seam pattern noted for the subagent `task` tool). Sub-commands via args: `{op:"query"|"impact"|"path"|"why", …}`. |
| `forge-cli` | `Command` enum (`crates/forge-cli/src/main.rs:29`) + dispatch (`:98`) | New `Command::Lattice { #[command(subcommand)] op }` with `Query/Explain/Path/Impact/Why/Update/Status`. A `lattice()` handler opens the `Store`, builds a `Lattice`, runs the op, prints via the same presenter style. |
| `forge-tui` | `PresenterEvent` (`crates/forge-tui/src/lib.rs:19`) | Add `ContextInjected { symbols: usize, files: usize, tokens: usize }` and `LatticeResult { … }`. `HeadlessPresenter::emit` (`:83`) renders a one-liner (`⌬ lattice → injected N symbols / M files (~K tok)`); the interactive `TuiPresenter`/`App` render the scoped subgraph + impact view (§5.5). |
| `forge-mesh` | `BudgetState` (`crates/forge-mesh/src/lib.rs:13`) | **No signature change.** Lattice reads the same `BudgetState` `run_turn` already builds (`crates/forge-core/src/lib.rs:172`) to size the injection budget down under `Warning`/`Exhausted`. Cheaper context → cheaper turns. |
| `forge-config` | `Config` (`crates/forge-config/src/lib.rs:31`) | New `lattice: LatticeConfig` block: `enabled` (default true for structural), `embeddings` (`off`/`local`/`provider`, default `off`), `inject_token_budget` (default ~1500), `retrieval_time_budget_ms` (default 15), `include`/`exclude` globs, `repos` (cross-repo roots), `auto_scope_git` (bool). Defaults in `Config::default()` (`:57`). |

**Risk notes.**
- *Tree-sitter binary size.* Each grammar adds to the binary. Mitigation: ship **Rust** in v1;
  gate additional grammars behind Cargo features (`lang-python`, `lang-ts`, …) so users compile
  in only what they need. This keeps the default footprint small (Rust ethos).
- *Shared `Store` access from `forge-index`.* The `Lattice` needs the same SQLite connection.
  Resolution (§5.7): `forge-index` takes an `Arc<Store>` (the store is already `Sync` via its
  internal `Mutex`, `crates/forge-store/src/lib.rs:24`) and graph methods live on `Store`, or a
  thin `with_conn()` accessor. No second database, no connection pool surprises.
- *Retrieval must never block the turn.* The retrieval step is bounded by a wall-clock budget
  and a node-visit cap; it returns best-so-far. A failure/timeout degrades to "no injection".

## 5. Technical design

### 5.1 Vertical slice — one auto-retrieval turn

Prompt: `"add a depth field to Session and thread it through start"` in the indexed Forge repo.

```
forge run "add a depth field to Session and thread it through start"
  │
  ▼ Session::run_turn(prompt)                        crates/forge-core/src/lib.rs:170
  ├─ 1. budget = BudgetState{ spent, daily_cap }     :172   (unchanged)
  ├─ 2. decision = router.route(prompt, budget)      :190   → [complex] anthropic::claude-opus-4-8
  ├─ 3. persist user message                         :198   (unchanged)
  │
  ├─ 4. ★ injected = self.retrieve_context(prompt, &budget)?      ◀── NEW (the killer step)
  │        a. Lattice::query(prompt):                            forge-index::retrieve
  │           - lexical/identifier match on "Session", "start", "depth", "thread"
  │           - structural expansion: node Session(struct) → contains(fields), defined_in lib.rs,
  │             reverse `calls`/`references` → start(), resume(), build() (callers/definers)
  │           - (if embeddings on) hybrid re-rank by semantic similarity
  │        b. budget_tokens = inject_budget(config, budget.status())   // shrinks if Warning/Exhausted
  │        c. pack top-ranked spans + signatures until budget_tokens   // never exceeds the window
  │        d. returns InjectedContext{ symbols:[Session, start, build], files:[lib.rs], ~1.2k tok }
  │     if non-empty:
  │        self.transcript.push(Message::system("Relevant code (Lattice):\n …spans…"))
  │        presenter.emit(PresenterEvent::ContextInjected{ symbols:3, files:1, tokens:1187 })
  │
  ├─ 5. specs = self.tool_specs()                    :203   (unchanged)
  ├─ 6. for step in 0..MAX_STEPS:                    :207
  │        provider.complete(model, &transcript /*now incl. injected ctx*/, &specs, …)   :213
  │        ─ model already SEES the Session struct + callers → proposes edits directly,
  │          NOT a sequence of read_file/search round trips (cost + latency saved)
  │        ─ model calls edit_file{ path:"crates/forge-core/src/lib.rs", … }
  │            invoke_tool → permission broker (Write) → tool.run → ok                    :324
  │            ★ self.lattice.reindex_path("…/lib.rs")  ◀── NEW: re-parse touched file    :331
  │        ─ next step: model finalises; resp.wants_tools()==false → final_text           :260
  │
  └─ 7. emit Cost + Done                             :282   (unchanged)
```

Two things to notice: the **only** new control-flow is step 4 (a single call) and the reindex
in step 6; and the injected context lets a turn that *would* have burned several `read_file`
calls (each a model↔tool round trip, each priced) resolve in fewer steps — directly helping the
Model Mesh keep turns cheap.

### 5.2 Data model (SQLite, in `crates/forge-store/src/schema.rs:4`)

```sql
-- One row per indexed source file.
CREATE TABLE IF NOT EXISTS lattice_file (
    id          TEXT PRIMARY KEY,         -- stable: hash(repo_root || rel_path)
    repo_root   TEXT NOT NULL,            -- multi-repo identity dimension
    rel_path    TEXT NOT NULL,
    lang        TEXT NOT NULL,            -- "rust" | "python" | … | "unsupported"
    content_hash TEXT NOT NULL,           -- SHA-256; incremental-update key
    parse_status TEXT NOT NULL,           -- "ok" | "skipped" | "error"
    indexed_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE(repo_root, rel_path)
);

-- One row per symbol/definition.
CREATE TABLE IF NOT EXISTS lattice_node (
    id          TEXT PRIMARY KEY,         -- SymbolId (see §5.3); namespaced by repo_root
    file_id     TEXT NOT NULL REFERENCES lattice_file(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,            -- file|module|function|method|struct|enum|trait|impl|const
    name        TEXT NOT NULL,            -- "Session", "run_turn", …
    qualname    TEXT,                     -- "forge_core::Session::run_turn"
    signature   TEXT,                     -- one-line signature for cheap injection
    span_start  INTEGER NOT NULL,         -- byte offsets into the file
    span_end    INTEGER NOT NULL,
    line_start  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lnode_name ON lattice_node(name);
CREATE INDEX IF NOT EXISTS idx_lnode_file ON lattice_node(file_id);

-- One row per relationship.
CREATE TABLE IF NOT EXISTS lattice_edge (
    id        TEXT PRIMARY KEY,
    src_id    TEXT NOT NULL REFERENCES lattice_node(id) ON DELETE CASCADE,
    dst_id    TEXT NOT NULL REFERENCES lattice_node(id) ON DELETE CASCADE,
    kind      TEXT NOT NULL,              -- defines|calls|imports|impls|references|contains
    unresolved_name TEXT                  -- set when dst is a not-yet-resolved reference
);
CREATE INDEX IF NOT EXISTS idx_ledge_src ON lattice_edge(src_id, kind);
CREATE INDEX IF NOT EXISTS idx_ledge_dst ON lattice_edge(dst_id, kind);  -- powers reverse/impact

-- Optional, lazy, feature-gated. Empty unless embeddings are enabled + consented.
CREATE TABLE IF NOT EXISTS lattice_embedding (
    node_id      TEXT PRIMARY KEY REFERENCES lattice_node(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL,           -- re-embed only when the symbol's text changes
    dim          INTEGER NOT NULL,
    vector       BLOB NOT NULL            -- f32 little-endian; brute-force cosine in v1
);
```

Reverse lookups (`idx_ledge_dst`) make **impact** a cheap recursive query: start at the target
node, walk `calls`/`references`/`impls` edges *backward* to find callers/dependents.

### 5.3 Key types (`forge-index`)

```rust
/// Stable, repo-namespaced identity for a symbol. Survives re-parses of unchanged code,
/// and never collides across repos (multi-repo identity, §edge cases).
pub struct SymbolId(String);              // e.g. blake3(repo_root | rel_path | kind | qualname)

pub enum NodeKind { File, Module, Function, Method, Struct, Enum, Trait, Impl, Const }
pub enum EdgeKind { Defines, Calls, Imports, Impls, References, Contains }

pub struct Node { pub id: SymbolId, pub kind: NodeKind, pub name: String,
                  pub qualname: Option<String>, pub signature: Option<String>,
                  pub file: RepoPath, pub line: u32 }

pub struct Lattice { store: Arc<Store>, cfg: LatticeConfig, embed: Option<Embedder> }

impl Lattice {
    pub fn update(&self, roots: &[RepoRoot]) -> Result<UpdateStats>;   // incremental, hash-gated
    pub fn reindex_path(&self, path: &Path) -> Result<()>;            // single file (post-edit)
    pub fn query(&self, q: &str, budget_tokens: usize) -> InjectedContext;
    pub fn path(&self, a: &str, b: &str) -> Option<Vec<Edge>>;
    pub fn impact(&self, symbol: &str) -> BlastRadius;                // reverse closure
    pub fn explain(&self, concept: &str) -> Subgraph;
    pub fn why(&self, target: &str) -> Provenance;                    // git blame/log overlay
    pub fn status(&self) -> IndexStatus;                             // counts, HEAD, drift, embeddings
}

pub struct InjectedContext { pub nodes: Vec<Node>, pub snippets: Vec<(RepoPath, String)>,
                             pub est_tokens: usize }
pub struct BlastRadius { pub root: Node, pub dependents: Vec<Node>, pub files: Vec<RepoPath>,
                         pub total_sites: usize }
```

### 5.4 Retrieval ranking + the injection token budget (Mesh-tied)

`query()` produces a ranked candidate set, then packs it to a token budget:

1. **Seed (lexical/structural).** Extract identifiers/quoted symbols from the prompt; match on
   `lattice_node.name`/`qualname` and file names (cheap, indexed). If `auto_scope_git`, boost
   nodes in files from `git diff --name-only` (the in-flight work).
2. **Expand (graph).** From each seed, pull 1-hop neighbours along `contains`/`defines`/`calls`/
   `impls` so the model sees the symbol *and* its definers/callers (the thing graphify's hooks
   approximate, but here it's intrinsic and ranked).
3. **Re-rank (optional semantic).** If embeddings are on, blend cosine similarity of the prompt
   embedding against node embeddings: `score = α·structural + β·lexical + γ·semantic`. With
   embeddings off, `γ=0` and ranking is structural+lexical (the graceful-degradation path).
4. **Budget-pack.** `inject_budget(cfg, status)` = `cfg.inject_token_budget` scaled by
   `BudgetStatus` (`crates/forge-mesh/src/lib.rs:34`): full at `Ok`, halved at `Warning`, a hard
   floor at `Exhausted`. Pack signatures first (cheap, high-value), then spans, until the budget
   is hit. **The injected context never pushes the request over the window**, and it shrinks as
   the user's daily budget tightens — context spend follows the same discipline as model spend.

This is the explicit answer to graphify limitation #6: the **agent**, inside its own loop,
decides what to retrieve and how much to spend on it — no human, no hook.

### 5.5 TUI mockups (monospace)

**(a) `forge lattice query "where do we resolve API keys"` — scoped subgraph**

```
 ⌬ LATTICE   query · "where do we resolve API keys"        (structural ranking; embeddings off)

 ● api_key()                       crates/forge-config/src/lib.rs:122   fn  → Result<String>
   ├─ calls   env_var_for()        crates/forge-config/src/lib.rs:113   (maps provider → ENV var)
   ├─ reads   keyring::Entry        (OS keyring fallback)                    :131
   └─ called-by inject_provider_keys()                                       :151
 ● inject_provider_keys()          crates/forge-config/src/lib.rs:151   fn  → ()
   └─ called-by main()             crates/forge-cli/src/main.rs                (startup)
 ● store_api_key()                 crates/forge-config/src/lib.rs:140   fn  → Result<()>

 5 nodes · 6 edges · 1 file · 0.4 ms                       press i=impact  w=why  ↵ open
```

**(b) `forge lattice impact "Router::route"` — blast radius**

```
 ⌬ LATTICE   impact · Router::route                        reverse-call + dependents closure

 ◎ Router::route (trait method)    crates/forge-mesh/src/lib.rs:53
   │
   ├─ impl HeuristicRouter::route  crates/forge-mesh/src/lib.rs:94      [1 impl]
   ├─ called-by Session::run_turn  crates/forge-core/src/lib.rs:190    ← agent loop (hot path!)
   │     └─ called-by run() / chat()  crates/forge-cli/src/main.rs
   └─ tests routes_*               crates/forge-mesh/src/lib.rs:123-174 [6 tests]

  BLAST RADIUS: 9 sites across 3 files (1 impl · 1 prod caller · 1 cli · 6 tests)
  ⚠ changing this trait signature ripples into the agent loop. proceed?           [shown to agent pre-edit]
```

**(c) Auto-injected-context indicator inside an interactive turn (`forge chat`)**

```
 ⚒ FORGE   add a depth field to Session and thread it through start

 ⚒ mesh → [complex] anthropic::claude-opus-4-8   (matched complex signal)
 ⌬ lattice → injected 3 symbols · 1 file · ~1.2k tok   (Session, Session::start, build)
   the model saw the relevant code up front — no read_file/search round-trips needed

 ▸ editing crates/forge-core/src/lib.rs … ✓ edit_file (Session gains `depth: usize`)
 ⌬ lattice → reindexed lib.rs (12 nodes refreshed)

 ⠹ working   [complex] anthropic::claude-opus-4-8   $0.0231     ↵ send · esc quit
```

Headless renders the same as plain lines (`⌬ lattice → …`), reusing the event-to-line mapping in
`HeadlessPresenter::emit` (`crates/forge-tui/src/lib.rs:83`).

### 5.6 Why this beats graphify

| Axis | graphify (external) | Lattice (native) |
|------|---------------------|------------------|
| Install / process | Separate tool + separate install; runs out-of-process | Built into the `forge` binary; pure Rust; zero setup |
| Storage | Own `graph.json` + files in `graphify-out/` | SQLite **alongside** sessions (`forge-store`); one db |
| Freshness | Manual `graphify update .`; goes stale (tracks a commit) | Incremental on edit (in-turn) + debounced watcher; hash-gated |
| Agent integration | Brittle **hooks** inject context on read/grep | Native `retrieve_context()` step **inside `run_turn`** |
| Who retrieves | Human / hook decides | The **agent** decides (ranked) + a `lattice` **tool** the model calls |
| Retrieval kind | Structural only | **Hybrid** structural + optional semantic (BYOK/local, lazy) |
| Impact analysis | None | **Blast-radius** (reverse-call/dependents closure), shown pre-edit |
| History/why | None | **git blame/log overlay** (`why`) — decision provenance |
| Git-native focus | None | Auto-scope to changed/branch files |
| Cross-repo | Single root | Multi-root, repo-namespaced `SymbolId`s |
| Cost awareness | None | Injection budget **tied to Model Mesh** `BudgetStatus` |
| Privacy | N/A (local) | Local by default; **never embeds code without consent** |
| Latency | Out-of-process query | In-process, indexed SQLite, sub-ms structural queries |

### 5.7 Wiring seam (Store ↔ index ↔ tool)

The `LatticeTool` (a `dyn Tool`, no core access) and the `Lattice` both need the shared `Store`.
Resolution: `Store` becomes `Arc<Store>` shared by `Session` and `Lattice` (already `Sync` via
its internal `Mutex`, `crates/forge-store/src/lib.rs:24`). The graph tables are accessed either
through new `Store` methods (§4) or a `Store::with_conn(|c| …)` accessor exposed to
`forge-index`. The `LatticeTool` holds an `Arc<Lattice>` injected when the registry is built
(`ToolRegistry::with_core_tools` gains a sibling `with_lattice(Arc<Lattice>)`), mirroring how the
subagent `task` tool is wired at the core boundary.

### 5.8 Repo-map (`forge lattice map`)

`forge lattice map [--budget N]` prints a compact, token-budgeted overview of the repo's most
important definitions grouped by file — the "aider-style repo-map" that lets a model (or a
human) orient quickly in an unfamiliar codebase without reading every source file.

**How it works:**

1. All indexed nodes are fetched ordered by `pagerank DESC` — higher centrality first. PageRank
   is already computed and stored by `Lattice::update`, so this is a single `SELECT … ORDER BY
   pagerank DESC LIMIT ?` (no re-computation in the map path).
2. Nodes are greedily packed into a token budget (default 2 000, overridable with `--budget N`).
   Token cost is estimated at ~4 chars/token, identical to the estimate in `retrieve.rs`. A file
   header line (e.g. `crates/forge-core/src/lib.rs:`) is charged once per file the first time a
   symbol from that file is selected.
3. Selected nodes are grouped by file path (`BTreeMap<String, …>`), so file headers appear in
   lexicographic order — deterministic and readable. Within each file, symbols are sorted by
   source line (ascending) so the output reads like the file itself.
4. Rendering: one `<rel_path>:` header per file, then each symbol indented by two spaces:
   `  <kind> <signature-or-qualname-or-name>`.

**Output example** (truncated):

```
crates/forge-core/src/lib.rs:
  function run_turn_with
  struct Session
crates/forge-index/src/lib.rs:
  struct Lattice
  function build_map
crates/forge-store/src/lib.rs:
  function lattice_nodes_ranked
```

**Implementation surface:**

| Item | Location |
|------|----------|
| `build_map(lat, budget)` free fn | `crates/forge-index/src/map.rs` |
| `Lattice::map(budget)` wrapper | `crates/forge-index/src/lib.rs` |
| `Store::lattice_nodes_ranked(limit)` | `crates/forge-store/src/lib.rs` |
| `LatticeOp::Map { budget }` clap variant | `crates/forge-cli/src/main.rs` |
| `LatticeConfig::map_orientation` (future hook) | `crates/forge-config/src/lib.rs` |

**`map_orientation` config field:** `[lattice] map_orientation = true` is wired in config but
intentionally **not yet active** in the map output or the agent turn loop. When activated in a
future PR, it will group the map by importance tier (high / medium / low pagerank bands) rather
than by file path — useful for very large repos where file grouping scatters related symbols.

### 5.9 Edge cases

| Edge case | Behaviour |
|-----------|-----------|
| Unsupported language | `lattice_file.parse_status="skipped"`, `lang="unsupported"`; file recorded (so hash tracking works) but no nodes; retrieval ignores it. Adding a grammar later upgrades it. |
| Binary / non-code file | Detected (null bytes / extension / size cap) → skipped, never parsed. |
| Generated / vendored code | Excluded by `exclude` globs (defaults: `target/`, `node_modules/`, `vendor/`, `*.min.*`, `dist/`). User-overridable. |
| Huge monorepo | `update` honours include/exclude + a file-size cap; `retrieve_context` is bounded by a node-visit cap + `retrieval_time_budget_ms` (default 15) and returns best-so-far — never blocks the provider call. |
| Stale index after external git op (checkout/rebase/pull) | `status()` compares indexed HEAD vs current HEAD and per-file hashes; on drift, changed files are lazily re-parsed before answering; deleted files' nodes are removed (CASCADE). Never serves stale nodes. |
| Agent edit mid-turn | `reindex_path` re-parses the touched file on tool success so later steps/queries in the same turn see the change. |
| Empty / absent index | `retrieve_context` returns nothing; the turn runs exactly as today (additive guarantee). |
| Embeddings disabled (default) | All NL queries use structural+lexical ranking; clearly labelled "(embeddings off)"; never errors for a missing backend. |
| Embedding backend unavailable (local model missing / provider 4xx) | Degrade to structural-only for that query; warn once; do not fail the turn. |
| Privacy / BYOK consent | `embeddings="provider"` requires an explicit consent flag; without it, no source text leaves the machine — fall back to local or structural. `status()` shows backend + consent state. |
| Multi-repo identity collision | `SymbolId` is namespaced by `repo_root`; two repos' `Config` are distinct nodes; results label the repo. |
| Symbol name ambiguous (`route` in N places) | `query`/`impact` return all matches ranked; `path`/`why` ask for a qualified name (`forge_mesh::Router::route`) when ambiguous. |
| Index corruption / schema drift | Tables are idempotent (`CREATE … IF NOT EXISTS`) like the existing schema; a `lattice rebuild` drops + re-parses from scratch; a parse panic per file is caught → `parse_status="error"`, others continue. |
| Concurrent writers (watcher + agent edit) | Single SQLite connection behind the `Store` mutex serialises writes (as today); the debounced watcher coalesces bursts. |
| Cost double-count | Lattice does **no** model calls in the structural path; embedding calls (if enabled) are recorded as usage against the session like any provider call — once. |

## 6. Definition of done

- [ ] `forge-index` crate exists: `parser` (tree-sitter Rust compiled in), `extract`, `graph`,
      `retrieve`, `git`, `watch`, `embed` (feature-gated, off by default), with unit tests.
- [ ] `lattice_file/node/edge/embedding` tables added to `forge-store` schema; new `Store`
      graph methods (or `with_conn`); `Store` shared as `Arc<Store>`; existing store tests green.
- [ ] `forge lattice update` builds the index for a Rust repo with **no network / no API key**;
      content-hash incremental update re-parses only changed files; `status` reports counts + HEAD.
- [ ] `retrieve_context()` wired into `run_turn` after routing/before the loop
      (`crates/forge-core/src/lib.rs:203`); injects a budgeted system message; emits
      `ContextInjected`; empty index → no injection, turn unchanged (additive test).
- [ ] Injection token budget scales with `BudgetStatus` (full/half/floor); proven to never
      exceed the configured budget.
- [ ] Post-edit `reindex_path` runs on `write_file`/`edit_file` success
      (`crates/forge-core/src/lib.rs:331`); in-turn freshness test passes.
- [ ] `lattice impact <symbol>` returns the reverse-call/dependents closure with a site count;
      surfaced to the agent pre-edit; correctness test on a known fan-in symbol (`Router::route`).
- [ ] `lattice query/explain/path` reach graphify parity on this repo; `query` degrades to
      structural ranking with embeddings off and never errors.
- [ ] `lattice why <symbol>` joins git blame/log → author/date/commit/subject; handles
      non-git paths.
- [ ] Optional embeddings: lazy, hash-cached, hybrid-ranked; **refuses to send code to a
      provider without explicit consent**; degrades gracefully when unavailable (tests for both).
- [ ] `LatticeTool` registered (`ReadOnly`), callable by the model; `forge lattice` subcommands
      dispatch in `forge-cli`; `[lattice]` config block with safe defaults + globs.
- [ ] TUI renders scoped subgraph + impact view + the auto-injected-context indicator; headless
      renders the `⌬ lattice → …` lines.
- [ ] Edge-case table behaviours covered by tests where feasible (unsupported lang, vendored
      exclude, git drift, huge-repo bound, multi-repo identity, empty index).
- [ ] Roadmap updated: the four Wave-4 items (`docs/roadmap.md:114-122`) marked consolidated
      into this feature.
- [ ] `cargo fmt` + `clippy -D warnings` clean; verified live in the TUI on this repo
      (`forge lattice query`, an `impact`, and an auto-injected turn against a real provider).
```
