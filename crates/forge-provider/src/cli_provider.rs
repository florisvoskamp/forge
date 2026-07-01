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
//! codex harness (RFC Phase 3) works the same way via codex's own MCP wiring: the Forge MCP
//! server is registered with `-c mcp_servers.forge.*` and its tools auto-approved
//! (`default_tools_approval_mode="approve"`). codex keeps a `read-only` sandbox, so its own
//! shell can only do read-only recon — every write/side-effect can only go through Forge's
//! gated mcp__forge tools.
//!
//! Either way Forge parses the rich event stream — thinking/reasoning, tool activity, answer
//! text — surfacing each as a [`StreamEvent`]. Each turn is a fresh invocation. Subscription-
//! billed, so usage costs $0 against Forge's USD budget. Forge never reads/transmits the CLI's
//! auth.

use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use forge_types::{Message, Role, Usage};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::{
    CheckpointContext, CompletionOptions, EventSink, ModelResponse, Provider, ProviderError,
    StreamEvent, ToolSpec,
};

/// Which official CLI to bridge to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliKind {
    /// Anthropic Claude Code (`claude`), Pro/Max subscription.
    ClaudeCode,
    /// OpenAI Codex (`codex`), ChatGPT subscription.
    Codex,
    /// Google Antigravity (`agy`) — free Gemini access (plus proxied Claude/GPT). Text-mode only
    /// (no MCP/`--tools` wiring), so it always runs as its own agent.
    Antigravity,
}

impl CliKind {
    /// The Forge model-id prefix that selects this bridge (`claude-cli::…` / `codex-cli::…`).
    pub fn prefix(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude-cli",
            CliKind::Codex => "codex-cli",
            CliKind::Antigravity => "agy-cli",
        }
    }

    fn default_binary(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => "claude",
            CliKind::Codex => "codex",
            CliKind::Antigravity => "agy",
        }
    }

    /// All bridge kinds.
    pub fn all() -> [CliKind; 3] {
        [CliKind::ClaudeCode, CliKind::Codex, CliKind::Antigravity]
    }

    /// Whether this bridge's CLI is installed (its binary resolves on `PATH`). A subscription
    /// bridge that's present is treated as always-available — it doesn't rate-limit like the free
    /// API tiers — so the mesh can fall back to it when metered providers are throttled.
    pub fn available(self) -> bool {
        resolve_on_path(self.default_binary()).is_some()
    }

    /// The bare Forge model id for this bridge (`claude-cli::` / `codex-cli::`), which resolves to
    /// the CLI's own default model.
    pub fn default_model_id(self) -> String {
        format!("{}::", self.prefix())
    }

    /// The model aliases this bridge exposes by default, so auto-discovery surfaces more than just
    /// the CLI's single default — letting the mesh size each turn (haiku/mini for trivial, opus for
    /// complex). The official CLIs publish no machine-readable model list, so these are the
    /// documented `--model` aliases; users can override the set via `[mesh.bridge_models]`, and any
    /// alias that's stale or unavailable just benches itself via failover (never a hard error).
    pub fn default_models(self) -> &'static [&'static str] {
        match self {
            // `claude --model` accepts these aliases (claude 2.x); they span the capability tiers.
            CliKind::ClaudeCode => &["opus", "sonnet", "haiku"],
            // `codex --model` (codex 0.13x model picker). gpt-5.4-mini is the fast/cheap tier.
            CliKind::Codex => &[
                "gpt-5.5",
                "gpt-5.3-codex",
                "gpt-5.2",
                "gpt-5.4",
                "gpt-5.4-mini",
            ],
            // `agy --model` (Antigravity 1.0.8 `agy models`). Free Gemini tiers: flash = fast/cheap
            // (trivial/standard), pro = the capable tier (complex). Verified accepted live.
            CliKind::Antigravity => &["gemini-3.5-flash", "gemini-3.1-pro"],
        }
    }

    /// How to tell the user to make this CLI usable.
    fn setup_hint(self) -> &'static str {
        match self {
            CliKind::ClaudeCode => {
                "install Claude Code and run `claude` once to log in (Pro/Max subscription)"
            }
            CliKind::Codex => "install Codex and run `codex login` (ChatGPT subscription)",
            CliKind::Antigravity => {
                "install Antigravity and run `agy` once to log in (free Gemini access)"
            }
        }
    }

    /// The CLI's hard limit on prompt length, in characters. `codex exec` rejects stdin over
    /// 1,048,576 chars outright (`input_too_large`), so a long transcript must be clamped before
    /// it's piped in (see [`clamp_to_chars`]) or the turn fails instead of failing over. Claude's
    /// print mode has no fixed character cap (it's bounded by the model's token context, which
    /// surfaces as a normal retryable error), so it's left unclamped.
    fn max_input_chars(self) -> Option<usize> {
        match self {
            CliKind::Codex => Some(1_048_576),
            CliKind::ClaudeCode | CliKind::Antigravity => None,
        }
    }
}

/// Resolve `bin` to an executable file on `PATH` (a lightweight `which`, no spawning). Windows-aware:
/// there it also tries the `.exe`/`.cmd`/`.bat` suffixes, because the official CLIs are installed by
/// npm as `.cmd` shims — a bare-name lookup (`claude`) misses `claude.cmd` and reports the bridge as
/// absent. A `bin` that already contains a path separator is checked directly (still trying the
/// Windows suffixes) instead of via `PATH`. Returns the matched path, or `None`.
fn resolve_on_path(bin: &str) -> Option<std::path::PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let try_base = |base: &std::path::Path| -> Option<std::path::PathBuf> {
        exts.iter().find_map(|e| {
            let cand = if e.is_empty() {
                base.to_path_buf()
            } else {
                let mut s = base.as_os_str().to_owned();
                s.push(e);
                std::path::PathBuf::from(s)
            };
            cand.is_file().then_some(cand)
        })
    };
    let p = std::path::Path::new(bin);
    if p.components().count() > 1 {
        return try_base(p);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| try_base(&dir.join(bin)))
}

/// Build the [`Command`] to launch a bridge CLI named/located at `binary` with `args`. On Windows the
/// CLIs are commonly `.cmd` shims (how npm installs them), which `CreateProcess` cannot launch
/// directly — those run through `cmd /C`. A real `.exe`, or any Unix binary, launches directly.
///
/// `args` is taken here (not appended by the caller) because the Windows path must build the whole
/// `cmd` command line at once: `cmd` strips the first/last quote of its `/C` string, so a quoted shim
/// path breaks the moment a second quoted token (an argument containing a space, e.g. an
/// `--mcp-config` path under `C:\Users\First Last\…`) appears. We pass `/S` + an outer-quoted command
/// with every token individually quoted, so spaces in the path AND in any argument survive — the
/// difference between the bridge launching and failing on a Windows profile whose path has a space.
fn bridge_command(binary: &str, args: &[String]) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if let Some(p) = resolve_on_path(binary) {
            let is_script = p
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
            if is_script {
                // `raw_arg` is a std-only extension; reach the inner std Command (this is a
                // tokio::process::Command). `/S` makes cmd strip just the outer quote pair below.
                let mut cmd = Command::new("cmd");
                cmd.as_std_mut().raw_arg("/S");
                cmd.as_std_mut().raw_arg("/C");
                cmd.as_std_mut().raw_arg(windows_cmd_line(&p, args));
                return cmd;
            }
        }
    }
    let mut cmd = Command::new(binary);
    cmd.args(args);
    cmd
}

/// The raw command line for `cmd /S /C` launching `program` (a resolved `.cmd`/`.bat`) with `args`:
/// every token double-quoted (embedded quotes doubled, per `cmd`), and the whole wrapped in an outer
/// pair that `/S` strips. Pure + cross-platform so it can be unit-tested off Windows.
#[cfg(any(windows, test))]
fn windows_cmd_line(program: &std::path::Path, args: &[String]) -> String {
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let mut inner = q(&program.to_string_lossy());
    for a in args {
        inner.push(' ');
        inner.push_str(&q(a));
    }
    format!("\"{inner}\"")
}

/// Forge model ids for every CLI bridge whose CLI is installed — the always-available
/// subscription models the mesh should fall back to (and prefer, being $0). Empty if none.
///
/// Each installed bridge contributes its bare default id (`claude-cli::`, the CLI's own configured
/// default) plus one id per [`CliKind::default_models`] alias (`claude-cli::opus`, …) so the mesh
/// has a model per tier rather than a single mid one. Callers that want a custom set per bridge
/// should build ids from [`CliKind::default_models`] / a config override instead.
pub fn available_bridge_models() -> Vec<String> {
    let mut out = Vec::new();
    for k in CliKind::all().into_iter().filter(|k| k.available()) {
        out.push(k.default_model_id());
        out.extend(
            k.default_models()
                .iter()
                .map(|m| format!("{}::{m}", k.prefix())),
        );
    }
    out
}

/// IDLE window (seconds) for a bridged CLI: kill only after this long with NO output. Not a total
/// cap — a turn streaming events stays alive indefinitely, so long hard tasks aren't truncated.
const DEFAULT_TIMEOUT_SECS: u64 = 300;
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(2);
/// Cap on captured stderr (for error messages) so a chatty CLI can't blow memory.
const STDERR_CAP: usize = 16 * 1024;

/// Cross-call session-continuity state for the bridge (claude `--resume`). After the first turn we
/// hold the CLI's own `session_id` and how many transcript messages it has already seen; the next
/// turn RESUMES that session and sends ONLY the new messages, so claude reloads its context from its
/// own store instead of Forge re-rendering + re-sending the whole transcript every re-drive. That is
/// the headline bridge-efficiency win (fewer tokens in *and* a prompt-cache hit on claude's side).
#[derive(Default)]
struct ResumeState {
    /// The CLI session id captured from a prior turn's stream (`None` → next turn is fresh).
    session_id: Option<String>,
    /// Count of transcript messages already handed to that session (the resume "high-water mark").
    sent: usize,
    /// The bare model the live session was started under. Resuming it under a DIFFERENT model (after
    /// a mesh re-route / failover) would be wrong, so a model change forces a fresh session.
    model: String,
    /// The identifying id ([`CheckpointContext::session`]) of the conversation that recorded this
    /// slot. This single `ResumeState` slot is shared by every caller of this `CliProvider`
    /// instance — the main session AND any subagents spawned within it — so without this key a
    /// resume decided for one conversation could `--resume` a DIFFERENT conversation's live
    /// claude/codex session, cross-wiring their turns. `None` when the call carried no checkpoint
    /// context (legacy inherited-env fallback).
    owner: Option<String>,
}

/// A [`Provider`] that delegates the completion to an external agent CLI.
pub struct CliProvider {
    kind: CliKind,
    binary: String,
    timeout: Duration,
    /// Harness mode (RFC cli-bridge-full-harness): the CLI runs Forge's tools via the
    /// `forge mcp-serve` MCP server under Forge's permission gate. When false, the CLI runs as
    /// its own agent with its own tools. Both claude (Phase 2) and codex (Phase 3) support it.
    harness: bool,
    /// Whether to reuse the CLI's session across calls via `--resume` (claude only). On by default;
    /// `with_session_resume(false)` forces the legacy full-transcript path (escape hatch / tests).
    resume_enabled: bool,
    /// Live `--resume` state (see [`ResumeState`]). Interior-mutable because [`Provider::complete`]
    /// takes `&self`; the lock is only ever held briefly to read/update, never across an `.await`.
    resume: std::sync::Mutex<ResumeState>,
    /// P1 persistent transport (claude only): keep ONE long-lived `--input-format stream-json`
    /// process alive across turns and write each turn's delta to its stdin, instead of re-spawning
    /// (and re-`--resume`-ing) the CLI every turn/re-drive. Removes the per-turn process-spawn +
    /// session-reload cost. Default on for claude; `FORGE_PERSISTENT_BRIDGE=0` or
    /// [`with_persistent(false)`](Self::with_persistent) opts out. Falls back to the one-shot path
    /// whenever the live session can't be (re)established BEFORE any turn output.
    ///
    /// **Why claude-only** (probed 2026-06-27, codex 0.141 / agy 1.0.12; see
    /// docs/features/persistent-bridge-transport.md):
    /// - **agy** has no streaming-input mode at all — only `--print` (one prompt, then exits).
    /// - **codex** `exec` reads stdin once then exits; its `exec-server --listen stdio` speaks
    ///   JSON-RPC but is an unimplemented STUB — every turn method (`thread/new`, …) returns
    ///   `-32601 "exec-server stub does not implement … yet"`. Blocked upstream, not on us.
    persistent: bool,
    /// The live persistent session, if one is running (claude only). A `tokio` mutex because the
    /// guard is held across the turn's stdout read (an `.await`); one turn at a time per provider,
    /// which matches the single underlying process.
    live: tokio::sync::Mutex<Option<LiveSession>>,
}

impl CliProvider {
    pub fn new(kind: CliKind) -> Self {
        Self {
            kind,
            binary: kind.default_binary().to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            harness: true,
            // Resume is a claude-only capability; the flag is harmless for the other kinds (they
            // never consult it). Default on so the efficiency win applies without opt-in.
            resume_enabled: true,
            resume: std::sync::Mutex::new(ResumeState::default()),
            // Persistent transport is claude-only (the one CLI with `--input-format stream-json`).
            // On by default for claude; the env var is the operator escape hatch.
            persistent: kind == CliKind::ClaudeCode
                && std::env::var("FORGE_PERSISTENT_BRIDGE").as_deref() != Ok("0"),
            live: tokio::sync::Mutex::new(None),
        }
    }

    /// Toggle the P1 persistent transport (claude long-lived `--input-format stream-json`).
    pub fn with_persistent(mut self, enabled: bool) -> Self {
        self.persistent = enabled && self.kind == CliKind::ClaudeCode;
        self
    }

    /// Toggle harness mode (Forge-tool MCP bridge) vs Phase-1 self-agent.
    pub fn with_harness(mut self, harness: bool) -> Self {
        self.harness = harness;
        self
    }

