//! The `shell` tool: run a non-interactive command via the OS shell (`sh -c` on Unix, `cmd /C`
//! on Windows), capture stdout/stderr and the exit code, with a timeout that kills the whole
//! process group. Safety (allow/ask/deny, the catastrophic denylist, secret reads) is enforced
//! upstream by the permission broker (`forge-core::permission`) against the rule engine — this
//! tool only executes.
//!
//! Non-interactive only: stdin is null, no TTY. Model-facing output is ANSI-stripped and
//! truncated to a token budget; the true byte size is reported.
//!
//! Note: the catastrophic denylist patterns are still POSIX-oriented (`rm -rf`, secret-file
//! reads); Windows-specific dangerous-command patterns are a follow-up (known-issues.md).
//!
//! Deferred to follow-ups (see docs/features/shell-tool.md): live output streaming to the
//! TUI (`ToolOutputDelta`/`ToolEnd`), background jobs (`shell_poll`/`shell_kill`),
//! session-remembered allows, and the rich command-context permission prompt.

use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use forge_types::SideEffect;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};

use crate::{str_arg, Tool, ToolError};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
/// Bytes captured per stream before we stop reading (memory bound).
const CAPTURE_CAP: usize = 1 << 20; // 1 MiB
/// Bytes of combined output handed back to the model (token budget).
const MODEL_BUDGET: usize = 8 * 1024;
/// Grace period between SIGTERM and SIGKILL on timeout (Unix process-group kill).
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(2);

pub struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run a non-interactive shell command in the project (`sh -c` on Unix, `cmd /C` on \
         Windows) and return its exit code and combined output. No TTY/stdin (commands that \
         block on input fail fast). Prefer read_file/search/list_dir over cat/grep/ls. Args: \
         command (required), cwd (default \".\"), timeout_secs (default 120, max 600)."
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
                "timeout_secs": { "type": "integer", "minimum": 1, "description": "Default 120; clamped to 600." }
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
        Ok(run_command(command, cwd, timeout_secs).await)
    }
}

/// Execute `command` and format a model-facing result. Never returns `Err`: a failed spawn,
/// a non-zero exit, and a timeout are all normal results the model can react to.
async fn run_command(command: &str, cwd: &str, timeout_secs: u64) -> String {
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

/// Kill the child's process group: SIGTERM, grace, SIGKILL (Unix); plain kill elsewhere.
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

        #[tokio::test]
        async fn runs_and_captures_stdout_and_exit() {
            let out = run_command("echo hello", ".", 30).await;
            assert!(out.contains("hello"), "stdout captured: {out}");
            assert!(out.contains("exit 0"), "exit code reported: {out}");
        }

        #[tokio::test]
        async fn non_zero_exit_is_reported_with_output() {
            let out = run_command("echo oops >&2; exit 3", ".", 30).await;
            assert!(out.contains("exit 3"), "non-zero exit: {out}");
            assert!(out.contains("oops"), "stderr captured: {out}");
        }

        #[tokio::test]
        async fn command_not_found_is_a_normal_result() {
            let out = run_command("definitelynotacommand_xyz", ".", 30).await;
            assert!(out.contains("exit 127"), "not-found exit 127: {out}");
        }

        #[tokio::test]
        async fn timeout_kills_and_reports() {
            let start = Instant::now();
            let out = run_command("sleep 30", ".", 1).await;
            assert!(out.contains("timed out"), "timeout reported: {out}");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "must not wait for the full sleep"
            );
        }

        #[tokio::test]
        async fn stdin_is_closed_no_hang() {
            // `cat` with no args reads stdin; with /dev/null it gets EOF and exits promptly.
            let out = run_command("cat", ".", 5).await;
            assert!(out.contains("exit 0"), "cat got EOF and exited: {out}");
        }

        #[tokio::test]
        async fn bad_cwd_is_a_spawn_failure_not_a_panic() {
            let out = run_command("echo hi", "/no/such/dir/xyz", 5).await;
            assert!(out.contains("failed to start"), "spawn failure: {out}");
        }
    }

    // Execution tests for the Windows `cmd /C` path — run on the windows-latest CI runner.
    #[cfg(windows)]
    mod exec_windows {
        use super::*;

        #[tokio::test]
        async fn runs_and_captures_stdout_and_exit() {
            let out = run_command("echo hello", ".", 30).await;
            assert!(out.contains("hello"), "stdout captured: {out}");
            assert!(out.contains("exit 0"), "exit code reported: {out}");
        }

        #[tokio::test]
        async fn non_zero_exit_is_reported() {
            let out = run_command("exit 3", ".", 30).await;
            assert!(out.contains("exit 3"), "non-zero exit: {out}");
        }

        #[tokio::test]
        async fn timeout_kills_and_reports() {
            // `ping -n 20` sleeps ~19s between echoes; a 1s timeout must kill it fast.
            let start = Instant::now();
            let out = run_command("ping -n 20 127.0.0.1", ".", 1).await;
            assert!(out.contains("timed out"), "timeout reported: {out}");
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "must not wait for the full ping"
            );
        }

        #[tokio::test]
        async fn bad_cwd_is_a_spawn_failure_not_a_panic() {
            let out = run_command("echo hi", "Z:\\no\\such\\dir\\xyz", 5).await;
            assert!(out.contains("failed to start"), "spawn failure: {out}");
        }
    }
}
