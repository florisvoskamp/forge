//! CLI-bridge [`Provider`]: run a turn by spawning the user's locally-installed, already
//! authenticated official agent CLI — `claude` (Claude Code) or `codex` (OpenAI Codex) — and
//! consuming its JSON event stream. This is the ToS-defensible way to use a Claude Pro/Max or
//! ChatGPT subscription: **Forge never reads, stores, or transmits the OAuth token.** The
//! official binary owns its own credentials under its own sanctioned auth; Forge only feeds it
//! a prompt and parses its stdout. See docs/features/provider-integrations.md (Part B) for the
//! honest ToS analysis — this is opt-in and run at the user's discretion.
//!
//! v1 runs the CLIs **tool-disabled** (`claude --allowedTools ""`, `codex exec --sandbox
//! read-only`) so they behave as a plain text-completion backend: Forge keeps its own mesh,
//! permission engine, and tool loop around the turn. Consequences (documented non-goals): a
//! CLI-bridge turn returns text only (no Forge-shaped tool calls), and each turn is a fresh
//! invocation (no CLI session reuse). Subscription-billed, so usage costs $0 against Forge's
//! USD budget.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use forge_types::{Message, Role, Usage};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::{ModelResponse, Provider, ProviderError, TextSink, ToolSpec};

/// Which official CLI to bridge to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    /// Anthropic Claude Code (`claude`), Pro/Max subscription.
    ClaudeCode,
    /// OpenAI Codex (`codex`), ChatGPT subscription.
    Codex,
}

impl CliKind {
    /// The Forge model-id prefix that selects this bridge (`claude-cli::…` / `codex-cli::…`).
    pub fn prefix(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude-cli",
            CliKind::Codex => "codex-cli",
        }
    }

    fn default_binary(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude",
            CliKind::Codex => "codex",
        }
    }

    /// How to tell the user to make this CLI usable.
    fn setup_hint(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => {
                "install Claude Code and run `claude` once to log in (Pro/Max subscription)"
            }
            CliKind::Codex => "install Codex and run `codex login` (ChatGPT subscription)",
        }
    }
}

const DEFAULT_TIMEOUT_SECS: u64 = 300;
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Cap on captured stderr (for error messages) so a chatty CLI can't blow memory.
const STDERR_CAP: usize = 16 * 1024;

/// A [`Provider`] that delegates the completion to an external agent CLI.
pub struct CliProvider {
    kind: CliKind,
    binary: String,
    timeout: Duration,
}

