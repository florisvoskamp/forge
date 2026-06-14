//! Local SQLite persistence (ADR-0005), via rusqlite with bundled SQLite. The store owns
//! a single connection behind a mutex; SQLite is in WAL mode for crash-resilient writes.
//! All persistence in Forge goes through this crate.

use std::path::Path;
use std::sync::Mutex;

use forge_types::{Role, TaskTier, Usage};
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
        let id = forge_types::new_id();
        self.lock()?.execute(
            "INSERT INTO message (id, session_id, seq, role, content, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (&id, session_id, seq, role.as_str(), content, model),
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
}