    /// Toggle CLI session reuse via `--resume` (claude only). Off → always send the full transcript.
    pub fn with_session_resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled;
        self
    }

    /// Whether this bridge reuses the CLI session across calls. claude (`--resume <id>`) and codex
    /// (`exec resume <id>`) both support it; agy does not. Gated by the `with_session_resume` flag.
    fn resumes(&self) -> bool {
        self.resume_enabled && matches!(self.kind, CliKind::ClaudeCode | CliKind::Codex)
    }

    /// Record a live session after a successful turn so the next call can `--resume` it. `owner` is
    /// the calling conversation's [`CheckpointContext::session`] id, so a later call from a
    /// DIFFERENT conversation sharing this provider never reuses this slot (see [`ResumeState::owner`]).
    fn record_session(&self, id: Option<String>, sent: usize, model: &str, owner: Option<&str>) {
        if let Ok(mut st) = self.resume.lock() {
            // Always overwrite — `None` CLEARS a stale id. Keeping the old id on a turn that produced
            // no session handle (e.g. a fresh-transcript turn where the CLI emitted no thread id) made
            // the NEXT turn `--resume` the PRIOR session and skip this turn's context entirely.
            st.session_id = id;
            st.sent = sent;
            st.model = model.to_string();
            st.owner = owner.map(str::to_string);
        }
    }

    /// Forget the session (after a resumed turn failed, or the transcript shrank) so the next call
    /// starts fresh with the full transcript.
    fn reset_session(&self) {
        if let Ok(mut st) = self.resume.lock() {
            *st = ResumeState::default();
        }
    }

    pub fn claude_code() -> Self {
        Self::new(CliKind::ClaudeCode)
    }

    pub fn codex() -> Self {
        Self::new(CliKind::Codex)
    }

    pub fn antigravity() -> Self {
        Self::new(CliKind::Antigravity)
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
/// server (Forge's tools under Forge's permission gate). No secrets — just the binary path and
/// the per-turn out-of-band env (sink + checkpoint context) the served tools need to report
/// tasks/plans and snapshot edits back to the parent. A host that hands its MCP servers a curated
/// env (codex strips everything but PATH/HOME/…) would otherwise leave `mcp-serve` blind, so the
/// values are passed explicitly rather than relied upon to inherit.
fn forge_mcp_config(forge_exe: &str, mcp_env: &[(String, String)]) -> String {
    let env_obj = serde_json::Value::Object(
        mcp_env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    );
    format!(
        r#"{{"mcpServers":{{"forge":{{"command":{exe},"args":["mcp-serve"],"env":{env}}}}}}}"#,
        exe = serde_json::Value::String(forge_exe.to_string()),
        env = env_obj,
    )
}

/// The out-of-band env handed to a bridge turn's `forge mcp-serve` child: the subagent sink plus
/// the per-turn checkpoint context. The checkpoint values come from the EXPLICIT [`CheckpointContext`]
/// the parent threaded through (so the parent never mutates its process-global env); when absent
/// (the base `complete` path) they fall back to this process's inherited env for legacy callers.
/// `FORGE_SUBAGENT_DEPTH` is an independent recursion bound that is always legitimately inherited.
/// Key names mirror `forge_core::snapshot::ENV_*` (stable cross-process contract strings;
/// forge-provider can't depend on that crate).
fn bridge_mcp_env(
    sink_path: Option<&std::path::Path>,
    checkpoint: Option<&CheckpointContext>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(p) = sink_path {
        if let Some(s) = p.to_str() {
            env.push((SUBAGENT_SINK_ENV.to_string(), s.to_string()));
        }
    }
    match checkpoint {
        Some(c) => {
            env.push(("FORGE_CHECKPOINT_SESSION".to_string(), c.session.clone()));
            env.push(("FORGE_CHECKPOINT_SEQ".to_string(), c.seq.to_string()));
            env.push(("FORGE_CHECKPOINT_ROOT".to_string(), c.root.clone()));
            // The parent's live temper, so the bridge's permission gate matches the UI mode
            // (Plan→Auto-edit switches reach mcp-serve instead of it using the stale config).
            env.push(("FORGE_PERMISSION_MODE".to_string(), c.mode.clone()));
        }
        None => {
            for key in [
                "FORGE_CHECKPOINT_SESSION",
                "FORGE_CHECKPOINT_SEQ",
                "FORGE_CHECKPOINT_ROOT",
                "FORGE_PERMISSION_MODE",
            ] {
                if let Ok(val) = std::env::var(key) {
                    env.push((key.to_string(), val));
                }
            }
        }
    }
    if let Ok(val) = std::env::var("FORGE_SUBAGENT_DEPTH") {
        env.push(("FORGE_SUBAGENT_DEPTH".to_string(), val));
    }
    env
}

/// Build the CLI argv for a bridge turn. The prompt is NOT included here — it is streamed to the
/// child's stdin by [`CliProvider::complete`] instead of passed as an argument, because a turn's
/// flattened transcript (system preamble + injected Lattice context + history) easily exceeds the
/// OS `ARG_MAX`, which surfaced as `failed to start codex: Argument list too long (os error 7)`.
/// Both `claude --print` and `codex exec` read their instructions from stdin when no prompt
/// positional is given.
fn build_args(
    kind: CliKind,
    bare_model: &str,
    harness: bool,
    forge_exe: &str,
    mcp_env: &[(String, String)],
    resume_id: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = match (kind, harness) {
        // Phase 2 harness: Forge serves its tools via `forge mcp-serve`. `--allowedTools
        // "mcp__forge"` permits ONLY Forge's tools, so claude can't use its built-ins (they'd
        // need a permission it can't get headless) — every side-effect goes through Forge's MCP
        // server + permission gate. NOTE: do NOT use --permission-mode bypassPermissions here —
        // it bypasses the allowlist and re-enables the built-ins. No secrets passed.
        (CliKind::ClaudeCode, true) => vec![
            // `-p`/--print with no positional → claude reads the prompt from stdin (see build_args).
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            // `--tools ""` disables claude's BUILT-IN tools (incl. auto-permitted read-only
            // Read/Grep/Glob), leaving only the MCP tools from --mcp-config available.
            "--tools".into(),
            "".into(),
            "--mcp-config".into(),
            forge_mcp_config(forge_exe, mcp_env),
            "--strict-mcp-config".into(),
            "--allowedTools".into(),
            "mcp__forge".into(),
        ],
        // Phase 1: claude as its own full agent (its own tools), acceptEdits so it doesn't
        // block on prompts headless. Forge parses its rich event stream either way.
        (CliKind::ClaudeCode, false) => vec![
            // `-p`/--print with no positional → claude reads the prompt from stdin (see build_args).
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "acceptEdits".into(),
        ],
        // Codex harness (RFC Phase 3): serve Forge's tools to codex over MCP, gated by Forge's
        // permission engine. The sandbox stays `read-only`, so codex's OWN shell can only do
        // read-only recon — every write/side-effect can only go through Forge's mcp__forge
        // tools (which run in `forge mcp-serve` under `permission::decide`). We do NOT use
        // `--dangerously-bypass-approvals-and-sandbox` (it would unsandbox codex's shell).
        //
        // Two non-obvious flags, verified live against codex 0.130.0:
        // - `mcp_servers.forge.default_tools_approval_mode="approve"`: codex exec auto-cancels
        //   MCP tool calls non-interactively (openai/codex#16685); "approve" is the only value
        //   that lets them through ("auto" still cancels).
        // - `model_reasoning_summary="detailed"`: makes codex emit `reasoning` items so Forge
        //   can stream the model's thinking (otherwise only a token count is reported).
        (CliKind::Codex, true) => vec![
            "exec".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
            // Isolate codex's tool surface to ONLY Forge's MCP server: don't load the user's
            // ~/.codex/config.toml, which can register extra MCP servers (e.g. a personal
            // brave-search) that would let codex search/act OUTSIDE Forge's gate. Without this,
            // codex used the user's own search MCP instead of Forge's web_search. Auth still
            // comes from CODEX_HOME (per the flag's contract); we re-supply everything Forge
            // needs via the -c overrides below. (claude's path is already isolated via
            // --strict-mcp-config + --tools "".) `--search` is intentionally never passed, so
            // codex's native Responses web_search stays off too.
            "--ignore-user-config".into(),
            "--sandbox".into(),
            "read-only".into(),
            "-c".into(),
            "approval_policy=\"never\"".into(),
            "-c".into(),
            "model_reasoning_summary=\"detailed\"".into(),
            "-c".into(),
            format!(
                "mcp_servers.forge.command={}",
                serde_json::Value::String(forge_exe.to_string())
            ),
            "-c".into(),
            "mcp_servers.forge.args=[\"mcp-serve\"]".into(),
            "-c".into(),
            "mcp_servers.forge.default_tools_approval_mode=\"approve\"".into(),
        ],
        // Phase-1 text mode: codex as its own read-only agent, no Forge tools.
        (CliKind::Codex, false) => vec![
            "exec".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
            "--sandbox".into(),
            "read-only".into(),
        ],
        // Antigravity (`agy`) has no MCP/`--tools` wiring, so it ALWAYS runs as its own agent
        // (text mode), regardless of `harness`. `-p`/--print with no positional → agy reads the
        // prompt from stdin (verified live, like claude). `--dangerously-skip-permissions`
        // auto-approves agy's own tools so it doesn't block headless. `--model` is appended below.
        (CliKind::Antigravity, _) => {
            vec!["-p".into(), "--dangerously-skip-permissions".into()]
        }
    };
    // codex hands its stdio MCP servers a CURATED env (only PATH/HOME/LANG/… survive — verified
    // live: a custom `FORGE_*` set on the codex process never reaches `forge mcp-serve`). So the
    // sink + checkpoint context the served tools need is injected explicitly as TOML overrides;
    // without it `update_tasks`/`present_plan` write to a dead sink and never reach the parent TUI,
    // and bridge edits aren't snapshotted for `/undo`. (claude carries the same env in its
    // `--mcp-config` JSON above.)
    if matches!((kind, harness), (CliKind::Codex, true)) {
        for (k, v) in mcp_env {
            args.push("-c".into());
            args.push(format!(
                "mcp_servers.forge.env.{k}={}",
                serde_json::Value::String(v.clone())
            ));
        }
    }
    if !bare_model.is_empty() {
        args.push("--model".into());
        args.push(bare_model.into());
    }
    // Resume a prior claude session (continuity + prompt-cache; claude-only). Appended after the
    // standard flags; only the new turn's messages are streamed to stdin (see `complete`).
    if let (CliKind::ClaudeCode, Some(id)) = (kind, resume_id) {
        args.push("--resume".into());
        args.push(id.into());
    }
    // Resume a prior codex session. Unlike claude's `--resume` flag, codex resumes via the
    // `exec resume <id>` SUBCOMMAND — and it REJECTS `--sandbox` on resume (the recorded session's
    // sandbox is inherited; verified live). So rewrite `exec …` → `exec resume <id> …` and drop the
    // `--sandbox read-only` pair. Everything else (`--json`/`--skip-git-repo-check`/
    // `--ignore-user-config`/`-c …`/`--model`) is accepted on resume and kept.
    if let (CliKind::Codex, Some(id)) = (kind, resume_id) {
        if let Some(p) = args.iter().position(|a| a == "--sandbox") {
            args.drain(p..(p + 2).min(args.len())); // remove the flag + its "read-only" value
        }
        // `exec` is always args[0] for codex; turn it into `exec resume <id>`.
        args.insert(1, "resume".into());
        args.insert(2, id.into());
    }
    // The prompt is fed via stdin (see build_args doc), so no trailing positional: `codex exec`
    // with no PROMPT reads instructions from stdin.
    args
}

/// Prepended to harness-mode prompts so the bridged CLI prefers Forge's gated MCP tools over
/// any built-in/native web search or browsing (which Forge can't see/cost-track/route).
const HARNESS_TOOL_PREAMBLE: &str = "[Forge harness] You are running inside the Forge coding \
agent. For ANY web access — searching the web or opening a URL — you MUST use the \
`mcp__forge__web_search` and `mcp__forge__web_fetch` tools exposed over MCP. Do NOT use any \
built-in, native, or provider web-search/browsing capability. Likewise route all file and \
shell actions through the `mcp__forge__*` tools.\n\n\
Running commands: your native built-in tools (Bash, Shell, Read, Edit, Write, …) are DISABLED \
here — the ONLY callable tools are the `mcp__forge__*` ones. To run ANY shell command — git, gh, \
cargo, ls, anything — call `mcp__forge__shell` with the command. Do NOT emit a `Bash`/`Shell` \
tool call, and never write a tool-call block out as plain text and then imagine its output: that \
output is fiction and will mislead you. `mcp__forge__shell` runs a clean non-interactive `sh -c` \
(no login banner, no prompt) and returns the real exit code + combined output. If a tool result \
ever looks garbled, truncated, or empty, the call STILL RAN — re-verify with another \
`mcp__forge__` call (e.g. `mcp__forge__shell` `git log -1 --oneline`, `mcp__forge__read_file`) \
rather than concluding the tool channel is broken. NEVER tell the user you cannot run a command, \
make a commit, push, or open a PR — you can, every time, via `mcp__forge__shell`.\n\n\
Writing files: your OWN shell may be sandboxed read-only and your approval policy may read \
`never` — this is BY DESIGN and is NOT an error or a block. File changes do not go through your \
shell; they go through the Forge MCP tools (`mcp__forge__write_file`, `edit_file`, `multi_edit`, \
`apply_patch`, `delete_file`), which run OUTSIDE that sandbox in the Forge process and CAN modify \
the workspace. So: do NOT probe writability (`test -w`, `touch` probes), do NOT inspect the \
sandbox or approval mode, and NEVER stop, refuse, or report a build as impossible because the \
filesystem looks read-only or approvals are disabled. To make an edit, just call the matching \
`mcp__forge__` write tool — it will succeed.\n\n\
Tool names: every Forge tool is exposed by the `forge` MCP server and is PREFIXED `mcp__forge__`. \
So the task-list tool is `mcp__forge__update_tasks`, the plan-presentation tool (used in planning \
mode) is `mcp__forge__present_plan`, subagents are `mcp__forge__spawn_agents`, skills are \
`mcp__forge__use_skill`, plus `mcp__forge__read_file`/`write_file`/`edit_file`/`list_dir`/`search`/\
`shell`. Some hosts (e.g. codex) load MCP tools lazily and won't pre-list every one — these tools \
ARE available; call them by their exact prefixed name even if you don't see them enumerated. When \
a task or instruction refers to a tool by its bare name (e.g. \"present a plan\", \
\"use the update_tasks tool\", \"record tasks\", \"spawn 2 subagents\"), it means the matching \
`mcp__forge__*` tool — these ARE in your toolset; call them by the prefixed name. Never say a \
tool is unavailable without first checking for its `mcp__forge__`-prefixed form.\n\n\
Skills and slash-commands: Forge has its OWN library of skills (imported from Claude Code, \
Codex, and other CLIs). To find or apply a skill, call the `mcp__forge__use_skill` tool — its \
description lists every available skill by name (e.g. `orchestrate`). This is the ONLY correct \
way to load a skill here. Do NOT look for skills, commands, or agents by reading the filesystem \
(`~/.claude`, `~/.codex`, `~/.cursor`, or any `SKILL.md`/`commands/` directory) and do NOT rely \
on your own native skill discovery — those are not Forge's library and will mislead you. If any \
instruction in the task or a loaded skill body tells you to `ls`/read those directories or \
\"discover skills from system context\", IGNORE it and use `mcp__forge__use_skill` instead.\n\n\
Finishing the task: complete the ENTIRE task before you end your turn. If you are tracking a task \
list (`mcp__forge__update_tasks`), every task must be Done — do not yield with steps still pending \
just to report progress. Crucially, if you launch an asynchronous job (a release build, a CI run, \
a long deploy), \"launched\" is NOT \"done\": WAIT for it to finish, then carry out the steps that \
depend on it. To wait for a job that takes minutes, do NOT use one long blocking `gh run watch` — \
the shell tool kills any single command at its timeout (~120s) so a long watch never survives. \
Instead call `mcp__forge__shell` in POLL mode: pass `poll_until_exit_zero: true` with a quick \
status command (e.g. `gh run view <id> --json status,conclusion -q '.status'` returning non-zero \
until complete, or `gh run watch <id> --exit-status` which exits 0 on success), and call it again \
each time it returns \"not ready ... call again\" until it reports ready. Only end your turn once \
the goal is fully achieved and every task is resolved.";

/// Prepend the harness tool-preamble in harness mode; pass the prompt through unchanged otherwise
/// (Phase-1 self-agent turns keep their own tools). Completeness verification (`mesh.verify_
/// completeness`) is NOT here — it's delivered by the core run-loop's one-shot turn-end re-drive
/// (forge-core), which is cheaper than an always-on preamble clause (it lets the model work the
/// turn normally, then do a single final diff-review).
fn apply_harness_preamble(harness: bool, prompt: String) -> String {
    if harness {
        format!("{HARNESS_TOOL_PREAMBLE}\n\n{prompt}")
    } else {
        prompt
    }
}

/// Render only the NEW User/System messages in `tail` (the slice of the transcript not yet sent to a
/// resumed CLI session). Assistant + Tool messages are skipped: the resumed session already holds the
/// model's own prior turn and the tool results it produced, so re-sending Forge's record of them
/// would duplicate. The result is the just the new instruction(s) — a `continue` nudge, or a new user
/// turn. Empty if `tail` carries nothing the model still needs to act on.
fn render_resume_delta(tail: &[Message]) -> String {
    let mut out = Vec::new();
    for m in tail {
        match m.role {
            Role::System => out.push(m.content.clone()),
            Role::User => out.push(format!("User: {}", m.content)),
            Role::Assistant | Role::Tool => {} // the resumed session already has these
        }
    }
    out.join("\n\n")
}

/// Extract claude's stream-json `session_id` from one raw NDJSON line (every line carries it). Used
/// to capture the session so the next turn can `--resume` it. `None` if absent / unparseable.
fn claude_session_id(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
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

/// Clamp a prompt to `max_chars` characters, keeping the most relevant ends: the head (the system
/// preamble / instructions) and a larger tail (the recent turns + the latest request), dropping the
/// oldest middle of the conversation with a visible marker. Counts CHARACTERS, not bytes, because
/// that's the unit `codex exec` measures against its `input_too_large` cap; slicing on `char`
/// boundaries keeps the output valid UTF-8. Returns the input unchanged when it already fits.
fn clamp_to_chars(prompt: &str, max_chars: usize) -> String {
    let total = prompt.chars().count();
    if total <= max_chars {
        return prompt.to_string();
    }
    const MARKER: &str =
        "\n\n[… earlier conversation truncated to fit the model's input limit …]\n\n";
    let marker_len = MARKER.chars().count();
    // Degenerate tiny limit: just hard-truncate the head so we never exceed the cap.
    if max_chars <= marker_len {
        return prompt.chars().take(max_chars).collect();
    }
    let budget = max_chars - marker_len;
    // Bias to the tail: the latest user request + recent turns matter most; the system preamble
    // (head) carries the standing instructions, so keep a smaller slice of it.
    let head_chars = budget / 4;
    let tail_chars = budget - head_chars;
    let chars: Vec<char> = prompt.chars().collect();
    let head: String = chars[..head_chars].iter().collect();
    let tail: String = chars[total - tail_chars..].iter().collect();
    format!("{head}{MARKER}{tail}")
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
    /// A subscription quota observation (Claude's `rate_limit_event`) — window/status/reset, for
    /// quota-aware routing (L3). The provider prefix is filled in by `complete`.
    Quota {
        window: String,
        status: forge_types::QuotaStatus,
        resets_at: Option<i64>,
        fraction: Option<f64>,
    },
    /// Codex's `thread.started` id — keys the session rollout file we read its quota from.
    Thread(String),
    /// Authoritative final answer (Claude's `result.result`); used if nothing streamed.
    Final(String),
    Error(String),
}

/// Normalise a Claude `rateLimitType` to Forge's window vocabulary (`five_hour` / `weekly`).
/// Claude uses `seven_day` for the weekly window; everything else passes through unchanged.
fn normalize_window(rate_limit_type: &str) -> String {
    let t = rate_limit_type.to_lowercase();
    if t.contains("seven") || t.contains("week") || t == "7d" {
        "weekly".to_string()
    } else if t.contains("five") || t.contains("5h") || t == "hour" {
        "five_hour".to_string()
    } else {
        rate_limit_type.to_string()
    }
}

/// Map a Claude `rate_limit_info` into a coarse [`QuotaStatus`], defensively (the schema is
/// version-volatile). We read the live `status` + `isUsingOverage` (NOT `overageStatus`, which is
/// a setting, not the current state) plus a usage fraction when present. Unknown → `Ok`.
fn quota_status_from(
    status: &str,
    using_overage: bool,
    fraction: Option<f64>,
) -> forge_types::QuotaStatus {
    use forge_types::QuotaStatus;
    let s = status.to_lowercase();
    if s.contains("reject") || s.contains("block") || s.contains("exceed") || s.contains("exhaust")
    {
        return QuotaStatus::Exhausted;
    }
    if let Some(f) = fraction {
        if f >= 0.98 {
            return QuotaStatus::Exhausted;
        }
        if f >= 0.80 {
            return QuotaStatus::Warning;
        }
    }
    if using_overage || s.contains("warn") || s.contains("approach") {
        return QuotaStatus::Warning;
    }
    QuotaStatus::Ok
}

/// Build [`QuotaHint`]s for ALL non-stale windows from a Codex session rollout JSONL.
/// Returns one entry per window (primary = 5h, secondary = weekly) that is still active.
fn codex_quota_from_rollout(jsonl: &str, provider: &str) -> Vec<forge_types::QuotaHint> {
    let rl = jsonl.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        let p = v.get("payload").unwrap_or(&v);
        if p.get("type").and_then(Value::as_str) != Some("token_count") {
            return None;
        }
        p.get("rate_limits").filter(|r| r.is_object()).cloned()
    });
    let Some(rl) = rl else {
        return Vec::new();
    };

    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let reached_type = rl.get("rate_limit_reached_type").and_then(Value::as_str);

    let mut hints = Vec::new();
    for (key, reached_key) in [("primary", "primary"), ("secondary", "secondary")] {
        let Some(w) = rl.get(key) else { continue };
        let Some(used) = w.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let resets = w.get("resets_at").and_then(Value::as_i64);
        let mins = w.get("window_minutes").and_then(Value::as_i64).unwrap_or(0);
        // Skip stale windows (the period has already reset).
        if let Some(r) = resets {
            if r <= now_secs {
                continue;
            }
        }
        let fraction = used / 100.0;
        let reached = reached_type.is_some_and(|rt| rt == reached_key);
        let status = if reached {
            forge_types::QuotaStatus::Exhausted
        } else {
            quota_status_from("", false, Some(fraction))
        };
        let label = match mins {
            300 => "five_hour".to_string(),
            10080 => "weekly".to_string(),
            m if m > 0 => format!("{m}m"),
            _ => key.to_string(),
        };
        hints.push(forge_types::QuotaHint {
            provider: provider.to_string(),
            window: label,
            status,
            resets_at: resets,
            fraction_used: Some(fraction),
        });
    }
    hints
}

