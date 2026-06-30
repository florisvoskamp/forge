//! Shell hooks (docs/features/hooks.md). Each `[[hooks]]` entry runs via the OS shell around
//! tool calls and session lifecycle events. A `PreToolUse` hook that exits non-zero **blocks**
//! the tool; a `UserPromptSubmit` hook can rewrite the user's prompt (stdout replaces it on
//! exit 0) or block the turn (non-zero). Session hooks (`SessionStart`/`SessionEnd`) observe
//! only. Hooks are time-bounded so a wedged hook can't hang the agent.

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
    /// Rewritten tool args from a `PreToolUse` hook that exited 0 and emitted a JSON object on
    /// stdout. The core substitutes these args for the model's original args before running the
    /// tool. `None` means use the original args unchanged.
    pub rewritten_args: Option<serde_json::Value>,
    /// Context strings a hook asked to inject into the transcript (`{"action":"inject",
    /// "context":"…"}`). The core queues each as a model-visible system hint after the tool runs —
    /// so a hook can feed the model extra context (lint output, "this file is generated", a policy
    /// reminder) without blocking or rewriting. Works for both `PreToolUse` and `PostToolUse`.
    pub injected_context: Vec<String>,
}

/// A structured directive a hook can emit on stdout as a JSON object with an `"action"` field.
/// This is the richer protocol on top of the legacy "bare JSON object = rewritten args" behavior:
/// a `PreToolUse` hook that emits a JSON object WITHOUT an `action` still rewrites args as before.
enum HookDirective {
    /// `{"action":"rewrite","args":{…}}` — replace the tool's args (PreToolUse).
    Rewrite(serde_json::Value),
    /// `{"action":"inject","context":"…"}` — add model-visible context after the call.
    Inject(String),
    /// `{"action":"block","reason":"…"}` — block the call (PreToolUse; downgraded to a note elsewhere).
    Block(String),
    /// `{"action":"allow"}` — explicit no-op (the hook approves without changing anything).
    Noop,
    /// Anything else (non-JSON, or a JSON object that isn't a recognised directive) → a user note.
    Note(String),
}

/// Interpret a hook's exit-0 stdout. The structured `action` protocol takes precedence; a bare JSON
/// object (no `action`) keeps the legacy meaning (rewrite args, but only for `PreToolUse`); anything
/// else is a note. A malformed structured directive (missing `args`/`context`) degrades to a note so
/// the author sees their output rather than it silently vanishing.
fn parse_hook_directive(stdout: &str, event: HookEvent) -> HookDirective {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(stdout)
    else {
        return HookDirective::Note(stdout.to_string());
    };
    if let Some(action) = map.get("action").and_then(serde_json::Value::as_str) {
        return match action {
            "rewrite" => map
                .get("args")
                .cloned()
                .map(HookDirective::Rewrite)
                .unwrap_or_else(|| HookDirective::Note(stdout.to_string())),
            "inject" => map
                .get("context")
                .and_then(serde_json::Value::as_str)
                .filter(|c| !c.trim().is_empty())
                .map(|c| HookDirective::Inject(c.to_string()))
                .unwrap_or_else(|| HookDirective::Note(stdout.to_string())),
            "block" => HookDirective::Block(
                map.get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("blocked by hook")
                    .to_string(),
            ),
            "allow" => HookDirective::Noop,
            _ => HookDirective::Note(stdout.to_string()),
        };
    }
    // Legacy: a bare JSON object rewrites args on PreToolUse; elsewhere it's just a note.
    if event == HookEvent::PreToolUse {
        HookDirective::Rewrite(serde_json::Value::Object(map))
    } else {
        HookDirective::Note(stdout.to_string())
    }
}

