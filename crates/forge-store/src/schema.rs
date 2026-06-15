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
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_message_session ON message(session_id, seq);

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

CREATE TABLE IF NOT EXISTS model_health (
    model          TEXT PRIMARY KEY,
    cooldown_until INTEGER NOT NULL,   -- epoch secs; the model is benched while this is > now
    reason         TEXT NOT NULL,      -- "rate-limited", "auth failed", "probe: quota 0", …
    updated_at     INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
"#;
