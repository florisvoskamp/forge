//! CLI-bridge [`Provider`]: run a turn by spawning the user's locally-installed, already
//! authenticated official agent CLI — `claude` (Claude Code) or `codex` (OpenAI Codex) — and
//! consuming its JSON event stream. This is the ToS-defensible way to use a Claude Pro/Max or
//! ChatGPT subscription: **Forge never reads, stores, or transmits the OAuth token.** The
//! official binary owns its own credentials under its own sanctioned auth; Forge only feeds it
//! a prompt and parses its stdout. See docs/features/provider-integrations.md (Part B) for the
//! honest ToS analysis — this is opt-in and run at the user's discretion.
//!
//! Two modes (RFC cli-bridge-full-harness), selected by `mesh.bridge_mode`:
//! - **harness** (default): claude's built-in tools are disabled (`--tools ""`) and Forge's
//!   own tools are served to it over MCP (`--mcp-config` → `forge mcp-serve`, `--strict-mcp-config`,
//!   `--allowedTools "mcp__forge"`), so every tool/side-effect runs through Forge's registry +
//!   permission gate. The Forge harness on the subscription model.
//! - **text**: claude runs as its own agent with its own tools (`--permission-mode acceptEdits`).
//!
//! Either way Forge parses the rich event stream — thinking/reasoning, tool activity, answer
//! text — surfacing each as a [`StreamEvent`]. codex always runs as its own read-only agent
//! (Forge-tool MCP for codex is Phase 3). Each turn is a fresh invocation. Subscription-billed,
//! so usage costs $0 against Forge's USD budget. Forge never reads/transmits the CLI's auth.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use forge_types::{Message, Role, Usage};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::{EventSink, ModelResponse, Provider, ProviderError, StreamEvent, ToolSpec};

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
    /// Harness mode (RFC cli-bridge-full-harness Phase 2): claude runs Forge's tools via the
    /// `forge mcp-serve` MCP server under Forge's permission gate. When false, the CLI runs as
    /// its own agent with its own tools (Phase 1). Only claude supports harness today.
    harness: bool,
}

impl CliProvider {
    pub fn new(kind: CliKind) -> Self {
        Self {
            kind,
            binary: kind.default_binary().to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            harness: true,
        }
    }

