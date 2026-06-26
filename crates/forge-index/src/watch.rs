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
///
/// Refuses to set up a recursive inotify watch when `root` lives on a non-native filesystem
/// (WSL2's `/mnt/*` DrvFs/9p, a FUSE mount, an SMB/NFS share). On those, registering a recursive
/// watch issues a per-entry RPC to a remote/host backend for the whole tree, and on 9p some of
/// those land in uninterruptible `D` state — which would otherwise hang `forge chat` on a blank
/// screen forever (the watcher setup used to gate TUI init). Returning an `Err` here makes the
/// caller log one line and continue WITHOUT a watcher; retrieval still works, it just won't auto-
/// reindex external edits (re-run `forge lattice update`, or move the project onto the Linux fs).
pub fn spawn_watcher(
    lattice: Arc<Lattice>,
    root: &Path,
    debounce: Duration,
) -> Result<LatticeWatcher, String> {
    if let Some(reason) = unwatchable_fs_reason(root) {
        return Err(reason);
    }
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

/// Filesystem types a recursive inotify watch must NOT be set up on: they back onto a remote/host
/// process, so the per-entry watch registration is an RPC that can stall (uninterruptibly, on 9p).
/// `v9fs`/`9p` is WSL2's DrvFs (`/mnt/c`); `fuse*` covers sshfs/rclone/etc.; `cifs`/`smb*` are
/// Windows shares; `nfs*` is NFS. Native local filesystems (ext4, btrfs, xfs, apfs, ntfs3, …) are
/// not listed, so they watch normally.
fn is_unwatchable_fstype(fstype: &str) -> bool {
    let fstype = fstype.trim();
    matches!(fstype, "9p" | "v9fs" | "cifs" | "smb3" | "smbfs" | "ncpfs")
        || fstype.starts_with("fuse")
        || fstype.starts_with("nfs")
}

/// Human-readable reason a recursive watch is being skipped for `root`, or `None` when `root` is on
/// a normal local filesystem (or the fs can't be determined — fail OPEN, i.e. watch). Linux-only
/// detection via `/proc/self/mountinfo`; other platforms always return `None`.
fn unwatchable_fs_reason(root: &Path) -> Option<String> {
    let fstype = root_fstype(root)?;
    if !is_unwatchable_fstype(&fstype) {
        return None;
    }
    let detail = if fstype == "9p" || fstype == "v9fs" {
        "a Windows drive (9p/DrvFs)".to_string()
    } else {
        format!("a {fstype} mount")
    };
    Some(format!(
        "working dir is on {detail} — file watching disabled (it would block on the remote \
         filesystem); retrieval still works. Move the project onto the Linux filesystem (e.g. ~) \
         for auto-reindex, or re-run `forge lattice update` after edits."
    ))
}

/// The filesystem type backing `root`, from `/proc/self/mountinfo` (Linux only). Returns `None`
/// off-Linux, or when the file can't be read / no mount matches.
fn root_fstype(root: &Path) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    // Canonicalize so symlinks resolve to the real mount; fall back to the path as given.
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target = canon.to_str()?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    fstype_for_path(&mountinfo, target).map(str::to_string)
}

/// Pure parse of `/proc/self/mountinfo`: return the fstype of the mount whose mount point is the
/// LONGEST prefix of `target` (the most specific mount that contains the path). mountinfo format:
/// `<id> <pid> <maj:min> <root> <MOUNTPOINT> <opts> <optfields...> - <FSTYPE> <source> <superopts>`
/// — the mount point is the 5th space-separated field and the fstype is the first token after the
/// ` - ` separator.
fn fstype_for_path<'a>(mountinfo: &'a str, target: &str) -> Option<&'a str> {
    let mut best: Option<(&str, &str)> = None; // (mountpoint, fstype)
    for line in mountinfo.lines() {
        let (pre, post) = match line.split_once(" - ") {
            Some(p) => p,
            None => continue,
        };
        let mountpoint = match pre.split_whitespace().nth(4) {
            Some(m) => m,
            None => continue,
        };
        let fstype = match post.split_whitespace().next() {
            Some(f) => f,
            None => continue,
        };
        // A mount point contains `target` if target == mountpoint or target starts with
        // `mountpoint/` (so `/mnt` doesn't spuriously match `/mntarget`). `/` matches everything.
        let contains = mountpoint == "/"
            || target == mountpoint
            || target
                .strip_prefix(mountpoint)
                .is_some_and(|rest| rest.starts_with('/'));
        if contains && best.is_none_or(|(m, _)| mountpoint.len() > m.len()) {
            best = Some((mountpoint, fstype));
        }
    }
    best.map(|(_, fstype)| fstype)
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

    #[test]
    fn unwatchable_fstype_classifies_remote_vs_native() {
        // Remote / host-backed → must skip the recursive watch.
        for fs in [
            "9p",
            "v9fs",
            "cifs",
            "smb3",
            "nfs",
            "nfs4",
            "fuse",
            "fuse.sshfs",
        ] {
            assert!(is_unwatchable_fstype(fs), "{fs} should be unwatchable");
        }
        // Native local → watch normally.
        for fs in [
            "ext4", "btrfs", "xfs", "zfs", "apfs", "ntfs3", "tmpfs", "overlay",
        ] {
            assert!(!is_unwatchable_fstype(fs), "{fs} should be watchable");
        }
    }

    // A realistic WSL2 mountinfo: `/` is ext4 (Linux home), `/mnt/c` is 9p (DrvFs).
    const WSL_MOUNTINFO: &str = "\
23 30 0:22 / /sys rw,nosuid - sysfs sysfs rw
24 30 0:23 / /proc rw,nosuid - proc proc rw
30 0 8:32 / / rw,relatime - ext4 /dev/sdc rw,discard,errors=remount-ro
70 30 0:55 / /mnt/c rw,noatime - 9p drvfs rw,dirsync,aname=drvfs;path=C:\\,mmap,trans=fd
71 30 0:56 / /mnt/wsl/docker rw - tmpfs tmpfs rw";

    #[test]
    fn fstype_for_path_picks_the_most_specific_mount() {
        // A path under /mnt/c resolves to 9p (DrvFs) — the reported hang case.
        assert_eq!(
            fstype_for_path(WSL_MOUNTINFO, "/mnt/c/Users/Quinn/project"),
            Some("9p")
        );
        // A path on the Linux home falls through to the root ext4 mount.
        assert_eq!(
            fstype_for_path(WSL_MOUNTINFO, "/home/quinn/project"),
            Some("ext4")
        );
        // The mount point itself matches exactly.
        assert_eq!(fstype_for_path(WSL_MOUNTINFO, "/mnt/c"), Some("9p"));
        // A sibling that only shares a name prefix must NOT match /mnt/c (longest-real-prefix).
        assert_eq!(
            fstype_for_path(WSL_MOUNTINFO, "/mnt/computer/x"),
            Some("ext4")
        );
    }

    #[test]
    fn unwatchable_reason_set_for_9p_clear_for_native() {
        // The /mnt/c (9p) case yields a skip reason mentioning the Windows drive.
        assert_eq!(fstype_for_path(WSL_MOUNTINFO, "/home/quinn"), Some("ext4"));
        assert!(is_unwatchable_fstype(
            fstype_for_path(WSL_MOUNTINFO, "/mnt/c/x").unwrap()
        ));
        assert!(!is_unwatchable_fstype(
            fstype_for_path(WSL_MOUNTINFO, "/home/quinn").unwrap()
        ));
    }
}