impl CliProvider {
    pub fn new(kind: CliKind) -> Self {
        Self {
            kind,
            binary: kind.default_binary().to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }

    pub fn claude_code() -> Self {
        Self::new(CliKind::ClaudeCode)
    }

    pub fn codex() -> Self {
        Self::new(CliKind::Codex)
    }

    /// Override the binary (path or name). Used by tests to point at a fake CLI.
    pub fn with_binary(mut self, binary: impl Into<String>) -> Self {
        self.binary = binary.into();
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// Strip the `claude-cli::`/`codex-cli::` prefix to the bare model name the CLI's `--model`
/// flag expects (empty → let the CLI use its own default model).
fn bare_model(model: &str) -> &str {
    model.split_once("::").map(|(_, m)| m).unwrap_or("")
}

/// Build the argv for one invocation. Pure (no I/O, no env, no secrets) so it can be asserted
/// in tests — the boundary guarantee is that Forge passes only the prompt and non-secret flags,
/// never a credential or auth token (the CLI sources those itself).
fn build_args(kind: CliKind, bare_model: &str, prompt: &str) -> Vec<String> {
    let mut args: Vec<String> = match kind {
        CliKind::ClaudeCode => vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // Tool-disabled: Forge runs its own tool loop; the CLI is a text backend only.
            // `--tools ""` truly disables all tools (an empty `--allowedTools` does NOT —
            // claude still uses Bash, then --max-turns 1 cuts it off as error_max_turns).
            "--tools".into(),
            "".into(),
            "--max-turns".into(),
            "1".into(),
        ],
        CliKind::Codex => vec![
            "exec".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
            "--sandbox".into(),
            "read-only".into(),
        ],
    };
    if !bare_model.is_empty() {
        args.push("--model".into());
        args.push(bare_model.into());
    }
    // Codex takes the prompt as the trailing positional argument.
    if kind == CliKind::Codex {
        args.push(prompt.into());
    }
    args
}

/// Flatten a transcript into a single prompt string for a one-shot CLI invocation. System
/// messages become a preamble; the rest is a role-labelled transcript ending on the latest
/// turn so the CLI responds to it.
fn render_prompt(messages: &[Message]) -> String {
    let mut system = Vec::new();
    let mut convo = Vec::new();
    for m in messages {
        match m.role {
            Role::System => system.push(m.content.clone()),
            Role::User => convo.push(format!("User: {}", m.content)),
            Role::Assistant => {
                if !m.content.is_empty() {
                    convo.push(format!("Assistant: {}", m.content));
                }
            }
            // Tools are disabled for the bridge, but include any prior tool results as context.
            Role::Tool => convo.push(format!("Tool result: {}", m.content)),
        }
    }
    let mut out = String::new();
    if !system.is_empty() {
        out.push_str(&system.join("\n\n"));
        out.push_str("\n\n");
    }
    out.push_str(&convo.join("\n\n"));
    out
}

/// What one parsed JSON event line contributes. A line may carry text to stream, final usage,
/// the authoritative final text, and/or an error — all optional.
#[derive(Debug, Default, PartialEq)]
struct LineOutcome {
    /// Assistant text to append + stream now.
    delta: Option<String>,
    usage: Option<Usage>,
    /// Authoritative final text (Claude's `result.result`); used only if nothing streamed.
    final_text: Option<String>,
    /// A terminal error reported by the CLI in-band.
    error: Option<String>,
}

fn usage_from(v: &Value) -> Usage {
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        // Subscription-billed via the user's own CLI — $0 against Forge's USD budget (FR-5).
        cost_usd: 0.0,
    }
}

/// Parse one Claude Code `--output-format stream-json` (NDJSON) line. Field-tolerant: unknown
/// event types and unexpected shapes are ignored, so CLI version drift degrades gracefully.
fn parse_claude_line(line: &str) -> LineOutcome {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return LineOutcome::default();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            // message.content[] → concat the text blocks.
            let text: String = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<String>()
                })
                .unwrap_or_default();
            LineOutcome {
                delta: (!text.is_empty()).then_some(text),
                ..Default::default()
            }
        }
        Some("result") => {
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let result_text = v.get("result").and_then(Value::as_str).map(str::to_string);
            LineOutcome {
                usage: v.get("usage").map(usage_from),
                final_text: result_text.clone(),
                error: is_error.then(|| {
                    v.get("api_error_status")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(result_text)
                        .unwrap_or_else(|| "claude reported an error".into())
                }),
                ..Default::default()
            }
        }
        _ => LineOutcome::default(),
    }
}

/// Parse one Codex `exec --json` (JSONL) line. Field-tolerant like the Claude parser.
fn parse_codex_line(line: &str) -> LineOutcome {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return LineOutcome::default();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("item.completed") => {
            let item = v.get("item");
            let is_msg =
                item.and_then(|i| i.get("type")).and_then(Value::as_str) == Some("agent_message");
            let text = item
                .and_then(|i| i.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string);
            LineOutcome {
                delta: if is_msg { text } else { None },
                ..Default::default()
            }
        }
        Some("turn.completed") => LineOutcome {
            usage: v.get("usage").map(usage_from),
            ..Default::default()
        },
        Some(t) if t.contains("error") || t.contains("failed") => LineOutcome {
            error: Some(
                v.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex reported an error")
                    .to_string(),
            ),
            ..Default::default()
        },
        _ => LineOutcome::default(),
    }
}

fn parse_line(kind: CliKind, line: &str) -> LineOutcome {
    match kind {
        CliKind::ClaudeCode => parse_claude_line(line),
        CliKind::Codex => parse_codex_line(line),
    }
}

