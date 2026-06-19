//! The `shell` tool: run a non-interactive command via the OS shell (`sh -c` on Unix, `cmd /C`
//! on Windows), capture stdout/stderr and the exit code, with a timeout that kills the whole
//! process group. Safety (allow/ask/deny, the catastrophic denylist, secret reads) is enforced
//! upstream by the permission broker (`forge-core::permission`) against the rule engine — this
//! tool only executes.
//!
//! Non-interactive only: stdin is null, no TTY. Model-facing output is ANSI-stripped and
//! truncated to a token budget; the true byte size is reported.
//!
//! PTY mode (opt-in via `"pty": true`): the command runs under a pseudo-terminal so
//! `isatty(1)` returns true. Useful for programs that detect a TTY and change their output
//! format (e.g. colour output, progress bars). Stdin is still closed (EOF) so interactive
//! prompts reading stdin exit rather than hanging. Combined stdout+stderr is captured from
//! the PTY master (the OS merges them). The Landlock sandbox does NOT apply to PTY-spawned
//! commands (portable-pty owns the spawn and does not expose a `pre_exec` hook); this is a
//! known V1 limitation documented in the shell-sandbox doc.
//!
//! Note: the catastrophic denylist patterns are still POSIX-oriented (`rm -rf`, secret-file
//! reads); Windows-specific dangerous-command patterns are a follow-up (known-issues.md).
//!
//! Deferred to follow-ups (see docs/features/shell-tool.md): live output streaming to the
//! TUI (`ToolOutputDelta`/`ToolEnd`), background jobs (`shell_poll`/`shell_kill`),
//! session-remembered allows, and the rich command-context permission prompt.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use forge_types::SideEffect;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::sandbox::{self, SandboxPolicy};
use crate::{str_arg, Tool, ToolError};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
/// Bytes captured per stream before we stop reading (memory bound).
const CAPTURE_CAP: usize = 1 << 20; // 1 MiB
/// Bytes of combined output handed back to the model (token budget).
const MODEL_BUDGET: usize = 64 * 1024;
/// Grace period between SIGTERM and SIGKILL on timeout (Unix process-group kill).
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct ShellTool {
    pub policy: SandboxPolicy,
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run a non-interactive shell command in the project (`sh -c` on Unix, `cmd /C` on \
         Windows) and return its exit code and combined output. No TTY by default (commands \
         that block on input fail fast); set `pty: true` to run under a pseudo-terminal so \
         tty-detecting programs see isatty=true — stdin is still closed so prompts get EOF. \
         Prefer read_file/search/list_dir over cat/grep/ls. Args: command (required), cwd \
         (default \".\"), timeout_secs (default 120, max 600), pty (default false)."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Shell
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "POSIX sh -c command line." },
                "cwd": { "type": "string", "description": "Working directory; defaults to the project root." },
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "Default 120; clamped to 600." },
                "pty": { "type": "boolean", "description": "Run under a pseudo-terminal so interactive/tty-detecting programs work (default false). Stdin is still closed (EOF) so prompts won't hang forever. Refused when the shell sandbox is enabled (the PTY path can't be confined by Landlock)." }
            },
            "required": ["command"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let command = str_arg(args, "command")?;
        let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
        let timeout_secs = args
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        let use_pty = args.get("pty").and_then(Value::as_bool).unwrap_or(false);
        // The PTY path can't carry the Landlock sandbox (portable-pty owns the spawn). Refuse pty
        // when the sandbox is on, otherwise it's a trivial escape hatch (always pass pty:true).
        if use_pty && self.policy.enabled {
            return Ok(
                "shell: pty:true is disabled while the shell sandbox is enabled (the PTY path \
                 cannot be confined by Landlock). Re-run without pty."
                    .to_string(),
            );
        }
        if use_pty {
            Ok(run_command_pty(command, cwd, timeout_secs).await)
        } else {
            Ok(run_command(command, cwd, timeout_secs, &self.policy).await)
        }
    }
}