/// Translate Forge's native hook payload (`{tool, args, result?, ok?}` / `{prompt}`) into the
/// Claude-Code stdin shape so a CC hook script reads the fields it expects (`tool_name`,
/// `tool_input`, `tool_response`, `prompt`, `hook_event_name`, `cwd`, `session_id`,
/// `transcript_path`). Best-effort: `session_id`/`transcript_path` are empty unless the caller
/// supplied them — most CC hooks only read `tool_name`/`tool_input`, which always round-trip.
fn to_cc_payload(forge_payload: &str, event: HookEvent, session_id: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_str(forge_payload).unwrap_or(serde_json::Value::Null);
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut obj = serde_json::Map::new();
    obj.insert("session_id".into(), session_id.into());
    obj.insert("transcript_path".into(), "".into());
    obj.insert("cwd".into(), cwd.into());
    obj.insert("hook_event_name".into(), event.cc_name().into());
    if let Some(tool) = v.get("tool").and_then(|t| t.as_str()) {
        obj.insert("tool_name".into(), tool.into());
    }
    if let Some(args) = v.get("args") {
        obj.insert("tool_input".into(), args.clone());
    }
    if let Some(result) = v.get("result") {
        obj.insert("tool_response".into(), result.clone());
    }
    if let Some(prompt) = v.get("prompt") {
        obj.insert("prompt".into(), prompt.clone());
    }
    // Carry through any extra lifecycle fields (message, trigger, …) the caller already put in.
    if let Some(map) = v.as_object() {
        for (k, val) in map {
            if !["tool", "args", "result", "ok", "prompt", "event"].contains(&k.as_str()) {
                obj.entry(k.clone()).or_insert_with(|| val.clone());
            }
        }
    }
    serde_json::Value::Object(obj).to_string()
}

/// What a Claude-Code hook's output asked for, after interpreting exit code + stdout/stderr.
enum CcDecision {
    /// Block the call/turn (exit 2, `decision:block`, or `permissionDecision:deny`).
    Block(String),
    /// Approve/allow with no change (`decision:approve`, `permissionDecision:allow`).
    Noop,
    /// Inject this string as model-visible context (`additionalContext` / `hookSpecificOutput`).
    Context(String),
    /// Surface this text as a user note (plain stdout, or an unrecognised JSON shape).
    Note(String),
}

/// Interpret a Claude-Code hook's result (CC protocol): exit-code 2 blocks (stderr = reason);
/// otherwise parse stdout for `{"decision":"block|approve","reason":…}`, a `hookSpecificOutput`
/// object (`permissionDecision` / `additionalContext`), or a top-level `additionalContext`. Plain
/// non-JSON stdout becomes a note. Empty stdout + exit 0 is a clean no-op.
fn parse_cc_output(code: i32, stdout: &str, stderr: &str) -> CcDecision {
    if code == 2 {
        let err = stderr.trim();
        let reason = if !err.is_empty() {
            err
        } else if !stdout.trim().is_empty() {
            stdout.trim()
        } else {
            "blocked by hook (exit 2)"
        };
        return CcDecision::Block(truncate(reason, 800));
    }
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return CcDecision::Noop;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return CcDecision::Note(truncate(trimmed, 800));
    };
    if let Some(decision) = v.get("decision").and_then(|d| d.as_str()) {
        match decision {
            "block" => {
                let reason = v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("blocked by hook");
                return CcDecision::Block(truncate(reason, 800));
            }
            "approve" | "allow" => return CcDecision::Noop,
            _ => {}
        }
    }
    if let Some(hso) = v.get("hookSpecificOutput") {
        if hso.get("permissionDecision").and_then(|d| d.as_str()) == Some("deny") {
            let reason = hso
                .get("permissionDecisionReason")
                .and_then(|r| r.as_str())
                .unwrap_or("denied by hook");
            return CcDecision::Block(truncate(reason, 800));
        }
        if let Some(ctx) = hso.get("additionalContext").and_then(|c| c.as_str()) {
            if !ctx.trim().is_empty() {
                return CcDecision::Context(truncate(ctx, 2000));
            }
        }
    }
    if let Some(ctx) = v.get("additionalContext").and_then(|c| c.as_str()) {
        if !ctx.trim().is_empty() {
            return CcDecision::Context(truncate(ctx, 2000));
        }
    }
    CcDecision::Note(truncate(trimmed, 800))
}

/// Run one CC-compat hook and fold its decision into `outcome`. Returns `true` if the call should
/// be blocked + short-circuited (PreToolUse only; PostToolUse downgrades a block to a note).
async fn run_cc_hook(
    h: &HookConfig,
    event: HookEvent,
    payload: &str,
    outcome: &mut HookOutcome,
) -> bool {
    let cc_payload = to_cc_payload(payload, event, "");
    match run_one(h, &cc_payload).await {
        Ok((code, stdout, stderr)) => match parse_cc_output(code, &stdout, &stderr) {
            CcDecision::Block(reason) => {
                if event == HookEvent::PreToolUse {
                    outcome.blocked = Some(reason);
                    return true;
                }
                outcome.notes.push(format!("⎇ hook: {reason}"));
            }
            CcDecision::Context(ctx) => outcome.injected_context.push(ctx),
            CcDecision::Note(text) => outcome.notes.push(format!("⎇ hook: {text}")),
            CcDecision::Noop => {}
        },
        Err(e) => outcome.notes.push(format!("⎇ hook error: {e}")),
    }
    false
}

