//! Auto-memory: durable, cross-session facts persisted in the store and scoped per project (or
//! `global`), with keyword + salience + recency recall. The `memory` table is declared in
//! `schema.rs`. Recall ranks by keyword overlap with the current prompt, then salience, then
//! recency — so the mesh injects only the few MOST relevant memories, not every note (the edge over
//! a dump-everything memory file). Repeated facts auto-dedup and bump salience instead of piling up.

use std::collections::HashSet;

use crate::{Store, StoreError};

type Result<T> = std::result::Result<T, StoreError>;

/// A stored memory entry.
#[derive(Debug, Clone)]
pub struct Memory {
    pub id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub source_session: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub salience: f64,
}

/// Jaccard token-overlap at or above this counts two memory texts as the same fact (dedup + bump).
const DUP_JACCARD: f64 = 0.6;

impl Store {
    /// Add a memory, or — if a near-duplicate already exists in the same scope — bump that one's
    /// salience + recency instead of inserting a second copy (auto-curation). Returns the row id.
    pub fn add_memory(
        &self,
        scope: &str,
        kind: &str,
        text: &str,
        source_session: &str,
    ) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            return Err(StoreError::Pool("empty memory text".into()));
        }
        if let Some(existing) = self.find_duplicate_memory(scope, text)? {
            self.lock()?.execute(
                "UPDATE memory SET salience = min(1.0, salience + 0.1), \
                 updated_at = strftime('%s','now') WHERE id = ?1",
                [&existing],
            )?;
            return Ok(existing);
        }
        let id = forge_types::new_id();
        self.lock()?.execute(
            "INSERT INTO memory (id, scope, kind, text, source_session) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, scope, kind, text, source_session],
        )?;
        Ok(id)
    }

    /// The id of an existing memory in `scope` whose text is a near-duplicate of `text`, if any.
    fn find_duplicate_memory(&self, scope: &str, text: &str) -> Result<Option<String>> {
        let want = tokenize(text);
        if want.is_empty() {
            return Ok(None);
        }
        let conn = self.lock()?;
        let mut stmt = conn.prepare("SELECT id, text FROM memory WHERE scope = ?1")?;
        let rows = stmt.query_map([scope], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows.flatten() {
            if jaccard(&want, &tokenize(&row.1)) >= DUP_JACCARD {
                return Ok(Some(row.0));
            }
        }
        Ok(None)
    }

    /// All memories in a scope, most-salient + most-recent first.
    pub fn list_memories(&self, scope: &str) -> Result<Vec<Memory>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, scope, kind, text, source_session, created_at, updated_at, salience \
             FROM memory WHERE scope = ?1 ORDER BY salience DESC, updated_at DESC",
        )?;
        let rows = stmt.query_map([scope], row_to_memory)?;
        Ok(rows.filter_map(std::result::Result::ok).collect())
    }

    /// The `limit` memories in `scope` most RELEVANT to `query`: keyword overlap first, then
    /// salience, then recency. Empty `query` falls back to salience+recency order. This is what the
    /// turn-start recall injects — a few targeted memories, not the whole set.
    pub fn recall_memories(&self, scope: &str, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let q = tokenize(query);
        let mut all = self.list_memories(scope)?;
        if !q.is_empty() {
            all.sort_by(|a, b| {
                overlap(&q, &tokenize(&b.text))
                    .cmp(&overlap(&q, &tokenize(&a.text)))
                    .then(b.salience.total_cmp(&a.salience))
                    .then(b.updated_at.cmp(&a.updated_at))
            });
        }
        all.truncate(limit);
        Ok(all)
    }

    /// Memories in `scope` sharing at least one keyword with `query`, ranked by overlap (for
    /// `forge memory search`).
    pub fn search_memories(&self, scope: &str, query: &str, limit: usize) -> Result<Vec<Memory>> {
        let q = tokenize(query);
        let mut hits: Vec<Memory> = self
            .list_memories(scope)?
            .into_iter()
            .filter(|m| overlap(&q, &tokenize(&m.text)) > 0)
            .collect();
        hits.sort_by_key(|m| std::cmp::Reverse(overlap(&q, &tokenize(&m.text))));
        hits.truncate(limit);
        Ok(hits)
    }

    /// Delete one memory by id. `Ok(true)` if a row was removed.
    pub fn delete_memory(&self, id: &str) -> Result<bool> {
        Ok(self
            .lock()?
            .execute("DELETE FROM memory WHERE id = ?1", [id])?
            > 0)
    }

    /// Delete every memory in a scope. Returns how many were removed.
    pub fn clear_memories(&self, scope: &str) -> Result<usize> {
        Ok(self
            .lock()?
            .execute("DELETE FROM memory WHERE scope = ?1", [scope])?)
    }

    /// Number of memories stored in a scope.
    pub fn count_memories(&self, scope: &str) -> Result<i64> {
        Ok(self.lock()?.query_row(
            "SELECT COUNT(*) FROM memory WHERE scope = ?1",
            [scope],
            |r| r.get(0),
        )?)
    }
}