#[async_trait]
impl Provider for CliProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        _tools: &[ToolSpec], // intentionally ignored — v1 bridge is text-only (tool-disabled)
        on_text: &mut TextSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let prompt = render_prompt(messages);
        let args = build_args(self.kind, bare_model(model), &prompt);

        let mut cmd = Command::new(&self.binary);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        put_in_own_process_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Request(format!(
                    "`{}` not found — {}",
                    self.binary,
                    self.kind.setup_hint()
                ))
            } else {
                ProviderError::Request(format!("failed to start `{}`: {e}", self.binary))
            }
        })?;

        let pgid = child.id().map(|id| id as i32);
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let err_task = tokio::spawn(read_to_cap(stderr));

        let mut content = String::new();
        let mut final_text: Option<String> = None;
        let mut usage = Usage::default();
        let mut in_band_error: Option<String> = None;

        let read = tokio::time::timeout(self.timeout, async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                let o = parse_line(self.kind, &line);
                if let Some(d) = o.delta {
                    content.push_str(&d);
                    on_text(&d);
                }
                if let Some(u) = o.usage {
                    usage = u;
                }
                if let Some(f) = o.final_text {
                    final_text = Some(f);
                }
                if let Some(e) = o.error {
                    in_band_error = Some(e);
                }
            }
            Ok::<(), std::io::Error>(())
        })
        .await;

        match read {
            Err(_elapsed) => {
                terminate(&mut child, pgid).await;
                return Err(ProviderError::Request(format!(
                    "`{}` timed out after {}s (killed)",
                    self.binary,
                    self.timeout.as_secs()
                )));
            }
            Ok(Err(e)) => {
                terminate(&mut child, pgid).await;
                return Err(ProviderError::Request(format!(
                    "reading `{}` output failed: {e}",
                    self.binary
                )));
            }
            Ok(Ok(())) => {}
        }

        let status = child.wait().await.ok();
        let stderr_text = err_task.await.unwrap_or_default();

        if let Some(e) = in_band_error {
            return Err(ProviderError::Request(format!(
                "{} error: {e}",
                self.binary
            )));
        }

        let text = if content.is_empty() {
            final_text.unwrap_or_default()
        } else {
            content
        };

        if text.is_empty() {
            let code = status.and_then(|s| s.code());
            if code != Some(0) {
                let tail = stderr_text.trim();
                let detail = if tail.is_empty() {
                    self.kind.setup_hint().to_string()
                } else {
                    tail.to_string()
                };
                return Err(ProviderError::Request(format!(
                    "`{}` exited with {} and no output — {} (is it authenticated? {})",
                    self.binary,
                    code.map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    detail,
                    self.kind.setup_hint(),
                )));
            }
        }

        Ok(ModelResponse {
            content: text,
            tool_calls: Vec::new(),
            usage,
        })
    }
}

