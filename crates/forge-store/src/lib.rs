//! Local SQLite persistence (ADR-0005), via rusqlite with bundled SQLite. The store owns
//! a single connection behind a mutex; SQLite is in WAL mode for crash-resilient writes.
//! All persistence in Forge goes through this crate.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, Local, TimeZone};
use forge_types::{Role, TaskTier, ToolCall, Usage};
use rusqlite::{Connection, OptionalExtension};

mod schema;

/// How long a permanently-failed model (a [`Store::exclude_model`] capability exclusion) stays out
/// of routing before it's re-probed: 7 days. Long enough to stop the per-session churn of
/// re-trying models that can't do tool calling, short enough that a provider adding support is
/// picked up within a week.
const CAPABILITY_EXCLUSION_SECS: i64 = 7 * 24 * 60 * 60;

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

/// Half-open `[start, end)` covering the last `hours` hours ending at `now`.
pub fn rolling_hours_bounds(now: DateTime<Local>, hours: i64) -> (i64, i64) {
    let end = now.timestamp() + 1;
    let start = end - hours * 3600;
    (start, end)
}

/// Half-open `[start, end)` epoch-second bounds of `now`'s **local** ISO calendar week
/// (Monday 00:00 local → 7 days later).
pub fn week_bounds_local(now: DateTime<Local>) -> (i64, i64) {
    use chrono::Datelike;
    let days_since_monday = now.weekday().num_days_from_monday() as i64;
    let monday = now.date_naive() - ChronoDuration::days(days_since_monday);
    let start = Local
        .from_local_datetime(&monday.and_hms_opt(0, 0, 0).expect("valid midnight"))
        .earliest()
        .unwrap_or(now);
    let end = start + ChronoDuration::weeks(1);
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

/// A fetched per-model price row: `(model, input_per_1k, output_per_1k, cache_read_per_1k)` in USD.
pub type ModelPriceRow = (String, f64, f64, Option<f64>);

pub struct Store {
    conn: Mutex<Connection>,
}

/// Migrate `subscription_usage` from its old single-column PK to the composite
/// `(provider, window_kind)` PK. Safe to call on any DB version: a no-op when the table
/// doesn't exist yet (schema will create it correctly) or already has the composite key.
fn migrate_subscription_usage(conn: &Connection) -> rusqlite::Result<()> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='subscription_usage'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if exists == 0 {
        return Ok(()); // table not yet created; schema will handle it
    }
    let pk_cols: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('subscription_usage') WHERE pk > 0",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if pk_cols >= 2 {
        return Ok(()); // already on composite PK
    }
    // Old single-column PK — recreate with composite key.
    // subscription_usage is a transient cache; data loss on migration is acceptable.
    conn.execute_batch(
        "DROP TABLE IF EXISTS subscription_usage_new;
         CREATE TABLE subscription_usage_new (
             provider    TEXT NOT NULL,
             window_kind TEXT NOT NULL,
             status      TEXT NOT NULL,
             resets_at   INTEGER,
             fraction    REAL,
             updated_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
             PRIMARY KEY (provider, window_kind)
         );
         DROP TABLE subscription_usage;
         ALTER TABLE subscription_usage_new RENAME TO subscription_usage;",
    )
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
        // Migrate before schema so old DBs get the composite PK before CREATE TABLE IF NOT EXISTS no-ops.
        let _ = migrate_subscription_usage(&conn);
        conn.execute_batch(schema::SCHEMA)?;
        // Best-effort migrations for databases created before these columns existed
        // (errors on already-present columns are expected and ignored).
        for stmt in [
            "ALTER TABLE message ADD COLUMN tool_calls_json TEXT",
            "ALTER TABLE message ADD COLUMN tool_call_id TEXT",
            "ALTER TABLE message ADD COLUMN active INTEGER NOT NULL DEFAULT 1",
            "ALTER TABLE session ADD COLUMN parent_session_id TEXT",
            "ALTER TABLE lattice_node ADD COLUMN pagerank REAL NOT NULL DEFAULT 0.0",
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

    /// Record usage for a side call (compact, diagnose) that has no corresponding agent message.
    /// Inserts a synthetic inactive system message as the FK anchor, then the usage row, and
    /// bumps the session total so daily/monthly budget queries (which read `usage`) stay accurate.
    pub fn record_side_call_usage(
        &self,
        session_id: &str,
        label: &str,
        usage: &Usage,
    ) -> Result<()> {
        let conn = self.lock()?;
        let msg_id = forge_types::new_id();
        let max_seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM message WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO message (id, session_id, seq, role, content, active) \
             VALUES (?1, ?2, ?3, 'system', ?4, 0)",
            (msg_id.as_str(), session_id, max_seq + 1, label),
        )?;
        conn.execute(
            "INSERT INTO usage (id, message_id, input_tokens, output_tokens, cost_usd) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                forge_types::new_id(),
                msg_id.as_str(),
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cost_usd,
            ),
        )?;
        conn.execute(
            "UPDATE session SET total_cost_usd = total_cost_usd + ?1, \
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

    /// `(input_tokens, output_tokens)` summed across a session's `usage` rows — the live token
    /// counter (tui-token-counter.md).
    pub fn session_tokens(&self, session_id: &str) -> Result<(u64, u64)> {
        let conn = self.lock()?;
        let (i, o): (i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(u.input_tokens), 0), COALESCE(SUM(u.output_tokens), 0)
             FROM usage u JOIN message m ON m.id = u.message_id
             WHERE m.session_id = ?1",
            [session_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok((i.max(0) as u64, o.max(0) as u64))
    }

    /// Number of provider calls (model steps) recorded in a session — one `usage` row per call.
    /// The Lattice benchmark uses this as the "steps" metric: fewer tool-exploration round-trips
    /// means fewer steps and fewer tokens.
    pub fn session_step_count(&self, session_id: &str) -> Result<u64> {
        let conn = self.lock()?;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM usage u JOIN message m ON m.id = u.message_id
             WHERE m.session_id = ?1",
            [session_id],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
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

    /// Per-model spend + token counts for the current calendar day.
    /// Returns `Vec<(model, cost_usd, input_tokens, output_tokens)>`, sorted by cost desc.
    /// Rows where `message.model` is NULL (side calls like compact/diagnose) are grouped under
    /// the empty string.
    pub fn spend_by_model_today(&self) -> Result<Vec<(String, f64, u64, u64)>> {
        let (s, e) = day_bounds_local(chrono::Local::now());
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(m.model, '') as mdl,
                    COALESCE(SUM(u.cost_usd), 0.0),
                    COALESCE(SUM(u.input_tokens), 0),
                    COALESCE(SUM(u.output_tokens), 0)
             FROM usage u JOIN message m ON m.id = u.message_id
             WHERE u.created_at >= ?1 AND u.created_at < ?2
             GROUP BY mdl
             ORDER BY SUM(u.cost_usd) DESC, SUM(u.input_tokens) DESC",
        )?;
        let rows = stmt.query_map((s, e), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Spend in the last 5 hours (rolling, not calendar-day-aligned).
    pub fn spend_last_5h_usd(&self) -> Result<f64> {
        let (s, e) = rolling_hours_bounds(chrono::Local::now(), 5);
        self.spend_between(s, e)
    }

    /// Spend in the current local ISO calendar week (Monday 00:00 → now).
    pub fn spend_this_week_usd(&self) -> Result<f64> {
        let (s, e) = week_bounds_local(chrono::Local::now());
        self.spend_between(s, e)
    }

    /// Per-model spend + token counts for the last 5 hours.
    pub fn spend_by_model_5h(&self) -> Result<Vec<(String, f64, u64, u64)>> {
        let (s, e) = rolling_hours_bounds(chrono::Local::now(), 5);
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(m.model, '') as mdl,
                    COALESCE(SUM(u.cost_usd), 0.0),
                    COALESCE(SUM(u.input_tokens), 0),
                    COALESCE(SUM(u.output_tokens), 0)
             FROM usage u JOIN message m ON m.id = u.message_id
             WHERE u.created_at >= ?1 AND u.created_at < ?2
             GROUP BY mdl
             ORDER BY SUM(u.cost_usd) DESC, SUM(u.input_tokens) DESC",
        )?;
        let rows = stmt.query_map((s, e), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Per-model spend + token counts for the current ISO week.
    pub fn spend_by_model_week(&self) -> Result<Vec<(String, f64, u64, u64)>> {
        let (s, e) = week_bounds_local(chrono::Local::now());
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(m.model, '') as mdl,
                    COALESCE(SUM(u.cost_usd), 0.0),
                    COALESCE(SUM(u.input_tokens), 0),
                    COALESCE(SUM(u.output_tokens), 0)
             FROM usage u JOIN message m ON m.id = u.message_id
             WHERE u.created_at >= ?1 AND u.created_at < ?2
             GROUP BY mdl
             ORDER BY SUM(u.cost_usd) DESC, SUM(u.input_tokens) DESC",
        )?;
        let rows = stmt.query_map((s, e), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Per-model spend + token counts for the current calendar month.
    pub fn spend_by_model_month(&self) -> Result<Vec<(String, f64, u64, u64)>> {
        let (s, e) = month_bounds_local(chrono::Local::now());
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT COALESCE(m.model, '') as mdl,
                    COALESCE(SUM(u.cost_usd), 0.0),
                    COALESCE(SUM(u.input_tokens), 0),
                    COALESCE(SUM(u.output_tokens), 0)
             FROM usage u JOIN message m ON m.id = u.message_id
             WHERE u.created_at >= ?1 AND u.created_at < ?2
             GROUP BY mdl
             ORDER BY SUM(u.cost_usd) DESC, SUM(u.input_tokens) DESC",
        )?;
        let rows = stmt.query_map((s, e), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, i64>(3)? as u64,
            ))
        })?;
        Ok(rows.filter_map(|r| r.ok()).collect())
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

    /// Exclude a model that failed *permanently* (no tool-calling support, unaffordable, malformed
    /// tool payload — see [`ProviderError::Capability`](forge_provider::ProviderError::Capability)).
    /// Modeled as a long bench window so it reuses the `model_health` table and naturally
    /// *re-probes* after the window elapses (a provider may add tool support later). The reason is
    /// prefixed `excluded:` so the UI / report can distinguish it from a transient bench.
    pub fn exclude_model(&self, model: &str, reason: &str) -> Result<()> {
        let until = chrono::Utc::now().timestamp() + CAPABILITY_EXCLUSION_SECS;
        self.bench_model(model, until, &format!("excluded: {reason}"))
    }

    /// The non-excluded model whose bench expires soonest (the "least dead" model), as a
    /// last-resort fallback when every routable model is currently benched but none is a permanent
    /// capability exclusion. `None` when nothing is benched or all benches are permanent
    /// exclusions. Used by the core loop so a turn never hard-fails while a transient bench exists.
    pub fn soonest_unbenched(&self) -> Result<Option<String>> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT model FROM model_health
                 WHERE reason NOT LIKE 'excluded:%'
                 ORDER BY cooldown_until ASC LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        Ok(row)
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

    /// Persist a model's fetched context window (tokens), from a provider's model API. Upsert so a
    /// later discovery refreshes it.
    pub fn set_model_context(&self, model: &str, window: u32) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO model_context (model, window, updated_at) VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(model) DO UPDATE SET window = excluded.window, updated_at = excluded.updated_at",
            (model, window),
        )?;
        Ok(())
    }

    /// A model's fetched context window (tokens), or `None` if we never stored one. The core
    /// prefers this over the family heuristic when bounding a turn's transcript.
    pub fn model_context(&self, model: &str) -> Result<Option<u32>> {
        let row = self
            .lock()?
            .query_row(
                "SELECT window FROM model_context WHERE model = ?1",
                [model],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(row.map(|w| w.max(0) as u32))
    }

    /// Persist a model's fetched USD price (per 1k tokens), from a provider's model API. Upsert so a
    /// later discovery refreshes it. `cache_read_per_1k` is the discounted prompt-cache-read rate
    /// (None if the provider didn't report one).
    pub fn set_model_pricing(
        &self,
        model: &str,
        input_per_1k: f64,
        output_per_1k: f64,
        cache_read_per_1k: Option<f64>,
    ) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO model_pricing (model, input_per_1k, output_per_1k, cache_read_per_1k, updated_at)
             VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
             ON CONFLICT(model) DO UPDATE SET input_per_1k = excluded.input_per_1k,
                 output_per_1k = excluded.output_per_1k, cache_read_per_1k = excluded.cache_read_per_1k,
                 updated_at = excluded.updated_at",
            (model, input_per_1k, output_per_1k, cache_read_per_1k),
        )?;
        Ok(())
    }

    /// Every fetched per-model price: `model -> (input_per_1k, output_per_1k, cache_read_per_1k)` in
    /// USD. Fed into the mesh's `Pricing` as overrides so gateway/credit spend is tracked, not $0.
    pub fn all_model_pricing(&self) -> Result<Vec<ModelPriceRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT model, input_per_1k, output_per_1k, cache_read_per_1k FROM model_pricing",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Clear every model bench (the `forge models --clear` rescan reset). Returns the number of
    /// benched rows removed so the caller can report it.
    pub fn clear_all_model_health(&self) -> Result<usize> {
        Ok(self.lock()?.execute("DELETE FROM model_health", [])?)
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

    /// Record the latest subscription quota observation (quota-aware routing, L3). One row per
    /// bridge provider, upserted — the most recent `rate_limit_event` wins.
    pub fn record_quota(&self, hint: &forge_types::QuotaHint) -> Result<()> {
        let status = match hint.status {
            forge_types::QuotaStatus::Ok => "ok",
            forge_types::QuotaStatus::Warning => "warning",
            forge_types::QuotaStatus::Exhausted => "exhausted",
        };
        self.lock()?.execute(
            "INSERT INTO subscription_usage (provider, window_kind, status, resets_at, fraction, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))
             ON CONFLICT(provider, window_kind) DO UPDATE SET
               status = excluded.status,
               resets_at = excluded.resets_at,
               fraction = excluded.fraction,
               updated_at = excluded.updated_at",
            (
                hint.provider.as_str(),
                hint.window.as_str(),
                status,
                hint.resets_at,
                hint.fraction_used,
            ),
        )?;
        Ok(())
    }

    /// Replace a session's task list (the `update_tasks` tool). Stored as one JSON row so a
    /// resumed session restores its tasks. An empty list clears it.
    pub fn set_tasks(&self, session_id: &str, tasks: &[forge_types::TodoItem]) -> Result<()> {
        let json = serde_json::to_string(tasks).unwrap_or_else(|_| "[]".to_string());
        self.lock()?.execute(
            "INSERT INTO session_tasks (session_id, tasks_json, updated_at)
             VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(session_id) DO UPDATE SET
               tasks_json = excluded.tasks_json, updated_at = excluded.updated_at",
            (session_id, json),
        )?;
        Ok(())
    }

    /// The session's persisted task list (empty if none/unparseable).
    pub fn tasks(&self, session_id: &str) -> Result<Vec<forge_types::TodoItem>> {
        let conn = self.lock()?;
        let json: Option<String> = conn
            .query_row(
                "SELECT tasks_json FROM session_tasks WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok();
        Ok(json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default())
    }

    /// Snapshot of currently-constraining subscription quotas (rows whose window hasn't reset),
    /// for the router. Only `Warning`/`Exhausted` providers are carried — `Ok` is the default.
    pub fn current_quota(&self) -> Result<forge_types::SubscriptionQuota> {
        self.quota_at(chrono::Utc::now().timestamp())
    }

    /// Seconds since the most recent quota update for `provider` (`None` if never recorded). Used
    /// to gate the on-demand claude rate-limit probe so it refreshes at most every few minutes.
    pub fn subscription_age_secs(&self, provider: &str) -> Option<i64> {
        let conn = self.lock().ok()?;
        let updated: Option<i64> = conn
            .query_row(
                "SELECT MAX(updated_at) FROM subscription_usage WHERE provider = ?1",
                [provider],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        updated.map(|u| chrono::Utc::now().timestamp() - u)
    }

    /// Per-provider, per-window fraction from `subscription_usage` (for display).
    /// Only returns non-stale rows (window hasn't reset yet or has no reset time).
    /// Returns `HashMap<provider, HashMap<window_kind, fraction>>`.
    pub fn bridge_fractions(
        &self,
    ) -> Result<std::collections::HashMap<String, std::collections::HashMap<String, f64>>> {
        let now = chrono::Utc::now().timestamp();
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT provider, window_kind, fraction FROM subscription_usage
         WHERE fraction IS NOT NULL AND (resets_at IS NULL OR resets_at > ?1)",
        )?;
        let mut out: std::collections::HashMap<String, std::collections::HashMap<String, f64>> =
            std::collections::HashMap::new();
        let rows = stmt.query_map([now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?;
        for row in rows.flatten() {
            out.entry(row.0).or_default().insert(row.1, row.2);
        }
        Ok(out)
    }

    /// [`current_quota`](Self::current_quota) at an explicit `now` (epoch secs) — testable clock.
    pub fn quota_at(&self, now: i64) -> Result<forge_types::SubscriptionQuota> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT provider,
                   CASE MAX(CASE status WHEN 'exhausted' THEN 2 WHEN 'warning' THEN 1 ELSE 0 END)
                       WHEN 2 THEN 'exhausted'
                       WHEN 1 THEN 'warning'
                       ELSE 'ok'
                   END
             FROM subscription_usage
             WHERE resets_at IS NULL OR resets_at > ?1
             GROUP BY provider",
        )?;
        let map = stmt
            .query_map([now], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(std::result::Result::ok)
            .filter_map(|(provider, status)| {
                let st = match status.as_str() {
                    "warning" => forge_types::QuotaStatus::Warning,
                    "exhausted" => forge_types::QuotaStatus::Exhausted,
                    _ => forge_types::QuotaStatus::Ok,
                };
                (st != forge_types::QuotaStatus::Ok).then_some((provider, st))
            })
            .collect::<std::collections::HashMap<_, _>>();
        // Also carry the strictest fraction per provider (incl. still-Ok ones) so the router's
        // graduated conservation can spread off a subscription before it crosses Warning.
        let mut frac_stmt = conn.prepare(
            "SELECT provider, MAX(fraction) FROM subscription_usage
             WHERE fraction IS NOT NULL AND (resets_at IS NULL OR resets_at > ?1)
             GROUP BY provider",
        )?;
        let fractions = frac_stmt
            .query_map([now], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })?
            .filter_map(std::result::Result::ok)
            .collect::<std::collections::HashMap<_, _>>();
        Ok(forge_types::SubscriptionQuota::new(map).with_fractions(fractions))
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
    /// `/undo` rewound past) are excluded — they remain in the table for audit/redo. If a
    /// compaction summary exists (written by [`compact_session_store`](Self::compact_session_store)),
    /// a synthetic System message is prepended so a resumed session sees the compacted view.
    pub fn load_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let conn = self.lock()?;
        // Read compaction summary before the message prepare (both are &self borrows; ordering
        // keeps the non-mut borrow from query_row from conflicting with the stmt lifetime).
        let summary: Option<String> = conn
            .query_row(
                "SELECT summary FROM session_compaction WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .ok();
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
        let mut msgs = rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)?;
        if let Some(s) = summary {
            msgs.insert(
                0,
                StoredMessage {
                    role: Role::System,
                    content: format!(
                        "[Earlier conversation summarized to save context]\n{}",
                        s.trim()
                    ),
                    model: None,
                    tool_calls: vec![],
                    tool_call_id: None,
                },
            );
        }
        Ok(msgs)
    }

    /// Persist the compacted view of a session: soft-delete the oldest active messages (keeping
    /// the last `keep_count`) and upsert `summary` into `session_compaction`. On the next resume,
    /// [`load_messages`](Self::load_messages) prepends a System message with the summary so the
    /// session rehydrates the compacted state instead of the full transcript.
    pub fn compact_session_store(
        &self,
        session_id: &str,
        summary: &str,
        keep_count: usize,
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        if keep_count == 0 {
            tx.execute(
                "UPDATE message SET active = 0 WHERE session_id = ?1 AND active = 1",
                [session_id],
            )?;
        } else {
            // Soft-delete every active message whose seq is below the (keep_count)-th newest.
            // LIMIT 1 OFFSET (keep_count-1) on DESC order gives the oldest row to KEEP.
            tx.execute(
                "UPDATE message SET active = 0
                 WHERE session_id = ?1 AND active = 1
                 AND seq < (
                     SELECT seq FROM message
                     WHERE session_id = ?1 AND active = 1
                     ORDER BY seq DESC
                     LIMIT 1 OFFSET ?2
                 )",
                (session_id, keep_count as i64 - 1),
            )?;
        }
        tx.execute(
            "INSERT INTO session_compaction (session_id, summary) VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET
               summary = excluded.summary,
               created_at = strftime('%s','now')",
            (session_id, summary),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Every active message of a session in turn order, each joined to its usage row so a
    /// replay can show the model, token counts, cost, and wall-clock time of each turn
    /// (docs/features/session-replay.md). Unlike [`load_messages`](Self::load_messages) this
    /// is for auditing a finished session, not rebuilding live state.
    pub fn load_replay(&self, session_id: &str) -> Result<Vec<ReplayEntry>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT m.seq, m.role, m.content, m.model, m.created_at, m.tool_calls_json,
                    u.input_tokens, u.output_tokens, u.cost_usd
             FROM message m LEFT JOIN usage u ON u.message_id = m.id
             WHERE m.session_id = ?1 AND m.active = 1 ORDER BY m.seq",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            let role: String = row.get(1)?;
            let tool_calls_json: Option<String> = row.get(5)?;
            let tool_calls = tool_calls_json
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            Ok(ReplayEntry {
                seq: row.get(0)?,
                role: Role::parse(&role).unwrap_or(Role::User),
                content: row.get(2)?,
                model: row.get(3)?,
                created_at: row.get(4)?,
                tool_calls,
                input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                cost_usd: row.get(8)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    // --- Assay runs + findings (docs/features/analysis-mode.md) ---

    /// Persist an assay run; returns its id. Add findings with [`add_finding`](Self::add_finding).
    pub fn create_assay_run(&self, scope: &str, cost_usd: f64) -> Result<String> {
        let id = forge_types::new_id();
        self.lock()?.execute(
            "INSERT INTO assay_run (id, scope, cost_usd) VALUES (?1, ?2, ?3)",
            (&id, scope, cost_usd),
        )?;
        Ok(id)
    }

    /// Persist one finding under a run.
    pub fn add_finding(&self, run_id: &str, f: &forge_types::Finding) -> Result<()> {
        self.lock()?.execute(
            "INSERT INTO finding (id, run_id, category, severity, confidence, file, line, title,
             rationale, suggested_fix, effort, lens, verified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::params![
                f.id,
                run_id,
                f.category.as_str(),
                f.severity.as_str(),
                f.confidence.as_str(),
                f.file,
                f.line,
                f.title,
                f.rationale,
                f.suggested_fix,
                f.effort.as_str(),
                f.lens,
                f.verified as i64,
            ],
        )?;
        Ok(())
    }

    /// Findings of a run, ranked (severity, confidence) at read time.
    pub fn load_findings(&self, run_id: &str) -> Result<Vec<forge_types::Finding>> {
        use forge_types::{Confidence, Effort, FindingCategory, Severity};
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, category, severity, confidence, file, line, title, rationale,
                    suggested_fix, effort, lens, verified
             FROM finding WHERE run_id = ?1",
        )?;
        let rows = stmt.query_map([run_id], |row| {
            let category: String = row.get(1)?;
            let severity: String = row.get(2)?;
            let confidence: String = row.get(3)?;
            let effort: String = row.get(9)?;
            Ok(forge_types::Finding {
                id: row.get(0)?,
                category: FindingCategory::parse(&category).unwrap_or(FindingCategory::Correctness),
                severity: Severity::parse(&severity).unwrap_or(Severity::Low),
                confidence: Confidence::parse(&confidence).unwrap_or(Confidence::Low),
                file: row.get(4)?,
                line: row.get(5)?,
                title: row.get(6)?,
                rationale: row.get(7)?,
                suggested_fix: row.get(8)?,
                effort: Effort::parse(&effort).unwrap_or(Effort::Small),
                lens: row.get(10)?,
                verified: row.get::<_, i64>(11)? != 0,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// The most recent assay run for `scope`, excluding `exclude_id` (the just-created run).
    /// Returns `None` when this is the first run for this scope.
    pub fn latest_run_for_scope(&self, scope: &str, exclude_id: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id FROM assay_run WHERE scope = ?1 AND id != ?2
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )?;
        let mut rows = stmt.query([scope, exclude_id])?;
        Ok(rows.next()?.map(|r| r.get(0)).transpose()?)
    }

    /// Past assay runs, newest first: `(id, scope, cost_usd, created_at)`.
    pub fn list_assay_runs(&self) -> Result<Vec<(String, String, f64, i64)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, scope, cost_usd, created_at FROM assay_run ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
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

/// One message of a session enriched with its usage row, for `forge replay`. The token/cost
/// fields are `None` for messages that never produced a usage record (user/tool messages, or
/// assistant turns from before usage tracking existed).
#[derive(Debug, Clone)]
pub struct ReplayEntry {
    pub seq: i64,
    pub role: Role,
    pub content: String,
    pub model: Option<String>,
    pub created_at: i64,
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
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

// ---- Lattice: code-intelligence graph (code-intelligence.md) ----

/// A persisted source-file row in the Lattice graph.
#[derive(Debug, Clone)]
pub struct LatticeFileRow {
    pub id: String,
    pub repo_root: String,
    pub rel_path: String,
    pub lang: String,
    pub content_hash: String,
    pub parse_status: String,
}

/// A persisted symbol node.
#[derive(Debug, Clone)]
pub struct LatticeNodeRow {
    pub id: String,
    pub file_id: String,
    pub kind: String,
    pub name: String,
    pub qualname: Option<String>,
    pub signature: Option<String>,
    pub span_start: i64,
    pub span_end: i64,
    pub line_start: i64,
    pub pagerank: f64,
}

/// A persisted relationship edge.
#[derive(Debug, Clone)]
pub struct LatticeEdgeRow {
    pub id: String,
    pub src_id: String,
    pub dst_id: String,
    pub kind: String,
    pub unresolved_name: Option<String>,
}

/// A persisted reference / call site (resolved to a node by name-join at query time).
#[derive(Debug, Clone)]
pub struct LatticeRefRow {
    pub id: String,
    pub src_id: String,
    pub name: String,
    pub kind: String,
    pub line: i64,
}

/// Read a [`LatticeNodeRow`] from the first 10 columns of a row (id, file_id, kind, name, qualname,
/// signature, span_start, span_end, line_start, pagerank).
fn lattice_node_from_row(r: &rusqlite::Row) -> rusqlite::Result<LatticeNodeRow> {
    Ok(LatticeNodeRow {
        id: r.get(0)?,
        file_id: r.get(1)?,
        kind: r.get(2)?,
        name: r.get(3)?,
        qualname: r.get(4)?,
        signature: r.get(5)?,
        span_start: r.get(6)?,
        span_end: r.get(7)?,
        line_start: r.get(8)?,
        pagerank: r.get(9).unwrap_or(0.0),
    })
}

impl Store {
    /// The stored content hash for a file, or `None` if it hasn't been indexed — the
    /// incremental-update gate (skip files whose hash is unchanged).
    pub fn lattice_file_hash(&self, repo_root: &str, rel_path: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        let hash = conn
            .query_row(
                "SELECT content_hash FROM lattice_file WHERE repo_root = ?1 AND rel_path = ?2",
                (repo_root, rel_path),
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(hash)
    }

    /// Insert or replace a file's row and its symbol nodes + edges atomically: the file's prior
    /// nodes are deleted first (cascading their edges), so re-indexing is idempotent.
    pub fn replace_lattice_file(
        &self,
        file: &LatticeFileRow,
        nodes: &[LatticeNodeRow],
        edges: &[LatticeEdgeRow],
        refs: &[LatticeRefRow],
    ) -> Result<()> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO lattice_file (id, repo_root, rel_path, lang, content_hash, parse_status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                content_hash = excluded.content_hash,
                lang = excluded.lang,
                parse_status = excluded.parse_status,
                indexed_at = strftime('%s','now')",
            (
                &file.id,
                &file.repo_root,
                &file.rel_path,
                &file.lang,
                &file.content_hash,
                &file.parse_status,
            ),
        )?;
        // Replace the file's symbols (FK ON DELETE CASCADE clears their edges too).
        tx.execute("DELETE FROM lattice_node WHERE file_id = ?1", (&file.id,))?;
        for n in nodes {
            tx.execute(
                "INSERT INTO lattice_node
                   (id, file_id, kind, name, qualname, signature, span_start, span_end, line_start, pagerank)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0.0)",
                rusqlite::params![
                    n.id,
                    n.file_id,
                    n.kind,
                    n.name,
                    n.qualname,
                    n.signature,
                    n.span_start,
                    n.span_end,
                    n.line_start,
                ],
            )?;
        }
        for e in edges {
            tx.execute(
                "INSERT INTO lattice_edge (id, src_id, dst_id, kind, unresolved_name)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![e.id, e.src_id, e.dst_id, e.kind, e.unresolved_name],
            )?;
        }
        for r in refs {
            tx.execute(
                "INSERT INTO lattice_ref (id, src_id, name, kind, line)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![r.id, r.src_id, r.name, r.kind, r.line],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Distinct definitions that reference `name` — the direct callers/dependents of a symbol
    /// (one hop of `impact`). Resolves the name-keyed `lattice_ref` rows back to their src nodes.
    pub fn lattice_callers_by_name(&self, name: &str, limit: usize) -> Result<Vec<LatticeNodeRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT n.id, n.file_id, n.kind, n.name, n.qualname, n.signature,
                    n.span_start, n.span_end, n.line_start, n.pagerank
             FROM lattice_ref r
             JOIN lattice_node n ON n.id = r.src_id
             WHERE r.name = ?1 AND n.name <> ?1
             ORDER BY n.name
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![name, limit as i64], lattice_node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct identifier names referenced *by* definitions named `name` — one forward hop for
    /// `path` BFS (what the symbol calls/uses).
    pub fn lattice_callees_of_name(&self, name: &str) -> Result<Vec<String>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT r.name
             FROM lattice_ref r
             JOIN lattice_node n ON n.id = r.src_id
             WHERE n.name = ?1 AND r.name <> ?1",
        )?;
        let rows = stmt
            .query_map([name], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Total reference rows — completes the `status` summary.
    pub fn lattice_ref_count(&self) -> Result<i64> {
        let conn = self.lock()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM lattice_ref", [], |r| r.get(0))?)
    }

    /// Symbols whose name contains `query` (case-insensitive), best-first: exact name, then
    /// prefix, then substring; capped at `limit`.
    pub fn lattice_nodes_by_name(&self, query: &str, limit: usize) -> Result<Vec<LatticeNodeRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT n.id, n.file_id, n.kind, n.name, n.qualname, n.signature,
                    n.span_start, n.span_end, n.line_start, n.pagerank,
                    CASE
                        WHEN lower(n.name) = lower(?1) THEN 0
                        WHEN lower(n.name) LIKE lower(?1) || '%' THEN 1
                        ELSE 2
                    END AS rank
             FROM lattice_node n
             WHERE lower(n.name) LIKE '%' || lower(?1) || '%'
             ORDER BY rank, length(n.name), n.name
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![query, limit as i64], |r| {
                Ok(LatticeNodeRow {
                    id: r.get(0)?,
                    file_id: r.get(1)?,
                    kind: r.get(2)?,
                    name: r.get(3)?,
                    qualname: r.get(4)?,
                    signature: r.get(5)?,
                    span_start: r.get(6)?,
                    span_end: r.get(7)?,
                    line_start: r.get(8)?,
                    pagerank: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A single node row by id — used to resolve embedding-ranked node ids back to nodes.
    pub fn lattice_node_by_id(&self, id: &str) -> Result<Option<LatticeNodeRow>> {
        let conn = self.lock()?;
        match conn.query_row(
            "SELECT id, file_id, kind, name, qualname, signature, span_start, span_end, line_start, pagerank
             FROM lattice_node WHERE id = ?1",
            [id],
            lattice_node_from_row,
        ) {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// The `rel_path` of an indexed file by its id (for rendering a node's location).
    pub fn lattice_file_path(&self, file_id: &str) -> Result<Option<String>> {
        let conn = self.lock()?;
        Ok(conn
            .query_row(
                "SELECT rel_path FROM lattice_file WHERE id = ?1",
                (file_id,),
                |r| r.get::<_, String>(0),
            )
            .ok())
    }

    /// `(files, nodes, edges)` row counts — the `forge lattice status` summary.
    pub fn lattice_counts(&self) -> Result<(i64, i64, i64)> {
        let conn = self.lock()?;
        let files = conn.query_row("SELECT COUNT(*) FROM lattice_file", [], |r| r.get(0))?;
        let nodes = conn.query_row("SELECT COUNT(*) FROM lattice_node", [], |r| r.get(0))?;
        let edges = conn.query_row("SELECT COUNT(*) FROM lattice_edge", [], |r| r.get(0))?;
        Ok((files, nodes, edges))
    }

    /// Upsert a node's embedding vector (semantic retrieval, code-intelligence.md §5.6). `vec` is
    /// stored as little-endian f32 components.
    pub fn put_lattice_embedding(&self, node_id: &str, vec: &[f32]) -> Result<()> {
        let mut bytes = Vec::with_capacity(vec.len() * 4);
        for f in vec {
            bytes.extend_from_slice(&f.to_le_bytes());
        }
        self.lock()?.execute(
            "INSERT INTO lattice_embedding (node_id, dim, vec) VALUES (?1, ?2, ?3)
             ON CONFLICT(node_id) DO UPDATE SET dim = excluded.dim, vec = excluded.vec",
            rusqlite::params![node_id, vec.len() as i64, bytes],
        )?;
        Ok(())
    }

    /// Nodes that don't yet have an embedding — the work list for incremental `embed_pending`.
    pub fn lattice_nodes_without_embedding(&self, limit: usize) -> Result<Vec<LatticeNodeRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT n.id, n.file_id, n.kind, n.name, n.qualname, n.signature,
                    n.span_start, n.span_end, n.line_start, n.pagerank
             FROM lattice_node n
             LEFT JOIN lattice_embedding e ON e.node_id = n.id
             WHERE e.node_id IS NULL
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], lattice_node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All stored `(node_id, vector)` embeddings — loaded once to cosine-rank a query vector.
    pub fn lattice_embeddings(&self) -> Result<Vec<(String, Vec<f32>)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT node_id, vec FROM lattice_embedding")?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            let vec = blob
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            Ok((id, vec))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// How many nodes currently have an embedding (`forge lattice status`: "embeddings: N").
    pub fn lattice_embedding_count(&self) -> Result<i64> {
        Ok(self
            .lock()?
            .query_row("SELECT COUNT(*) FROM lattice_embedding", [], |r| r.get(0))?)
    }

    /// All (src_id, dst_name) pairs from lattice_ref — the directed reference graph for PageRank.
    /// `src_id` is the referencing node's id; `dst_name` is the referenced identifier (resolved to
    /// node ids by name-join at call time). Returns (src_node_id, referenced_name) pairs.
    pub fn lattice_ref_edges(&self) -> Result<Vec<(String, String)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT src_id, name FROM lattice_ref")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All nodes ordered by pagerank descending, capped at `limit` — the repo-map selection query.
    /// Returns the top-N most important symbols across all files in the index; the caller applies
    /// a token-budget cutoff. Use `usize::MAX` to retrieve every node (for small repos).
    pub fn lattice_nodes_ranked(&self, limit: usize) -> Result<Vec<LatticeNodeRow>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT n.id, n.file_id, n.kind, n.name, n.qualname, n.signature,
                    n.span_start, n.span_end, n.line_start, n.pagerank
             FROM lattice_node n
             ORDER BY n.pagerank DESC
             LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit as i64], lattice_node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// All (node_id, node_name) pairs — needed to resolve reference names to node ids for PageRank.
    pub fn lattice_node_ids_and_names(&self) -> Result<Vec<(String, String)>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT id, name FROM lattice_node")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Batch-update pagerank scores: for each `(node_id, score)` pair, set `pagerank = score`.
    /// Runs inside one transaction for performance.
    pub fn set_lattice_pageranks(&self, scores: &[(String, f64)]) -> Result<()> {
        if scores.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE lattice_node SET pagerank = ?2 WHERE id = ?1")?;
            for (id, score) in scores {
                stmt.execute(rusqlite::params![id, score])?;
            }
        }
        tx.commit()?;
        Ok(())
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
                    cached_input_tokens: 0,
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
                    cached_input_tokens: 0,
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
    fn load_replay_joins_usage_and_orders_by_seq() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        store.add_message(&sid, 0, Role::User, "ask", None).unwrap();
        let mid = store
            .add_message(&sid, 1, Role::Assistant, "answer", Some("openai::gpt-4o"))
            .unwrap();
        store
            .record_usage(
                &sid,
                &mid,
                &Usage {
                    input_tokens: 12,
                    output_tokens: 7,
                    cached_input_tokens: 0,
                    cost_usd: 0.03,
                },
            )
            .unwrap();

        let entries = store.load_replay(&sid).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 0);
        assert_eq!(entries[0].role, Role::User);
        assert!(entries[0].cost_usd.is_none(), "user turn has no usage row");
        assert_eq!(entries[1].model.as_deref(), Some("openai::gpt-4o"));
        assert_eq!(entries[1].input_tokens, Some(12));
        assert!((entries[1].cost_usd.unwrap() - 0.03).abs() < 1e-9);
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

    // --- Assay runs + findings ---

    #[test]
    fn assay_run_and_findings_round_trip() {
        use forge_types::{Confidence, Effort, Finding, FindingCategory, Severity};
        let store = Store::open_in_memory().unwrap();
        let run = store.create_assay_run("repo", 0.12).unwrap();
        let f = Finding {
            id: forge_types::new_id(),
            category: FindingCategory::Correctness,
            severity: Severity::Critical,
            confidence: Confidence::High,
            file: "core/lib.rs".into(),
            line: Some(204),
            title: "unwrap on provider result panics the turn".into(),
            rationale: "a transient 5xx aborts the session".into(),
            suggested_fix: "propagate via ?".into(),
            effort: Effort::Small,
            lens: "correctness".into(),
            verified: true,
        };
        store.add_finding(&run, &f).unwrap();

        let loaded = store.load_findings(&run).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], f, "finding round-trips through the store");

        let runs = store.list_assay_runs().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, run);
        assert_eq!(runs[0].1, "repo");
        assert!((runs[0].2 - 0.12).abs() < 1e-9);
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
    fn session_tasks_round_trip_and_replace() {
        use forge_types::{TodoItem, TodoStatus};
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        assert!(store.tasks(&sid).unwrap().is_empty(), "none initially");

        let tasks = vec![
            TodoItem {
                title: "write the parser".into(),
                status: TodoStatus::Done,
            },
            TodoItem {
                title: "wire it up".into(),
                status: TodoStatus::InProgress,
            },
        ];
        store.set_tasks(&sid, &tasks).unwrap();
        assert_eq!(store.tasks(&sid).unwrap(), tasks, "round-trips");

        // A second write replaces the list wholesale.
        let next = vec![TodoItem {
            title: "ship".into(),
            status: TodoStatus::Pending,
        }];
        store.set_tasks(&sid, &next).unwrap();
        assert_eq!(store.tasks(&sid).unwrap(), next, "replaced, not appended");
    }

    #[test]
    fn compact_session_store_prepends_summary_on_resume() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        for i in 0..8i64 {
            store
                .add_message(&sid, i, Role::User, &format!("msg {i}"), None)
                .unwrap();
        }

        // Keep the last 3, summarize the first 5.
        store
            .compact_session_store(&sid, "Summary of first 5 messages.", 3)
            .unwrap();

        let msgs = store.load_messages(&sid).unwrap();
        // 1 summary + 3 kept = 4
        assert_eq!(msgs.len(), 4, "summary + 3 kept messages");
        assert_eq!(
            msgs[0].role,
            Role::System,
            "prepended summary is a System message"
        );
        assert!(
            msgs[0].content.contains("Summary of first 5 messages."),
            "summary content preserved"
        );
        assert_eq!(msgs[1].content, "msg 5");
        assert_eq!(msgs[2].content, "msg 6");
        assert_eq!(msgs[3].content, "msg 7");
    }

    #[test]
    fn compact_session_store_upserts_summary_on_second_compact() {
        let store = Store::open_in_memory().unwrap();
        let sid = store.create_session("/tmp", "default").unwrap();
        for i in 0..6i64 {
            store
                .add_message(&sid, i, Role::User, &format!("msg {i}"), None)
                .unwrap();
        }
        store
            .compact_session_store(&sid, "First summary.", 3)
            .unwrap();
        // Add 3 more messages (simulate new turns after first compact).
        for i in 6..9i64 {
            store
                .add_message(&sid, i, Role::User, &format!("msg {i}"), None)
                .unwrap();
        }
        store
            .compact_session_store(&sid, "Second summary.", 3)
            .unwrap();

        let msgs = store.load_messages(&sid).unwrap();
        assert_eq!(msgs.len(), 4, "summary + 3 kept after second compact");
        assert!(
            msgs[0].content.contains("Second summary."),
            "upserted summary"
        );
        assert_eq!(msgs[1].content, "msg 6");
        assert_eq!(msgs[2].content, "msg 7");
        assert_eq!(msgs[3].content, "msg 8");
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
    fn quota_is_upserted_and_expires_when_the_window_resets() {
        let store = Store::open_in_memory().unwrap();
        let hint = |status, resets_at| forge_types::QuotaHint {
            provider: "claude-cli".into(),
            window: "five_hour".into(),
            status,
            resets_at,
            fraction_used: None,
        };
        // A warning that resets at t=1000.
        store
            .record_quota(&hint(forge_types::QuotaStatus::Warning, Some(1000)))
            .unwrap();
        assert!(store.quota_at(500).unwrap().is_pressured("claude-cli"));
        // Past the reset → no longer constraining.
        assert!(!store.quota_at(2000).unwrap().is_pressured("claude-cli"));

        // Upsert to exhausted; an Ok provider isn't carried at all.
        store
            .record_quota(&hint(forge_types::QuotaStatus::Exhausted, Some(3000)))
            .unwrap();
        assert!(store.quota_at(500).unwrap().is_exhausted("claude-cli"));
        store
            .record_quota(&forge_types::QuotaHint {
                provider: "codex-cli".into(),
                window: String::new(),
                status: forge_types::QuotaStatus::Ok,
                resets_at: Some(9999),
                fraction_used: None,
            })
            .unwrap();
        assert!(!store.quota_at(500).unwrap().is_pressured("codex-cli"));
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
    fn model_context_round_trips_and_upserts() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.model_context("openrouter::x:free").unwrap(), None);
        store
            .set_model_context("openrouter::x:free", 131_072)
            .unwrap();
        assert_eq!(
            store.model_context("openrouter::x:free").unwrap(),
            Some(131_072)
        );
        // Upsert: a later fetch refreshes the window.
        store
            .set_model_context("openrouter::x:free", 65_536)
            .unwrap();
        assert_eq!(
            store.model_context("openrouter::x:free").unwrap(),
            Some(65_536)
        );
    }

    #[test]
    fn model_pricing_round_trips_and_upserts() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.all_model_pricing().unwrap().is_empty());
        store
            .set_model_pricing("openrouter::vendor/m", 0.0002, 0.0008, Some(0.00005))
            .unwrap();
        let rows = store.all_model_pricing().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "openrouter::vendor/m");
        assert!((rows[0].1 - 0.0002).abs() < 1e-12);
        assert!((rows[0].2 - 0.0008).abs() < 1e-12);
        assert!((rows[0].3.unwrap() - 0.00005).abs() < 1e-12);
        // Upsert refreshes in place, including clearing the cache-read rate.
        store
            .set_model_pricing("openrouter::vendor/m", 0.001, 0.002, None)
            .unwrap();
        let rows = store.all_model_pricing().unwrap();
        assert_eq!(rows.len(), 1);
        assert!((rows[0].1 - 0.001).abs() < 1e-12);
        assert!(rows[0].3.is_none());
    }

    #[test]
    fn exclude_model_benches_long_and_soonest_skips_exclusions() {
        let store = Store::open_in_memory().unwrap();
        let now = chrono::Utc::now().timestamp();

        // A permanent exclusion: benched far into the future, reason prefixed "excluded:".
        store
            .exclude_model("dead::no-tools", "no tool calling")
            .unwrap();
        assert!(
            store
                .current_benched()
                .unwrap()
                .is_benched("dead::no-tools"),
            "excluded model is benched now"
        );
        let report = store.current_benched_report().unwrap();
        let row = report
            .iter()
            .find(|(m, _, _)| m == "dead::no-tools")
            .unwrap();
        assert!(
            row.1 > now + 6 * 24 * 60 * 60,
            "exclusion window is ~7 days"
        );
        assert!(row.2.starts_with("excluded:"));

        // A transient bench alongside it.
        store
            .bench_for(
                "rl::model",
                std::time::Duration::from_secs(120),
                "rate-limited",
            )
            .unwrap();

        // soonest_unbenched returns the transient one, never the permanent exclusion.
        assert_eq!(
            store.soonest_unbenched().unwrap().as_deref(),
            Some("rl::model")
        );

        // With only exclusions left, there's no last-resort candidate.
        store.clear_model_health("rl::model").unwrap();
        assert_eq!(store.soonest_unbenched().unwrap(), None);
    }

    #[test]
    fn lattice_embedding_round_trips_and_upserts() {
        let store = Store::open_in_memory().unwrap();
        // A node row is required (FK). Insert one via the file-replace path.
        let file = LatticeFileRow {
            id: "f1".into(),
            repo_root: "/r".into(),
            rel_path: "a.rs".into(),
            lang: "rust".into(),
            content_hash: "h".into(),
            parse_status: "ok".into(),
        };
        let node = LatticeNodeRow {
            id: "n1".into(),
            file_id: "f1".into(),
            kind: "function".into(),
            name: "foo".into(),
            qualname: None,
            signature: None,
            span_start: 0,
            span_end: 1,
            line_start: 1,
            pagerank: 0.0,
        };
        store
            .replace_lattice_file(&file, &[node], &[], &[])
            .unwrap();

        store
            .put_lattice_embedding("n1", &[1.0, -0.5, 0.25])
            .unwrap();
        assert_eq!(store.lattice_embedding_count().unwrap(), 1);
        let all = store.lattice_embeddings().unwrap();
        assert_eq!(all, vec![("n1".to_string(), vec![1.0, -0.5, 0.25])]);
        // Upsert replaces, not duplicates.
        store.put_lattice_embedding("n1", &[2.0, 2.0]).unwrap();
        assert_eq!(store.lattice_embedding_count().unwrap(), 1);
        assert_eq!(store.lattice_embeddings().unwrap()[0].1, vec![2.0, 2.0]);
    }

    #[test]
    fn clear_all_model_health_wipes_every_bench() {
        let store = Store::open_in_memory().unwrap();
        store.bench_model("a", 2000, "rate-limited").unwrap();
        store.bench_model("b", 2000, "auth failed").unwrap();
        assert_eq!(store.clear_all_model_health().unwrap(), 2);
        assert!(store.benched_models(500).unwrap().is_empty());
        assert_eq!(store.clear_all_model_health().unwrap(), 0, "idempotent");
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
