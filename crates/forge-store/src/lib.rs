//! Local SQLite persistence (ADR-0005), via rusqlite with bundled SQLite. The store owns
//! a single connection behind a mutex; SQLite is in WAL mode for crash-resilient writes.
//! All persistence in Forge goes through this crate.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, TimeZone};
use forge_types::{Role, TaskTier, ToolCall, Usage};
use rusqlite::Connection;

mod schema;

/// Half-open `[start, end)` epoch-second bounds of `now`'s **local** calendar day. Computed
/// in Rust (not SQLite `strftime`) so the day rolls at the user's midnight and survives DST.
pub fn day_bounds_local(now: DateTime<Local>) -> (i64, i64) {
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("valid midnight");
    let start = Local
        .from_local_datetime(&midnight)
        .earliest()
        .unwrap_or(now);
    let end = start + ChronoDuration::days(1);
    (start.timestamp(), end.timestamp())
}

/// Half-open `[start, end)` epoch-second bounds of `now`'s **local** calendar month.
pub fn month_bounds_local(now: DateTime<Local>) -> (i64, i64) {
    let first = now
        .date_naive()
        .with_day(1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("valid first-of-month");
    let start = Local.from_local_datetime(&first).earliest().unwrap_or(now);
    let next_first = if first.month() == 12 {
        first
            .with_year(first.year() + 1)
            .and_then(|d| d.with_month(1))
    } else {
        first.with_month(first.month() + 1)
    }
    .expect("valid next month");
    let end = Local
        .from_local_datetime(&next_first)
        .earliest()
        .unwrap_or(now);
    (start.timestamp(), end.timestamp())
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("store lock poisoned")]
    Lock,
}

