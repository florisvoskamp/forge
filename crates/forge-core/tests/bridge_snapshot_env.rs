//! Cross-process code-checkpoint path (RFC PR3): `forge mcp-serve` snapshots a bridge model's
//! file writes into the parent turn's dir using the env context the parent's `run_turn` exported.
//! Isolated in its own test binary because it mutates process-global env (`FORGE_CHECKPOINT_*`),
//! which the in-process `run_turn` tests also write — a shared process would race.

use forge_core::snapshot::{
    record_from_env_after_write, restore_turn, snapshot_from_env_before_write, ENV_ROOT, ENV_SEQ,
    ENV_SESSION,
};

#[test]
fn bridge_writes_snapshotted_via_env_are_restorable_by_the_parent() {
    let root = std::env::temp_dir().join(format!("forge-bridge-{}", forge_types::new_id()));
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let file = work.join("edited.rs");
    std::fs::write(&file, "PRE-TURN").unwrap();

    // The parent's run_turn exports this context; mcp-serve reads it.
    std::env::set_var(ENV_SESSION, "parent-session");
    std::env::set_var(ENV_SEQ, "7");
    std::env::set_var(ENV_ROOT, root.to_str().unwrap());

    // What mcp-serve's call_tool does around a Write tool:
    snapshot_from_env_before_write(&file).unwrap();
    std::fs::write(&file, "BRIDGE MODEL EDIT").unwrap();
    record_from_env_after_write(&file).unwrap();

    // What the parent's /undo does: rewind restores the turn's snapshot.
    let report = restore_turn(&root, "parent-session", 7).unwrap();
    assert!(!report.restored.is_empty(), "the bridge write was captured");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "PRE-TURN",
        "a bridge model's edit is reverted by the parent session's /undo"
    );

    std::fs::remove_dir_all(&root).ok();
}