fn row_to_memory(r: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?,
        scope: r.get(1)?,
        kind: r.get(2)?,
        text: r.get(3)?,
        source_session: r.get(4)?,
        created_at: r.get(5)?,
        updated_at: r.get(6)?,
        salience: r.get(7)?,
    })
}

/// Lowercase alphanumeric tokens of length ≥3 (drops stop-ish short words and punctuation).
fn tokenize(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 3)
        .map(str::to_string)
        .collect()
}

fn overlap(a: &HashSet<String>, b: &HashSet<String>) -> usize {
    a.intersection(b).count()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let u = a.union(b).count();
    if u == 0 {
        0.0
    } else {
        a.intersection(b).count() as f64 / u as f64
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    #[test]
    fn add_list_and_dedup_bumps_salience() {
        let s = store();
        let id1 = s
            .add_memory(
                "proj",
                "preference",
                "user prefers tabs over spaces",
                "sess1",
            )
            .unwrap();
        // A near-duplicate fact updates the same row instead of inserting a second.
        let id2 = s
            .add_memory(
                "proj",
                "preference",
                "the user prefers tabs over spaces always",
                "sess2",
            )
            .unwrap();
        assert_eq!(id1, id2, "near-duplicate must update, not insert");
        assert_eq!(s.count_memories("proj").unwrap(), 1);
        // A distinct fact is a new row.
        s.add_memory("proj", "decision", "deploys happen on fridays", "sess1")
            .unwrap();
        assert_eq!(s.count_memories("proj").unwrap(), 2);
        // Scope isolation.
        assert_eq!(s.count_memories("other").unwrap(), 0);
    }

    #[test]
    fn recall_ranks_by_keyword_overlap() {
        let s = store();
        s.add_memory("p", "fact", "the database is postgres on port 5432", "x")
            .unwrap();
        s.add_memory("p", "fact", "the frontend uses react and vite", "x")
            .unwrap();
        let hits = s
            .recall_memories("p", "what database do we use", 1)
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].text.contains("postgres"),
            "most keyword-relevant first: {hits:?}"
        );
    }

    #[test]
    fn search_filters_to_keyword_matches() {
        let s = store();
        s.add_memory("p", "fact", "the database is postgres", "x")
            .unwrap();
        s.add_memory("p", "fact", "the frontend uses react", "x")
            .unwrap();
        let hits = s.search_memories("p", "react", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("react"));
    }

    #[test]
    fn delete_and_clear() {
        let s = store();
        let id = s
            .add_memory("p", "fact", "alpha bravo charlie", "x")
            .unwrap();
        assert!(s.delete_memory(&id).unwrap());
        assert!(!s.delete_memory(&id).unwrap());
        s.add_memory("p", "fact", "delta echo foxtrot", "x")
            .unwrap();
        s.add_memory("p", "fact", "golf hotel india", "x").unwrap();
        assert_eq!(s.clear_memories("p").unwrap(), 2);
    }
}
