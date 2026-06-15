//! Forge shadow snapshots for code checkpoints (RFC session-management-and-commands, PR3).
//!
//! Before a turn's first write to a file, we copy its prior bytes into
//! `<root>/<session>/<turn_seq>/` and record it in a per-turn `manifest.json`. `/undo` restores
//! the turn by copying the blobs back (and deleting files the turn *created*). This never touches
//! the user's git state — it's a self-contained, runs-anywhere safety net. Only files a turn
//! actually writes are copied, so the cost is the changed set, not the tree.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Env vars that carry the current turn's snapshot context across the process boundary to the
/// CLI bridge's `forge mcp-serve`, so files a subscription model (claude/codex) edits get
/// snapshotted into the *parent* turn's dir and are restorable by `/undo` (RFC PR3, cross-process).
pub const ENV_SESSION: &str = "FORGE_CHECKPOINT_SESSION";
pub const ENV_SEQ: &str = "FORGE_CHECKPOINT_SEQ";
pub const ENV_ROOT: &str = "FORGE_CHECKPOINT_ROOT";

/// Snapshot a write happening in the bridge subprocess into the parent turn's dir, reading the
/// turn context from the environment (set by the parent's `run_turn`). No-op when unset (the
/// bridge wasn't launched by a checkpointing turn) — so `mcp-serve` used standalone is unaffected.
pub fn snapshot_from_env_before_write(path: &Path) -> std::io::Result<()> {
    if let Some((root, session, seq)) = env_context() {
        snapshot_before_write(&root, &session, seq, path)?;
    }
    Ok(())
}

/// Record post-write bytes for a bridge write (see [`snapshot_from_env_before_write`]).
pub fn record_from_env_after_write(path: &Path) -> std::io::Result<()> {
    if let Some((root, session, seq)) = env_context() {
        record_post_write(&root, &session, seq, path)?;
    }
    Ok(())
}

fn env_context() -> Option<(PathBuf, String, i64)> {
    let session = std::env::var(ENV_SESSION).ok()?;
    let seq = std::env::var(ENV_SEQ).ok()?.parse::<i64>().ok()?;
    let root = std::env::var(ENV_ROOT).ok()?;
    Some((PathBuf::from(root), session, seq))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    seq: i64,
    files: Vec<FileEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    /// The path the tool wrote to (as given in the tool args).
    path: String,
    /// "modified" (existed pre-turn → blob holds prior bytes) or "created" (no prior bytes).
    status: String,
    /// Blob filename holding the pre-turn bytes, for `modified` entries.
    blob: Option<String>,
    /// Stable hash of the bytes Forge wrote (post-edit), so restore can warn if the user has
    /// since changed the file by hand.
    post_hash: Option<String>,
}

/// What a restore did, surfaced to the user.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Paths restored to their pre-turn state (modified → prior bytes; created → deleted).
    pub restored: Vec<String>,
    /// Paths whose on-disk bytes differed from what Forge last wrote — restored anyway, but the
    /// user is told their manual edit was overwritten.
    pub warnings: Vec<String>,
}

impl RestoreReport {
    pub fn is_empty(&self) -> bool {
        self.restored.is_empty() && self.warnings.is_empty()
    }
}

fn turn_dir(root: &Path, session: &str, seq: i64) -> PathBuf {
    root.join(session).join(seq.to_string())
}