/// Execute `command` and format a model-facing result. Never returns `Err`: a failed spawn,
/// a non-zero exit, and a timeout are all normal results the model can react to.
///
/// When `policy.enabled` is true and the platform supports Landlock, the child process runs
/// under a kernel-enforced sandbox that confines filesystem writes to the workspace + temp dir.
/// If Landlock is unavailable a one-time `tracing::warn!` is emitted (in the parent, once per
/// process) and the command runs unconfined.
pub async fn run_command(
    command: &str,
    cwd: &str,
    timeout_secs: u64,
    policy: &SandboxPolicy,
) -> String {
    let (shell, flag) = shell_invocation();
    let mut cmd = Command::new(shell);
    cmd.arg(flag)
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    put_in_own_process_group(&mut cmd);

    // Sandbox wiring: probe in the parent, install pre_exec only when supported.
    maybe_install_sandbox(&mut cmd, policy, cwd);

    let start = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return format!("shell: failed to start (cwd {cwd}): {e}"),
    };
    let pgid = child.id().map(|id| id as i32);
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let out_task = tokio::spawn(read_capped(stdout));
    let err_task = tokio::spawn(read_capped(stderr));

    let (status_line, exit_code) =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
            Ok(Ok(status)) => {
                let code = status.code();
                (
                    format!(
                        "exit {}",
                        code.map(|c| c.to_string()).unwrap_or("signal".into())
                    ),
                    code,
                )
            }
            Ok(Err(e)) => (format!("error: {e}"), None),
            Err(_) => {
                terminate(&mut child, pgid).await;
                (format!("timed out after {timeout_secs}s (killed)"), None)
            }
        };

    let (out_bytes, out_capped) = out_task.await.unwrap_or_default();
    let (err_bytes, err_capped) = err_task.await.unwrap_or_default();
    let duration_ms = start.elapsed().as_millis();

    let body = render_streams(&out_bytes, &err_bytes);
    let (body, truncated) = truncate_for_model(&body, MODEL_BUDGET);
    let total = out_bytes.len() + err_bytes.len();
    let mut header = format!("shell: {status_line} in {duration_ms}ms");
    if truncated || out_capped || err_capped {
        header.push_str(&format!("  ({total} bytes captured, output truncated)"));
    }
    let _ = exit_code; // exit code is conveyed in status_line; reserved for richer wiring later
    if body.trim().is_empty() {
        header
    } else {
        format!("{header}\n\n{body}")
    }
}

