//! Background file watcher: reindex supported source files as they change on disk (external
//! editor edits), so retrieval stays fresh without a manual `forge lattice update`. Debounced to
//! coalesce save bursts; skips build/VCS/vendored dirs. A watcher must never crash the session, so
//! once running, per-file reindex errors are swallowed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use notify_debouncer_mini::notify::{
    Config as NotifyConfig, PollWatcher, RecommendedWatcher, RecursiveMode,
};
use notify_debouncer_mini::{
    new_debouncer, new_debouncer_opt, Config as DebouncerConfig, DebounceEventResult, Debouncer,
};

use crate::{is_skippable_dir, lang_for_path, Lattice};

/// Keeps the background watcher alive; dropping it stops watching AND joins the reindex worker so no
/// thread leaks. Holds the OS watcher backend (native inotify or polling) plus the worker thread that
/// drains changed paths and reindexes them OFF the notify/debouncer thread (so a save burst can't
/// serialize reindexing on the watcher thread or hold the store write lock too long).
pub struct LatticeWatcher {
    // Dropped FIRST (see the explicit `Drop`): the debouncer owns the channel `Sender` (it lives in
    // the handler closure), so dropping it disconnects the channel, which is the worker's shutdown
    // signal. `Option` so `Drop` can `take()` it before joining the worker.
    inner: Option<WatcherInner>,
    worker: Option<JoinHandle<()>>,
}

impl LatticeWatcher {
    fn new(inner: WatcherInner, worker: JoinHandle<()>) -> Self {
        Self {
            inner: Some(inner),
            worker: Some(worker),
        }
    }
}