/// Resolve a tool's path to an absolute one (without requiring it to exist), so the snapshot key
/// is independent of the working directory — the same file resolves identically whether the write
/// happened in-process or in the `forge mcp-serve` bridge subprocess, and whether the model passed
/// a relative or absolute path.
fn abs(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn manifest_path(root: &Path, session: &str, seq: i64) -> PathBuf {
    turn_dir(root, session, seq).join("manifest.json")
}

fn load_manifest(root: &Path, session: &str, seq: i64) -> Option<Manifest> {
    let raw = std::fs::read_to_string(manifest_path(root, session, seq)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn save_manifest(root: &Path, session: &str, seq: i64, m: &Manifest) -> std::io::Result<()> {
    let dir = turn_dir(root, session, seq);
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(m).unwrap_or_default();
    std::fs::write(manifest_path(root, session, seq), json)
}

/// Stable (process- and restart-independent) FNV-1a 64-bit hash, hex-encoded. Used only to detect
/// that a file changed since Forge wrote it — not for security.
fn stable_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Record `path`'s pre-turn state, once per path per turn (first touch wins, preserving the bytes
/// from before the turn began). Call immediately before a write tool runs.
pub fn snapshot_before_write(
    root: &Path,
    session: &str,
    seq: i64,
    path: &Path,
) -> std::io::Result<()> {
    let path = abs(path);
    let key = path.to_string_lossy().to_string();
    let mut manifest = load_manifest(root, session, seq).unwrap_or(Manifest {
        seq,
        files: Vec::new(),
    });
    if manifest.files.iter().any(|f| f.path == key) {
        return Ok(()); // already captured this turn — keep the earliest bytes.
    }
    let idx = manifest.files.len();
    let entry = if path.exists() {
        let blob = format!("{idx}.blob");
        let dir = turn_dir(root, session, seq);
        std::fs::create_dir_all(&dir)?;
        std::fs::copy(&path, dir.join(&blob))?;
        FileEntry {
            path: key,
            status: "modified".into(),
            blob: Some(blob),
            post_hash: None,
        }
    } else {
        FileEntry {
            path: key,
            status: "created".into(),
            blob: None,
            post_hash: None,
        }
    };
    manifest.files.push(entry);
    save_manifest(root, session, seq, &manifest)
}

/// After the write applied, record the hash of what Forge wrote so a later restore can detect a
/// manual edit. No-op if the path wasn't snapshotted this turn.
pub fn record_post_write(root: &Path, session: &str, seq: i64, path: &Path) -> std::io::Result<()> {
    let Some(mut manifest) = load_manifest(root, session, seq) else {
        return Ok(());
    };
    let path = abs(path);
    let key = path.to_string_lossy().to_string();
    let hash = std::fs::read(&path).ok().map(|b| stable_hash(&b));
    if let Some(entry) = manifest.files.iter_mut().find(|f| f.path == key) {
        entry.post_hash = hash;
        return save_manifest(root, session, seq, &manifest);
    }
    Ok(())
}

/// Restore a turn's files to their pre-turn state. `modified` → prior bytes copied back;
/// `created` → file deleted. Files whose current bytes differ from what Forge wrote get a warning
/// (the user changed them by hand) but are still restored. Empty report if the turn has no snapshot.
pub fn restore_turn(root: &Path, session: &str, seq: i64) -> std::io::Result<RestoreReport> {
    let Some(manifest) = load_manifest(root, session, seq) else {
        return Ok(RestoreReport::default());
    };
    let dir = turn_dir(root, session, seq);
    let mut report = RestoreReport::default();
    for entry in &manifest.files {
        let path = Path::new(&entry.path);
        // Warn if the file no longer matches what Forge wrote (a manual edit is being overwritten).
        if let Some(expected) = &entry.post_hash {
            let current = std::fs::read(path).ok().map(|b| stable_hash(&b));
            if current.as_deref() != Some(expected.as_str()) {
                report.warnings.push(entry.path.clone());
            }
        }
        match entry.status.as_str() {
            "created" => {
                let _ = std::fs::remove_file(path);
            }
            _ => {
                if let Some(blob) = &entry.blob {
                    std::fs::copy(dir.join(blob), path)?;
                }
            }
        }
        report.restored.push(entry.path.clone());
    }
    // The turn's snapshot is consumed once restored. Remove it so that if this seq is later
    // reused (a new turn after the rewind), `snapshot_before_write` starts fresh and captures the
    // new pre-turn bytes instead of seeing a stale manifest entry ("first touch" against old data).
    let _ = std::fs::remove_dir_all(&dir);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::temp_dir().join(format!("forge-snap-{}", forge_types::new_id()))
    }

    #[test]
    fn modified_file_is_restored_to_prior_bytes() {
        let root = root();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = work.join("a.txt");
        std::fs::write(&file, "original").unwrap();

        snapshot_before_write(&root, "sess", 3, &file).unwrap();
        std::fs::write(&file, "edited by the model").unwrap(); // the turn's write

        let report = restore_turn(&root, "sess", 3).unwrap();
        assert_eq!(report.restored, vec![file.to_string_lossy().to_string()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn created_file_is_deleted_on_restore() {
        let root = root();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = work.join("new.rs");
        // File does NOT exist pre-turn.
        snapshot_before_write(&root, "sess", 1, &file).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap(); // the turn created it

        restore_turn(&root, "sess", 1).unwrap();
        assert!(
            !file.exists(),
            "a file created this turn is deleted on undo"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn first_touch_wins_preserving_pre_turn_bytes() {
        let root = root();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = work.join("a.txt");
        std::fs::write(&file, "v0").unwrap();

        snapshot_before_write(&root, "s", 1, &file).unwrap();
        std::fs::write(&file, "v1").unwrap();
        // A second edit in the SAME turn must not overwrite the captured v0.
        snapshot_before_write(&root, "s", 1, &file).unwrap();
        std::fs::write(&file, "v2").unwrap();

        restore_turn(&root, "s", 1).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v0",
            "earliest bytes win"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn restore_warns_when_file_changed_since_forge_wrote_it() {
        let root = root();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = work.join("a.txt");
        std::fs::write(&file, "original").unwrap();

        snapshot_before_write(&root, "s", 1, &file).unwrap();
        std::fs::write(&file, "forge wrote this").unwrap();
        record_post_write(&root, "s", 1, &file).unwrap();
        // User edits it by hand afterwards.
        std::fs::write(&file, "user changed it").unwrap();

        let report = restore_turn(&root, "s", 1).unwrap();
        assert_eq!(report.warnings, vec![file.to_string_lossy().to_string()]);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "original",
            "restored anyway"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn reusing_a_seq_after_restore_captures_fresh_bytes_not_stale() {
        // Undo then a new turn can reuse the same seq. The second turn must snapshot its OWN
        // pre-bytes, not see the (consumed) earlier manifest and skip on "first touch".
        let root = root();
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let file = work.join("a.txt");

        std::fs::write(&file, "v0").unwrap();
        snapshot_before_write(&root, "s", 1, &file).unwrap();
        std::fs::write(&file, "v1").unwrap();
        restore_turn(&root, "s", 1).unwrap(); // back to v0, snapshot consumed

        // A new turn reuses seq 1; pre-bytes are now v0.
        snapshot_before_write(&root, "s", 1, &file).unwrap();
        std::fs::write(&file, "v2").unwrap();
        restore_turn(&root, "s", 1).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v0",
            "the reused seq captured fresh pre-bytes, not a stale manifest"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn restoring_a_turn_with_no_snapshot_is_empty() {
        let root = root();
        let report = restore_turn(&root, "s", 99).unwrap();
        assert!(report.is_empty());
    }
}