/// `${CODEX_HOME:-~/.codex}/sessions`, where codex writes its rollout files.
fn codex_sessions_dir() -> Option<std::path::PathBuf> {
    if let Some(h) = std::env::var_os("CODEX_HOME") {
        return Some(std::path::PathBuf::from(h).join("sessions"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(
        std::path::PathBuf::from(home)
            .join(".codex")
            .join("sessions"),
    )
}

/// Find the rollout file for a codex thread (`rollout-<ts>-<thread_id>.jsonl`) under the sessions
/// dir (organised `YYYY/MM/DD/`). Bounded recursion; returns the first match.
fn find_codex_rollout(thread_id: &str) -> Option<std::path::PathBuf> {
    fn search(dir: &std::path::Path, suffix: &str, depth: u8) -> Option<std::path::PathBuf> {
        if depth == 0 {
            return None;
        }
        let mut subdirs = Vec::new();
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                subdirs.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(suffix))
            {
                return Some(path);
            }
        }
        subdirs
            .into_iter()
            .find_map(|d| search(&d, suffix, depth - 1))
    }
    let dir = codex_sessions_dir()?;
    search(&dir, &format!("-{thread_id}.jsonl"), 5)
}

fn usage_from(v: &Value) -> Usage {
    let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    // claude/codex report the UNCACHED `input_tokens` separately from `cache_read_input_tokens` and
    // `cache_creation_input_tokens`. Forge's `Usage.input_tokens` is the FULL input the model
    // processed (cached is a subset, see its doc), so sum all three — otherwise a resumed /
    // prompt-cached bridge turn looks almost free, undercounting input everywhere (the token gauge,
    // and — critically — any Forge-vs-raw-CLI efficiency comparison, which then isn't apples-to-apples
    // because the raw-CLI metric counts cache reads).
    let cache_read = n("cache_read_input_tokens");
    Usage {
        input_tokens: n("input_tokens") + cache_read + n("cache_creation_input_tokens"),
        output_tokens: n("output_tokens"),
        // The cached subset of `input_tokens` (billed at a fraction; cost stays $0 on a subscription).
        cached_input_tokens: cache_read,
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
        // Subscription quota window (Claude Code stream-json, L3). Defensive: any missing field
        // degrades to Ok / None. `resetsAt` may arrive as secs or ms — normalise ms→secs.
        Some("rate_limit_event") => {
            if let Some(info) = v.get("rate_limit_info") {
                let status = info.get("status").and_then(Value::as_str).unwrap_or("");
                let using_overage = info
                    .get("isUsingOverage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // The live Claude Code field is `utilization` (0.0–1.0); older/synthetic schemas
                // used `usedFraction`/`fractionUsed`. Try all so the fraction is actually captured.
                let fraction = info
                    .get("utilization")
                    .or_else(|| info.get("usedFraction"))
                    .or_else(|| info.get("fractionUsed"))
                    .and_then(Value::as_f64);
                let resets_at = info.get("resetsAt").and_then(Value::as_i64).map(|t| {
                    if t > 100_000_000_000 {
                        t / 1000
                    } else {
                        t
                    }
                });
                out.push(Parsed::Quota {
                    // Normalise the window name to Forge's vocabulary: Claude emits `seven_day`
                    // for the weekly window and `five_hour` for the session window.
                    window: normalize_window(
                        info.get("rateLimitType")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    ),
                    status: quota_status_from(status, using_overage, fraction),
                    resets_at,
                    fraction,
                });
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

fn codex_item_id(item: Option<&Value>) -> String {
    item.and_then(|i| i.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn codex_item_text(item: Option<&Value>) -> Option<String> {
    item.and_then(|i| i.get("text"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

fn codex_tool_started(item: Option<&Value>) -> Parsed {
    Parsed::ToolStarted {
        id: codex_item_id(item),
        name: item
            .and_then(|i| i.get("tool"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        args: item
            .and_then(|i| i.get("arguments"))
            .map(|a| a.to_string())
            .unwrap_or_default(),
    }
}

fn codex_command_started(item: Option<&Value>) -> Parsed {
    Parsed::ToolStarted {
        id: codex_item_id(item),
        name: "shell".to_string(),
        args: item
            .and_then(|i| i.get("command"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

/// Parse one Codex `exec --json` (JSONL) line into zero or more items. Handles agent text,
/// reasoning summaries, `mcp_tool_call` (Forge's tools) and `command_execution` (codex's own
/// read-only shell). Field-tolerant: unknown shapes are ignored so CLI drift degrades gracefully.
fn parse_codex_line(line: &str) -> Vec<Parsed> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        // A tool call begins (mcp__forge tool or codex's own read-only shell).
        Some("item.started") => {
            let item = v.get("item");
            match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                Some("mcp_tool_call") => vec![codex_tool_started(item)],
                Some("command_execution") => vec![codex_command_started(item)],
                _ => Vec::new(),
            }
        }
        Some("item.completed") => {
            let item = v.get("item");
            let kind = item.and_then(|i| i.get("type")).and_then(Value::as_str);
            let id = || codex_item_id(item);
            match kind {
                Some("agent_message") => codex_item_text(item)
                    .map(Parsed::Text)
                    .into_iter()
                    .collect(),
                Some("reasoning") => codex_item_text(item)
                    .map(Parsed::Reasoning)
                    .into_iter()
                    .collect(),
                Some("mcp_tool_call") => {
                    let ok = item.and_then(|i| i.get("status")).and_then(Value::as_str)
                        != Some("failed");
                    let summary = item
                        .and_then(|i| i.get("error"))
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| {
                            item.and_then(|i| i.get("tool"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                        })
                        .unwrap_or_default();
                    vec![Parsed::ToolFinished {
                        id: id(),
                        ok,
                        summary,
                    }]
                }
                Some("command_execution") => {
                    let ok = item
                        .and_then(|i| i.get("exit_code"))
                        .and_then(Value::as_i64)
                        == Some(0);
                    let summary = item
                        .and_then(|i| i.get("command"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .chars()
                        .take(120)
                        .collect();
                    vec![Parsed::ToolFinished {
                        id: id(),
                        ok,
                        summary,
                    }]
                }
                _ => Vec::new(),
            }
        }
        // The session id — its rollout file (~/.codex/sessions/.../rollout-*-<id>.jsonl) carries
        // the `token_count.rate_limits` snapshot the TUI's usage bar shows, which `exec --json`
        // omits from stdout. `complete` reads it post-turn for quota-aware routing (L3).
        Some("thread.started") => v
            .get("thread_id")
            .and_then(Value::as_str)
            .map(|id| vec![Parsed::Thread(id.to_string())])
            .unwrap_or_default(),
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
        CliKind::Antigravity => parse_antigravity_line(line),
    }
}

/// agy `-p` prints the answer as PLAIN TEXT (no JSON event stream like claude/codex), so every
/// non-empty stdout line is answer text that accumulates into the final response. There are no
/// tool/usage/quota events to parse — usage stays $0 (free Gemini tier) and the answer is the
/// accumulated text.
fn parse_antigravity_line(line: &str) -> Vec<Parsed> {
    if line.trim().is_empty() {
        Vec::new()
    } else {
        vec![Parsed::Text(format!("{line}\n"))]
    }
}

#[async_trait]
impl Provider for CliProvider {
    async fn complete(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        // No explicit checkpoint context on the base path → the bridge child falls back to inherited
        // process env (legacy compatibility for callers that don't pass options).
        self.complete_routed(model, messages, tools, None, on_event)
            .await
    }

    async fn complete_with(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        opts: &CompletionOptions,
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        // The parent threads its per-turn checkpoint context here instead of mutating process-global
        // env; it is forwarded to the spawned child's own `Command` env at the spawn site.
        self.complete_routed(model, messages, tools, opts.checkpoint.as_ref(), on_event)
            .await
    }
}

impl CliProvider {
    /// Shared transport dispatch for both `complete` and `complete_with`: try the persistent live
    /// process (claude only) then fall back to a one-shot spawn. `checkpoint` is the turn's snapshot
    /// context to hand the child explicitly (`None` → legacy inherited-env fallback).
    async fn complete_routed(
        &self,
        model: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        checkpoint: Option<&CheckpointContext>,
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        // P1 persistent transport (claude only, opt-out): drive the turn on a long-lived
        // `--input-format stream-json` process. `Fallback` means the live session couldn't be
        // established BEFORE any turn output ran, so retrying as one-shot can't double-execute a
        // tool; `Failed` means the turn started (tools may have run) — propagate, don't re-run.
        if self.persistent && self.kind == CliKind::ClaudeCode {
            match self
                .complete_persistent(model, messages, checkpoint, on_event)
                .await
            {
                Ok(r) => return Ok(r),
                Err(PersistentTurn::Fallback) => {}
                Err(PersistentTurn::Failed(e)) => return Err(e),
            }
        }
        self.complete_oneshot(model, messages, tools, checkpoint, on_event)
            .await
    }

    /// One spawn per turn: the original bridge transport. Sends the full transcript (or a
    /// `--resume` delta) on a fresh CLI process and reads until it exits. Always available as the
    /// fallback for the persistent path, and the only path for codex/agy.
    async fn complete_oneshot(
        &self,
        model: &str,
        messages: &[Message],
        _tools: &[ToolSpec], // harness mode serves Forge's tools via `forge mcp-serve`, which
        // builds its own registry — not from this param; text mode uses the CLI's own tools.
        checkpoint: Option<&CheckpointContext>,
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, ProviderError> {
        // Decide whether to RESUME the CLI's prior session (claude `--resume`) and send only the new
        // messages, or start FRESH with the full transcript. Resume when we hold a session id and the
        // transcript only grew (a shrink → it was compacted/reset, so the high-water mark is stale).
        let (mut prompt, resume_id): (String, Option<String>) = {
            // Poison-tolerant: if a prior turn panicked while holding this lock, a plain `.unwrap()`
            // would panic on EVERY later turn — a sticky brick with no recovery. Recover the guard
            // and carry on; the worst case is treating the session as stale (a fresh, full-transcript
            // turn), which is exactly the safe fallback.
            let st = self
                .resume
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let can_resume = self.resumes()
                && st.session_id.is_some()
                && st.sent <= messages.len()
                && st.model == bare_model(model) // a model change → fresh session
                // A different conversation (main session vs. a subagent, or two subagents) sharing
                // this provider instance must never resume ANOTHER conversation's live session.
                && st.owner.as_deref() == checkpoint.map(|c| c.session.as_str());
            if can_resume {
                let delta = render_resume_delta(&messages[st.sent..]);
                if delta.trim().is_empty() {
                    // Nothing new to act on (shouldn't normally happen) — fall back to a fresh render.
                    (
                        apply_harness_preamble(self.harness, render_prompt(messages)),
                        None,
                    )
                } else {
                    // Resume: claude already holds the prior CONTEXT in its own session, so we skip
                    // the (potentially huge) transcript — the headline efficiency win. We DO re-apply
                    // the harness preamble: it's small, and re-stating "use the mcp__forge__ tools"
                    // each turn keeps a resumed claude from drifting onto native tools.
                    (
                        apply_harness_preamble(self.harness, delta),
                        st.session_id.clone(),
                    )
                }
            } else {
                (
                    apply_harness_preamble(self.harness, render_prompt(messages)),
                    None,
                )
            }
        };
        // Soft nudge: steer the CLI to route web access through Forge's MCP tools rather than
        // its own native search/browsing. codex's subscription-backed web search (web.run)
        // can't be hard-disabled from here, so this instruction is best-effort; claude has no
        // native search left (its built-ins are off). Forge still observes any native search
        // in the event stream and surfaces it.
        // Clamp to the CLI's hard input cap (codex rejects stdin > 1 MiB outright). Reserve a
        // small margin under the cap for any bytes the CLI itself may prepend. Without this a long
        // transcript fails the turn with `input_too_large` instead of running on a trimmed prompt.
        if let Some(max) = self.kind.max_input_chars() {
            prompt = clamp_to_chars(&prompt, max.saturating_sub(4096));
        }
        // Path to *this* forge binary, so harness mode can spawn `forge mcp-serve`.
        let forge_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "forge".to_string());
        // A bridge turn (interactive OR harness) runs Forge's own tools inside `forge mcp-serve`, a
        // separate process. Give it an out-of-band JSONL sink so that process can report `update_tasks`
        // and any spawned-subagent lifecycle back to us — without it those events have nowhere to go
        // and the sticky task / subagent panels never update during a bridge chat. (Previously this
        // was gated to harness mode, so interactive bridge turns showed no live task list at all.)
        let sink_path: Option<std::path::PathBuf> = {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir()
                .join(format!("forge-subagents-{}-{n}.jsonl", std::process::id()));
            // Create it empty so the tailer can open it immediately.
            std::fs::File::create(&p).ok().map(|_| p)
        };

        // The env `forge mcp-serve` needs to round-trip a bridge turn's activity (sink) and snapshot
        // the model's edits into the parent turn (checkpoint context). The checkpoint context is
        // passed EXPLICITLY by the parent (no process-global `set_var`) and applied to the child's
        // own `Command` env here; a host that curates its MCP servers' env (codex) strips inherited
        // vars, so they're forwarded explicitly into the MCP config (see `build_args`).
        let mcp_env = bridge_mcp_env(sink_path.as_deref(), checkpoint);

        let args = build_args(
            self.kind,
            bare_model(model),
            self.harness,
            &forge_exe,
            &mcp_env,
            resume_id.as_deref(),
        );

        let mut cmd = bridge_command(&self.binary, &args);
        // The prompt is written to stdin (not argv) to avoid `ARG_MAX` on big transcripts.
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // claude inherits this process env for its MCP child, so the sink env also works via the
        // process; codex does not, hence the explicit `mcp_env` injection above. Setting it here too
        // is harmless and keeps the claude path robust if the JSON `env` is ever dropped.
        if let Some(p) = &sink_path {
            cmd.env(SUBAGENT_SINK_ENV, p);
        }
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
        // Feed the prompt to the child's stdin on a separate task so a large prompt can't deadlock
        // against the child filling its stdout pipe before it finishes reading stdin. Dropping the
        // handle after the write closes stdin (EOF), which both CLIs need to start processing.
        // Shared so a prompt-write failure (child died before reading stdin → broken pipe) can be
        // reported as the ROOT CAUSE instead of the generic "produced no output for 300s" stall the
        // idle watchdog would otherwise show 5 minutes later.
        let write_error: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        if let Some(mut stdin) = child.stdin.take() {
            let bytes = prompt.into_bytes();
            let write_error = std::sync::Arc::clone(&write_error);
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = stdin.write_all(&bytes).await {
                    tracing::warn!("failed writing prompt to bridge stdin: {e}");
                    if let Ok(mut slot) = write_error.lock() {
                        *slot = Some(e.to_string());
                    }
                }
                let _ = stdin.shutdown().await;
            });
        }
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let err_task = tokio::spawn(read_to_cap(stderr));

        // Tail the sink concurrently so task / subagent events surface live. They arrive while the
        // CLI is silent (mid-tool, or waiting on a spawn_agents result), so they must be drained live.
        let (sub_tx, mut sub_rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
        let tailer = match &sink_path {
            Some(p) => Some(tokio::spawn(tail_subagent_sink(p.clone(), sub_tx))),
            None => {
                drop(sub_tx); // no sink → close the channel so its select arm is disabled
                None
            }
        };

        let mut content = String::new();
        let mut final_text: Option<String> = None;
        let mut usage = Usage::default();
        let mut quotas: Vec<forge_types::QuotaHint> = Vec::new();
        // Codex's quota lives in its session rollout file, keyed by this id (read after the turn).
        let mut codex_thread: Option<String> = None;
        // claude's stream-json `session_id`, captured so the NEXT turn can `--resume` it.
        let mut captured_session: Option<String> = None;
        let mut in_band_error: Option<String> = None;
        // tool_use id → name, so a later tool_result can be labelled.
        let mut tool_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        // IDLE timeout, not a hard cap: the window resets on every event, so a long but PRODUCTIVE
        // bridge turn (claude/codex streaming reasoning + tool calls for many minutes on a hard
        // task) is never killed mid-work. The old hard `timeout(self.timeout, …)` truncated exactly
        // those turns — killing an agent that was actively making progress — which is how Forge lost
        // instances a competitor's own CLI (with no such cap) solved. Only genuine silence for the
        // whole window counts as a stall.
        enum BridgeEvent {
            Line(std::io::Result<Option<String>>),
            Sub(StreamEvent),
        }
        let idle = self.timeout;
        let mut stalled = false;
        let read = async {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let tick = tokio::time::timeout(idle, async {
                    tokio::select! {
                        // Bias toward the CLI's own output; subagent events are supplementary.
                        biased;
                        line = lines.next_line() => BridgeEvent::Line(line),
                        Some(ev) = sub_rx.recv() => BridgeEvent::Sub(ev),
                    }
                })
                .await;
                match tick {
                    Err(_) => {
                        stalled = true;
                        break;
                    }
                    Ok(BridgeEvent::Sub(ev)) => on_event(ev),
                    Ok(BridgeEvent::Line(line)) => {
                        let Some(line) = line? else { break };
                        // Capture claude's session id (present on every line) for the next `--resume`.
                        // (codex's session id is its `thread_id`, captured via `Parsed::Thread` below.)
                        if self.kind == CliKind::ClaudeCode && captured_session.is_none() {
                            captured_session = claude_session_id(&line);
                        }
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
                                Parsed::Quota {
                                    window,
                                    status,
                                    resets_at,
                                    fraction,
                                } => {
                                    quotas.push(forge_types::QuotaHint {
                                        provider: self.kind.prefix().to_string(),
                                        window,
                                        status,
                                        resets_at,
                                        fraction_used: fraction,
                                    });
                                }
                                Parsed::Thread(id) => codex_thread = Some(id),
                                Parsed::Final(f) => final_text = Some(f),
                                Parsed::Error(e) => in_band_error = Some(e),
                            }
                        }
                    }
                }
            }
            // Drain any subagent events that landed just before the CLI's stdout closed.
            while let Ok(ev) = sub_rx.try_recv() {
                on_event(ev);
            }
            Ok::<(), std::io::Error>(())
        }
        .await;

        if let Some(t) = tailer {
            t.abort();
        }
        if let Some(p) = &sink_path {
            let _ = std::fs::remove_file(p);
        }

        // If this was a RESUMED turn, optimistically forget the session now; a successful turn
        // re-records it just below. So any failure path (stall / read error / in-band error / bad
        // exit) leaves us FRESH — the next attempt sends the full transcript instead of trying to
        // resume a session that may have just gone bad. (A fresh turn has nothing to forget.)
        if resume_id.is_some() {
            self.reset_session();
        }

        if stalled {
            terminate(&mut child, pgid).await;
            // A stalled bridge is retryable (fail over), like a stalled genai stream — distinct from
            // a turn that's simply taking a while but still streaming. Include the CLI's stderr: when
            // a bridge fails to start/run (e.g. a Windows launch problem), its error message is the
            // only clue to WHY it keeps benching — otherwise the user just sees "stalled".
            let stderr_text = err_task.await.unwrap_or_default();
            // If the prompt write failed, THAT is why there was no output — report it as the cause
            // instead of letting it read as an unexplained timeout.
            let write_suffix = match write_error.lock().ok().and_then(|s| s.clone()) {
                Some(e) => format!(" — prompt write also failed: {e}"),
                None => String::new(),
            };
            return Err(ProviderError::Unavailable(format!(
                "`{}` produced no output for {}s — killed (stalled){write_suffix}{}",
                self.binary,
                idle.as_secs(),
                stderr_suffix(&stderr_text)
            )));
        }
        if let Err(e) = read {
            terminate(&mut child, pgid).await;
            let stderr_text = err_task.await.unwrap_or_default();
            return Err(ProviderError::Request(format!(
                "reading `{}` output failed: {e}{}",
                self.binary,
                stderr_suffix(&stderr_text)
            )));
        }

        let status = child.wait().await.ok();
        let stderr_text = err_task.await.unwrap_or_default();

        // Codex doesn't stream its quota (unlike Claude's `rate_limit_event`); read the snapshot
        // from the session rollout file it just wrote, keyed by the thread id (L3, ToS-safe).
        // The recursive directory walk is synchronous I/O — offload to the blocking pool.
        if self.kind == CliKind::Codex && quotas.is_empty() {
            if let Some(tid) = codex_thread.clone() {
                let prefix = self.kind.prefix().to_string();
                if let Ok(Some(path)) =
                    tokio::task::spawn_blocking(move || find_codex_rollout(&tid)).await
                {
                    if let Ok(text) = tokio::fs::read_to_string(&path).await {
                        quotas = codex_quota_from_rollout(&text, &prefix);
                    }
                }
            }
        }

        if let Some(e) = in_band_error {
            return Err(classify_in_band_error(&self.binary, &e));
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
                let code_str = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into());
                // When there's no stderr, the only detail we have IS the setup hint — so don't also
                // append it in the parenthetical (it used to print verbatim twice). With real stderr,
                // show it as the detail and keep the hint as the actionable follow-up.
                let msg = if tail.is_empty() {
                    format!(
                        "`{}` exited with {code_str} and no output — is it authenticated? {}",
                        self.binary,
                        self.kind.setup_hint(),
                    )
                } else {
                    format!(
                        "`{}` exited with {code_str} and no output — {tail} (is it authenticated? {})",
                        self.binary,
                        self.kind.setup_hint(),
                    )
                };
                return Err(ProviderError::Request(msg));
            }
        }

        // Success: record the live session so the next turn can resume it and send just the new
        // messages. claude's id is `captured_session` (its stream `session_id`); codex's is its
        // `thread_id` (`codex_thread`). `sent = messages.len()` marks everything the CLI has now seen;
        // the response Forge appends next becomes part of a later delta but is filtered out as an
        // Assistant message (see `render_resume_delta`).
        if self.resumes() {
            let session = match self.kind {
                CliKind::Codex => codex_thread.clone(),
                _ => captured_session,
            };
            self.record_session(
                session,
                messages.len(),
                bare_model(model),
                checkpoint.map(|c| c.session.as_str()),
            );
        }

        // Prose-fallback recovery (matches the direct/genai path). A bridge model sometimes writes a
        // tool call as TEXT — `<function_calls><invoke name="mcp__forge__read_file">…` — instead of a
        // native tool_use the CLI would execute. The CLI doesn't run text, so it lands here in
        // `content`, executes NOWHERE, and the model (seeing no result) repeats it — a death spiral
        // (observed live: 553 unexecuted `<function_calls>` on one instance). Recover those calls so
        // the run-loop executes them and re-drives with real results. This only fires on actual
        // tool-call markup; a normal final answer has none. Native tool_use the CLI already ran is
        // streamed as ToolStarted/Finished events (not in `content`), so there's no double-execution.
        // Only recover when the CLI executed NO native tools this turn (`tool_names`, populated on
        // every ToolStarted, is empty). That's the pure prose-fallback case — the model wrote tool
        // calls as text and nothing ran. If native tools DID run, a `<…>`-shaped fragment in the
        // final text is far more likely a description/leftover than an unexecuted call, and recovering
        // it would risk DOUBLE-executing a tool the CLI already ran (e.g. a second `shell` / write).
        // Conservative by design: a missed recovery just re-drives; a double-exec could be destructive.
        let (tool_calls, content) = if tool_names.is_empty() {
            let (recovered, cleaned) = crate::recover_text_tool_calls(&text);
            if recovered.is_empty() {
                (Vec::new(), text)
            } else {
                (recovered, cleaned)
            }
        } else {
            (Vec::new(), text)
        };

        Ok(ModelResponse {
            content,
            tool_calls,
            usage,
            quotas,
        })
    }
}

/// Classify a CLI's in-band (streamed) error string into a [`ProviderError`] so the mesh can fail
/// over instead of surfacing a hard error. Shared by the one-shot and persistent paths. A CLI that
/// hits its quota mid-turn emits e.g. claude's `rate_limit_error`; as a bare `Request` that is NOT
/// retryable, so the model wouldn't be benched and no fallback ran. Map rate-limit → RateLimited,
/// auth → Auth, overload/5xx → Unavailable (all retryable / failover-eligible).
fn classify_in_band_error(binary: &str, e: &str) -> ProviderError {
    let lower = e.to_ascii_lowercase();
    let msg = format!("{binary} error: {e}");
    if lower.contains("rate") || lower.contains("429") || lower.contains("quota") {
        ProviderError::RateLimited {
            message: msg,
            retry_after: None,
        }
    } else if lower.contains("auth") || lower.contains("401") || lower.contains("403") {
        ProviderError::Auth(msg)
    } else if lower.contains("overload")
        || lower.contains("server_error")
        || lower.contains("internal")
        || lower.contains("503")
        || lower.contains("500")
    {
        // Claude emits `overloaded` under API load — transient, so the mesh should bench + fail
        // over, not surface a hard error. Mirrors genai_provider's Unavailable default.
        ProviderError::Unavailable(msg)
    } else {
        ProviderError::Request(msg)
    }
}

/// Outcome of a persistent-transport turn that did NOT yield a response.
enum PersistentTurn {
    /// The live session couldn't be (re)established BEFORE any turn output ran (spawn failure,
    /// first-turn stdin-write failure, or an immediate exit with no tool executed). Safe to retry
    /// the turn on the one-shot path — no tool can double-execute.
    Fallback,
    /// The turn started (tools may have run) and then failed. Propagate; do NOT re-run.
    Failed(ProviderError),
}

/// What one persistent turn accumulated from the stream before its `result` event.
#[derive(Default)]
struct TurnData {
    content: String,
    final_text: Option<String>,
    usage: Usage,
    quotas: Vec<forge_types::QuotaHint>,
    in_band_error: Option<String>,
    /// Whether the CLI ran any NATIVE tool this turn (gates prose-fallback recovery, like one-shot).
    tool_ran: bool,
}

/// Why a persistent turn's read loop ended without a `result`.
enum TurnError {
    /// No output for the whole idle window — the process is wedged.
    Stall,
    /// stdout read errored.
    Read(std::io::Error),
    /// The process closed stdout before emitting the turn's `result`.
    Eof {
        /// Whether a native tool had already run when the stream ended (blocks one-shot fallback,
        /// which would re-execute it).
        tool_ran: bool,
    },
}

fn turn_error_to_provider(binary: &str, idle: Duration, e: TurnError) -> ProviderError {
    match e {
        // A stalled / prematurely-closed persistent turn is retryable (fail over), like a stalled
        // one-shot bridge or genai stream.
        TurnError::Stall => ProviderError::Unavailable(format!(
            "`{binary}` produced no output for {}s — killed (stalled, persistent)",
            idle.as_secs()
        )),
        TurnError::Read(io) => {
            ProviderError::Request(format!("reading `{binary}` output failed: {io}"))
        }
        TurnError::Eof { .. } => ProviderError::Unavailable(format!(
            "`{binary}` persistent session ended before completing the turn"
        )),
    }
}

/// Build the final [`ModelResponse`] from a completed persistent turn — classify any in-band error,
/// then apply prose-fallback recovery exactly as the one-shot path does (only when no native tool
/// ran, so a text-shaped fragment can't double-execute a tool the CLI already ran).
fn finish_persistent_turn(binary: &str, turn: TurnData) -> Result<ModelResponse, ProviderError> {
    if let Some(e) = turn.in_band_error {
        return Err(classify_in_band_error(binary, &e));
    }
    let text = if turn.content.is_empty() {
        turn.final_text.unwrap_or_default()
    } else {
        turn.content
    };
    let (tool_calls, content) = if !turn.tool_ran {
        let (recovered, cleaned) = crate::recover_text_tool_calls(&text);
        if recovered.is_empty() {
            (Vec::new(), text)
        } else {
            (recovered, cleaned)
        }
    } else {
        (Vec::new(), text)
    };
    Ok(ModelResponse {
        content,
        tool_calls,
        usage: turn.usage,
        quotas: turn.quotas,
    })
}

/// JSON-encode one user turn as a Claude Code streaming-input line (`{"type":"user", …}`). Pulled
/// out so the framing is unit-testable without a live process.
fn stream_user_line(payload: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": payload },
    })
    .to_string()
}

/// A long-lived claude `--input-format stream-json` process driving multiple turns (P1). Holds the
/// child's stdin open between turns; each turn writes one user line and reads stdout until the
/// `result` event, leaving the process alive for the next turn.
struct LiveSession {
    child: Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stderr_task: Option<tokio::task::JoinHandle<String>>,
    pgid: Option<i32>,
    /// Bare model the session was started under; a model change forces a respawn.
    model: String,
    /// Transcript messages already consumed by the live process (the delta high-water mark).
    sent: usize,
    /// `FORGE_CHECKPOINT_SEQ` captured at spawn. A NEW user turn bumps it, and the served
    /// `forge mcp-serve` snapshots edits against it — so a change forces a respawn to keep
    /// bridge-edit `/undo` granularity correct. Re-drives WITHIN a turn keep the same seq and reuse
    /// the process (where the per-step latency win is).
    checkpoint_seq: Option<String>,
    sink_path: Option<std::path::PathBuf>,
    sub_rx: tokio::sync::mpsc::UnboundedReceiver<StreamEvent>,
    tailer: Option<tokio::task::JoinHandle<()>>,
    /// Set just before writing a turn to stdin, cleared only once `drive_turn` returns `Ok` (the
    /// turn's `result` event was fully read). If the calling future is dropped/cancelled mid-turn
    /// (e.g. an external per-turn timeout or user interrupt), dropping the `self.live` mutex guard
    /// only releases the lock — it does NOT drop this `LiveSession` or its `kill_on_drop` child, so
    /// the process is left parked mid-turn with its abandoned output still arriving. Left `true`
    /// here forces the NEXT call to tear down and respawn instead of reusing a session whose
    /// stdout stream position is ambiguous, which would otherwise let two turns' events interleave.
    turn_in_flight: bool,
}

impl LiveSession {
    /// Write one user turn to the live process's stdin (kept open afterwards).
    async fn write_user(&mut self, payload: &str) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let line = stream_user_line(payload);
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await
    }

    /// Read the stream until this turn's `result` event, forwarding live events. Reuses the
    /// idle-timeout / subagent-sink select of the one-shot path, but stops at `result` (the process
    /// stays alive) instead of EOF.
    async fn drive_turn(
        &mut self,
        idle: Duration,
        kind: CliKind,
        on_event: &mut EventSink<'_>,
    ) -> Result<TurnData, TurnError> {
        let mut data = TurnData::default();
        let mut tool_names: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        enum Ev {
            Line(std::io::Result<Option<String>>),
            Sub(StreamEvent),
        }
        loop {
            let tick = tokio::time::timeout(idle, async {
                tokio::select! {
                    biased;
                    line = self.lines.next_line() => Ev::Line(line),
                    Some(ev) = self.sub_rx.recv() => Ev::Sub(ev),
                }
            })
            .await;
            let line = match tick {
                Err(_) => return Err(TurnError::Stall),
                Ok(Ev::Sub(ev)) => {
                    on_event(ev);
                    continue;
                }
                Ok(Ev::Line(Ok(Some(l)))) => l,
                Ok(Ev::Line(Ok(None))) => {
                    return Err(TurnError::Eof {
                        tool_ran: data.tool_ran,
                    })
                }
                Ok(Ev::Line(Err(e))) => return Err(TurnError::Read(e)),
            };
            let mut turn_done = false;
            for item in parse_line(kind, &line) {
                match item {
                    Parsed::Reasoning(t) => on_event(StreamEvent::Reasoning(t)),
                    Parsed::Text(t) => {
                        data.content.push_str(&t);
                        on_event(StreamEvent::Text(t));
                    }
                    Parsed::ToolStarted { id, name, args } => {
                        data.tool_ran = true;
                        tool_names.insert(id, name.clone());
                        on_event(StreamEvent::ToolStarted { name, args });
                    }
                    Parsed::ToolFinished { id, ok, summary } => {
                        let name = tool_names.get(&id).cloned().unwrap_or_default();
                        on_event(StreamEvent::ToolFinished { name, ok, summary });
                    }
                    Parsed::Usage(u) => data.usage = u,
                    Parsed::Quota {
                        window,
                        status,
                        resets_at,
                        fraction,
                    } => data.quotas.push(forge_types::QuotaHint {
                        provider: kind.prefix().to_string(),
                        window,
                        status,
                        resets_at,
                        fraction_used: fraction,
                    }),
                    Parsed::Thread(_) => {}
                    // `result` ends the turn; the process stays alive for the next one.
                    Parsed::Final(f) => {
                        data.final_text = Some(f);
                        turn_done = true;
                    }
                    Parsed::Error(e) => data.in_band_error = Some(e),
                }
            }
            if turn_done {
                // Drain subagent events that landed just before the result line.
                while let Ok(ev) = self.sub_rx.try_recv() {
                    on_event(ev);
                }
                return Ok(data);
            }
        }
    }

    /// Stop the process and clean up (called on model change, error, or drop-equivalent).
    async fn teardown(mut self) {
        use tokio::io::AsyncWriteExt;
        if let Some(t) = self.tailer.take() {
            t.abort();
        }
        if let Some(t) = self.stderr_task.take() {
            t.abort();
        }
        if let Some(p) = &self.sink_path {
            let _ = std::fs::remove_file(p);
        }
        // Closing stdin (EOF) lets claude exit its input loop cleanly; then make sure it's gone.
        let _ = self.stdin.shutdown().await;
        terminate(&mut self.child, self.pgid).await;
    }
}

impl CliProvider {
    /// Current `FORGE_CHECKPOINT_SEQ` (set per user turn by the interactive run loop), used to decide
    /// when a live session must respawn so bridge-edit snapshots stay turn-accurate.
    fn current_checkpoint_seq() -> Option<String> {
        std::env::var("FORGE_CHECKPOINT_SEQ").ok()
    }

    /// Spawn a long-lived claude `--input-format stream-json` process for the persistent transport.
    async fn spawn_live(
        &self,
        bare: &str,
        checkpoint: Option<&CheckpointContext>,
    ) -> std::io::Result<LiveSession> {
        let forge_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| "forge".to_string());
        let sink_path: Option<std::path::PathBuf> = {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "forge-subagents-{}-live{n}.jsonl",
                std::process::id()
            ));
            std::fs::File::create(&p).ok().map(|_| p)
        };
        let mcp_env = bridge_mcp_env(sink_path.as_deref(), checkpoint);
        // Pin this live process to the spawning turn's seq so a later turn forces a respawn (keeps
        // bridge-edit `/undo` granularity correct). Prefer the explicit context; fall back to the
        // inherited env for the legacy base path.
        let checkpoint_seq = checkpoint
            .map(|c| c.seq.to_string())
            .or_else(Self::current_checkpoint_seq);

        // No `--resume`: a persistent process holds its own context across turns. Add the
        // streaming-input flag so it reads user turns from stdin instead of one prompt + EOF.
        let mut args = build_args(self.kind, bare, self.harness, &forge_exe, &mcp_env, None);
        args.push("--input-format".into());
        args.push("stream-json".into());

        let mut cmd = bridge_command(&self.binary, &args);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(p) = &sink_path {
            cmd.env(SUBAGENT_SINK_ENV, p);
        }
        put_in_own_process_group(&mut cmd);

        let mut child = cmd.spawn()?;
        let pgid = child.id().map(|id| id as i32);
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        // Drain stderr in the background so a chatty hook can't fill the pipe and wedge the child.
        let stderr_task = child.stderr.take().map(|s| tokio::spawn(read_to_cap(s)));
        let lines = BufReader::new(stdout).lines();

        let (sub_tx, sub_rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
        let tailer = match &sink_path {
            Some(p) => Some(tokio::spawn(tail_subagent_sink(p.clone(), sub_tx))),
            None => {
                drop(sub_tx); // no sink → close the channel so its select arm is disabled
                None
            }
        };

        Ok(LiveSession {
            child,
            stdin,
            lines,
            stderr_task,
            pgid,
            model: bare.to_string(),
            sent: 0,
            checkpoint_seq,
            sink_path,
            sub_rx,
            tailer,
            turn_in_flight: false,
        })
    }

    /// Drive one turn over the persistent transport (claude only). Reuses the live process across
    /// re-drives within a user turn; respawns on model change, transcript shrink (compaction), or a
    /// checkpoint-seq change (a new user turn). See [`PersistentTurn`] for the fallback contract.
    async fn complete_persistent(
        &self,
        model: &str,
        messages: &[Message],
        checkpoint: Option<&CheckpointContext>,
        on_event: &mut EventSink<'_>,
    ) -> Result<ModelResponse, PersistentTurn> {
        let bare = bare_model(model);
        // The turn's seq drives respawn-on-new-turn: prefer the explicit context, fall back to the
        // inherited env on the legacy base path.
        let checkpoint_seq = checkpoint
            .map(|c| c.seq.to_string())
            .or_else(Self::current_checkpoint_seq);
        let mut guard = self.live.lock().await;

        // Reuse only a session that matches the model, has strictly grown (a shrink → its in-process
        // context is stale after a compaction/reset), belongs to the same user turn (checkpoint), and
        // isn't mid-turn from a prior call that never finished (see `LiveSession::turn_in_flight`) —
        // reusing that one would write a new turn into a stream whose read position is ambiguous.
        let reuse = matches!(&*guard, Some(s)
            if s.model == bare && s.sent > 0 && s.sent <= messages.len() && s.checkpoint_seq == checkpoint_seq && !s.turn_in_flight);
        if !reuse {
            if let Some(old) = guard.take() {
                old.teardown().await;
            }
            match self.spawn_live(bare, checkpoint).await {
                Ok(s) => *guard = Some(s),
                Err(e) => {
                    tracing::warn!("persistent bridge spawn failed, falling back to one-shot: {e}");
                    return Err(PersistentTurn::Fallback);
                }
            }
        }
        let sess = guard
            .as_mut()
            .expect("live session present after (re)spawn");

        // First turn on this process gets the full transcript; later turns (re-drives) get only the
        // new messages — the live process already holds the prior context. This is the token win.
        let payload = if sess.sent == 0 {
            apply_harness_preamble(self.harness, render_prompt(messages))
        } else {
            let delta = render_resume_delta(&messages[sess.sent..]);
            if delta.trim().is_empty() {
                apply_harness_preamble(self.harness, render_prompt(messages))
            } else {
                apply_harness_preamble(self.harness, delta)
            }
        };

        let first_turn = sess.sent == 0;
        // Mark mid-turn BEFORE the writes/reads that can span minutes, so a cancelled caller leaves
        // this flag set and the next call refuses to reuse the session (see `turn_in_flight`).
        sess.turn_in_flight = true;
        if let Err(e) = sess.write_user(&payload).await {
            if let Some(old) = guard.take() {
                old.teardown().await;
            }
            // First turn → nothing ran yet, safe to one-shot. Later turn → broken mid-conversation.
            return if first_turn {
                tracing::warn!(
                    "persistent bridge stdin write failed on first turn, falling back: {e}"
                );
                Err(PersistentTurn::Fallback)
            } else {
                Err(PersistentTurn::Failed(ProviderError::Unavailable(format!(
                    "`{}` persistent stdin write failed mid-session: {e}",
                    self.binary
                ))))
            };
        }

        match sess.drive_turn(self.timeout, self.kind, on_event).await {
            Ok(turn) => {
                sess.sent = messages.len();
                sess.turn_in_flight = false;
                match finish_persistent_turn(&self.binary, turn) {
                    Ok(resp) => Ok(resp),
                    Err(e) => {
                        if let Some(old) = guard.take() {
                            old.teardown().await;
                        }
                        Err(PersistentTurn::Failed(e))
                    }
                }
            }
            // Fresh process exited before any turn output AND ran no tool → a startup/auth failure
            // with nothing executed; one-shot reports those precisely, so fall back.
            Err(TurnError::Eof { tool_ran: false }) if first_turn => {
                if let Some(old) = guard.take() {
                    old.teardown().await;
                }
                Err(PersistentTurn::Fallback)
            }
            Err(e) => {
                let err = turn_error_to_provider(&self.binary, self.timeout, e);
                if let Some(old) = guard.take() {
                    old.teardown().await;
                }
                Err(PersistentTurn::Failed(err))
            }
        }
    }
}

/// Env var naming the out-of-band JSONL sink that `forge mcp-serve` writes subagent lifecycle
/// events to; the bridge sets it on the spawned CLI (inherited forge → claude → mcp-serve) and
/// tails it so bridge-spawned subagents surface in the TUI (RFC subagent-orchestration 3c).
pub const SUBAGENT_SINK_ENV: &str = "FORGE_SUBAGENT_SINK";

/// Parse one line of the subagent sink into a [`StreamEvent`]. Field-tolerant.
fn parse_sink_line(line: &str) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match v.get("k").and_then(Value::as_str)? {
        "start" => Some(StreamEvent::SubagentStarted {
            id: s("id"),
            agent: s("agent"),
            task: s("task"),
        }),
        "progress" => Some(StreamEvent::SubagentProgress {
            id: s("id"),
            snippet: s("snippet"),
        }),
        "done" => Some(StreamEvent::SubagentFinished {
            id: s("id"),
            agent: s("agent"),
            ok: v.get("ok").and_then(Value::as_bool).unwrap_or(true),
            summary: s("summary"),
            cost_usd: v.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
        }),
        "tasks" => {
            let tasks = serde_json::from_value(v.get("tasks")?.clone()).ok()?;
            Some(StreamEvent::Tasks(tasks))
        }
        "plan" => {
            let plan = serde_json::from_value(v.get("plan")?.clone()).ok()?;
            Some(StreamEvent::Plan(plan))
        }
        _ => None,
    }
}

/// Tail the subagent sink file, forwarding each event over `tx` as it is appended. Runs until
/// aborted by the caller (after the CLI process exits). Tolerant of the file not existing yet.
async fn tail_subagent_sink(
    path: std::path::PathBuf,
    tx: tokio::sync::mpsc::UnboundedSender<StreamEvent>,
) {
    use tokio::io::AsyncBufReadExt;
    // Wait for the file to appear (mcp-serve creates/opens it on first write).
    let file = loop {
        match tokio::fs::File::open(&path).await {
            Ok(f) => break f,
            Err(_) => tokio::time::sleep(Duration::from_millis(40)).await,
        }
    };
    let mut reader = tokio::io::BufReader::new(file);
    let mut buf = String::new();
    loop {
        match reader.read_line(&mut buf).await {
            Ok(0) => tokio::time::sleep(Duration::from_millis(40)).await, // EOF: await more
            Ok(_) if !buf.ends_with('\n') => {
                // Torn read: the reader caught up to the file's current EOF mid-line (no trailing
                // newline yet). Keep the partial bytes buffered — do NOT parse or clear — so the
                // next read_line appends the rest of this same line instead of losing/mis-parsing it.
                continue;
            }
            Ok(_) => {
                if let Some(ev) = parse_sink_line(buf.trim()) {
                    if tx.send(ev).is_err() {
                        break;
                    }
                }
                buf.clear();
            }
            Err(_) => break,
        }
    }
}

/// Format a bridge's captured stderr as a trailing ` — stderr: …` clause for an error message
/// (empty when there's nothing). Trimmed and tail-capped so a noisy CLI can't bloat the error.
fn stderr_suffix(stderr: &str) -> String {
    let t = stderr.trim();
    if t.is_empty() {
        return String::new();
    }
    const TAIL: usize = 600;
    let tail = if t.len() > TAIL {
        let start = t.len() - TAIL;
        // Snap to a char boundary so slicing can't panic on multibyte output.
        let start = (start..t.len())
            .find(|i| t.is_char_boundary(*i))
            .unwrap_or(t.len());
        format!("…{}", &t[start..])
    } else {
        t.to_string()
    };
    format!(" — stderr: {tail}")
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
        // A `.cmd`/`.bat` shim (how npm installs `claude`/`codex`) is launched via `cmd /S /C`
        // (see `bridge_command`), so the direct child here is `cmd.exe`, which spawns the real CLI
        // (often `node.exe`) as its OWN child. Windows does not kill descendants when a parent is
        // terminated, and there is no process group on this platform (`put_in_own_process_group` is
        // a no-op here) — so `child.start_kill()` alone would leave the real CLI process running
        // after Forge reports the turn "killed (stalled)". `taskkill /T` kills the whole process
        // tree rooted at this PID; it's a no-op if the child was launched directly (a real `.exe`,
        // no descendants) or has already exited.
        if let Some(id) = child.id() {
            let _ = Command::new("taskkill")
                .args(["/PID", &id.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await;
        }
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_on_path_detects_present_and_absent() {
        // A ubiquitous binary resolves; a nonsense one does not. (PATH-based, no spawning.)
        // On Windows `cmd` resolves only because the resolver tries the `.exe` suffix.
        let real = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(resolve_on_path(real).is_some(), "{real} should be on PATH");
        assert!(resolve_on_path("forge-definitely-not-a-real-binary-zzz").is_none());
    }

    #[test]
    fn resolve_on_path_resolves_an_explicit_file() {
        let dir = std::env::temp_dir().join(format!("forge-resolve-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("marker.txt");
        std::fs::write(&f, b"x").unwrap();
        assert_eq!(
            resolve_on_path(f.to_str().unwrap()).as_deref(),
            Some(f.as_path())
        );
        assert!(resolve_on_path(dir.join("nope.txt").to_str().unwrap()).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn bridge_command_wraps_cmd_shims_but_not_exes() {
        let dir = std::env::temp_dir().join(format!("forge-bridge-cmd-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let shim = dir.join("fakeclaude.cmd");
        std::fs::write(&shim, b"@echo off\n").unwrap();
        // A `.cmd` shim is launched through `cmd /C`, not directly.
        let cmd = bridge_command(shim.to_str().unwrap(), &["-p".into()]);
        assert_eq!(cmd.as_std().get_program(), std::ffi::OsStr::new("cmd"));
        // A non-script path launches directly.
        let exe = dir.join("faketool.exe");
        std::fs::write(&exe, b"MZ").unwrap();
        let direct = bridge_command(exe.to_str().unwrap(), &["-p".into()]);
        assert_eq!(direct.as_std().get_program(), exe.as_os_str());
    }

    #[test]
    fn windows_cmd_line_quotes_path_and_every_arg_for_cmd() {
        // A shim path AND an argument both containing spaces: each token must stay quoted, with an
        // outer pair `/S` strips. Without this, `cmd` strips the path's quotes and the launch breaks
        // on any Windows profile whose name has a space.
        let p = std::path::Path::new(r"C:\Users\First Last\npm\claude.cmd");
        let args = vec![
            "-p".to_string(),
            "--mcp-config".to_string(),
            r"C:\Users\First Last\cfg.json".to_string(),
        ];
        let line = windows_cmd_line(p, &args);
        assert_eq!(
            line,
            r#"""C:\Users\First Last\npm\claude.cmd" "-p" "--mcp-config" "C:\Users\First Last\cfg.json"""#
        );
        // Outer pair present (the one `/S` consumes), and each space-bearing token is individually
        // quoted so cmd keeps it intact.
        assert!(line.starts_with("\"\"") && line.ends_with("\"\""));
    }

    #[test]
    fn default_model_id_is_the_bare_prefix() {
        assert_eq!(CliKind::ClaudeCode.default_model_id(), "claude-cli::");
        assert_eq!(CliKind::Codex.default_model_id(), "codex-cli::");
    }

    #[test]
    fn default_models_span_tiers_and_namespace_under_the_prefix() {
        // Each bridge exposes more than one model so the mesh can size a turn (haiku/mini ↔ opus).
        let claude = CliKind::ClaudeCode.default_models();
        assert!(
            claude.contains(&"opus") && claude.contains(&"haiku"),
            "{claude:?}"
        );
        let codex = CliKind::Codex.default_models();
        assert!(
            codex.contains(&"gpt-5.5") && codex.contains(&"gpt-5.4-mini"),
            "{codex:?}"
        );
        // A model alias namespaces into a full Forge id under the bridge prefix.
        assert_eq!(
            format!("{}::{}", CliKind::ClaudeCode.prefix(), claude[0]),
            "claude-cli::opus"
        );
    }

    #[test]
    fn sink_lines_parse_into_subagent_events() {
        match parse_sink_line(r#"{"k":"start","id":"x1","agent":"reviewer","task":"review"}"#) {
            Some(StreamEvent::SubagentStarted { id, agent, task }) => {
                assert_eq!(
                    (id.as_str(), agent.as_str(), task.as_str()),
                    ("x1", "reviewer", "review")
                );
            }
            other => panic!("expected SubagentStarted, got {other:?}"),
        }
        assert_eq!(
            parse_sink_line(r#"{"k":"progress","id":"x1","snippet":"reading"}"#),
            Some(StreamEvent::SubagentProgress {
                id: "x1".into(),
                snippet: "reading".into()
            })
        );
        match parse_sink_line(
            r#"{"k":"done","id":"x1","agent":"reviewer","ok":true,"summary":"2 issues","cost":0.01}"#,
        ) {
            Some(StreamEvent::SubagentFinished {
                ok,
                cost_usd,
                summary,
                ..
            }) => {
                assert!(ok && (cost_usd - 0.01).abs() < 1e-9 && summary == "2 issues");
            }
            other => panic!("expected SubagentFinished, got {other:?}"),
        }
        assert!(parse_sink_line("not json").is_none());
        assert!(parse_sink_line(r#"{"k":"unknown"}"#).is_none());

        // A bridged `update_tasks` reported over the sink → a live Tasks event for the TUI panel.
        match parse_sink_line(
            r#"{"k":"tasks","tasks":[{"title":"scan","status":"in_progress"},{"title":"map","status":"pending"}]}"#,
        ) {
            Some(StreamEvent::Tasks(t)) => {
                assert_eq!(t.len(), 2);
                assert_eq!(t[0].title, "scan");
                assert_eq!(t[0].status, forge_types::TodoStatus::InProgress);
            }
            other => panic!("expected Tasks, got {other:?}"),
        }
    }

    #[test]
    fn bare_model_strips_cli_prefix() {
        assert_eq!(bare_model("claude-cli::sonnet"), "sonnet");
        assert_eq!(bare_model("codex-cli::gpt-5-codex"), "gpt-5-codex");
        assert_eq!(bare_model("claude-cli"), "");
    }

    #[test]
    fn claude_harness_args_route_tools_through_forge_mcp() {
        let args = build_args(CliKind::ClaudeCode, "sonnet", true, "/bin/forge", &[], None);
        // Forge owns the tools: strict MCP + only mcp__forge tools.
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        assert!(args.contains(&"mcp__forge".to_string()));
        // The prompt is fed via stdin, never as an argv entry (ARG_MAX fix): -p has no value.
        assert!(args.contains(&"-p".to_string()));
        let p = args.iter().position(|a| a == "-p").unwrap();
        assert_eq!(
            args[p + 1],
            "--output-format",
            "no prompt positional after -p"
        );
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
    fn resume_id_adds_resume_flag_for_claude_only() {
        // claude: a session id appends `--resume <id>`.
        let args = build_args(
            CliKind::ClaudeCode,
            "sonnet",
            true,
            "/bin/forge",
            &[],
            Some("sess-123"),
        );
        let r = args
            .iter()
            .position(|a| a == "--resume")
            .expect("--resume present");
        assert_eq!(args[r + 1], "sess-123");
        // codex has no `--resume` flag (it uses an `exec resume` subcommand), so it's never added.
        let codex = build_args(
            CliKind::Codex,
            "",
            true,
            "/bin/forge",
            &[],
            Some("sess-123"),
        );
        // codex has no `--resume` FLAG; it resumes via the `exec resume <id>` subcommand and must
        // DROP `--sandbox` (rejected on resume). Verify the rewrite.
        assert!(!codex.iter().any(|a| a == "--resume"));
        assert_eq!(
            &codex[0..3],
            &["exec", "resume", "sess-123"],
            "exec resume <id>"
        );
        assert!(
            !codex.iter().any(|a| a == "--sandbox"),
            "--sandbox dropped on resume"
        );
        assert!(
            codex.iter().any(|a| a == "--json"),
            "other codex flags kept"
        );
        // A fresh codex turn keeps `exec` + `--sandbox` and has no resume subcommand.
        let codex_fresh = build_args(CliKind::Codex, "", true, "/bin/forge", &[], None);
        assert_eq!(codex_fresh[0], "exec");
        assert!(codex_fresh.iter().any(|a| a == "--sandbox"));
        assert!(!codex_fresh.iter().any(|a| a == "resume"));
        // No resume id → no claude flag either.
        let fresh = build_args(CliKind::ClaudeCode, "sonnet", true, "/bin/forge", &[], None);
        assert!(!fresh.iter().any(|a| a == "--resume"));
    }

    #[test]
    fn render_resume_delta_sends_only_new_user_and_system_messages() {
        use forge_types::Message;
        // A resumed session already holds the model's prior turn + tool results, so the delta is just
        // the NEW user/system instruction(s); Assistant + Tool messages are skipped.
        let tail = vec![
            Message::assistant("I finished step 1."),
            Message::tool_result("call-1", "ok".to_string()),
            Message::system("The plan is NOT finished — continue."),
            Message::user("also handle the edge case"),
        ];
        let delta = render_resume_delta(&tail);
        assert!(delta.contains("The plan is NOT finished"));
        assert!(delta.contains("User: also handle the edge case"));
        assert!(
            !delta.contains("I finished step 1"),
            "assistant turn is not re-sent"
        );
        assert!(!delta.contains("ok"), "tool result is not re-sent");
        // Nothing actionable → empty (caller falls back to a fresh full render).
        assert!(render_resume_delta(&[Message::assistant("x")]).is_empty());
    }

    #[test]
    fn claude_session_id_extracts_the_field() {
        assert_eq!(
            claude_session_id(r#"{"type":"system","session_id":"abc-123","x":1}"#),
            Some("abc-123".to_string())
        );
        assert_eq!(claude_session_id(r#"{"type":"assistant"}"#), None);
        assert_eq!(claude_session_id("not json"), None);
        assert_eq!(claude_session_id(r#"{"session_id":""}"#), None);
    }

    #[tokio::test]
    #[ignore = "spawns the real `claude` CLI (needs install + auth + network); run with --ignored"]
    async fn e2e_claude_resume_preserves_context_across_calls() {
        use forge_types::Message;
        // Pinned to one-shot: this test validates `--resume` specifically, which the persistent
        // transport bypasses (it holds context in-process instead).
        let provider = CliProvider::claude_code()
            .with_harness(false)
            .with_persistent(false);
        let model = "claude-cli::haiku";
        let mut sink = |_e: StreamEvent| {};

        // Turn 1: establish a fact; the provider captures claude's session id.
        let msgs1 = vec![Message::user(
            "Remember this codeword: BANANA. Just acknowledge.",
        )];
        let r1 = provider
            .complete(model, &msgs1, &[], &mut sink)
            .await
            .expect("turn 1 should succeed");
        assert!(!r1.content.trim().is_empty());

        // Grow the transcript the way Forge does (assistant reply + a NEW user turn). Turn 2 RESUMES:
        // only the new user message is sent, yet claude must still recall the codeword from turn 1.
        let msgs2 = vec![
            msgs1[0].clone(),
            Message::assistant(&r1.content),
            Message::user("What was the codeword? Reply with just the word."),
        ];
        let r2 = provider
            .complete(model, &msgs2, &[], &mut sink)
            .await
            .expect("turn 2 should succeed");
        assert!(
            r2.content.to_uppercase().contains("BANANA"),
            "the resumed session must recall the codeword from turn 1; got: {}",
            r2.content
        );
    }

    #[tokio::test]
    #[ignore = "spawns the real `codex` CLI (needs install + auth + network); run with --ignored"]
    async fn e2e_codex_resume_preserves_context_across_calls() {
        use forge_types::Message;
        // Text mode (no MCP needed); explicit model so the recorded + resumed model match (codex
        // warns on a model change, which our model-match gate also prevents in the harness).
        let provider = CliProvider::codex().with_harness(false);
        let model = "codex-cli::gpt-5.5";
        let mut sink = |_e: StreamEvent| {};

        let msgs1 = vec![Message::user(
            "Remember this codeword: ORANGUTAN. Acknowledge briefly.",
        )];
        let r1 = provider
            .complete(model, &msgs1, &[], &mut sink)
            .await
            .expect("codex turn 1 should succeed");
        assert!(!r1.content.trim().is_empty());

        // Turn 2 resumes via `codex exec resume <thread_id>`, sending only the new user message.
        let msgs2 = vec![
            msgs1[0].clone(),
            Message::assistant(&r1.content),
            Message::user("What was the codeword? Reply with just the word."),
        ];
        let r2 = provider
            .complete(model, &msgs2, &[], &mut sink)
            .await
            .expect("codex turn 2 should succeed");
        assert!(
            r2.content.to_uppercase().contains("ORANGUTAN"),
            "the resumed codex session must recall the codeword; got: {}",
            r2.content
        );
    }

    #[test]
    fn resume_sends_dramatically_fewer_prompt_bytes_over_a_turn() {
        use forge_types::Message;
        // The efficiency win is in what FORGE writes to the subprocess stdin each call: resume OFF
        // re-renders the WHOLE (growing) transcript every turn; resume ON sends just the new delta.
        // (claude's own token accounting hides this because it prompt-caches the repeat — the saving
        // is the prompt Forge has to serialize + stream, which also shrinks claude's work.) This is
        // deterministic — no live CLI — so it documents the win as a reproducible number.
        //
        // Simulate a realistic harness turn: a big system preamble + several assistant turns + tool
        // results accumulate, and the bridge re-drives a few times.
        let big_system = "S".repeat(4000); // lattice + preamble + project context, etc.
        let mut messages = vec![
            Message::system(big_system),
            Message::user("Do the multi-step task."),
        ];

        let mut off_bytes = 0usize; // full transcript re-rendered every call
        let mut on_bytes = 0usize; // fresh full render once, then deltas
        let mut sent = 0usize; // resume high-water mark
        for turn in 0..6 {
            // Each call: OFF always re-renders everything; ON renders the delta (after the 1st call).
            off_bytes += render_prompt(&messages).len();
            if turn == 0 {
                on_bytes += render_prompt(&messages).len();
            } else {
                on_bytes += render_resume_delta(&messages[sent..]).len();
            }
            sent = messages.len();
            // The bridge produced a (chunky) assistant turn + a tool result; then a re-drive nudge.
            messages.push(Message::assistant("A".repeat(1500)));
            messages.push(Message::tool_result("t", "T".repeat(800)));
            messages.push(Message::system("The plan is NOT finished — continue."));
        }
        let pct = (off_bytes - on_bytes) * 100 / off_bytes;
        // Document the measured win (visible with --nocapture); assert it's a large reduction.
        println!(
            "bridge resume prompt bytes over 6 re-drives: OFF={off_bytes} ON={on_bytes} ({pct}% fewer)"
        );
        assert!(
            on_bytes * 4 < off_bytes,
            "resume should cut prompt bytes by well over half (OFF={off_bytes}, ON={on_bytes})"
        );
    }

    #[test]
    fn only_claude_resumes_and_it_is_toggleable() {
        assert!(
            CliProvider::claude_code().resumes(),
            "claude resumes by default"
        );
        assert!(CliProvider::codex().resumes(), "codex resumes too");
        assert!(
            !CliProvider::antigravity().resumes(),
            "agy has no resume mechanism"
        );
        assert!(
            !CliProvider::codex().with_session_resume(false).resumes(),
            "the escape hatch disables it"
        );
    }

    #[test]
    fn claude_text_mode_runs_a_self_agent_with_accept_edits() {
        let args = build_args(
            CliKind::ClaudeCode,
            "sonnet",
            false,
            "/bin/forge",
            &[],
            None,
        );
        assert!(!args.iter().any(|a| a == "--strict-mcp-config"));
        let i = args.iter().position(|a| a == "--permission-mode").unwrap();
        assert_eq!(args[i + 1], "acceptEdits");
        assert!(args.contains(&"--model".to_string()) && args.contains(&"sonnet".to_string()));
    }

    #[test]
    fn antigravity_args_are_text_mode_print_with_model() {
        // agy has no MCP/--tools, so it's text mode regardless of the harness flag.
        for harness in [true, false] {
            let args = build_args(
                CliKind::Antigravity,
                "gemini-3.5-flash",
                harness,
                "/bin/forge",
                &[],
                None,
            );
            assert!(
                args.contains(&"-p".to_string()),
                "non-interactive print mode"
            );
            assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
            // Never wires Forge's MCP server (agy can't host it).
            assert!(!args.iter().any(|a| a.contains("mcp")));
            assert!(!args.iter().any(|a| a == "--tools"));
            let i = args.iter().position(|a| a == "--model").unwrap();
            assert_eq!(args[i + 1], "gemini-3.5-flash");
        }
    }

    #[test]
    fn antigravity_parse_line_treats_plaintext_as_answer_text() {
        assert_eq!(parse_antigravity_line("   "), Vec::<Parsed>::new());
        assert_eq!(
            parse_antigravity_line("hello world"),
            vec![Parsed::Text("hello world\n".to_string())]
        );
    }

    #[test]
    fn codex_harness_args_wire_forge_mcp_and_approve_tools() {
        let args = build_args(CliKind::Codex, "", true, "/bin/forge", &[], None);
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        // Sandbox stays read-only: codex's OWN shell can't write, so every write/side-effect
        // can only go through Forge's gated mcp__forge tools.
        assert!(args.contains(&"read-only".to_string()));
        // NEVER bypass the sandbox/approvals — that would unsandbox codex's own shell.
        assert!(!args.iter().any(|a| a.contains("dangerously-bypass")));
        // Forge MCP server wired via -c overrides, and its tools auto-approved ("approve",
        // not "auto" — verified live against codex 0.130: "auto" still cancels).
        let joined = args.join(" ");
        assert!(joined.contains("mcp_servers.forge.command=\"/bin/forge\""));
        assert!(joined.contains("mcp_servers.forge.args=[\"mcp-serve\"]"));
        assert!(joined.contains("mcp_servers.forge.default_tools_approval_mode=\"approve\""));
        // approval_policy never so codex doesn't block on stdin prompts headless.
        assert!(joined.contains("approval_policy=\"never\""));
        // reasoning summaries on so codex emits reasoning items (streamed thinking).
        assert!(joined.contains("model_reasoning_summary"));
        // Isolate codex from the user's ~/.codex/config.toml so its only tool surface is
        // Forge's MCP server (no personal brave-search / extra MCP servers escaping the gate).
        assert!(args.contains(&"--ignore-user-config".to_string()));
        // Native web search must stay off — Forge's web_search is the only search path.
        assert!(!args.contains(&"--search".to_string()));
        // The prompt is fed via stdin, never as an argv positional (ARG_MAX fix).
        assert!(!args.iter().any(|a| a == "do a thing"));
        assert!(!args.contains(&"--model".to_string()));
    }

    #[test]
    fn bridge_mcp_env_uses_explicit_checkpoint_context_not_process_env() {
        // The checkpoint context handed to the `forge mcp-serve` child must come from the EXPLICIT
        // `CheckpointContext` the parent threaded through — not from a process-global `set_var`.
        let ctx = CheckpointContext {
            session: "explicit-sess".to_string(),
            seq: 42,
            root: "/abs/checkpoints".to_string(),
            mode: "accept-edits".to_string(),
        };
        let sink = std::path::PathBuf::from("/tmp/sink.jsonl");
        let env = bridge_mcp_env(Some(sink.as_path()), Some(&ctx));

        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("FORGE_CHECKPOINT_SESSION"), Some("explicit-sess"));
        assert_eq!(get("FORGE_CHECKPOINT_SEQ"), Some("42"));
        assert_eq!(get("FORGE_CHECKPOINT_ROOT"), Some("/abs/checkpoints"));
        assert_eq!(get("FORGE_PERMISSION_MODE"), Some("accept-edits"));
        assert_eq!(get("FORGE_SUBAGENT_SINK"), Some("/tmp/sink.jsonl"));

        // The function only READS env (for the legacy/depth fallback) — it must never WRITE it, so a
        // value present after the call cannot have been published by this code path.
        assert!(
            std::env::var("FORGE_CHECKPOINT_SESSION").ok().as_deref() != Some("explicit-sess"),
            "explicit context is not leaked into the process env"
        );
    }

    #[test]
    fn harness_forwards_sink_and_checkpoint_env_to_the_mcp_server() {
        // The out-of-band env `forge mcp-serve` needs (sink + checkpoint context). codex curates
        // its MCP server env to PATH/HOME/… and drops everything else (verified live), so these
        // must be injected explicitly — otherwise update_tasks/present_plan write to a dead sink
        // and never reach the parent TUI, and bridge edits aren't snapshotted for /undo.
        let env = vec![
            (
                "FORGE_SUBAGENT_SINK".to_string(),
                "/tmp/s.jsonl".to_string(),
            ),
            ("FORGE_CHECKPOINT_SESSION".to_string(), "sess-1".to_string()),
        ];
        // codex: nested TOML overrides.
        let codex = build_args(CliKind::Codex, "", true, "/bin/forge", &env, None).join(" ");
        assert!(codex.contains("mcp_servers.forge.env.FORGE_SUBAGENT_SINK=\"/tmp/s.jsonl\""));
        assert!(codex.contains("mcp_servers.forge.env.FORGE_CHECKPOINT_SESSION=\"sess-1\""));
        // claude: carried in the --mcp-config JSON `env` object.
        let claude = build_args(
            CliKind::ClaudeCode,
            "sonnet",
            true,
            "/bin/forge",
            &env,
            None,
        );
        let mc = claude.iter().position(|a| a == "--mcp-config").unwrap();
        let cfg = &claude[mc + 1];
        assert!(cfg.contains("\"FORGE_SUBAGENT_SINK\":\"/tmp/s.jsonl\""));
        assert!(cfg.contains("\"FORGE_CHECKPOINT_SESSION\":\"sess-1\""));
        // Text-mode (no Forge MCP server) ignores the env — no overrides leak in.
        let text = build_args(CliKind::Codex, "", false, "/bin/forge", &env, None).join(" ");
        assert!(!text.contains("mcp_servers.forge.env"));
    }

    #[test]
    fn harness_preamble_nudges_to_forge_tools_only_in_harness_mode() {
        let out = apply_harness_preamble(true, "User: search the web".into());
        assert!(out.contains("mcp__forge__web_search"));
        assert!(out.contains("Do NOT use any") || out.contains("MUST use"));
        // Skills steering: point the bridged model at Forge's use_skill, not the filesystem.
        assert!(out.contains("mcp__forge__use_skill"));
        assert!(out.contains("~/.claude"));
        // Tool-name mapping: bare names (update_tasks/spawn_agents) → their mcp__forge__ form.
        assert!(out.contains("mcp__forge__update_tasks"));
        assert!(out.contains("mcp__forge__spawn_agents"));
        assert!(out.ends_with("User: search the web"));
        // Phase-1 self-agent turns are untouched.
        assert_eq!(apply_harness_preamble(false, "User: hi".into()), "User: hi");
    }

    #[test]
    fn codex_text_mode_is_a_plain_read_only_agent() {
        let args = build_args(CliKind::Codex, "", false, "/bin/forge", &[], None);
        assert_eq!(args[0], "exec");
        assert!(args.contains(&"--json".to_string()));
        assert!(args.contains(&"read-only".to_string()));
        // text mode does NOT wire the Forge MCP server.
        assert!(!args.join(" ").contains("mcp_servers.forge"));
        // The prompt is fed via stdin, never as an argv positional (ARG_MAX fix).
        assert!(!args.iter().any(|a| a == "do a thing"));
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

    #[test]
    fn clamp_leaves_a_fitting_prompt_untouched() {
        let p = "short prompt";
        assert_eq!(clamp_to_chars(p, 1_000), p);
    }

    #[test]
    fn clamp_trims_an_oversized_prompt_keeping_head_and_tail() {
        // HEAD…(middle)…TAIL where the middle is the bulk.
        let prompt = format!("HEAD-INSTRUCTIONS{}TAIL-LATEST-REQUEST", "x".repeat(50_000));
        let out = clamp_to_chars(&prompt, 1_000);
        assert!(out.chars().count() <= 1_000, "stays within the cap");
        assert!(out.starts_with("HEAD"), "keeps the head: {}", &out[..20]);
        assert!(out.ends_with("TAIL-LATEST-REQUEST"), "keeps the tail");
        assert!(out.contains("truncated"), "marks the cut");
    }

    #[test]
    fn codex_caps_input_claude_does_not() {
        assert_eq!(CliKind::Codex.max_input_chars(), Some(1_048_576));
        assert_eq!(CliKind::ClaudeCode.max_input_chars(), None);
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
            cached_input_tokens: 0,
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
    fn claude_rate_limit_event_parses_into_a_quota_hint() {
        use forge_types::QuotaStatus;
        // The documented shape: status "allowed", overage not in use → Ok.
        let ok = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1781485800,"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":false}}"#;
        match &parse_claude_line(ok)[0] {
            Parsed::Quota {
                window,
                status,
                resets_at,
                ..
            } => {
                assert_eq!(window, "five_hour");
                assert_eq!(*status, QuotaStatus::Ok);
                assert_eq!(*resets_at, Some(1781485800));
            }
            other => panic!("expected Quota, got {other:?}"),
        }
        // Using overage → Warning (near/over the included limit).
        let warn = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","rateLimitType":"weekly","isUsingOverage":true}}"#;
        assert!(matches!(
            &parse_claude_line(warn)[0],
            Parsed::Quota {
                status: QuotaStatus::Warning,
                ..
            }
        ));
        // A high used-fraction → Exhausted.
        let full = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","usedFraction":0.99}}"#;
        assert!(matches!(
            &parse_claude_line(full)[0],
            Parsed::Quota {
                status: QuotaStatus::Exhausted,
                ..
            }
        ));
        // The REAL live shape (verified against `claude --output-format stream-json`): the field is
        // `utilization`, and the weekly window is `seven_day`. Both must be parsed.
        let real = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1781942400,"rateLimitType":"seven_day","utilization":0.81,"isUsingOverage":false}}"#;
        match &parse_claude_line(real)[0] {
            Parsed::Quota {
                window,
                status,
                fraction,
                ..
            } => {
                assert_eq!(window, "weekly", "seven_day → weekly");
                assert_eq!(*fraction, Some(0.81), "utilization parsed");
                assert_eq!(*status, QuotaStatus::Warning, "0.81 → Warning");
            }
            other => panic!("expected Quota, got {other:?}"),
        }
    }

    #[test]
    fn codex_thread_started_is_captured() {
        let line =
            r#"{"type":"thread.started","thread_id":"019eccdc-9390-72d2-b798-5134cceb95fe"}"#;
        assert_eq!(
            parse_codex_line(line),
            vec![Parsed::Thread(
                "019eccdc-9390-72d2-b798-5134cceb95fe".into()
            )]
        );
    }

    #[test]
    fn codex_rollout_quota_reads_both_windows() {
        use forge_types::QuotaStatus;
        // Real shape captured from ~/.codex/sessions/.../rollout-*.jsonl.
        // Primary (5h) 85%, secondary (weekly) 4%. Both windows returned.
        let jsonl = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"limit_id":"codex","primary":{"used_percent":85.0,"window_minutes":300,"resets_at":9999999999},"secondary":{"used_percent":4.0,"window_minutes":10080,"resets_at":9999999999},"plan_type":"plus","rate_limit_reached_type":null}}}"#;
        let hints = codex_quota_from_rollout(jsonl, "codex-cli");
        assert_eq!(hints.len(), 2, "both windows expected");
        let five_h = hints
            .iter()
            .find(|h| h.window == "five_hour")
            .expect("five_hour");
        assert_eq!(five_h.provider, "codex-cli");
        assert_eq!(five_h.status, QuotaStatus::Warning); // 85% >= 80%
        assert_eq!(five_h.resets_at, Some(9999999999));
        assert!((five_h.fraction_used.unwrap() - 0.85).abs() < 1e-9);
        let weekly = hints.iter().find(|h| h.window == "weekly").expect("weekly");
        assert_eq!(weekly.status, QuotaStatus::Ok); // 4%
        assert!((weekly.fraction_used.unwrap() - 0.04).abs() < 1e-9);
    }

    #[test]
    fn codex_rollout_quota_exhausted_when_a_limit_was_reached() {
        use forge_types::QuotaStatus;
        let jsonl = r#"{"payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":12.0,"window_minutes":300,"resets_at":9999999999},"rate_limit_reached_type":"primary"}}}"#;
        let hints = codex_quota_from_rollout(jsonl, "codex-cli");
        let five_h = hints
            .iter()
            .find(|h| h.window == "five_hour")
            .expect("five_hour");
        assert_eq!(five_h.status, QuotaStatus::Exhausted);
    }

    #[test]
    fn codex_rollout_quota_ok_for_low_usage() {
        use forge_types::QuotaStatus;
        let jsonl = "{\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"primary\":{\"used_percent\":50.0,\"window_minutes\":300,\"resets_at\":9999999999}}}}\n{\"timestamp\":\"x\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":null,\"rate_limits\":{\"limit_id\":\"codex\",\"primary\":{\"used_percent\":1.0,\"window_minutes\":300,\"resets_at\":9999999999},\"secondary\":{\"used_percent\":1.0,\"window_minutes\":10080,\"resets_at\":9999999999},\"plan_type\":\"plus\",\"rate_limit_reached_type\":null}}}";
        let hints = codex_quota_from_rollout(jsonl, "codex-cli");
        assert_eq!(hints.len(), 2, "both windows; last snapshot wins");
        for h in &hints {
            assert_eq!(h.status, QuotaStatus::Ok);
        }
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
    fn codex_mcp_tool_call_round_trips_started_and_finished() {
        let started = r#"{"type":"item.started","item":{"id":"item_2","type":"mcp_tool_call","server":"forge","tool":"read_file","arguments":{"path":"Cargo.toml"},"status":"in_progress"}}"#;
        match &parse_codex_line(started)[0] {
            Parsed::ToolStarted { id, name, args } => {
                assert_eq!(id, "item_2");
                assert_eq!(name, "read_file");
                assert!(args.contains("Cargo.toml"));
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        let done = r#"{"type":"item.completed","item":{"id":"item_2","type":"mcp_tool_call","server":"forge","tool":"read_file","status":"completed"}}"#;
        assert_eq!(
            parse_codex_line(done),
            vec![Parsed::ToolFinished {
                id: "item_2".into(),
                ok: true,
                summary: "read_file".into()
            }]
        );
        let failed = r#"{"type":"item.completed","item":{"id":"item_3","type":"mcp_tool_call","server":"forge","tool":"write_file","status":"failed","error":{"message":"denied by Forge permission policy: write_file"}}}"#;
        assert_eq!(
            parse_codex_line(failed),
            vec![Parsed::ToolFinished {
                id: "item_3".into(),
                ok: false,
                summary: "denied by Forge permission policy: write_file".into()
            }]
        );
    }

    #[test]
    fn codex_command_execution_round_trips_as_shell_tool() {
        let started = r#"{"type":"item.started","item":{"id":"item_7","type":"command_execution","command":"rg foo","status":"in_progress"}}"#;
        match &parse_codex_line(started)[0] {
            Parsed::ToolStarted { name, args, .. } => {
                assert_eq!(name, "shell");
                assert!(args.contains("rg foo"));
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        let done = r#"{"type":"item.completed","item":{"id":"item_7","type":"command_execution","command":"rg foo","exit_code":0,"status":"completed"}}"#;
        assert_eq!(
            parse_codex_line(done),
            vec![Parsed::ToolFinished {
                id: "item_7".into(),
                ok: true,
                summary: "rg foo".into()
            }]
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
                cached_input_tokens: 0,
                cost_usd: 0.0
            })]
        );
    }

    #[test]
    fn codex_lifecycle_lines_are_ignored() {
        // `thread.started` is now captured for the quota lookup (see codex_thread_started_*).
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
        let ProviderError::Request(msg) = err else {
            panic!("expected Request, got {err:?}");
        };
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
    async fn recovers_prose_tool_call_the_bridge_did_not_execute() {
        // Regression for the 553x spiral: the model wrote a tool call as TEXT (Anthropic
        // `<function_calls><invoke>` markup) instead of a native tool_use, so the CLI never ran it
        // and it landed in the final text. complete() must recover it into structured tool_calls so
        // the run-loop executes it and re-drives — not leak it as prose the model repeats forever.
        let fake = make_fake_cli(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me read them.\n<function_calls>\n<invoke name=\"mcp__forge__read_file\">\n<parameter name=\"paths\">[\"a.py\",\"b.py\"]</parameter>\n</invoke>\n</function_calls>"}]}}
{"type":"result","is_error":false,"result":"","usage":{"input_tokens":5,"output_tokens":3}}"#,
        );
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut on_event = |_: StreamEvent| {};
        let res = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect("fake stream parses");

        assert_eq!(
            res.tool_calls.len(),
            1,
            "the prose tool call must be recovered"
        );
        assert_eq!(
            res.tool_calls[0].name, "read_file",
            "mcp__forge__ prefix normalized"
        );
        assert_eq!(res.tool_calls[0].args["paths"][0], "a.py");
        assert!(
            !res.content.contains("<function_calls>"),
            "recovered markup must be stripped from content: {:?}",
            res.content
        );
    }

    #[test]
    fn stream_user_line_is_a_valid_streaming_input_envelope() {
        let line = stream_user_line("hello \"world\"\nline2");
        let v: Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hello \"world\"\nline2");
        assert!(!line.contains('\n'), "the envelope itself is a single line");
    }

    #[test]
    fn classify_in_band_error_maps_to_retryable_variants() {
        assert!(matches!(
            classify_in_band_error("claude", "rate_limit_error: slow down"),
            ProviderError::RateLimited { .. }
        ));
        assert!(matches!(
            classify_in_band_error("claude", "Overloaded (529)"),
            ProviderError::Unavailable(_)
        ));
        assert!(matches!(
            classify_in_band_error("claude", "401 unauthorized"),
            ProviderError::Auth(_)
        ));
        assert!(matches!(
            classify_in_band_error("claude", "weird unmapped thing"),
            ProviderError::Request(_)
        ));
    }

    /// A fake `claude --input-format stream-json`: loops reading user lines from stdin and emits one
    /// assistant+result turn PER line, staying alive (exits only on stdin EOF). So a 2nd turn served
    /// by the SAME process answers "reply 2"; a fresh one-shot spawn would always answer "reply 1".
    #[cfg(unix)]
    fn make_fake_persistent_cli() -> String {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("forge-fake-live-{}-{n}", std::process::id()));
        let mut f = std::fs::File::create(&path).unwrap();
        // dash/bash `printf` is a builtin and writes unbuffered, so each turn's lines reach the
        // reader immediately (the process never exits to flush).
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "i=0").unwrap();
        writeln!(f, "while IFS= read -r line; do").unwrap();
        writeln!(f, "  i=$((i+1))").unwrap();
        writeln!(
            f,
            r#"  printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"reply %d"}}]}}}}\n' "$i""#
        )
        .unwrap();
        writeln!(
            f,
            r#"  printf '{{"type":"result","is_error":false,"result":"reply %d","usage":{{"input_tokens":5,"output_tokens":3}}}}\n' "$i""#
        )
        .unwrap();
        writeln!(f, "done").unwrap();
        f.sync_all().unwrap();
        drop(f);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        // Probe-exec past any transient ETXTBSY (a concurrent fork briefly holding the write fd).
        for _ in 0..200 {
            match std::process::Command::new(&path)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(mut c) => {
                    let _ = c.wait();
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        path.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_transport_reuses_one_process_across_turns() {
        // The decisive P1 test: one long-lived process serves BOTH turns. A fresh spawn per turn
        // (the one-shot transport, or a silent fallback) would answer "reply 1" both times.
        let fake = make_fake_persistent_cli();
        let provider = CliProvider::claude_code()
            .with_harness(false)
            .with_persistent(true)
            .with_binary(&fake)
            .with_timeout(Duration::from_secs(10)); // fail fast if a turn ever stalls
        let model = "claude-cli::sonnet";
        let mut sink = |_e: StreamEvent| {};

        let r1 = provider
            .complete(model, &[Message::user("one")], &[], &mut sink)
            .await
            .expect("persistent turn 1");
        assert_eq!(r1.content, "reply 1");

        // Grow the transcript (assistant reply + a NEW user turn) so turn 2 sends only the delta to
        // the SAME live process.
        let msgs2 = vec![
            Message::user("one"),
            Message::assistant(&r1.content),
            Message::user("two"),
        ];
        let r2 = provider
            .complete(model, &msgs2, &[], &mut sink)
            .await
            .expect("persistent turn 2");
        assert_eq!(
            r2.content, "reply 2",
            "the same persistent process must serve turn 2 (a fresh spawn would say 'reply 1')"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_falls_back_to_one_shot_when_binary_is_missing() {
        // Spawn failure happens BEFORE any turn output, so the persistent path yields `Fallback` and
        // complete() transparently runs the one-shot path — which surfaces the clean missing-binary
        // error rather than a persistent-specific hang.
        let provider = CliProvider::claude_code()
            .with_persistent(true)
            .with_binary("forge-no-such-cli-xyz");
        let mut sink = |_: StreamEvent| {};
        let err = provider
            .complete("claude-cli::sonnet", &[Message::user("hi")], &[], &mut sink)
            .await
            .expect_err("missing binary must still error");
        assert!(
            matches!(&err, ProviderError::Request(m) if m.contains("not found")),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    #[ignore = "spawns the real `claude` CLI in persistent mode (needs install + auth + network); run with --ignored"]
    async fn e2e_claude_persistent_preserves_context_across_calls() {
        use forge_types::Message;
        let provider = CliProvider::claude_code()
            .with_harness(false)
            .with_persistent(true);
        let model = "claude-cli::haiku";
        let mut sink = |_e: StreamEvent| {};

        let msgs1 = vec![Message::user(
            "Remember this codeword: BANANA. Just acknowledge.",
        )];
        let r1 = provider
            .complete(model, &msgs1, &[], &mut sink)
            .await
            .expect("persistent turn 1 should succeed");
        assert!(!r1.content.trim().is_empty());

        let msgs2 = vec![
            msgs1[0].clone(),
            Message::assistant(&r1.content),
            Message::user("What was the codeword? Reply with just the word."),
        ];
        let r2 = provider
            .complete(model, &msgs2, &[], &mut sink)
            .await
            .expect("persistent turn 2 should succeed");
        assert!(
            r2.content.to_uppercase().contains("BANANA"),
            "the long-lived session must recall the codeword from turn 1; got: {}",
            r2.content
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn truncated_stream_line_is_skipped_not_fatal() {
        // Stream resilience: a corrupt/truncated NDJSON line spliced between valid lines must be
        // skipped (parse fails open → empty), not abort the turn or panic. Text from the valid lines
        // still accumulates.
        let fake = make_fake_cli(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"before \"}]}}\n\
             {\"type\":\"assistant\",\"message\":{\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"after\"}]}}\n\
             {\"type\":\"result\",\"is_error\":false,\"result\":\"\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}",
        );
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut on_event = |_: StreamEvent| {};
        let res = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect("a truncated line must not fail the turn");
        assert!(res.content.contains("before"), "got: {:?}", res.content);
        assert!(res.content.contains("after"), "got: {:?}", res.content);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn orphan_tool_result_without_started_does_not_panic_or_phantom() {
        // A tool_result for an id with no preceding tool_use (stream corruption / missing start) must
        // not panic and must not synthesize a phantom tool call — its name just defaults empty.
        let fake = make_fake_cli(
            "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"ghost\",\"is_error\":false,\"content\":\"x\"}]}}\n\
             {\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n\
             {\"type\":\"result\",\"is_error\":false,\"result\":\"done\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}",
        );
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut on_event = |_: StreamEvent| {};
        let res = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect("orphan tool_result must not fail the turn");
        assert!(
            res.tool_calls.is_empty(),
            "no phantom call: {:?}",
            res.tool_calls
        );
        assert_eq!(res.content, "done");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn prose_recovery_skipped_when_cli_ran_a_native_tool() {
        // Double-execution guard: if the CLI executed a native tool this turn (a streamed tool_use),
        // a tool-call-shaped fragment in the final text must NOT be recovered — recovering it would
        // run the tool a SECOND time (Forge-side). Recovery is only for the pure prose-fallback case
        // where nothing ran. Here a native `shell` runs AND prose `<function=shell>` is in the text.
        let fake = make_fake_cli(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"shell","input":{"command":"ls"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false,"content":"ok"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"ran it: <function=shell>{\"command\":\"rm -rf /\"}</function>"}]}}
{"type":"result","is_error":false,"result":"","usage":{"input_tokens":5,"output_tokens":3}}"#,
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

        assert!(
            res.tool_calls.is_empty(),
            "must NOT recover prose after a native tool ran (double-exec risk): {:?}",
            res.tool_calls
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, StreamEvent::ToolStarted { name, .. } if name == "shell")),
            "the native shell tool_use should still have streamed as an event"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_band_rate_limit_is_retryable_for_failover() {
        // A CLI that hits its quota mid-turn emits an in-band rate-limit result. It must surface as a
        // RETRYABLE error so the mesh benches the model and fails over — not a hard `Request` that
        // ends the turn.
        let fake = make_fake_cli(
            r#"{"type":"result","is_error":true,"api_error_status":"rate_limit_error"}"#,
        );
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut on_event = |_: StreamEvent| {};
        let err = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect_err("in-band rate-limit → error");
        assert!(
            err.is_retryable(),
            "in-band rate-limit must be retryable (failover-eligible); got {err:?}"
        );
        assert!(
            matches!(err, ProviderError::RateLimited { .. }),
            "got {err:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_band_overloaded_is_retryable_for_failover() {
        // Claude emits `overloaded` under API load — transient, so it must be retryable (failover),
        // not a hard `Request` that ends the turn. (Earlier the classifier only caught rate/auth.)
        let fake =
            make_fake_cli(r#"{"type":"result","is_error":true,"api_error_status":"overloaded"}"#);
        let provider = CliProvider::claude_code().with_binary(&fake);
        let mut on_event = |_: StreamEvent| {};
        let err = provider
            .complete(
                "claude-cli::sonnet",
                &[Message::user("hi")],
                &[],
                &mut on_event,
            )
            .await
            .expect_err("in-band overloaded → error");
        assert!(
            err.is_retryable() && matches!(err, ProviderError::Unavailable(_)),
            "overloaded must be retryable Unavailable; got {err:?}"
        );
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
        let ProviderError::Request(msg) = err else {
            panic!("expected Request, got {err:?}");
        };
        assert!(msg.contains("authenticated"), "got: {msg}");
        // With empty stderr the setup hint is the only detail — it must appear exactly ONCE, not
        // duplicated verbatim (it used to print as both the `detail` and the parenthetical hint).
        let hint = CliKind::Codex.setup_hint();
        assert_eq!(
            msg.matches(hint).count(),
            1,
            "setup hint duplicated in error: {msg}"
        );
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

    /// Deterministic fuzz for `clamp_to_chars` — the function that trims an over-long prompt to
    /// `codex exec`'s `input_too_large` cap. It does raw char-index arithmetic on a `Vec<char>`
    /// (`chars[..head]`, `chars[total - tail..]`), the exact shape that has produced char-boundary
    /// panics before (v0.3.10). It carries a hard CONTRACT: the result must never EXCEED `max_chars`
    /// (codex rejects the turn otherwise) and must stay valid UTF-8. Throw random multi-byte/emoji/
    /// combining-char strings at random caps (including the degenerate 0/1/around-marker-length ones)
    /// via a seeded LCG and assert the contract holds and nothing panics on ALL of them.
    #[test]
    fn clamp_to_chars_never_panics_and_never_exceeds_cap() {
        const CHARS: &[char] = &[
            'a', ' ', '\n', '你', '😀', 'é', '\u{0301}', 'A', '\t', '界', '\u{fffd}',
        ];
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..6000 {
            let len = next() % 400;
            let s: String = (0..len).map(|_| CHARS[next() % CHARS.len()]).collect();
            // Bias caps toward the small/degenerate region where boundary bugs live.
            let cap = match next() % 5 {
                0 => 0,
                1 => 1,
                2 => next() % 70, // around the truncation-marker length
                3 => next() % 200,
                _ => next() % 800,
            };
            let out = clamp_to_chars(&s, cap);
            // Contract 1: never exceed the cap (codex rejects an over-cap prompt).
            assert!(
                out.chars().count() <= cap,
                "clamp exceeded cap {cap}: got {} chars (input {} chars)",
                out.chars().count(),
                s.chars().count()
            );
            // Contract 2: a prompt that already fits is returned unchanged.
            if s.chars().count() <= cap {
                assert_eq!(out, s, "clamp altered an already-fitting prompt");
            }
        }
    }

    /// Deterministic adversarial fuzz for the bridge stdout parsers. Every bridge turn streams the
    /// CLI subprocess's stdout line-by-line through `parse_line` (claude/codex/antigravity) and, in
    /// harness mode, `parse_sink_line` — UNTRUSTED input that drifts with each CLI version. A panic
    /// in any of them crashes the turn mid-stream (worse than a clean failure: partial/inconsistent
    /// state). Assemble thousands of pathological lines from the JSON-event fragments these parsers
    /// key on (truncated/unbalanced JSON, wrong-typed fields, the real event `type`s with missing
    /// payloads, control chars, huge repeats, unicode) via a seeded LCG (identical corpus on every
    /// CI box) and assert both entry points never panic and are deterministic on ALL of them.
    #[test]
    fn bridge_line_parsers_never_panic_on_adversarial_input() {
        const FRAGMENTS: &[&str] = &[
            "{",
            "}",
            "[",
            "]",
            ":",
            ",",
            "\"type\"",
            "\"text\"",
            "\"delta\"",
            "\"content\"",
            "\"tool_use\"",
            "\"assistant\"",
            "\"message\"",
            "\"usage\"",
            "\"session_id\"",
            "\"name\"",
            "\"input\"",
            "\"thread.started\"",
            "\"item.completed\"",
            "\"agent_message\"",
            "\"token_count\"",
            "null",
            "true",
            "false",
            "0",
            "-1",
            "1e999",
            "\"\"",
            "\\",
            "\\u0000",
            "\u{1f4a9}",
            "日本語",
            "\t",
            "\r",
            "{\"type\":\"result\"}",
            "{\"type\":",
            "{\"type\":\"assistant\",\"message\":{\"content\":[",
            "}]}}",
            "rate limit",
            "ok",
        ];
        let mut seed: u64 = 0xda3e_39cb_94b9_5bdb;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..6000 {
            let pieces = 1 + next() % 20;
            let mut line = String::new();
            for _ in 0..pieces {
                let frag = FRAGMENTS[next() % FRAGMENTS.len()];
                if next() % 19 == 0 {
                    line.push_str(&frag.repeat(1 + next() % 40));
                } else {
                    line.push_str(frag);
                }
            }
            // No panic (implicit: any unwind fails the test) + determinism, for every bridge kind.
            for kind in CliKind::all() {
                let a = parse_line(kind, &line);
                let b = parse_line(kind, &line);
                assert_eq!(
                    a.len(),
                    b.len(),
                    "non-deterministic parse_line({kind:?}): {line:?}"
                );
            }
            let s1 = parse_sink_line(&line);
            let s2 = parse_sink_line(&line);
            assert_eq!(
                s1.is_some(),
                s2.is_some(),
                "non-deterministic parse_sink_line: {line:?}"
            );
        }
    }
}