/// Execute `command` under a pseudo-terminal (PTY) so `isatty(stdout)` returns true in the child.
///
/// This is the opt-in path (`pty: true`). Key differences from [`run_command`]:
///
/// - Uses `portable-pty` to open a native PTY (Unix `openpty`, Windows ConPTY).
/// - Stdin is the slave end — the OS sees a real tty, but we do not write to it, so the child
///   receives EOF on any stdin read (programs prompting on stdin exit rather than hanging).
/// - Combined stdout+stderr comes from the PTY master (the OS merges both streams).
/// - **Sandbox**: the Landlock sandbox does NOT apply here. `portable-pty` owns the spawn and
///   does not expose a `pre_exec` hook. V1 limitation — see shell-sandbox docs.
/// - Timeout + kill: on timeout the master fd is dropped (closing the PTY) and the child PID is
///   killed with SIGKILL (Unix) / TerminateProcess (Windows) via a dedicated blocking task.
///   Dropping the master makes the reader's blocking `read()` return immediately (EIO/EOF),
///   so the reader task unblocks within milliseconds without needing cancellation.
///
/// Output format is identical to [`run_command`]: `shell: <status> in <ms>ms\n\n<body>`.
pub async fn run_command_pty(command: &str, cwd: &str, timeout_secs: u64) -> String {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::time::Instant;

    let start = Instant::now();
    let pty_system = native_pty_system();

    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => return format!("shell(pty): failed to open pty: {e}"),
    };

    let (shell, flag) = shell_invocation();
    let mut cb = CommandBuilder::new(shell);
    cb.arg(flag);
    cb.arg(command);
    cb.cwd(cwd);

    // Spawn into the slave end.
    let mut child = match pair.slave.spawn_command(cb) {
        Ok(c) => c,
        Err(e) => return format!("shell(pty): failed to spawn (cwd {cwd}): {e}"),
    };
    // Drop the slave fd after spawn — when the child exits the master side will see EOF.
    drop(pair.slave);

    // Clone a reader from the master before we need to move `pair.master` for the kill path.
    let mut master_reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            let _ = child.kill();
            return format!("shell(pty): failed to clone pty reader: {e}");
        }
    };

    // Read the PTY master in a blocking task. The loop exits on EOF or error (EIO after the
    // master fd is closed — which we trigger on timeout by dropping `pair.master`).
    let read_task: tokio::task::JoinHandle<(Vec<u8>, bool)> =
        tokio::task::spawn_blocking(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 8192];
            let mut capped = false;
            loop {
                match std::io::Read::read(&mut master_reader, &mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < CAPTURE_CAP {
                            let take = n.min(CAPTURE_CAP - buf.len());
                            buf.extend_from_slice(&tmp[..take]);
                            if buf.len() >= CAPTURE_CAP {
                                capped = true;
                            }
                        } else {
                            capped = true;
                        }
                    }
                    Err(_) => break, // EIO when master fd is closed
                }
            }
            (buf, capped)
        });

    // Wait for the child with a timeout.
    //
    // Kill strategy on timeout:
    //   1. Drop the PTY master — this sends HUP/EIO to the child and unblocks the reader task.
    //   2. Kill the child's OS process directly via its PID (SIGKILL on Unix).
    //
    // We cannot move `child` into `spawn_blocking` and also keep it accessible for killing,
    // so we wait on a channel: the blocking task sends the exit status back.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut child_for_kill = child;
    // Extract PID before moving into the blocking task for use in the kill path.
    let child_pid = child_for_kill.process_id();
    tokio::task::spawn_blocking(move || {
        let result = child_for_kill.wait();
        let _ = tx.send(result);
    });

    let (status_line, _exit_code) =
        match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(Ok(status))) => {
                let code = status.exit_code();
                (
                    format!(
                        "exit {}",
                        if status.success() {
                            "0".to_string()
                        } else {
                            code.to_string()
                        }
                    ),
                    Some(code),
                )
            }
            Ok(Ok(Err(e))) => (format!("error: {e}"), None),
            Ok(Err(_)) => ("error: wait channel dropped".to_string(), None),
            Err(_) => {
                // Timeout: close the master (sends EIO to the reader task) then kill the process.
                drop(pair.master);
                pty_kill_child(child_pid);
                (format!("timed out after {timeout_secs}s (killed)"), None)
            }
        };

    let duration_ms = start.elapsed().as_millis();

    // The reader task unblocks as soon as the master is closed (on timeout) or the child exits
    // (normal path). Give it a short extra window to flush any buffered bytes.
    let (raw_bytes, capped) = tokio::time::timeout(Duration::from_secs(5), read_task)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();

    // PTY merges stdout+stderr; render the combined bytes as a single stream.
    let body = stream_text(&raw_bytes)
        .map(|s| {
            if s.trim().is_empty() {
                String::new()
            } else {
                s
            }
        })
        .unwrap_or_default();
    let (body, truncated) = truncate_for_model(&body, MODEL_BUDGET);
    let total = raw_bytes.len();
    let mut header = format!("shell: {status_line} in {duration_ms}ms");
    if truncated || capped {
        header.push_str(&format!("  ({total} bytes captured, output truncated)"));
    }
    if body.trim().is_empty() {
        header
    } else {
        format!("{header}\n\n{body}")
    }
}

