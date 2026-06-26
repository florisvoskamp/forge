//! Background file watcher: reindex supported source files as they change on disk (external
//! editor edits), so retrieval stays fresh without a manual `forge lattice update`. Debounced to
//! coalesce save bursts; skips build/VCS/vendored dirs. A watcher must never crash the session, so
//! once running, per-file reindex errors are swallowed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify_debouncer_mini::notify::{
    Config as NotifyConfig, PollWatcher, RecommendedWatcher, RecursiveMode,
};
use notify_debouncer_mini::{
    new_debouncer, new_debouncer_opt, Config as DebouncerConfig, DebounceEventResult, Debouncer,
};

use crate::{is_skippable_dir, lang_for_path, Lattice};

/// Keeps the background watcher alive; dropping it stops watching. Holds either the native inotify
/// backend or the polling backend (used on filesystems where inotify is unreliable — see below).
pub struct LatticeWatcher {
    _inner: WatcherInner,
}

// Variants hold the debouncer purely to keep its background thread alive (dropping stops watching);
// the value is never read back out.
#[allow(dead_code)]
enum WatcherInner {
    Native(Debouncer<RecommendedWatcher>),
    Poll(Debouncer<PollWatcher>),
}

/// How often the POLLING backend rescans the tree on a filesystem without working inotify (9p/etc.).
/// A balance between reindex latency and the cost of a full stat-walk over a remote/host link.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Watch `root` recursively and reindex changed source files into `lattice`. Returns an error only
/// if the OS watcher can't be set up at all; the returned handle must be kept alive for watching to
/// continue.
///
/// Picks the backend by filesystem: native local filesystems use the efficient inotify watcher;
/// a non-native filesystem (WSL2's `/mnt/*` DrvFs/9p, a FUSE mount, an SMB/NFS share) uses a
/// **polling** watcher instead. Recursive inotify registration on 9p RPCs to the host per entry and
/// some calls land in uninterruptible `D` state — which used to hang `forge chat` — whereas polling
/// just stat-walks the tree on a timer (ordinary file ops that work over 9p). Both register a
/// recursive watch on `root`; the caller runs this off the startup path so the initial poll scan
/// (synchronous, and slow over a remote link) can't gate the UI.
pub fn spawn_watcher(
    lattice: Arc<Lattice>,
    root: &Path,
    debounce: Duration,
) -> Result<LatticeWatcher, String> {
    let inner = build_watcher(lattice, root, debounce, needs_polling(root), POLL_INTERVAL)?;
    Ok(LatticeWatcher { _inner: inner })
}

