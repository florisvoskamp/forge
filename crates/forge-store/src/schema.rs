//! Embedded base schema (idempotent `CREATE TABLE IF NOT EXISTS`). Versioned, ordered schema
//! changes live in `MIGRATIONS` (lib.rs), gated by `PRAGMA user_version`. Anything that depends on a
//! migrated column (e.g. indexes on `message.active`) must be created in a migration, NOT here —
//! `CREATE TABLE IF NOT EXISTS` won't add columns to an existing table, so a column-dependent index
//! in this batch would fail to open a pre-migration DB.

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS session (
    id              TEXT PRIMARY KEY,
    title           TEXT,
    cwd             TEXT NOT NULL,
    permission_mode TEXT NOT NULL,
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    total_cost_usd  REAL NOT NULL DEFAULT 0,
    parent_session_id TEXT          -- non-null for subagent child sessions (RFC subagent-orchestration)
);

CREATE TABLE IF NOT EXISTS message (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    model           TEXT,
    tool_calls_json TEXT,
    tool_call_id    TEXT,
    active          INTEGER NOT NULL DEFAULT 1,   -- 0 = soft-deleted by /undo (kept for audit/redo)
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_message_session ON message(session_id, seq);
-- `idx_message_session_active` (covers `WHERE session_id=? AND active=1 ORDER BY seq`) and the
-- UNIQUE(session_id, seq) index both depend on the migrated `active` column, so they are created in
-- `migration_0001` after the ALTER — not here. See the module doc comment above.

-- A labeled rewind point: messages with seq < this boundary are kept on restore
-- (RFC session-management-and-commands, PR2). label NULL = an auto per-turn checkpoint.
CREATE TABLE IF NOT EXISTS checkpoint (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
    label       TEXT,
    seq         INTEGER NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_checkpoint_session ON checkpoint(session_id, seq);

CREATE TABLE IF NOT EXISTS tool_call (
    id          TEXT PRIMARY KEY,
    message_id  TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
    tool_name   TEXT NOT NULL,
    args_json   TEXT NOT NULL,
    result_json TEXT,
    permission  TEXT NOT NULL,
    status      TEXT NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE TABLE IF NOT EXISTS routing_decision (
    id           TEXT PRIMARY KEY,
    message_id   TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
    task_tier    TEXT NOT NULL,
    chosen_model TEXT NOT NULL,
    rationale    TEXT NOT NULL,
    budget_state TEXT,
    created_at   INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE TABLE IF NOT EXISTS usage (
    id            TEXT PRIMARY KEY,
    message_id    TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
    provider      TEXT,
    model         TEXT,
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cost_usd      REAL NOT NULL,
    created_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_usage_created_at ON usage(created_at);
-- Speeds up spend-by-model JOINs: `JOIN message m ON m.id = u.message_id`.
CREATE INDEX IF NOT EXISTS idx_usage_message ON usage(message_id);

-- Assay (AI-slop / quality analysis) runs + their findings (docs/features/analysis-mode.md).
CREATE TABLE IF NOT EXISTS assay_run (
    id          TEXT PRIMARY KEY,
    scope       TEXT NOT NULL,             -- human label of the analyzed scope
    cost_usd    REAL NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

CREATE TABLE IF NOT EXISTS finding (
    id            TEXT PRIMARY KEY,
    run_id        TEXT NOT NULL REFERENCES assay_run(id) ON DELETE CASCADE,
    category      TEXT NOT NULL,
    severity      TEXT NOT NULL,
    confidence    TEXT NOT NULL,
    file          TEXT NOT NULL,
    line          INTEGER,
    title         TEXT NOT NULL,
    rationale     TEXT NOT NULL,
    suggested_fix TEXT NOT NULL,
    effort        TEXT NOT NULL,
    lens          TEXT NOT NULL,
    verified      INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_finding_run ON finding(run_id);

CREATE TABLE IF NOT EXISTS model_health (
    model          TEXT PRIMARY KEY,
    cooldown_until INTEGER NOT NULL,   -- epoch secs; the model is benched while this is > now
    reason         TEXT NOT NULL,      -- "rate-limited", "auth failed", "probe: quota 0", …
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- Per-model context-window sizes (tokens), fetched from provider APIs at discovery (e.g.
-- OpenRouter's /api/v1/models `context_length`). The core trims a turn's transcript to fit the
-- routed model's window, so a long conversation never overflows it (which surfaced as every model
-- failing "unavailable"). A model absent here falls back to the family heuristic, then a floor.
CREATE TABLE IF NOT EXISTS model_context (
    model      TEXT PRIMARY KEY,
    window     INTEGER NOT NULL,       -- context window in tokens
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- Per-model USD prices (per 1k tokens), fetched from provider APIs at discovery (e.g. OpenRouter's
-- /api/v1/models `pricing`). Most gateway/credit models aren't in the bundled default rate table,
-- so without this their spend computes to $0 and the budget cap can't see it. A model absent here
-- falls back to the bundled defaults, then to $0 (unpriced).
CREATE TABLE IF NOT EXISTS model_pricing (
    model             TEXT PRIMARY KEY,
    input_per_1k      REAL NOT NULL,    -- USD per 1,000 input tokens
    output_per_1k     REAL NOT NULL,    -- USD per 1,000 output tokens
    cache_read_per_1k REAL,             -- USD per 1,000 cached (prompt-cache read) tokens; NULL if unknown
    updated_at        INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- Subscription quota windows (quota-aware routing, L3). One row per bridge provider per window;
-- composite PK so 5h and weekly windows are tracked independently.
-- The row stops constraining once `resets_at` passes (the window rolled over).
CREATE TABLE IF NOT EXISTS subscription_usage (
    provider    TEXT NOT NULL,         -- bridge prefix: claude-cli / codex-cli
    window_kind TEXT NOT NULL,         -- five_hour / weekly / … ("" if unknown)
    status      TEXT NOT NULL,         -- ok / warning / exhausted
    resets_at   INTEGER,               -- epoch secs; row is stale once now > resets_at
    fraction    REAL,                  -- 0.0–1.0 window used, if reported
    updated_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    PRIMARY KEY (provider, window_kind)
);

-- The agent's task/todo list (the `update_tasks` tool). One row per session holding the latest
-- list as JSON, so a resumed session restores its task list. Replaced wholesale on each update.
CREATE TABLE IF NOT EXISTS session_tasks (
    session_id TEXT PRIMARY KEY REFERENCES session(id) ON DELETE CASCADE,
    tasks_json TEXT NOT NULL,          -- JSON array of {title, status}
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- Lattice: native code-intelligence graph (code-intelligence.md), in the SAME db as sessions.
-- One row per indexed source file; content_hash gates incremental re-parsing.
CREATE TABLE IF NOT EXISTS lattice_file (
    id           TEXT PRIMARY KEY,      -- stable: repo_root || 0x00 || rel_path
    repo_root    TEXT NOT NULL,
    rel_path     TEXT NOT NULL,
    lang         TEXT NOT NULL,         -- "rust" | … | "unsupported"
    content_hash TEXT NOT NULL,         -- SHA-256; the incremental-update key
    parse_status TEXT NOT NULL,         -- "ok" | "skipped" | "error"
    indexed_at   INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    UNIQUE(repo_root, rel_path)
);

-- One row per symbol/definition.
CREATE TABLE IF NOT EXISTS lattice_node (
    id          TEXT PRIMARY KEY,       -- repo-namespaced SymbolId
    file_id     TEXT NOT NULL REFERENCES lattice_file(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL,          -- function|method|struct|enum|trait|impl|const|module|type
    name        TEXT NOT NULL,
    qualname    TEXT,
    signature   TEXT,
    span_start  INTEGER NOT NULL,
    span_end    INTEGER NOT NULL,
    line_start  INTEGER NOT NULL,
    pagerank    REAL NOT NULL DEFAULT 0.0
);
CREATE INDEX IF NOT EXISTS idx_lnode_name ON lattice_node(name);
CREATE INDEX IF NOT EXISTS idx_lnode_file ON lattice_node(file_id);

-- One row per relationship (PR1 emits `contains`; resolved call/ref edges come later).
CREATE TABLE IF NOT EXISTS lattice_edge (
    id              TEXT PRIMARY KEY,
    src_id          TEXT NOT NULL REFERENCES lattice_node(id) ON DELETE CASCADE,
    dst_id          TEXT NOT NULL REFERENCES lattice_node(id) ON DELETE CASCADE,
    kind            TEXT NOT NULL,      -- defines|calls|imports|impls|references|contains
    unresolved_name TEXT
);
CREATE INDEX IF NOT EXISTS idx_ledge_src ON lattice_edge(src_id, kind);
CREATE INDEX IF NOT EXISTS idx_ledge_dst ON lattice_edge(dst_id, kind);

-- Unresolved references / call sites: one row per identifier use inside a definition. dst is a
-- NAME (not a node id) resolved by join at query time, so cross-file calls survive incremental
-- reindexing (a reference is tied to its own file's src node and cascades with it). This powers
-- `impact` (who references X) and `path` (call-chain BFS) without fragile stored dst ids.
CREATE TABLE IF NOT EXISTS lattice_ref (
    id      TEXT PRIMARY KEY,
    src_id  TEXT NOT NULL REFERENCES lattice_node(id) ON DELETE CASCADE,
    name    TEXT NOT NULL,             -- referenced identifier (callee / type / module)
    kind    TEXT NOT NULL,             -- calls | references | type | module | …
    line    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_lref_name ON lattice_ref(name);
CREATE INDEX IF NOT EXISTS idx_lref_src ON lattice_ref(src_id);

-- Optional per-node embedding vector for semantic retrieval (code-intelligence.md §5.6;
-- off by default). `vec` is the f32 components packed little-endian; `dim` is the length.
-- Cascades with the node so a reindex/delete drops a stale vector.
CREATE TABLE IF NOT EXISTS lattice_embedding (
    node_id TEXT PRIMARY KEY REFERENCES lattice_node(id) ON DELETE CASCADE,
    dim     INTEGER NOT NULL,
    vec     BLOB NOT NULL
);

-- Persisted compaction summary (/compact). When compact() runs, the older messages are
-- soft-deleted and their model-generated summary is stored here. load_messages prepends
-- a synthetic System message with this summary so a resumed session rehydrates the compacted view.
CREATE TABLE IF NOT EXISTS session_compaction (
    session_id TEXT PRIMARY KEY REFERENCES session(id) ON DELETE CASCADE,
    summary    TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- Auto-memory: durable facts extracted from turns, scoped per project (or global).
-- kind: preference | decision | fact | reference
-- scope: global | <project-path>
-- salience: 0.0-1.0, boosted on repeat hits; used for relevance ranking.
-- embedding: optional f32 vector (little-endian) for semantic recall when forge-index is available.
CREATE TABLE IF NOT EXISTS memory (
    id            TEXT PRIMARY KEY,
    scope         TEXT NOT NULL,          -- "global" or project path
    kind          TEXT NOT NULL,          -- preference | decision | fact | reference
    text          TEXT NOT NULL,          -- the durable fact
    source_session TEXT NOT NULL,         -- session that produced this memory
    created_at    INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at    INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    salience      REAL NOT NULL DEFAULT 0.5,
    embedding     BLOB                    -- optional f32 vector (little-endian)
);
CREATE INDEX IF NOT EXISTS idx_memory_scope ON memory(scope);
CREATE INDEX IF NOT EXISTS idx_memory_kind ON memory(kind);
CREATE INDEX IF NOT EXISTS idx_memory_salience ON memory(salience DESC);
CREATE INDEX IF NOT EXISTS idx_memory_updated ON memory(updated_at DESC);

-- Live-event ring buffer: MCP agent sessions write events here so the TUI can
-- observe them in real-time. Pruned to last 2000 rows per session.
CREATE TABLE IF NOT EXISTS live_event (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
    payload_json TEXT NOT NULL,
    created_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_live_event_session ON live_event(session_id, id);
"#;