impl Drop for LatticeWatcher {
    fn drop(&mut self) {
        // Drop the debouncer first: that drops its handler closure, the sole channel `Sender`, so the
        // worker's `recv()` returns `Err` (disconnected) and the loop exits. Then join the worker so
        // it's torn down deterministically instead of leaked.
        self.inner.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
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

/// After the worker pulls the first changed path of a batch, it waits this long for stragglers from
/// the same save burst (a multi-file save, a `git checkout`) before reindexing — so paths arriving in
/// quick succession coalesce into one reindex pass instead of one pass each. Short enough to stay
/// imperceptible on top of the debounce window.
const COALESCE_WINDOW: Duration = Duration::from_millis(50);

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
    build_watcher(
        lattice,
        root,
        debounce,
        needs_polling(root),
        POLL_INTERVAL,
        COALESCE_WINDOW,
    )
}

/// Build the backend explicitly (the `poll` decision + interval are parameters so tests can exercise
/// the polling path on a native test filesystem). `poll=false` → inotify; `poll=true` → stat-walk
/// every `poll_interval`. `coalesce` is the worker's batch window (see [`COALESCE_WINDOW`]).
///
/// The notify/debouncer thread runs the cheap `handler`: it only *enqueues* changed paths onto a
/// channel and returns immediately, so a burst of saves never serializes reindexing (which takes the
/// store write lock) on the watcher thread. A dedicated worker thread drains the channel, coalesces
/// duplicate paths within `coalesce`, and reindexes — off the watcher thread.
fn build_watcher(
    lattice: Arc<Lattice>,
    root: &Path,
    debounce: Duration,
    poll: bool,
    poll_interval: Duration,
    coalesce: Duration,
) -> Result<LatticeWatcher, String> {
    let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
    let worker = std::thread::Builder::new()
        .name("forge-lattice-reindex".into())
        .spawn(move || {
            run_reindex_worker(rx, coalesce, |path| {
                let _ = lattice.reindex_path(path);
            });
        })
        .map_err(|e| e.to_string())?;

    // The handler runs ON the notify/debouncer thread, so it must stay cheap: just forward each
    // supported changed path to the worker. A send only fails once the worker has exited (channel
    // closed); that can't normally happen while the debouncer is alive, so it's ignored.
    let handler = move |res: DebounceEventResult| {
        let Ok(events) = res else { return };
        for ev in events {
            if should_reindex(&ev.path) {
                let _ = tx.send(ev.path);
            }
        }
    };

    let inner = if poll {
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
        WatcherInner::Poll(debouncer)
    } else {
        let mut debouncer = new_debouncer(debounce, handler).map_err(|e| e.to_string())?;
        debouncer
            .watcher()
            .watch(root, RecursiveMode::Recursive)
            .map_err(|e| e.to_string())?;
        WatcherInner::Native(debouncer)
    };
    Ok(LatticeWatcher::new(inner, worker))
}

/// Drain reindex requests off `rx` and apply `reindex` to each changed path, OFF the watcher thread.
/// Coalesces a burst: after the first path arrives it grabs everything already queued, waits
/// `coalesce` for stragglers from the same save, grabs those too, then reindexes each unique path
/// ONCE — so one save that emits the same path several times (or several debounce windows for one
/// file) reindexes it a single time, not N times. Exits when the channel disconnects (every `Sender`
/// dropped, i.e. the watcher was dropped), which is the clean-shutdown signal.
fn run_reindex_worker<F: FnMut(&Path)>(rx: Receiver<PathBuf>, coalesce: Duration, mut reindex: F) {
    while let Ok(first) = rx.recv() {
        let mut batch: HashSet<PathBuf> = HashSet::new();
        batch.insert(first);
        drain_pending(&rx, &mut batch);
        if !coalesce.is_zero() {
            std::thread::sleep(coalesce);
            drain_pending(&rx, &mut batch);
        }
        for path in &batch {
            reindex(path);
        }
    }
}

/// Move every currently-queued path from `rx` into `batch` (deduping), without blocking. Stops at the
/// first `Empty` (nothing more queued right now) or `Disconnected` (sender gone) — both end the drain.
fn drain_pending(rx: &Receiver<PathBuf>, batch: &mut HashSet<PathBuf>) {
    while let Ok(path) = rx.try_recv() {
        batch.insert(path);
    }
}

/// A changed path is worth reindexing only if it's a supported source file and none of its path
/// components is a skipped directory (build output, `.git`, `node_modules`, …).
fn should_reindex(path: &Path) -> bool {
    // Only the DIRECTORY components are skip-tested — not the filename. `is_skippable_dir` treats any
    // dot-prefixed name as a skipped dir, so checking the final component wrongly excluded dotfile
    // SOURCE files (`.eslintrc.js`, a hidden `.foo.rs`) that the initial `update()` walk DID index
    // (it only applies the skip to directory entries), leaving the watcher unable to refresh them.
    let skipped = path
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
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
    fn should_reindex_allows_dotfile_source_but_skips_dot_dirs() {
        // A dot-prefixed SOURCE file must be reindexed (the initial walk indexes it); only a
        // dot/skip DIRECTORY in the path excludes it.
        assert!(
            should_reindex(Path::new("src/.hidden.rs")),
            "dotfile source"
        );
        assert!(should_reindex(Path::new(".eslintrc.js")), "dotfile at root");
        assert!(!should_reindex(Path::new(".git/config.rs")), "inside .git");
        assert!(
            !should_reindex(Path::new("node_modules/x.js")),
            "vendor dir"
        );
        assert!(!should_reindex(Path::new("src/a.txt")), "unsupported ext");
    }

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

        let _w = build_watcher(
            Arc::clone(&lattice),
            &root,
            Duration::from_millis(100),
            true, // force the polling backend
            Duration::from_millis(150),
            COALESCE_WINDOW,
        )
        .expect("poll watcher starts");

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
    fn worker_coalesces_a_burst_and_stops_on_sender_drop() {
        // A rapid burst of changes to the SAME path must coalesce into far fewer reindexes than the
        // number of events (don't reindex a file 10× for one save), and dropping the sole `Sender`
        // (what `LatticeWatcher::drop` does to the debouncer's handler) must stop the worker so its
        // thread can be joined — no leak.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let reindexes = Arc::new(AtomicUsize::new(0));
        let r2 = Arc::clone(&reindexes);
        let worker = std::thread::spawn(move || {
            run_reindex_worker(rx, Duration::from_millis(80), move |_p| {
                r2.fetch_add(1, Ordering::SeqCst);
            });
        });

        // Fire 20 events for one path back-to-back; they should land within a single coalesce window.
        let path = PathBuf::from("src/a.rs");
        for _ in 0..20 {
            tx.send(path.clone()).unwrap();
        }

        drop(tx); // sole sender gone → worker must finish the batch and exit
        worker
            .join()
            .expect("worker thread joins after sender drop");

        let n = reindexes.load(Ordering::SeqCst);
        assert!(n >= 1, "the burst must reindex the path at least once");
        assert!(
            n < 20,
            "20 events for one path must coalesce to fewer reindexes, got {n}"
        );
    }

    #[test]
    fn worker_reindexes_distinct_paths_in_a_batch() {
        // Coalescing dedups by path, so distinct paths in one burst are each reindexed once.
        use std::sync::Mutex;

        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let seen: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
        let s2 = Arc::clone(&seen);
        let worker = std::thread::spawn(move || {
            run_reindex_worker(rx, Duration::from_millis(80), move |p| {
                s2.lock().unwrap().insert(p.to_path_buf());
            });
        });

        for name in ["src/a.rs", "src/b.rs", "src/c.rs"] {
            // each path sent twice — dedup should still reindex each once
            tx.send(PathBuf::from(name)).unwrap();
            tx.send(PathBuf::from(name)).unwrap();
        }
        drop(tx);
        worker.join().expect("worker joins");

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "each distinct path reindexed");
        assert!(seen.contains(Path::new("src/b.rs")));
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