/// Build the backend explicitly (the `poll` decision + interval are parameters so tests can exercise
/// the polling path on a native test filesystem). `poll=false` → inotify; `poll=true` → stat-walk
/// every `poll_interval`.
fn build_watcher(
    lattice: Arc<Lattice>,
    root: &Path,
    debounce: Duration,
    poll: bool,
    poll_interval: Duration,
) -> Result<WatcherInner, String> {
    let handler = move |res: DebounceEventResult| {
        let Ok(events) = res else { return };
        for ev in events {
            if should_reindex(&ev.path) {
                let _ = lattice.reindex_path(&ev.path);
            }
        }
    };
    if poll {
        // compare_contents so a same-SIZE edit (changing a value, a rename of equal length) is still
        // caught — metadata-only polling would miss it if mtime granularity coincides. Costs a content
        // read per file per tick, bounded by the project-root scope + debounce.
        let notify_config = NotifyConfig::default()
            .with_poll_interval(poll_interval)
            .with_compare_contents(true);
        let config = DebouncerConfig::default()
            .with_timeout(debounce)
            .with_notify_config(notify_config);
        let mut debouncer =
            new_debouncer_opt::<_, PollWatcher>(config, handler).map_err(|e| e.to_string())?;
        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        Ok(WatcherInner::Poll(debouncer))
    } else {
        let mut debouncer = new_debouncer(debounce, handler).map_err(|e| e.to_string())?;
        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        Ok(WatcherInner::Native(debouncer))
    }
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

/// Filesystem types where a recursive inotify watch is unreliable or outright blocking, so the
/// watcher uses the POLLING backend instead. They back onto a remote/host process, so per-entry
/// inotify registration is an RPC that can stall (uninterruptibly, on 9p). `v9fs`/`9p` is WSL2's
/// DrvFs (`/mnt/c`); `fuse*` covers sshfs/rclone/etc.; `cifs`/`smb*` are Windows shares; `nfs*` is
/// NFS. Native local filesystems (ext4, btrfs, xfs, apfs, ntfs3, …) are not listed → inotify.
fn is_poll_only_fstype(fstype: &str) -> bool {
    let fstype = fstype.trim();
    matches!(fstype, "9p" | "v9fs" | "cifs" | "smb3" | "smbfs" | "ncpfs")
        || fstype.starts_with("fuse")
        || fstype.starts_with("nfs")
}

/// Whether `root` lives on a filesystem that needs the polling backend (inotify unreliable/blocking).
/// Linux-only detection via `/proc/self/mountinfo`; other platforms / undetectable fs → `false`
/// (fail toward the efficient inotify backend, which works on every native filesystem).
fn needs_polling(root: &Path) -> bool {
    root_fstype(root)
        .map(|fs| is_poll_only_fstype(&fs))
        .unwrap_or(false)
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

/// Resolve the directory to recursively watch, given the launch `cwd` and the user's `home`.
/// Prefers the nearest enclosing PROJECT ROOT (a dir holding `.git`, `.forge`, or `AGENTS.md`) so
/// the watch covers the codebase rather than whatever happens to sit above it. Returns `None` —
/// "don't watch" — when the resolved root would be the home directory itself: recursively watching
/// all of `$HOME` is pathological (it pulls in `.cargo`, cloned `.git` trees, caches — thousands of
/// inotify watches and a slow initial walk) and is virtually never intended. The upward climb stops
/// at `home`, so we never walk past it into `/` and watch a system root either. When `home` is
/// unknown (`None`), nothing is refused — fail open and watch the discovered root / `cwd`.
pub fn resolve_watch_root(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    const MARKERS: [&str; 3] = [".git", ".forge", "AGENTS.md"];
    let mut dir = cwd;
    let mut found: Option<&Path> = None;
    loop {
        if MARKERS.iter().any(|m| dir.join(m).exists()) {
            found = Some(dir);
            break;
        }
        if Some(dir) == home {
            break; // never climb above $HOME
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
    let root = found.unwrap_or(cwd);
    if Some(root) == home {
        return None; // refuse to recursively watch the entire home directory
    }
    Some(root.to_path_buf())
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
    fn poll_backend_reindexes_external_edit() {
        // Proves the POLLING backend (used on 9p/remote filesystems where inotify is unreliable)
        // actually picks up edits — exercised here on the native test fs via `build_watcher(poll=true)`
        // with a short interval, since CI has no 9p mount.
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("forge-poll-{}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/a.rs");
        std::fs::write(&file, "pub fn alpha() {}\n").unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let lattice = Arc::new(Lattice::new(store, &root));
        lattice.update().unwrap();

        let inner = build_watcher(
            Arc::clone(&lattice),
            &root,
            Duration::from_millis(100),
            true, // force the polling backend
            Duration::from_millis(150),
        )
        .expect("poll watcher starts");
        let _w = LatticeWatcher { _inner: inner };

        std::fs::write(&file, "pub fn gamma() {}\n").unwrap();
        let mut reindexed = false;
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(100));
            if lattice.query("gamma", 5).unwrap().len() == 1 {
                reindexed = true;
                break;
            }
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            reindexed,
            "polling watcher did not reindex the external edit"
        );
    }

    #[test]
    fn watcher_held_only_through_a_channel_still_reindexes() {
        // Production never drains the watcher: it's sent into an mpsc channel and the Session holds
        // the Receiver for keep-alive (so setup is off-thread + the watcher is owned per-session).
        // Prove a watcher sitting UN-received in the channel buffer still runs and reindexes — and
        // that dropping the Receiver tears it down.
        let n = N.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("forge-chan-{}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/a.rs");
        std::fs::write(&file, "pub fn alpha() {}\n").unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let lattice = Arc::new(Lattice::new(store, &root));
        lattice.update().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        let watcher =
            spawn_watcher(Arc::clone(&lattice), &root, Duration::from_millis(100)).expect("starts");
        tx.send(watcher).unwrap(); // hand off to the channel; never received, kept alive by `rx`

        std::fs::write(&file, "pub fn omega() {}\n").unwrap();
        let mut reindexed = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            if lattice.query("omega", 5).unwrap().len() == 1 {
                reindexed = true;
                break;
            }
        }
        drop(rx); // dropping the Receiver drops the buffered watcher → watching stops
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            reindexed,
            "channel-held watcher did not reindex the external edit"
        );
    }

    #[test]
    fn skips_build_dirs_and_unsupported_files() {
        assert!(should_reindex(Path::new("crates/forge-index/src/lib.rs")));
        assert!(!should_reindex(Path::new("target/debug/build.rs")));
        assert!(!should_reindex(Path::new("notes.txt")));
        assert!(!should_reindex(Path::new("node_modules/x/index.js")));
    }

    #[test]
    fn poll_only_fstype_classifies_remote_vs_native() {
        // Remote / host-backed → inotify unreliable, use the polling backend.
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
            assert!(is_poll_only_fstype(fs), "{fs} should use polling");
        }
        // Native local → efficient inotify backend.
        for fs in [
            "ext4", "btrfs", "xfs", "zfs", "apfs", "ntfs3", "tmpfs", "overlay",
        ] {
            assert!(!is_poll_only_fstype(fs), "{fs} should use inotify");
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
    fn resolve_watch_root_prefers_project_root_and_refuses_home() {
        let tmp = std::env::temp_dir().join(format!(
            "forge-root-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let home = tmp.join("home");
        let proj = home.join("work/myproj");
        let sub = proj.join("crates/x/src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(proj.join(".git")).unwrap();

        // From deep inside the project, the watch root climbs to the .git project root.
        assert_eq!(
            resolve_watch_root(&sub, Some(&home)),
            Some(proj.clone()),
            "should scope the watch to the nearest project root"
        );
        // From the project root itself, it stays there.
        assert_eq!(resolve_watch_root(&proj, Some(&home)), Some(proj.clone()));
        // Launched in $HOME with no project marker → refuse (don't watch all of home).
        assert_eq!(resolve_watch_root(&home, Some(&home)), None);
        // A marker-less subdir of home that is NOT home → watch that specific dir (not all of home).
        let loose = home.join("scratch");
        std::fs::create_dir_all(&loose).unwrap();
        assert_eq!(resolve_watch_root(&loose, Some(&home)), Some(loose.clone()));
        // Even if $HOME itself holds a .git (dotfiles repo), refuse — the root resolves to home.
        std::fs::create_dir_all(home.join(".git")).unwrap();
        assert_eq!(resolve_watch_root(&loose, Some(&home)), None);
        // Unknown home → never refuse; resolves to the nearest project root (home/.git now exists
        // from the line above), proving home=None can't trigger the refuse branch.
        assert_eq!(resolve_watch_root(&loose, None), Some(home.clone()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fstype_drives_poll_decision_9p_polls_native_inotify() {
        // The /mnt/c (9p) case → polling backend; the Linux home → inotify.
        assert_eq!(fstype_for_path(WSL_MOUNTINFO, "/home/quinn"), Some("ext4"));
        assert!(is_poll_only_fstype(
            fstype_for_path(WSL_MOUNTINFO, "/mnt/c/x").unwrap()
        ));
        assert!(!is_poll_only_fstype(
            fstype_for_path(WSL_MOUNTINFO, "/home/quinn").unwrap()
        ));
    }
}