type Result<T> = std::result::Result<T, StoreError>;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) a database file and run migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory store, primarily for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(schema::SCHEMA)?;
        // Best-effort migrations for databases created before these columns existed
        // (errors on already-present columns are expected and ignored).
        for stmt in [
            "ALTER TABLE message ADD COLUMN tool_calls_json TEXT",
            "ALTER TABLE message ADD COLUMN tool_call_id TEXT",
            "ALTER TABLE message ADD COLUMN active INTEGER NOT NULL DEFAULT 1",
            "ALTER TABLE session ADD COLUMN parent_session_id TEXT",
        ] {
            let _ = conn.execute(stmt, []);
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| StoreError::Lock)
    }

    /// Create a new session row and return its id.
    pub fn create_session(&self, cwd: &str, mode: &str) -> Result<String> {
        let id = forge_types::new_id();
        self.lock()?.execute(
            "INSERT INTO session (id, cwd, permission_mode, total_cost_usd) VALUES (?1, ?2, ?3, 0)",
            (&id, cwd, mode),
        )?;
        Ok(id)
    }

    /// Create a subagent child session linked to `parent_id` (RFC subagent-orchestration).
    pub fn create_child_session(&self, cwd: &str, mode: &str, parent_id: &str) -> Result<String> {
        let id = forge_types::new_id();
        self.lock()?.execute(
            "INSERT INTO session (id, cwd, permission_mode, total_cost_usd, parent_session_id) \
             VALUES (?1, ?2, ?3, 0, ?4)",
            (&id, cwd, mode, parent_id),
        )?;
        Ok(id)
    }

    /// A session's stored permission mode (temper) string.
    pub fn session_mode(&self, session_id: &str) -> Result<String> {
        Ok(self.lock()?.query_row(
            "SELECT permission_mode FROM session WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )?)
    }

    /// Update a session's permission mode (temper) — persisted when the user switches it live.
    pub fn update_session_mode(&self, session_id: &str, mode: &str) -> Result<()> {
        self.lock()?.execute(
            "UPDATE session SET permission_mode = ?2, updated_at = strftime('%s','now') WHERE id = ?1",
            (session_id, mode),
        )?;
        Ok(())
    }

    /// Ids of the subagent child sessions spawned by `parent_id`, oldest first.
    pub fn child_sessions(&self, parent_id: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM session WHERE parent_session_id = ?1 ORDER BY created_at, id",
        )?;
        let rows = stmt.query_map([parent_id], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Append a message to a session and return its id.
    pub fn add_message(
        &self,
        session_id: &str,
        seq: i64,
        role: Role,
        content: &str,
        model: Option<&str>,
    ) -> Result<String> {
        self.add_message_full(session_id, seq, role, content, model, &[], None)
    }

    /// Append a message, including any tool-call linkage (assistant tool calls / tool
    /// result ids), so the transcript round-trips faithfully on resume.
    #[allow(clippy::too_many_arguments)]
    pub fn add_message_full(
        &self,
        session_id: &str,
        seq: i64,
        role: Role,
        content: &str,
        model: Option<&str>,
        tool_calls: &[ToolCall],
        tool_call_id: Option<&str>,
    ) -> Result<String> {
        let id = forge_types::new_id();
        let tool_calls_json = if tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(tool_calls).unwrap_or_default())
        };
        self.lock()?.execute(
            "INSERT INTO message (id, session_id, seq, role, content, model, tool_calls_json, tool_call_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (&id, session_id, seq, role.as_str(), content, model, tool_calls_json, tool_call_id),
        )?;
        Ok(id)
    }

    /// Record the Mesh's routing decision for a message.
    pub fn record_routing(
        &self,
        message_id: &str,
        tier: TaskTier,
        chosen_model: &str,
        rationale: &str,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO routing_decision (id, message_id, task_tier, chosen_model, rationale)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                forge_types::new_id(),
                message_id,
                tier.as_str(),
                chosen_model,
                rationale,
            ),
        )?;
        Ok(())
    }

    /// Record token usage/cost for a message and bump the session's running total.
    pub fn record_usage(&self, session_id: &str, message_id: &str, usage: &Usage) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO usage (id, message_id, input_tokens, output_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                forge_types::new_id(),
                message_id,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cost_usd,
            ),
        )?;
        conn.execute(
            "UPDATE session SET total_cost_usd = total_cost_usd + ?1,
             updated_at = strftime('%s','now') WHERE id = ?2",
            (usage.cost_usd, session_id),
        )?;
        Ok(())
    }

    /// Record a tool call and its permission outcome.
    pub fn record_tool_call(
        &self,
        message_id: &str,
        tool_name: &str,
        args_json: &str,
        result: &str,
        permission: &str,
        status: &str,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO tool_call (id, message_id, tool_name, args_json, result_json, permission, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (forge_types::new_id(), message_id, tool_name, args_json, result, permission, status),
        )?;
        Ok(())
    }

    /// Current running cost of a session (the per-session meter — unchanged).
    pub fn session_cost(&self, session_id: &str) -> Result<f64> {
        Ok(self.lock()?.query_row(
            "SELECT total_cost_usd FROM session WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )?)
    }

    /// Models the Mesh routed to within a session (chosen_model per routing_decision), oldest
    /// first. Used to verify subagents route independently of the parent.
    pub fn session_models(&self, session_id: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT r.chosen_model FROM routing_decision r \
             JOIN message m ON m.id = r.message_id \
             WHERE m.session_id = ?1 ORDER BY m.seq",
        )?;
        let rows = stmt.query_map([session_id], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// Total spend across ALL sessions whose `usage` rows fall in `[start, end)` epoch secs.
    /// This is the authoritative budget figure (FR-5): it aggregates `usage.cost_usd` across
    /// every session, not one session's running total.
    pub fn spend_between(&self, start: i64, end: i64) -> Result<f64> {
        Ok(self.lock()?.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM usage \
             WHERE created_at >= ?1 AND created_at < ?2",
            (start, end),
            |row| row.get(0),
        )?)
    }

    /// Spend across all sessions in the current local calendar day.
    pub fn spend_today_usd(&self) -> Result<f64> {
        let (s, e) = day_bounds_local(chrono::Local::now());
        self.spend_between(s, e)
    }

    /// Spend across all sessions in the current local calendar month.
    pub fn spend_this_month_usd(&self) -> Result<f64> {
        let (s, e) = month_bounds_local(chrono::Local::now());
        self.spend_between(s, e)
    }

    // --- Model health / failover (docs/features/model-health-failover.md) ---

    /// Bench a model until `cooldown_until` (epoch secs), recording why. Upsert: a fresh failure
    /// or probe overwrites any prior bench.
    pub fn bench_model(&self, model: &str, cooldown_until: i64, reason: &str) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO model_health (model, cooldown_until, reason, updated_at)
             VALUES (?1, ?2, ?3, strftime('%s','now'))
             ON CONFLICT(model) DO UPDATE SET
               cooldown_until = excluded.cooldown_until,
               reason = excluded.reason,
               updated_at = excluded.updated_at",
            (model, cooldown_until, reason),
        )?;
        Ok(())
    }

    /// Bench a model for `cooldown` from now (convenience over [`bench_model`] that owns the
    /// clock, like [`spend_today_usd`](Self::spend_today_usd)).
    pub fn bench_for(
        &self,
        model: &str,
        cooldown: std::time::Duration,
        reason: &str,
    ) -> Result<()> {
        let until = chrono::Utc::now().timestamp() + cooldown.as_secs() as i64;
        self.bench_model(model, until, reason)
    }

    /// Currently-benched snapshot as of *now* (convenience over [`benched_models`]).
    pub fn current_benched(&self) -> Result<forge_types::ModelHealth> {
        self.benched_models(chrono::Utc::now().timestamp())
    }

    /// Currently-benched detailed report as of *now* (convenience over [`benched_report`]).
    pub fn current_benched_report(&self) -> Result<Vec<(String, i64, String)>> {
        self.benched_report(chrono::Utc::now().timestamp())
    }

    /// Clear any bench on a model (e.g. a healthy probe). No-op if it wasn't benched.
    pub fn clear_model_health(&self, model: &str) -> Result<()> {
        self.lock()?
            .execute("DELETE FROM model_health WHERE model = ?1", [model])?;
        Ok(())
    }

    /// Snapshot of models still benched as of `now` (epoch secs) — cooldown not yet elapsed.
    pub fn benched_models(&self, now: i64) -> Result<forge_types::ModelHealth> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT model FROM model_health WHERE cooldown_until > ?1")?;
        let set = stmt
            .query_map([now], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<std::collections::HashSet<_>, _>>()?;
        Ok(forge_types::ModelHealth::new(set))
    }

    /// Detailed view of currently-benched models (model, cooldown_until, reason) for the CLI /
    /// startup hint, newest cooldown first.
    pub fn benched_report(&self, now: i64) -> Result<Vec<(String, i64, String)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT model, cooldown_until, reason FROM model_health
             WHERE cooldown_until > ?1 ORDER BY cooldown_until DESC",
        )?;
        let rows = stmt
            .query_map([now], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Number of messages in a session.
    pub fn message_count(&self, session_id: &str) -> Result<i64> {
        Ok(self.lock()?.query_row(
            "SELECT COUNT(*) FROM message WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )?)
    }

    /// Past sessions, newest first (by insertion order, then creation time).
    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT s.id, s.cwd, s.permission_mode, s.created_at, s.total_cost_usd,
                    (SELECT COUNT(*) FROM message m WHERE m.session_id = s.id),
                    (SELECT content FROM message m WHERE m.session_id = s.id
                       AND m.role = 'user' ORDER BY m.seq LIMIT 1)
             FROM session s ORDER BY s.created_at DESC, s.rowid DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                cwd: row.get(1)?,
                permission_mode: row.get(2)?,
                created_at: row.get(3)?,
                total_cost_usd: row.get(4)?,
                message_count: row.get(5)?,
                preview: row.get(6)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Full session ids whose id starts with `prefix` (git-style abbreviation).
    pub fn matching_session_ids(&self, prefix: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT id FROM session WHERE id LIKE ?1 || '%'")?;
        let rows = stmt.query_map([prefix], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Whether a session with this id exists.
    pub fn session_exists(&self, session_id: &str) -> Result<bool> {
        let n: i64 = self.lock()?.query_row(
            "SELECT COUNT(*) FROM session WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        Ok(n > 0)
    }

    /// All *active* messages of a session, in turn order (by seq). Soft-deleted rows (those a
    /// `/undo` rewound past) are excluded — they remain in the table for audit/redo.
    pub fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT role, content, model, tool_calls_json, tool_call_id
             FROM message WHERE session_id = ?1 AND active = 1 ORDER BY seq",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            let role: String = row.get(0)?;
            let tool_calls_json: Option<String> = row.get(3)?;
            let tool_calls = tool_calls_json
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            Ok(StoredMessage {
                role: Role::parse(&role).unwrap_or(Role::User),
                content: row.get(1)?,
                model: row.get(2)?,
                tool_calls,
                tool_call_id: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    // --- Conversation checkpoints / undo (RFC session-management-and-commands, PR2) ---

    /// Soft-delete every message of a session with `seq >= from_seq` (an `/undo` / checkpoint
    /// rewind). The rows stay in the table (`active = 0`) for audit/redo; [`load_messages`]
    /// excludes them. Returns the number of messages deactivated.
    pub fn deactivate_messages_from(&self, session_id: &str, from_seq: i64) -> Result<usize> {
        Ok(self.lock()?.execute(
            "UPDATE message SET active = 0 WHERE session_id = ?1 AND seq >= ?2 AND active = 1",
            (session_id, from_seq),
        )?)
    }

    /// Save a checkpoint (rewind point) at `seq`. `label` NULL = an auto per-turn checkpoint.
    pub fn add_checkpoint(
        &self,
        session_id: &str,
        label: Option<&str>,
        seq: i64,
    ) -> Result<String> {
        let id = forge_types::new_id();
        self.lock()?.execute(
            "INSERT INTO checkpoint (id, session_id, label, seq) VALUES (?1, ?2, ?3, ?4)",
            (&id, session_id, label, seq),
        )?;
        Ok(id)
    }

    /// A session's named checkpoints, newest first.
    pub fn list_checkpoints(&self, session_id: &str) -> Result<Vec<CheckpointRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, label, seq, created_at FROM checkpoint
             WHERE session_id = ?1 ORDER BY seq DESC, created_at DESC",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok(CheckpointRow {
                id: row.get(0)?,
                label: row.get(1)?,
                seq: row.get(2)?,
                created_at: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

/// A persisted message, as read back from the store.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub role: Role,
    pub content: String,
    pub model: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
}

/// A persisted checkpoint (rewind point) of a session.
#[derive(Debug, Clone)]
pub struct CheckpointRow {
    pub id: String,
    /// User-given name, or `None` for an auto per-turn checkpoint.
    pub label: Option<String>,
    /// Transcript boundary: messages with `seq < this` survive a rewind to here.
    pub seq: i64,
    pub created_at: i64,
}

/// A one-line summary of a past session, for `forge sessions`.
#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: String,
    pub cwd: String,
    pub permission_mode: String,
    pub created_at: i64,
    pub total_cost_usd: f64,
    pub message_count: i64,
    /// First user message, if any.
    pub preview: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persist_a_turn() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();

        let mid = store
            .add_message(&sid, 0, Role::User, "hello", None)
            .unwrap();
        store
            .record_routing(
                &mid,
                TaskTier::Standard,
                "openai::gpt-4o-mini",
                "medium prompt",
            )
            .unwrap();
        store
            .record_usage(
                &sid,
                &mid,
                &Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cost_usd: 0.02,
                },
            )
            .unwrap();
        store
            .record_tool_call(&mid, "read_file", "{}", "ok", "allowed", "ok")
            .unwrap();

        assert_eq!(store.message_count(&sid).unwrap(), 1);
        assert!((store.session_cost(&sid).unwrap() - 0.02).abs() < 1e-9);
    }

    fn record_cost(store: &Store, cost: f64) {
        let sid = store.create_session("/tmp", "default").unwrap();
        let mid = store.add_message(&sid, 0, Role::User, "x", None).unwrap();
        store
            .record_usage(
                &sid,
                &mid,
                &Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cost_usd: cost,
                },
            )
            .unwrap();
    }

    #[test]
    fn spend_today_sums_across_sessions() {
        // AC-1: the day total aggregates usage across DIFFERENT sessions, not one session's
        // running total.
        let store = Store::open_in_memory().unwrap();
        record_cost(&store, 0.06);
        record_cost(&store, 0.05);
        let today = store.spend_today_usd().unwrap();
        assert!(
            (today - 0.11).abs() < 1e-9,
            "summed across sessions: {today}"
        );
    }

    #[test]
    fn spend_between_excludes_out_of_window_rows() {
        let store = Store::open_in_memory().unwrap();
        record_cost(&store, 0.03);
        assert_eq!(
            store.spend_between(0, 1).unwrap(),
            0.0,
            "a 1970 window excludes today's row"
        );
        let (s, e) = day_bounds_local(Local::now());
        assert!(
            store.spend_between(s, e).unwrap() > 0.0,
            "today's window includes it"
        );
    }

    #[test]
    fn day_bounds_are_24h_and_exclude_prior_day() {
        let now = Local.with_ymd_and_hms(2026, 6, 15, 13, 30, 0).unwrap();
        let (s, e) = day_bounds_local(now);
        assert_eq!(e - s, 86_400, "a day is 24h (no DST on this date)");
        assert!(now.timestamp() >= s && now.timestamp() < e);
        let prev = Local.with_ymd_and_hms(2026, 6, 14, 23, 0, 0).unwrap();
        assert!(prev.timestamp() < s, "yesterday is excluded (AC-4)");
    }

    #[test]
    fn month_bounds_exclude_prior_month() {
        let now = Local.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        let (s, e) = month_bounds_local(now);
        assert!(now.timestamp() >= s && now.timestamp() < e);
        let jun1 = Local.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        assert_eq!(
            s,
            jun1.timestamp(),
            "window starts at the first of the month"
        );
        let may = Local.with_ymd_and_hms(2026, 5, 31, 23, 0, 0).unwrap();
        assert!(may.timestamp() < s, "May is excluded from June (AC-3)");
    }

    #[test]
    fn tool_linkage_round_trips() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        let calls = vec![ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            args: serde_json::json!({ "path": "x" }),
        }];
        store
            .add_message_full(&sid, 0, Role::Assistant, "calling", Some("m"), &calls, None)
            .unwrap();
        store
            .add_message_full(&sid, 1, Role::Tool, "result", None, &[], Some("c1"))
            .unwrap();

        let msgs = store.load_messages(&sid).unwrap();
        assert_eq!(msgs[0].tool_calls.len(), 1);
        assert_eq!(msgs[0].tool_calls[0].name, "read_file");
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn load_messages_returns_seq_order() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        // Insert out of order; load must sort by seq.
        store
            .add_message(&sid, 2, Role::Tool, "tool result", None)
            .unwrap();
        store
            .add_message(&sid, 0, Role::User, "do the thing", None)
            .unwrap();
        store
            .add_message(&sid, 1, Role::Assistant, "on it", Some("opus"))
            .unwrap();

        let msgs = store.load_messages(&sid).unwrap();
        let roles: Vec<_> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::Tool]);
        assert_eq!(msgs[0].content, "do the thing");
        assert_eq!(msgs[1].model.as_deref(), Some("opus"));
    }

    #[test]
    fn list_sessions_newest_first_with_preview_and_count() {
        let store = Store::open_in_memory().unwrap();

        let a = store.create_session("/a", "default").unwrap();
        store
            .add_message(&a, 0, Role::User, "first task", None)
            .unwrap();

        let b = store.create_session("/b", "plan").unwrap();
        store
            .add_message(&b, 0, Role::User, "second task", None)
            .unwrap();
        store
            .add_message(&b, 1, Role::Assistant, "working", Some("opus"))
            .unwrap();

        let sessions = store.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        // Newest (b) first.
        assert_eq!(sessions[0].id, b);
        assert_eq!(sessions[0].preview.as_deref(), Some("second task"));
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[1].id, a);
        assert_eq!(sessions[1].message_count, 1);
    }

    #[test]
    fn session_exists_reports_presence() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_session("/x", "default").unwrap();
        assert!(store.session_exists(&id).unwrap());
        assert!(!store.session_exists("nope").unwrap());
    }

    #[test]
    fn matching_session_ids_resolves_a_prefix() {
        let store = Store::open_in_memory().unwrap();
        let id = store.create_session("/x", "default").unwrap();
        let prefix: String = id.chars().take(8).collect();

        let matches = store.matching_session_ids(&prefix).unwrap();
        assert_eq!(matches, vec![id]);
        assert!(store.matching_session_ids("zzzzzzzz").unwrap().is_empty());
    }

    // --- Conversation checkpoints / undo (PR2) ---

    #[test]
    fn deactivate_excludes_messages_from_load_but_keeps_earlier_ones() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        store
            .add_message(&sid, 0, Role::User, "turn 1", None)
            .unwrap();
        store
            .add_message(&sid, 1, Role::Assistant, "reply 1", Some("m"))
            .unwrap();
        store
            .add_message(&sid, 2, Role::User, "turn 2", None)
            .unwrap();
        store
            .add_message(&sid, 3, Role::Assistant, "reply 2", Some("m"))
            .unwrap();

        // Rewind to the start of turn 2 (seq 2): turn 2's two messages drop out.
        let n = store.deactivate_messages_from(&sid, 2).unwrap();
        assert_eq!(n, 2, "two messages deactivated");

        let msgs = store.load_messages(&sid).unwrap();
        let contents: Vec<_> = msgs.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["turn 1", "reply 1"],
            "only the surviving turn loads"
        );
    }

    #[test]
    fn checkpoints_round_trip_newest_first() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        store
            .add_checkpoint(&sid, Some("before refactor"), 2)
            .unwrap();
        store.add_checkpoint(&sid, None, 5).unwrap();

        let cps = store.list_checkpoints(&sid).unwrap();
        assert_eq!(cps.len(), 2);
        assert_eq!(cps[0].seq, 5, "newest (highest seq) first");
        assert_eq!(cps[0].label, None, "auto checkpoint has no label");
        assert_eq!(cps[1].label.as_deref(), Some("before refactor"));
    }

    // --- Model health / failover ---

    #[test]
    fn benched_model_is_in_snapshot_until_cooldown_elapses() {
        let store = Store::open_in_memory().unwrap();
        store
            .bench_model("gemini::antigravity", 1000, "rate-limited")
            .unwrap();
        // now=500 < cooldown 1000 → still benched (AC-3).
        assert!(store
            .benched_models(500)
            .unwrap()
            .is_benched("gemini::antigravity"));
        // now=1001 > cooldown → eligible again (AC-4).
        assert!(!store
            .benched_models(1001)
            .unwrap()
            .is_benched("gemini::antigravity"));
    }

    #[test]
    fn bench_is_upsert_and_clear_removes_it() {
        let store = Store::open_in_memory().unwrap();
        store.bench_model("m", 1000, "rate-limited").unwrap();
        store.bench_model("m", 2000, "auth failed").unwrap(); // upsert, no PK clash
        let report = store.benched_report(500).unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(
            report[0],
            ("m".to_string(), 2000, "auth failed".to_string())
        );
        store.clear_model_health("m").unwrap();
        assert!(store.benched_models(500).unwrap().is_empty());
    }

    #[test]
    fn bench_persists_across_reopen() {
        // Same file → a daily-quota bench survives a Forge restart (AC-3).
        let dir = std::env::temp_dir().join(forge_types::new_id());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forge.db");
        {
            let store = Store::open(&path).unwrap();
            store
                .bench_model("m", 9_999_999_999, "probe: quota 0")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(store.benched_models(500).unwrap().is_benched("m"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
