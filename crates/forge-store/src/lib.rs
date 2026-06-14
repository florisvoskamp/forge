//! Local SQLite persistence (ADR-0005), via rusqlite with bundled SQLite. The store owns
//! a single connection behind a mutex; SQLite is in WAL mode for crash-resilient writes.
//! All persistence in Forge goes through this crate.

use std::path::Path;
use std::sync::Mutex;

use forge_types::{Role, TaskTier, ToolCall, Usage};
use rusqlite::Connection;

mod schema;

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

    /// Current running cost of a session.
    pub fn session_cost(&self, session_id: &str) -> Result<f64> {
        Ok(self.lock()?.query_row(
            "SELECT total_cost_usd FROM session WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )?)
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

    /// All messages of a session, in turn order (by seq).
    pub fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT role, content, model, tool_calls_json, tool_call_id
             FROM message WHERE session_id = ?1 ORDER BY seq",
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
}
