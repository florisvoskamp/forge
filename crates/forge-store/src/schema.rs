//! Embedded schema. For v0.1 this is a single idempotent batch; a versioned migration
//! mechanism is a planned enhancement (ADR-0005 follow-up).

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

-- Subscription quota windows (quota-aware routing, L3). One row per bridge provider; the latest
-- observation from the CLI stream (Claude's `rate_limit_event`). The row stops constraining once
-- `resets_at` passes (the window rolled over).
CREATE TABLE IF NOT EXISTS subscription_usage (
    provider    TEXT PRIMARY KEY,      -- bridge prefix: claude-cli / codex-cli
    window_kind TEXT NOT NULL,         -- five_hour / weekly / … ("" if unknown)
    status      TEXT NOT NULL,         -- ok / warning / exhausted
    resets_at   INTEGER,               -- epoch secs; row is stale once now > resets_at
    fraction    REAL,                  -- 0.0–1.0 window used, if reported
    updated_at  INTEGER NOT NULL DEFAULT (strftime('%s','now'))
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
    line_start  INTEGER NOT NULL
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
"#;