/// Run every hook matching `event` + `tool`, in declaration order. The first `PreToolUse` hook
/// that exits non-zero blocks and short-circuits. A hook that fails to launch is noted, not fatal.
/// CC-compat hooks ([`HookConfig::cc_compat`]) speak the Claude-Code protocol (CC stdin payload +
/// `decision`/exit-2 output); native hooks keep Forge's directive protocol.
pub async fn run_hooks(
    hooks: &[HookConfig],
    event: HookEvent,
    tool: &str,
    payload: &str,
) -> HookOutcome {
    let mut outcome = HookOutcome::default();
    for h in hooks.iter().filter(|h| h.event == event && h.matches(tool)) {
        if h.cc_compat {
            if run_cc_hook(h, event, payload, &mut outcome).await {
                break;
            }
            continue;
        }
        match run_one(h, payload).await {
            Ok((code, stdout, stderr)) => {
                let trimmed = stdout.trim();
                if event == HookEvent::PreToolUse && code != 0 {
                    let err = stderr.trim();
                    let reason = if !err.is_empty() {
                        truncate(err, 800)
                    } else if !trimmed.is_empty() {
                        truncate(trimmed, 800)
                    } else {
                        format!("{tool} blocked by hook (exit {code})")
                    };
                    outcome.blocked = Some(reason);
                    break;
                }
                // exit 0 + non-empty stdout: interpret the structured directive protocol (rewrite /
                // inject / block / allow), falling back to the legacy "bare object = rewrite" and to
                // a plain note. `block` only blocks on PreToolUse (Post can't unwind a finished call).
                if !trimmed.is_empty() {
                    match parse_hook_directive(trimmed, event) {
                        HookDirective::Rewrite(args) => outcome.rewritten_args = Some(args),
                        HookDirective::Inject(ctx) => outcome.injected_context.push(ctx),
                        HookDirective::Block(reason) => {
                            if event == HookEvent::PreToolUse {
                                outcome.blocked = Some(truncate(&reason, 800));
                                break;
                            }
                            outcome
                                .notes
                                .push(format!("⎇ hook: {}", truncate(&reason, 800)));
                        }
                        HookDirective::Noop => {}
                        HookDirective::Note(text) => outcome
                            .notes
                            .push(format!("⎇ hook: {}", truncate(&text, 800))),
                    }
                }
            }
            Err(e) => outcome.notes.push(format!("⎇ hook error: {e}")),
        }
    }
    outcome
}

/// Run `user_prompt_submit` hooks in declaration order.
///
/// Returns `Ok(prompt)` where `prompt` is either the original (no hook rewrote it) or the
/// stdout from the first hook that exited 0 and produced non-empty output.
/// Returns `Err(reason)` if any hook exits non-zero — the turn should be blocked.
pub async fn run_prompt_hooks(hooks: &[HookConfig], prompt: &str) -> Result<String, String> {
    let payload = format!(
        "{{\"prompt\":{}}}",
        serde_json::to_string(prompt).unwrap_or_default()
    );
    let mut current = prompt.to_string();
    for h in hooks
        .iter()
        .filter(|h| h.event == HookEvent::UserPromptSubmit)
    {
        if h.cc_compat {
            // CC UserPromptSubmit: a block decision / exit-2 blocks the turn; otherwise stdout (and
            // `additionalContext`) is APPENDED as extra context (CC semantics — it doesn't replace
            // the prompt the way a native prompt hook does).
            let cc_payload = to_cc_payload(&payload, HookEvent::UserPromptSubmit, "");
            match run_one(h, &cc_payload).await {
                Ok((code, stdout, stderr)) => match parse_cc_output(code, &stdout, &stderr) {
                    CcDecision::Block(reason) => return Err(reason),
                    CcDecision::Context(ctx) | CcDecision::Note(ctx) => {
                        current = format!("{current}\n\n{ctx}");
                    }
                    CcDecision::Noop => {}
                },
                Err(e) => eprintln!("⎇ hook error: {e}"),
            }
            continue;
        }
        match run_one(h, &payload).await {
            Ok((code, stdout, stderr)) => {
                if code != 0 {
                    let reason = if !stderr.trim().is_empty() {
                        truncate(stderr.trim(), 800)
                    } else if !stdout.trim().is_empty() {
                        truncate(stdout.trim(), 800)
                    } else {
                        format!("prompt blocked by hook (exit {code})")
                    };
                    return Err(reason);
                }
                let out = stdout.trim().to_string();
                if !out.is_empty() {
                    current = out;
                }
            }
            Err(e) => {
                // Launch failure is noted but doesn't block the turn.
                eprintln!("⎇ hook error: {e}");
            }
        }
    }
    Ok(current)
}