async fn read_to_cap<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = r.read(&mut chunk).await {
        if n == 0 || buf.len() >= STDERR_CAP {
            break;
        }
        let take = n.min(STDERR_CAP - buf.len());
        buf.extend_from_slice(&chunk[..take]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_model_strips_cli_prefix() {
        assert_eq!(bare_model("claude-cli::sonnet"), "sonnet");
        assert_eq!(bare_model("codex-cli::gpt-5-codex"), "gpt-5-codex");
        assert_eq!(bare_model("claude-cli"), "");
    }

    #[test]
    fn claude_args_are_tool_disabled_and_carry_no_secret() {
        let args = build_args(CliKind::ClaudeCode, "sonnet", "hi there");
        assert!(args.contains(&"--tools".to_string()));
        // tool list is the empty string right after the flag → all tools disabled
        let i = args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(args[i + 1], "");
        assert!(args.contains(&"stream-json".to_string()));
        assert_eq!(args.iter().filter(|a| *a == "hi there").count(), 1);
        assert!(args.contains(&"--model".to_string()) && args.contains(&"sonnet".to_string()));
        // Boundary: no credential/auth material in the argv.
        assert!(!args.iter().any(|a| a.contains("API_KEY")
            || a.contains("token")
            || a.contains(".credentials")
            || a.contains("auth")));
    }

    #[test]
    fn codex_args_put_prompt_last_and_are_read_only() {
        let args = build_args(CliKind::Codex, "", "do a thing");
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"read-only".to_string()));
        assert_eq!(args.last().unwrap(), "do a thing");
        // no --model when bare model is empty
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn render_prompt_puts_system_first_then_transcript() {
        let msgs = vec![
            Message::system("be terse"),
            Message::user("hello"),
            Message::assistant("hi"),
            Message::user("explain x"),
        ];
        let p = render_prompt(&msgs);
        assert!(p.starts_with("be terse"));
        assert!(p.contains("User: hello"));
        assert!(p.contains("Assistant: hi"));
        assert!(p.ends_with("User: explain x"));
    }

    // --- Parser fixtures: real lines captured from claude 2.1.177 / codex-cli 0.130.0 ---

    #[test]
    fn claude_assistant_line_yields_streamed_text() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello world"}],"usage":{"input_tokens":10,"output_tokens":2}}}"#;
        let o = parse_claude_line(line);
        assert_eq!(o.delta.as_deref(), Some("hello world"));
        assert!(o.error.is_none());
    }

    #[test]
    fn claude_result_line_yields_usage_and_final_text_zero_cost() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","total_cost_usd":0.125,"usage":{"input_tokens":16612,"output_tokens":4}}"#;
        let o = parse_claude_line(line);
        let u = o.usage.expect("usage");
        assert_eq!(u.input_tokens, 16612);
        assert_eq!(u.output_tokens, 4);
        assert_eq!(u.cost_usd, 0.0, "subscription-billed: $0 to Forge");
        assert_eq!(o.final_text.as_deref(), Some("hello"));
        assert!(o.error.is_none());
    }

    #[test]
    fn claude_system_and_noise_lines_are_ignored() {
        assert_eq!(
            parse_claude_line(r#"{"type":"system","subtype":"init"}"#),
            LineOutcome::default()
        );
        assert_eq!(
            parse_claude_line(r#"{"type":"rate_limit_event"}"#),
            LineOutcome::default()
        );
        assert_eq!(parse_claude_line("not json at all"), LineOutcome::default());
    }

    #[test]
    fn claude_error_result_is_surfaced() {
        let line = r#"{"type":"result","is_error":true,"api_error_status":"overloaded"}"#;
        let o = parse_claude_line(line);
        assert_eq!(o.error.as_deref(), Some("overloaded"));
    }

    #[test]
    fn codex_agent_message_yields_text() {
        let line = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}"#;
        let o = parse_codex_line(line);
        assert_eq!(o.delta.as_deref(), Some("hello"));
    }

    #[test]
    fn codex_turn_completed_yields_usage_zero_cost() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":16927,"cached_input_tokens":3456,"output_tokens":23}}"#;
        let o = parse_codex_line(line);
        let u = o.usage.expect("usage");
        assert_eq!(u.input_tokens, 16927);
        assert_eq!(u.output_tokens, 23);
        assert_eq!(u.cost_usd, 0.0);
    }

    #[test]
    fn codex_non_message_items_and_lifecycle_are_ignored() {
        assert_eq!(
            parse_codex_line(r#"{"type":"thread.started","thread_id":"x"}"#),
            LineOutcome::default()
        );
        assert_eq!(
            parse_codex_line(r#"{"type":"turn.started"}"#),
            LineOutcome::default()
        );
        // reasoning items carry text but aren't the assistant message → ignored
        assert_eq!(
            parse_codex_line(
                r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#
            ),
            LineOutcome::default()
        );
    }

    #[tokio::test]
    async fn missing_binary_is_a_clean_error_not_a_hang() {
        let provider = CliProvider::claude_code().with_binary("forge-no-such-cli-xyz");
        let mut sink = String::new();
        let mut on_text = |s: &str| sink.push_str(s);
        let err = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_text,
            )
            .await
            .expect_err("missing binary must error");
        let ProviderError::Request(msg) = err;
        assert!(msg.contains("not found"), "got: {msg}");
        assert!(
            msg.contains("log in"),
            "should hint how to authenticate: {msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn streams_and_parses_a_faked_claude_stream() {
        // A fake `claude` that emits two real-shaped NDJSON lines, ignoring its args.
        let fake = make_fake_cli(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"streamed "}]}}
{"type":"result","is_error":false,"result":"streamed answer","usage":{"input_tokens":5,"output_tokens":3}}"#,
        );
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut sink = String::new();
        let mut on_text = |s: &str| sink.push_str(s);
        let res = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_text,
            )
            .await
            .expect("fake stream parses");
        assert_eq!(sink, "streamed ", "assistant text streamed to the sink");
        assert_eq!(res.content, "streamed ");
        assert_eq!(res.usage.input_tokens, 5);
        assert_eq!(res.usage.cost_usd, 0.0);
        assert!(res.tool_calls.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_with_no_output_reports_auth_hint() {
        let fake = make_fake_cli_exit("", 1);
        let provider = CliProvider::codex().with_binary(&fake);
        let mut sink = String::new();
        let mut on_text = |s: &str| sink.push_str(s);
        let err = provider
            .complete(
                "codex-cli::gpt-5",
                &[Message::user("hi")],
                &[],
                &mut on_text,
            )
            .await
            .expect_err("nonzero exit, no output → error");
        let ProviderError::Request(msg) = err;
        assert!(msg.contains("authenticated"), "got: {msg}");
    }

    // Write an executable shell script that prints `stdout` then exits 0.
    #[cfg(unix)]
    fn make_fake_cli(stdout: &str) -> String {
        make_fake_cli_exit(stdout, 0)
    }

    #[cfg(unix)]
    fn make_fake_cli_exit(stdout: &str, code: i32) -> String {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        // Unique path per call without external deps.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("forge-fake-cli-{}-{n}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        // Use a heredoc-free script: printf the payload (escaped) then exit.
        writeln!(f, "#!/bin/sh").unwrap();
        write!(f, "cat <<'FORGE_EOF'\n{stdout}\nFORGE_EOF\n").unwrap();
        writeln!(f, "exit {code}").unwrap();
        f.sync_all().unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        // Wait out ETXTBSY: a *concurrent* test's fork can briefly inherit this file's open
        // write-fd, so exec'ing it would transiently fail with "Text file busy". Probe-exec
        // (retrying past ETXTBSY) until the OS lets us run it — once it does, no writer holds
        // the file, so the provider's real spawn won't flake.
        for _ in 0..200 {
            match std::process::Command::new(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut child) => {
                    let _ = child.wait();
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        path.to_string_lossy().into_owned()
    }
}
