//! Background file watcher: reindex supported source files as they change on disk (external
//! editor edits), so retrieval stays fresh without a manual `forge lattice update`. Debounced to
//! coalesce save bursts; skips build/VCS/vendored dirs. A watcher must never crash the session, so
//! once running, per-file reindex errors are swallowed.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};

use crate::{is_skippable_dir, lang_for_path, Lattice};

/// Keeps the background watcher thread alive; dropping it stops watching.
pub struct LatticeWatcher {
    _debouncer: Debouncer<RecommendedWatcher>,
}

/// Watch `root` recursively and reindex changed source files into `lattice`. Returns an error if
/// the OS watcher can't be set up; the returned handle must be kept alive for watching to continue.
pub fn spawn_watcher(
    lattice: Arc<Lattice>,
    root: &Path,
    debounce: Duration,
) -> Result<LatticeWatcher, String> {
    let mut debouncer = new_debouncer(debounce, move |res: DebounceEventResult| {
        let Ok(events) = res else { return };
        for ev in events {
            if should_reindex(&ev.path) {
                let _ = lattice.reindex_path(&ev.path);
            }
        }
    })
    .map_err(|e| e.to_string())?;
    debouncer
        .watcher()
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| e.to_string())?;
    Ok(LatticeWatcher {
        _debouncer: debouncer,
    })
}

/// A changed path is worth reindexing only if it's a supported source file and none of its path
/// components is a skipped directory (build output, `.git`, `node_modules`, …).
fn should_reindex(path: &Path) -> bool {
    let skipped = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .any(is_skippable_dir);
    if skipped {
        return false;
    }
    path.to_str().and_then(lang_for_path).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lattice;
    use forge_store::Store;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn external_edit_is_reindexed_automatically() {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("forge-watch-{}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/a.rs");
        std::fs::write(&file, "pub fn alpha() {}\n").unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let lattice = Arc::new(Lattice::new(store, &root));
        lattice.update().unwrap();
        assert_eq!(lattice.query("alpha", 5).unwrap().len(), 1);

        let _watcher = spawn_watcher(Arc::clone(&lattice), &root, Duration::from_millis(150))
            .expect("watcher starts");

        // Simulate an external editor writing a new symbol into the file.
        std::fs::write(&file, "pub fn beta() {}\n").unwrap();

        // Poll: the watcher should pick up the change and reindex within a few seconds.
        let mut reindexed = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            if lattice.query("beta", 5).unwrap().len() == 1 {
                reindexed = true;
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(reindexed, "watcher did not reindex the external edit");
    }

    #[test]
    fn skips_build_dirs_and_unsupported_files() {
        assert!(should_reindex(Path::new("crates/forge-index/src/lib.rs")));
        assert!(!should_reindex(Path::new("target/debug/build.rs")));
        assert!(!should_reindex(Path::new("notes.txt")));
        assert!(!should_reindex(Path::new("node_modules/x/index.js")));
    }
}