/// Run session lifecycle hooks (`session_start` / `session_end`). Observe-only — exit code
/// is advisory, output is printed to stderr as a note.
pub async fn run_session_hooks(hooks: &[HookConfig], event: HookEvent, session_id: &str) {
    debug_assert!(
        matches!(event, HookEvent::SessionStart | HookEvent::SessionEnd),
        "run_session_hooks called with non-session event"
    );
    let event_str = match event {
        HookEvent::SessionStart => "session_start",
        HookEvent::SessionEnd => "session_end",
        _ => return,
    };
    let payload = format!(
        "{{\"session_id\":{},\"event\":{}}}",
        serde_json::to_string(session_id).unwrap_or_default(),
        serde_json::to_string(event_str).unwrap_or_default()
    );
    for h in hooks.iter().filter(|h| h.event == event) {
        match run_one(h, &payload).await {
            Ok((_, stdout, _)) => {
                let out = stdout.trim();
                if !out.is_empty() {
                    eprintln!("⎇ hook: {}", truncate(out, 800));
                }
            }
            Err(e) => eprintln!("⎇ hook error: {e}"),
        }
    }
}

/// The combined effect of running lifecycle hooks (`notification`, `pre_compact`, `post_compact`,
/// `stop`, `subagent_stop`) for one event.
#[derive(Debug, Default)]
pub struct LifecycleOutcome {
    /// `Some(reason)` if a hook asked to block (exit 2 / `decision:block`). For `stop`/`subagent_stop`
    /// this is the "keep going, don't stop yet" signal; the caller decides whether to honor it.
    pub blocked: Option<String>,
    /// Lines to surface to the user (hook stdout / decision reasons).
    pub notes: Vec<String>,
}

/// Run the Claude-Code lifecycle hooks Forge previously lacked: `Notification`, `PreCompact`,
/// `PostCompact`, `Stop`, `SubagentStop`. `fields` are merged into the stdin payload (e.g.
/// `{"message":…}` for a notification, `{"trigger":…}` for compaction). Native hooks receive
/// `{session_id, event, …fields}`; CC-compat hooks receive the CC shape with `hook_event_name`.
/// Observe-by-default: output is collected as notes; a block decision is reported but acting on it
/// is left to the caller (compaction/turn-stop continue regardless in this MVP).
pub async fn run_lifecycle_hooks(
    hooks: &[HookConfig],
    event: HookEvent,
    session_id: &str,
    fields: serde_json::Value,
) -> LifecycleOutcome {
    let mut outcome = LifecycleOutcome::default();
    // Native payload: {session_id, event, ...fields}.
    let mut base = serde_json::Map::new();
    base.insert("session_id".into(), session_id.into());
    base.insert("event".into(), event.cc_name().into());
    if let Some(map) = fields.as_object() {
        for (k, v) in map {
            base.insert(k.clone(), v.clone());
        }
    }
    let native_payload = serde_json::Value::Object(base).to_string();

    for h in hooks.iter().filter(|h| h.event == event) {
        let payload = if h.cc_compat {
            to_cc_payload(&native_payload, event, session_id)
        } else {
            native_payload.clone()
        };
        match run_one(h, &payload).await {
            Ok((code, stdout, stderr)) => match parse_cc_output(code, &stdout, &stderr) {
                CcDecision::Block(reason) => {
                    outcome.notes.push(format!("⎇ hook: {reason}"));
                    if outcome.blocked.is_none() {
                        outcome.blocked = Some(reason);
                    }
                }
                CcDecision::Context(ctx) | CcDecision::Note(ctx) => {
                    outcome.notes.push(format!("⎇ hook: {ctx}"));
                }
                CcDecision::Noop => {}
            },
            Err(e) => outcome.notes.push(format!("⎇ hook error: {e}")),
        }
    }
    outcome
}