    /// Toggle harness mode (Forge-tool MCP bridge) vs Phase-1 self-agent.
    pub fn with_harness(mut self, harness: bool) -> Self {
        self.harness = harness;
        self
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
/// The `--mcp-config` JSON wiring claude to spawn `<forge_exe> mcp-serve` as a stdio MCP
/// server (Forge's tools under Forge's permission gate). No secrets — just the binary path.
fn forge_mcp_config(forge_exe: &str) -> String {
    format!(
        r#"{{"mcpServers":{{"forge":{{"command":{exe},"args":["mcp-serve"]}}}}}}"#,
        exe = serde_json::Value::String(forge_exe.to_string())
    )
}

fn build_args(
    kind: CliKind,
    bare_model: &str,
    prompt: &str,
    harness: bool,
    forge_exe: &str,
) -> Vec<String> {
    let mut args: Vec<String> = match (kind, harness) {
        // Phase 2 harness: Forge serves its tools via `forge mcp-serve`. `--allowedTools
        // "mcp__forge"` permits ONLY Forge's tools, so claude can't use its built-ins (they'd
        // need a permission it can't get headless) — every side-effect goes through Forge's MCP
        // server + permission gate. NOTE: do NOT use --permission-mode bypassPermissions here —
        // it bypasses the allowlist and re-enables the built-ins. No secrets passed.
        (CliKind::ClaudeCode, true) => vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // `--tools ""` disables claude's BUILT-IN tools (incl. auto-permitted read-only
            // Read/Grep/Glob), leaving only the MCP tools from --mcp-config available.
            "--tools".into(),
            "".into(),
            "--mcp-config".into(),
            forge_mcp_config(forge_exe),
            "--strict-mcp-config".into(),
            "--allowedTools".into(),
            "mcp__forge".into(),
        ],
        // Phase 1: claude as its own full agent (its own tools), acceptEdits so it doesn't
        // block on prompts headless. Forge parses its rich event stream either way.
        (CliKind::ClaudeCode, false) => vec![
            "-p".into(),
            prompt.into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "acceptEdits".into(),
        ],
        // codex harness (Forge-tool MCP) is Phase 3; for now codex runs read-only as its own agent.
        (CliKind::Codex, _) => vec![
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

/// One item extracted from a CLI event line. A single line may yield several (e.g. an
/// assistant message with both thinking and a tool call). Control items (Usage/Final/Error)
/// are handled by `complete`; the rest map to [`StreamEvent`]s.
#[derive(Debug, PartialEq)]
enum Parsed {
    Reasoning(String),
    Text(String),
    ToolStarted {
        id: String,
        name: String,
        args: String,
    },
    ToolFinished {
        id: String,
        ok: bool,
        summary: String,
    },
    Usage(Usage),
    /// Authoritative final answer (Claude's `result.result`); used if nothing streamed.
    Final(String),
    Error(String),
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

/// Parse one Claude Code `--output-format stream-json` (NDJSON) line into zero or more items.
/// Field-tolerant: unknown event types and shapes are ignored, so CLI drift degrades gracefully.
fn parse_claude_line(line: &str) -> Vec<Parsed> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match v.get("type").and_then(Value::as_str) {
        // assistant message: content blocks of thinking / text / tool_use.
        Some("assistant") => {
            if let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("thinking") => {
                            if let Some(t) = b.get("thinking").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    out.push(Parsed::Reasoning(t.to_string()));
                                }
                            }
                        }
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                if !t.is_empty() {
                                    out.push(Parsed::Text(t.to_string()));
                                }
                            }
                        }
                        Some("tool_use") => {
                            let id = b
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = b
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let args = b.get("input").map(|i| i.to_string()).unwrap_or_default();
                            out.push(Parsed::ToolStarted { id, name, args });
                        }
                        _ => {}
                    }
                }
            }
        }
        // user message: tool_result blocks (the outcome of a tool the agent ran).
        Some("user") => {
            if let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(Value::as_array)
            {
                for b in blocks {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        let id = b
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let ok = !b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                        let summary = tool_result_summary(b.get("content"));
                        out.push(Parsed::ToolFinished { id, ok, summary });
                    }
                }
            }
        }
        Some("result") => {
            if let Some(u) = v.get("usage").map(usage_from) {
                out.push(Parsed::Usage(u));
            }
            let result_text = v.get("result").and_then(Value::as_str).map(str::to_string);
            if let Some(f) = &result_text {
                out.push(Parsed::Final(f.clone()));
            }
            if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                out.push(Parsed::Error(
                    v.get("api_error_status")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(result_text)
                        .unwrap_or_else(|| "claude reported an error".into()),
                ));
            }
        }
        _ => {}
    }
    out
}

/// Collapse a tool_result `content` (string, or array of {type:text,text}) into a short summary.
fn tool_result_summary(content: Option<&Value>) -> String {
    let text = match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    };
    let one_line = text.split('\n').next().unwrap_or("").trim();
    one_line.chars().take(120).collect()
}

/// Parse one Codex `exec --json` (JSONL) line. Reasoning-item schema is best-effort.
fn parse_codex_line(line: &str) -> Vec<Parsed> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("item.completed") => {
            let item = v.get("item");
            let kind = item.and_then(|i| i.get("type")).and_then(Value::as_str);
            let text = item
                .and_then(|i| i.get("text"))
                .and_then(Value::as_str)
                .map(str::to_string);
            match (kind, text) {
                (Some("agent_message"), Some(t)) => vec![Parsed::Text(t)],
                (Some("reasoning"), Some(t)) => vec![Parsed::Reasoning(t)],
                _ => Vec::new(),
            }
        }
        Some("turn.completed") => v
            .get("usage")
            .map(usage_from)
            .map(Parsed::Usage)
            .into_iter()
            .collect(),
        Some(t) if t.contains("error") || t.contains("failed") => vec![Parsed::Error(
            v.get("message")
                .and_then(Value::as_str)
                .unwrap_or("codex reported an error")
                .to_string(),
        )],
        _ => Vec::new(),
    }
}

