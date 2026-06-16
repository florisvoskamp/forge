//! Pre/post tool-use shell hooks (docs/features/hooks.md). Each `[[hooks]]` entry runs a POSIX
//! `sh -c` command around a matching tool call, receiving the call as JSON on stdin. A
//! `PreToolUse` hook that exits non-zero **blocks** the tool (its output is the reason the model
//! sees); `PostToolUse` hooks observe (their stdout surfaces as a note). Hooks are POSIX-only
//! (same as the shell tool — see known-issues.md) and time-bounded so a wedged hook can't hang
//! the agent.

use std::process::Stdio;
use std::time::Duration;

use forge_config::{HookConfig, HookEvent};
use tokio::io::AsyncWriteExt;

/// The combined effect of running the hooks that matched one tool call + event.
#[derive(Debug, Default)]
pub struct HookOutcome {
    /// `Some(reason)` if a `PreToolUse` hook blocked the call.
    pub blocked: Option<String>,
    /// Lines to surface to the user (hook stdout / errors).
    pub notes: Vec<String>,
}

/// Run every hook matching `event` + `tool`, in declaration order. The first `PreToolUse` hook
/// that exits non-zero blocks and short-circuits. A hook that fails to launch is noted, not fatal.
pub async fn run_hooks(
    hooks: &[HookConfig],
    event: HookEvent,
    tool: &str,
    payload: &str,
) -> HookOutcome {
    let mut outcome = HookOutcome::default();
    for h in hooks.iter().filter(|h| h.event == event && h.matches(tool)) {
        match run_one(h, payload).await {
            Ok((code, stdout, stderr)) => {
                let out = truncate(stdout.trim(), 800);
                if !out.is_empty() {
                    outcome.notes.push(format!("⎇ hook: {out}"));
                }
                if event == HookEvent::PreToolUse && code != 0 {
                    let err = stderr.trim();
                    let reason = if !err.is_empty() {
                        truncate(err, 800)
                    } else if !out.is_empty() {
                        out
                    } else {
                        format!("{tool} blocked by hook (exit {code})")
                    };
                    outcome.blocked = Some(reason);
                    break;
                }
            }
            Err(e) => outcome.notes.push(format!("⎇ hook error: {e}")),
        }
    }
    outcome
}

async fn run_one(h: &HookConfig, payload: &str) -> Result<(i32, String, String), String> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&h.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true) // a timeout drops the future → the child is killed, not orphaned
        .spawn()
        .map_err(|e| e.to_string())?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        // Dropping `stdin` here sends EOF so a hook that reads to end returns.
    }

    let out = match tokio::time::timeout(
        Duration::from_secs(h.timeout_secs),
        child.wait_with_output(),
    )
    .await
    {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err(format!("timed out after {}s", h.timeout_secs)),
    };

    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars().take(max).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: HookEvent, command: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: None,
            command: command.into(),
            timeout_secs: 10,
        }
    }

    #[tokio::test]
    async fn pretooluse_nonzero_exit_blocks_with_stderr_reason() {
        let hooks = vec![hook(HookEvent::PreToolUse, "echo nope 1>&2; exit 1")];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert_eq!(o.blocked.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn pretooluse_zero_exit_does_not_block_and_stdout_is_a_note() {
        let hooks = vec![hook(HookEvent::PreToolUse, "echo looks-good")];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none());
        assert!(o.notes.iter().any(|n| n.contains("looks-good")));
    }

    #[tokio::test]
    async fn hook_receives_payload_on_stdin() {
        // The hook echoes back stdin; we assert the payload round-trips.
        let hooks = vec![hook(HookEvent::PostToolUse, "cat")];
        let o = run_hooks(
            &hooks,
            HookEvent::PostToolUse,
            "shell",
            "{\"tool\":\"shell\"}",
        )
        .await;
        assert!(o.notes.iter().any(|n| n.contains("\"tool\":\"shell\"")));
    }

    #[tokio::test]
    async fn matcher_skips_non_matching_tools() {
        let mut h = hook(HookEvent::PreToolUse, "exit 1");
        h.matcher = Some("edit_file".into());
        // Tool is "shell", hook matches only "edit_file" → not run → not blocked.
        let o = run_hooks(&[h], HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none());
    }

    #[tokio::test]
    async fn a_wedged_hook_times_out_instead_of_hanging() {
        let mut h = hook(HookEvent::PreToolUse, "sleep 30");
        h.timeout_secs = 1;
        let o = run_hooks(&[h], HookEvent::PreToolUse, "shell", "{}").await;
        // Timeout is a launch error (noted), not a block.
        assert!(o.blocked.is_none());
        assert!(o.notes.iter().any(|n| n.contains("timed out")));
    }
}