fn hook_shell() -> (&'static str, &'static str) {
    #[cfg(windows)]
    return ("cmd", "/C");
    #[cfg(not(windows))]
    ("sh", "-c")
}

async fn run_one(h: &HookConfig, payload: &str) -> Result<(i32, String, String), String> {
    let (sh, sh_flag) = hook_shell();
    let mut child = tokio::process::Command::new(sh)
        .arg(sh_flag)
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
            cc_compat: false,
        }
    }

    fn cc_hook(event: HookEvent, command: &str) -> HookConfig {
        HookConfig {
            event,
            matcher: None,
            command: command.into(),
            timeout_secs: 10,
            cc_compat: true,
        }
    }

    #[tokio::test]
    async fn pretooluse_nonzero_exit_blocks_with_stderr_reason() {
        // sh uses `;` as separator; cmd uses `&` and needs `exit /b` for subprocess exit.
        #[cfg(not(windows))]
        let cmd = "echo nope 1>&2; exit 1";
        #[cfg(windows)]
        let cmd = "echo nope 1>&2 & exit /b 1";
        let hooks = vec![hook(HookEvent::PreToolUse, cmd)];
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

    #[tokio::test]
    async fn prompt_hook_exit_zero_passthrough_when_no_stdout() {
        let hooks = vec![hook(HookEvent::UserPromptSubmit, "true")];
        let result = run_prompt_hooks(&hooks, "hello world").await;
        assert_eq!(result.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn prompt_hook_exit_zero_with_stdout_rewrites_prompt() {
        let hooks = vec![hook(HookEvent::UserPromptSubmit, "echo rewritten")];
        let result = run_prompt_hooks(&hooks, "original").await;
        assert_eq!(result.unwrap(), "rewritten");
    }

    #[tokio::test]
    async fn prompt_hook_nonzero_exit_blocks_turn() {
        #[cfg(not(windows))]
        let cmd = "echo blocked reason 1>&2; exit 1";
        #[cfg(windows)]
        let cmd = "echo blocked reason 1>&2 & exit /b 1";
        let hooks = vec![hook(HookEvent::UserPromptSubmit, cmd)];
        let result = run_prompt_hooks(&hooks, "hello").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blocked reason"));
    }

    #[tokio::test]
    async fn session_hooks_observe_only_do_not_panic() {
        let hooks = vec![hook(HookEvent::SessionStart, "true")];
        run_session_hooks(&hooks, HookEvent::SessionStart, "test-session-id").await;
        // No assertion needed — observe-only hooks must not panic or hang.
    }

    // Windows cmd.exe mangles double-quoted JSON in `echo` output; the JSON-detection
    // logic is pure Rust and is exercised on Linux + macOS.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn pretooluse_exit_zero_json_object_stdout_rewrites_args() {
        let hooks = vec![hook(
            HookEvent::PreToolUse,
            "echo '{\"path\":\"rewritten.rs\"}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none());
        assert!(o.notes.is_empty(), "json stdout should not become a note");
        let rewritten = o.rewritten_args.expect("should have rewritten args");
        assert_eq!(rewritten["path"], "rewritten.rs");
    }

    #[tokio::test]
    async fn pretooluse_exit_zero_plain_text_stdout_is_a_note_not_rewrite() {
        let hooks = vec![hook(HookEvent::PreToolUse, "echo 'just a message'")];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none());
        assert!(o.rewritten_args.is_none());
        assert!(o.notes.iter().any(|n| n.contains("just a message")));
    }

    #[tokio::test]
    async fn prompt_hooks_not_fired_for_tool_events() {
        // A pre_tool_use hook must not fire when run_prompt_hooks is called.
        let hooks = vec![hook(HookEvent::PreToolUse, "exit 1")];
        let result = run_prompt_hooks(&hooks, "hello").await;
        assert_eq!(result.unwrap(), "hello"); // no hook matched → prompt unchanged
    }

    // --- Structured directive protocol (completes the hooks system: rewrite / inject / block) ---

    // Windows cmd.exe preserves the single quotes in `echo '{…}'`, so the JSON directive can't be
    // emitted from a hook this way; the directive parsing is pure Rust, exercised on Linux + macOS.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn inject_action_queues_context_not_a_note() {
        let hooks = vec![hook(
            HookEvent::PreToolUse,
            "echo '{\"action\":\"inject\",\"context\":\"this file is auto-generated\"}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none());
        assert!(o.rewritten_args.is_none());
        assert!(o.notes.is_empty(), "an inject directive is not a user note");
        assert_eq!(o.injected_context, vec!["this file is auto-generated"]);
    }

    #[cfg(not(windows))] // Windows cmd echo keeps the single quotes around JSON (see note above).
    #[tokio::test]
    async fn inject_action_works_on_posttooluse_too() {
        let hooks = vec![hook(
            HookEvent::PostToolUse,
            "echo '{\"action\":\"inject\",\"context\":\"lint: 2 warnings\"}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PostToolUse, "shell", "{}").await;
        assert_eq!(o.injected_context, vec!["lint: 2 warnings"]);
        assert!(o.notes.is_empty());
    }

    #[cfg(not(windows))] // Windows cmd echo keeps the single quotes around JSON (see note above).
    #[tokio::test]
    async fn rewrite_action_replaces_args() {
        let hooks = vec![hook(
            HookEvent::PreToolUse,
            "echo '{\"action\":\"rewrite\",\"args\":{\"path\":\"safe.rs\"}}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        let rewritten = o.rewritten_args.expect("rewrite action sets args");
        assert_eq!(rewritten["path"], "safe.rs");
        assert!(o.injected_context.is_empty());
    }

    #[cfg(not(windows))] // Windows cmd echo keeps the single quotes around JSON (see note above).
    #[tokio::test]
    async fn block_action_blocks_pretooluse_with_reason() {
        let hooks = vec![hook(
            HookEvent::PreToolUse,
            "echo '{\"action\":\"block\",\"reason\":\"writes outside the project are denied\"}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert_eq!(
            o.blocked.as_deref(),
            Some("writes outside the project are denied")
        );
    }

    #[tokio::test]
    async fn block_action_downgrades_to_note_on_posttooluse() {
        // PostToolUse can't unwind a finished call, so a block directive becomes a note.
        let hooks = vec![hook(
            HookEvent::PostToolUse,
            "echo '{\"action\":\"block\",\"reason\":\"too late\"}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PostToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none());
        assert!(o.notes.iter().any(|n| n.contains("too late")));
    }

    #[cfg(not(windows))] // Windows cmd echo keeps the single quotes around JSON (see note above).
    #[tokio::test]
    async fn allow_action_is_a_clean_noop() {
        let hooks = vec![hook(HookEvent::PreToolUse, "echo '{\"action\":\"allow\"}'")];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none());
        assert!(o.rewritten_args.is_none());
        assert!(o.injected_context.is_empty());
        assert!(o.notes.is_empty(), "allow approves without any side effect");
    }

    #[tokio::test]
    async fn unknown_action_falls_back_to_a_note() {
        let hooks = vec![hook(
            HookEvent::PreToolUse,
            "echo '{\"action\":\"frobnicate\"}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        // Not a recognised directive AND has an `action` key → surfaced as a note, NOT rewritten args.
        assert!(o.rewritten_args.is_none());
        assert!(o.notes.iter().any(|n| n.contains("frobnicate")));
    }

    // --- Claude-Code-compatible hook mode (run unmodified CC hook scripts) ---

    #[tokio::test]
    async fn cc_pretooluse_exit_2_blocks_with_stderr_reason() {
        // CC protocol: exit code 2 = block, stderr fed back as the reason. Cross-platform.
        #[cfg(not(windows))]
        let cmd = "echo cc-denied 1>&2; exit 2";
        #[cfg(windows)]
        let cmd = "echo cc-denied 1>&2 & exit /b 2";
        let hooks = vec![cc_hook(HookEvent::PreToolUse, cmd)];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert_eq!(o.blocked.as_deref(), Some("cc-denied"));
    }

    #[tokio::test]
    async fn cc_pretooluse_nonblocking_exit_1_does_not_block() {
        // A non-2 non-zero exit is a non-blocking error under CC semantics (unlike native hooks,
        // where any non-zero PreToolUse exit blocks).
        #[cfg(not(windows))]
        let cmd = "echo oops 1>&2; exit 1";
        #[cfg(windows)]
        let cmd = "echo oops 1>&2 & exit /b 1";
        let hooks = vec![cc_hook(HookEvent::PreToolUse, cmd)];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o.blocked.is_none(), "exit 1 is non-blocking in CC mode");
    }

    // The CC JSON-decision scripts echo a single-quoted JSON object, which Windows cmd.exe mangles;
    // `parse_cc_output` is pure Rust and is exercised on Linux + macOS.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn cc_pretooluse_decision_block_is_honored() {
        let hooks = vec![cc_hook(
            HookEvent::PreToolUse,
            "echo '{\"decision\":\"block\",\"reason\":\"policy: no writes to /etc\"}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PreToolUse, "shell", "{}").await;
        assert_eq!(o.blocked.as_deref(), Some("policy: no writes to /etc"));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn cc_posttooluse_additional_context_is_injected() {
        let hooks = vec![cc_hook(
            HookEvent::PostToolUse,
            "echo '{\"hookSpecificOutput\":{\"additionalContext\":\"ran clippy: 0 warnings\"}}'",
        )];
        let o = run_hooks(&hooks, HookEvent::PostToolUse, "shell", "{}").await;
        assert_eq!(o.injected_context, vec!["ran clippy: 0 warnings"]);
        assert!(o.notes.is_empty(), "additionalContext is not a user note");
    }

    #[tokio::test]
    async fn cc_hook_receives_cc_shaped_payload_on_stdin() {
        // The hook echoes its stdin back; assert the CC fields are present (translated from Forge's
        // native {tool, args} payload). `cat` is available on the project's CI on every OS.
        let hooks = vec![cc_hook(HookEvent::PreToolUse, "cat")];
        let o = run_hooks(
            &hooks,
            HookEvent::PreToolUse,
            "shell",
            "{\"tool\":\"shell\",\"args\":{\"command\":\"ls\"}}",
        )
        .await;
        let joined = o.notes.join(" ");
        assert!(
            joined.contains("\"hook_event_name\":\"PreToolUse\""),
            "{joined}"
        );
        assert!(joined.contains("\"tool_name\":\"shell\""), "{joined}");
        assert!(joined.contains("\"tool_input\""), "{joined}");
    }

    #[tokio::test]
    async fn cc_matcher_uses_cc_tool_alias() {
        // A CC matcher written against CC tool names ("Write|Edit") fires on Forge's edit tool.
        let mut h = cc_hook(HookEvent::PreToolUse, "exit 2");
        h.matcher = Some("Write|Edit".into());
        let o = run_hooks(&[h.clone()], HookEvent::PreToolUse, "edit_file", "{}").await;
        assert!(o.blocked.is_some(), "Edit alias should match edit_file");
        let o2 = run_hooks(&[h], HookEvent::PreToolUse, "shell", "{}").await;
        assert!(o2.blocked.is_none(), "Bash is not in the matcher");
    }

    // --- New lifecycle events (notification / pre_compact / post_compact / stop / subagent_stop) ---

    #[tokio::test]
    async fn each_new_lifecycle_event_fires_its_hook() {
        for event in [
            HookEvent::Notification,
            HookEvent::PreCompact,
            HookEvent::PostCompact,
            HookEvent::Stop,
            HookEvent::SubagentStop,
        ] {
            // `cat` echoes the payload so we can prove the hook actually ran for THIS event.
            let hooks = vec![hook(event, "cat")];
            let o = run_lifecycle_hooks(&hooks, event, "sess-1", serde_json::json!({})).await;
            assert!(
                o.notes.iter().any(|n| n.contains(event.cc_name())),
                "{:?} hook must fire and echo its event; notes: {:?}",
                event,
                o.notes
            );
        }
    }

    #[tokio::test]
    async fn lifecycle_hook_only_fires_for_its_event() {
        // A Stop hook must not fire when a Notification event is dispatched.
        let hooks = vec![hook(HookEvent::Stop, "cat")];
        let o = run_lifecycle_hooks(
            &hooks,
            HookEvent::Notification,
            "sess-1",
            serde_json::json!({ "message": "hi" }),
        )
        .await;
        assert!(
            o.notes.is_empty(),
            "no Stop hook should run for Notification"
        );
    }

    #[tokio::test]
    async fn lifecycle_cc_hook_exit_2_reports_block() {
        #[cfg(not(windows))]
        let cmd = "echo stay 1>&2; exit 2";
        #[cfg(windows)]
        let cmd = "echo stay 1>&2 & exit /b 2";
        let hooks = vec![cc_hook(HookEvent::Stop, cmd)];
        let o = run_lifecycle_hooks(&hooks, HookEvent::Stop, "sess-1", serde_json::json!({})).await;
        assert_eq!(o.blocked.as_deref(), Some("stay"));
    }
}