fn parse_line(kind: CliKind, line: &str) -> Vec<Parsed> {
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
        _tools: &[ToolSpec], // Phase 1: the CLI uses its OWN tools (Forge-tool MCP bridge is Phase 2)
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        let prompt = render_prompt(messages);
        // Path to *this* forge binary, so harness mode can spawn `forge mcp-serve`.
        let forge_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "forge".to_string());
        let args = build_args(
            self.kind,
            bare_model(model),
            &prompt,
            self.harness,
            &forge_exe,
        );

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
        // tool_use id → name, so a later tool_result can be labelled.
        let mut tool_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        let read = tokio::time::timeout(self.timeout, async {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                for item in parse_line(self.kind, &line) {
                    match item {
                        Parsed::Reasoning(t) => on_event(StreamEvent::Reasoning(t)),
                        Parsed::Text(t) => {
                            content.push_str(&t);
                            on_event(StreamEvent::Text(t));
                        }
                        Parsed::ToolStarted { id, name, args } => {
                            tool_names.insert(id, name.clone());
                            on_event(StreamEvent::ToolStarted { name, args });
                        }
                        Parsed::ToolFinished { id, ok, summary } => {
                            let name = tool_names.get(&id).cloned().unwrap_or_default();
                            on_event(StreamEvent::ToolFinished { name, ok, summary });
                        }
                        Parsed::Usage(u) => usage = u,
                        Parsed::Final(f) => final_text = Some(f),
                        Parsed::Error(e) => in_band_error = Some(e),
                    }
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
    fn claude_harness_args_route_tools_through_forge_mcp() {
        let args = build_args(
            CliKind::ClaudeCode,
            "sonnet",
            "hi there",
            true,
            "/bin/forge",
        );
        // Forge owns the tools: strict MCP + only mcp__forge tools.
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(args.contains(&"mcp__forge".to_string()));
        // bypassPermissions must NOT be set — it overrides the allowlist and re-enables built-ins.
        assert!(!args.iter().any(|a| a == "bypassPermissions"));
        let mc = args.iter().position(|a| a == "--mcp-config").unwrap();
        assert!(args[mc + 1].contains("mcp-serve") && args[mc + 1].contains("/bin/forge"));
        // Boundary: no credential/auth material in the argv.
        assert!(!args
            .iter()
            .any(|a| a.contains("API_KEY") || a.contains("token")));
    }

    #[test]
    fn claude_text_mode_runs_a_self_agent_with_accept_edits() {
        let args = build_args(
            CliKind::ClaudeCode,
            "sonnet",
            "hi there",
            false,
            "/bin/forge",
        );
        assert!(!args.iter().any(|a| a == "--strict-mcp-config"));
        let i = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[i + 1], "acceptEdits");
        assert!(args.contains(&"--model".to_string()) && args.contains(&"sonnet".to_string()));
    }

    #[test]
    fn codex_args_put_prompt_last_and_are_read_only() {
        let args = build_args(CliKind::Codex, "", "do a thing", true, "/bin/forge");
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

    // --- Parser fixtures: real-shaped lines from claude 2.1.177 / codex-cli 0.130.0 ---

    fn texts(items: &[Parsed]) -> String {
        items
            .iter()
            .filter_map(|p| match p {
                Parsed::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn claude_assistant_text_block_yields_text() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello world"}]}}"#;
        assert_eq!(texts(&parse_claude_line(line)), "hello world");
    }

    #[test]
    fn claude_thinking_block_yields_reasoning() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"let me think"}]}}"#;
        assert_eq!(
            parse_claude_line(line),
            vec![Parsed::Reasoning("let me think".into())]
        );
    }

    #[test]
    fn claude_tool_use_and_result_round_trip() {
        let use_line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"path":"x"}}]}}"#;
        match &parse_claude_line(use_line)[0] {
            Parsed::ToolStarted { id, name, args } => {
                assert_eq!(id, "t1");
                assert_eq!(name, "Read");
                assert!(args.contains("\"path\""));
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        let res_line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"file body"}]}}"#;
        assert_eq!(
            parse_claude_line(res_line),
            vec![Parsed::ToolFinished {
                id: "t1".into(),
                ok: true,
                summary: "file body".into()
            }]
        );
    }

    #[test]
    fn claude_result_line_yields_usage_and_final_text_zero_cost() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","total_cost_usd":0.125,"usage":{"input_tokens":16612,"output_tokens":4}}"#;
        let items = parse_claude_line(line);
        assert!(items.contains(&Parsed::Usage(Usage {
            input_tokens: 16612,
            output_tokens: 4,
            cost_usd: 0.0
        })));
        assert!(items.contains(&Parsed::Final("hello".into())));
    }

    #[test]
    fn claude_system_and_noise_lines_are_ignored() {
        assert!(parse_claude_line(r#"{"type":"system","subtype":"init"}"#).is_empty());
        assert!(parse_claude_line(r#"{"type":"rate_limit_event"}"#).is_empty());
        assert!(parse_claude_line("not json at all").is_empty());
    }

    #[test]
    fn claude_error_result_is_surfaced() {
        let line = r#"{"type":"result","is_error":true,"api_error_status":"overloaded"}"#;
        assert!(parse_claude_line(line).contains(&Parsed::Error("overloaded".into())));
    }

    #[test]
    fn codex_agent_message_yields_text_and_reasoning() {
        let msg = r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hello"}}"#;
        assert_eq!(texts(&parse_codex_line(msg)), "hello");
        let reasoning =
            r#"{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}"#;
        assert_eq!(
            parse_codex_line(reasoning),
            vec![Parsed::Reasoning("thinking".into())]
        );
    }

    #[test]
    fn codex_turn_completed_yields_usage_zero_cost() {
        let line = r#"{"type":"turn.completed","usage":{"input_tokens":16927,"output_tokens":23}}"#;
        assert_eq!(
            parse_codex_line(line),
            vec![Parsed::Usage(Usage {
                input_tokens: 16927,
                output_tokens: 23,
                cost_usd: 0.0
            })]
        );
    }

    #[test]
    fn codex_lifecycle_lines_are_ignored() {
        assert!(parse_codex_line(r#"{"type":"thread.started","thread_id":"x"}"#).is_empty());
        assert!(parse_codex_line(r#"{"type":"turn.started"}"#).is_empty());
    }

    #[tokio::test]
    async fn missing_binary_is_a_clean_error_not_a_hang() {
        let provider = CliProvider::claude_code().with_binary("forge-no-such-cli-xyz");
        let mut on_event = |_: StreamEvent| {};
        let err = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
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
    async fn streams_thinking_text_and_tools_from_a_faked_claude_stream() {
        // A fake `claude` emitting thinking + a tool use/result + text, ignoring its args.
        let fake = make_fake_cli(
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"planning"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"path":"x"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"body"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"the answer"}]}}
{"type":"result","is_error":false,"result":"the answer","usage":{"input_tokens":5,"output_tokens":3}}"#,
        );
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut events: Vec<StreamEvent> = Vec::new();
        let mut on_event = |ev: StreamEvent| events.push(ev);
        let res = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect("fake stream parses");

        assert!(events.contains(&StreamEvent::Reasoning("planning".into())));
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolStarted { name, .. } if name == "Read")));
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::ToolFinished { name, ok, .. } if name == "Read" && *ok)
        ));
        assert!(events.contains(&StreamEvent::Text("the answer".into())));
        assert_eq!(res.content, "the answer", "content is the answer text only");
        assert_eq!(res.usage.input_tokens, 5);
        assert_eq!(res.usage.cost_usd, 0.0);
        assert!(res.tool_calls.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_with_no_output_reports_auth_hint() {
        let fake = make_fake_cli_exit("", 1);
        let provider = CliProvider::codex().with_binary(&fake);
        let mut on_event = |_: StreamEvent| {};
        let err = provider
            .complete(
                "codex-cli::gpt-5",
                &[Message::user("hi")],
                &[],
                &mut on_event,
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