/// Kill a child process by PID after a PTY timeout.
///
/// On Unix: SIGKILL to the process (not the group — portable-pty does not set a separate pgid).
/// On Windows: no-op (the PTY master close is sufficient for ConPTY to terminate the child).
/// This is a best-effort cleanup; errors are swallowed.
fn pty_kill_child(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(p) = pid {
        unsafe { libc::kill(p as i32, libc::SIGKILL) };
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

/// Render combined streams, ANSI-stripped, with binary output summarized rather than dumped.
fn render_streams(out: &[u8], err: &[u8]) -> String {
    let mut parts = Vec::new();
    if let Some(s) = stream_text(out) {
        if !s.trim().is_empty() {
            parts.push(s);
        }
    }
    if let Some(s) = stream_text(err) {
        if !s.trim().is_empty() {
            parts.push(format!("[stderr]\n{s}"));
        }
    }
    parts.join("\n")
}

fn stream_text(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    if bytes.contains(&0) {
        return Some(format!("<binary output: {} bytes, not shown>", bytes.len()));
    }
    Some(strip_ansi(&String::from_utf8_lossy(bytes)))
}

/// Read an async stream up to [`CAPTURE_CAP`] bytes; returns the bytes and whether it capped.
async fn read_capped<R: AsyncRead + Unpin>(mut r: R) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut capped = false;
    loop {
        match r.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                if buf.len() < CAPTURE_CAP {
                    let take = n.min(CAPTURE_CAP - buf.len());
                    buf.extend_from_slice(&chunk[..take]);
                    if buf.len() >= CAPTURE_CAP {
                        capped = true;
                    }
                } else {
                    capped = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, capped)
}

/// Probe Landlock support once per process and emit the "unconfined" warning at most once.
/// Returns `true` if a sandbox pre_exec should be installed.
fn sandbox_supported_once(policy: &SandboxPolicy) -> bool {
    if !policy.enabled {
        return false;
    }
    // Fast path: check support and warn once.
    use std::sync::OnceLock;
    static CHECKED: OnceLock<bool> = OnceLock::new();
    let supported = *CHECKED.get_or_init(|| {
        let s = sandbox::is_supported();
        if !s {
            tracing::warn!("Landlock unavailable; shell runs unconfined (sandbox = true has no effect on this kernel)");
        }
        s
    });
    supported
}

/// Attach a `pre_exec` hook to `cmd` that applies the Landlock sandbox in the child process.
/// On non-Unix platforms and when the sandbox is disabled or unsupported this is a no-op.
fn maybe_install_sandbox(cmd: &mut Command, policy: &SandboxPolicy, cwd: &str) {
    if !sandbox_supported_once(policy) {
        return;
    }

    // Build the writable set in the parent (before fork) — PathBuf is Send + Clone.
    let cwd_path = PathBuf::from(cwd);
    let extra: Vec<PathBuf> = policy.writable.iter().map(PathBuf::from).collect();
    let writable = sandbox::effective_writable(&cwd_path, &extra);

    // Install the pre_exec closure. It runs after fork, before exec — in the child only.
    // Landlock syscalls are async-signal-safe.
    #[cfg(target_os = "linux")]
    {
        unsafe {
            // tokio::process::Command exposes pre_exec via std::os::unix::process::CommandExt
            // which is blanket-implemented — no explicit use needed.
            cmd.pre_exec(move || {
                // Errors are swallowed: a sandbox failure must never prevent the command
                // from running (best-effort confinement).
                let _ = crate::sandbox::linux::apply_landlock(&writable);
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cmd, writable);
    }
}

/// The OS shell + its "run this command string" flag: `sh -c` on Unix, `cmd /C` on Windows
/// (Windows has no `sh` by default). Keeps the tool runnable on all three platforms
/// (cross-platform mandate; known-issues.md).
fn shell_invocation() -> (&'static str, &'static str) {
    #[cfg(windows)]
    {
        ("cmd", "/C")
    }
    #[cfg(not(windows))]
    {
        ("sh", "-c")
    }
}

/// Put the child in its own process group so a timeout kill takes down the whole tree.
fn put_in_own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Kill the child's whole process tree on timeout: SIGTERM→grace→SIGKILL on the process group
/// (Unix); `taskkill /F /T` on Windows. The tree matters because `cmd /C`/`sh -c` spawn the real
/// command as a child — killing only the shell would leave it running and hold the output pipes
/// open, hanging the read until it exits on its own.
async fn terminate(child: &mut Child, pgid: Option<i32>) {
    #[cfg(unix)]
    {
        if let Some(pg) = pgid {
            unsafe { libc::kill(-pg, libc::SIGTERM) };
            tokio::time::sleep(KILL_GRACE).await;
            unsafe { libc::kill(-pg, libc::SIGKILL) };
        }
        let _ = child.wait().await;
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        if let Some(pid) = child.id() {
            // `/T` kills the tree (the command `cmd /C` spawned), `/F` forces it.
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

/// Strip ANSI/CSI escape sequences (token noise) from model-facing text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // CSI: ESC [ ... <final byte 0x40..0x7e>
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&n) {
                        break;
                    }
                }
            } else {
                // other escape (e.g. ESC ] ...): drop the next char defensively
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Keep head + tail within `budget` bytes, with a middle marker. Char-boundary safe.
fn truncate_for_model(s: &str, budget: usize) -> (String, bool) {
    if s.len() <= budget {
        return (s.to_string(), false);
    }
    let head_len = floor_boundary(s, budget / 2);
    let tail_len = floor_boundary_back(s, budget - head_len);
    let dropped = s.len() - head_len - tail_len;
    let head = &s[..head_len];
    let tail = &s[s.len() - tail_len..];
    (
        format!("{head}\n… {dropped} bytes truncated …\n{tail}"),
        true,
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn floor_boundary_back(s: &str, len: usize) -> usize {
    let mut start = s.len().saturating_sub(len);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s.len() - start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_color_codes() {
        let colored = "\x1b[31mred\x1b[0m plain";
        assert_eq!(strip_ansi(colored), "red plain");
    }

    #[test]
    fn truncate_keeps_head_and_tail_with_marker() {
        let s = "a".repeat(20_000);
        let (t, truncated) = truncate_for_model(&s, 8 * 1024);
        assert!(truncated);
        assert!(t.contains("truncated"));
        assert!(t.len() < s.len());
    }

    #[test]
    fn small_output_not_truncated() {
        let (t, truncated) = truncate_for_model("hello", 8 * 1024);
        assert!(!truncated);
        assert_eq!(t, "hello");
    }

    #[test]
    fn binary_output_is_summarized() {
        let s = stream_text(&[0u8, 1, 2, 3]).unwrap();
        assert!(s.contains("binary output"));
    }

    // Execution tests are POSIX-only (the tool shells out to `sh`).
    #[cfg(unix)]
    mod exec {
        use super::*;

        fn no_sandbox() -> SandboxPolicy {
            SandboxPolicy::default()
        }

        #[tokio::test]
        async fn runs_and_captures_stdout_and_exit() {
            let out = run_command("echo hello", ".", 30, &no_sandbox()).await;
            assert!(out.contains("hello"), "stdout captured: {out}");
            assert!(out.contains("exit 0"), "exit code reported: {out}");
        }

        #[tokio::test]
        async fn non_zero_exit_is_reported_with_output() {
            let out = run_command("echo oops >&2; exit 3", ".", 30, &no_sandbox()).await;
            assert!(out.contains("exit 3"), "non-zero exit: {out}");
            assert!(out.contains("oops"), "stderr captured: {out}");
        }

        #[tokio::test]
        async fn command_not_found_is_a_normal_result() {
            let out = run_command("definitelynotacommand_xyz", ".", 30, &no_sandbox()).await;
            assert!(out.contains("exit 127"), "not-found exit 127: {out}");
        }

        #[tokio::test]
        async fn timeout_kills_and_reports() {
            let start = Instant::now();
            let out = run_command("sleep 30", ".", 1, &no_sandbox()).await;
            assert!(out.contains("timed out"), "timeout reported: {out}");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "must not wait for the full sleep"
            );
        }

        #[tokio::test]
        async fn stdin_is_closed_no_hang() {
            // `cat` with no args reads stdin; with /dev/null it gets EOF and exits promptly.
            let out = run_command("cat", ".", 5, &no_sandbox()).await;
            assert!(out.contains("exit 0"), "cat got EOF and exited: {out}");
        }

        #[tokio::test]
        async fn bad_cwd_is_a_spawn_failure_not_a_panic() {
            let out = run_command("echo hi", "/no/such/dir/xyz", 5, &no_sandbox()).await;
            assert!(out.contains("failed to start"), "spawn failure: {out}");
        }

        /// Cross-platform: a disabled sandbox must not change command behaviour.
        #[tokio::test]
        async fn disabled_sandbox_runs_normally() {
            let policy = SandboxPolicy {
                enabled: false,
                writable: vec![],
            };
            let out = run_command("echo sandbox_off", ".", 10, &policy).await;
            assert!(out.contains("sandbox_off"), "output: {out}");
            assert!(out.contains("exit 0"), "exit: {out}");
        }
    }

    // PTY execution tests — Unix only (portable-pty's UnixPty backend).
    #[cfg(unix)]
    mod pty {
        use super::*;

        /// pty:true must be refused while the shell sandbox is on (else it's a Landlock escape).
        #[tokio::test]
        async fn pty_refused_under_sandbox() {
            let tool = ShellTool {
                policy: crate::SandboxPolicy {
                    enabled: true,
                    writable: vec![],
                },
            };
            let out = tool
                .run(&serde_json::json!({"command": "echo hi", "pty": true}))
                .await
                .unwrap();
            assert!(
                out.contains("disabled while the shell sandbox"),
                "got: {out}"
            );
        }

        /// A plain `echo hello` under PTY must return "hello" in the output and exit 0.
        #[tokio::test]
        async fn pty_echo_hello() {
            let out = run_command_pty("echo hello", ".", 30).await;
            assert!(out.contains("hello"), "pty echo output: {out}");
            assert!(out.contains("exit 0"), "pty exit code: {out}");
        }

        /// With pty:true, the child process sees a real TTY (isatty returns true).
        /// Without pty (default), the child has no TTY.
        #[tokio::test]
        async fn pty_isatty_true_vs_notty() {
            // `test -t 1` exits 0 if fd 1 is a tty; we echo a marker based on that.
            let pty_out = run_command_pty("test -t 1 && echo TTY || echo NOTTY", ".", 10).await;
            assert!(
                pty_out.contains("TTY") && !pty_out.contains("NOTTY"),
                "pty:true should report TTY, got: {pty_out}"
            );

            let no_pty_out = run_command(
                "test -t 1 && echo TTY || echo NOTTY",
                ".",
                10,
                &SandboxPolicy::default(),
            )
            .await;
            assert!(
                no_pty_out.contains("NOTTY"),
                "pty:false should report NOTTY, got: {no_pty_out}"
            );
        }

        /// A slow command under PTY must time out quickly without hanging.
        #[tokio::test]
        async fn pty_timeout_kills_fast() {
            let start = Instant::now();
            let out = run_command_pty("sleep 60", ".", 1).await;
            assert!(out.contains("timed out"), "pty timeout reported: {out}");
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "must not wait for the full sleep, elapsed: {:?}",
                start.elapsed()
            );
        }
    }

    // Execution tests for the Windows `cmd /C` path — run on the windows-latest CI runner.
    #[cfg(windows)]
    mod exec_windows {
        use super::*;

        fn no_sandbox() -> SandboxPolicy {
            SandboxPolicy::default()
        }

        #[tokio::test]
        async fn runs_and_captures_stdout_and_exit() {
            let out = run_command("echo hello", ".", 30, &no_sandbox()).await;
            assert!(out.contains("hello"), "stdout captured: {out}");
            assert!(out.contains("exit 0"), "exit code reported: {out}");
        }

        #[tokio::test]
        async fn non_zero_exit_is_reported() {
            let out = run_command("exit 3", ".", 30, &no_sandbox()).await;
            assert!(out.contains("exit 3"), "non-zero exit: {out}");
        }

        #[tokio::test]
        async fn timeout_kills_and_reports() {
            // `ping -n 20` sleeps ~19s between echoes; a 1s timeout must kill it fast.
            let start = Instant::now();
            let out = run_command("ping -n 20 127.0.0.1", ".", 1, &no_sandbox()).await;
            assert!(out.contains("timed out"), "timeout reported: {out}");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "must not wait for the full ping"
            );
        }

        #[tokio::test]
        async fn bad_cwd_is_a_spawn_failure_not_a_panic() {
            let out = run_command("echo hi", "Z:\\no\\such\\dir\\xyz", 5, &no_sandbox()).await;
            assert!(out.contains("failed to start"), "spawn failure: {out}");
        }
    }
}
