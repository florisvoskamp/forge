//! `forge mcp agent` — expose a persistent Forge session as an MCP server on stdio.
//!
//! Another agent (Claude Code, another Forge instance) connects via `.mcp.json` and drives
//! the session with three tools:
//!   - `forge_chat(message)` — send a prompt, get the full response (with tool-call metadata)
//!   - `forge_status()` — inspect session ID, permission mode, active model, and tasks
//!   - `forge_set_mode(mode)` — switch the session's permission mode at runtime
//!   - `forge_interrupt()` — stop the in-flight `forge_chat` turn at its next await point
//!
//! `forge_chat` streams live progress as MCP logging notifications while it runs, so the
//! orchestrating agent can watch the turn unfold and call `forge_interrupt` (concurrently —
//! it never blocks on the session lock) to abort a turn that has gone off the rails.
//!
//! The session is persistent: history, memory, mesh routing, skills, MCP tools, and the code
//! graph are all retained across calls — the orchestrating agent treats it as a stateful
//! coding agent, not a stateless per-prompt subprocess.
//!
//! Example `.mcp.json` (for Claude Code):
//! ```json
//! {
//!   "forge": {
//!     "type": "stdio",
//!     "command": "forge",
//!     "args": ["mcp", "agent", "--cwd", "/your/project"]
//!   }
//! }
//! ```
//! Then call `mcp__forge__forge_chat("fix the bug in auth.rs")` from any MCP-aware agent.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use forge_types::{PermissionMode, SideEffect};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, JsonObject, ListToolsResult, LoggingLevel,
    LoggingMessageNotificationParam, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::cli::commands::run::build_session_with;

// ---------------------------------------------------------------------------
// Event stream: shared between the Presenter (writes) and the MCP handler (reads)
// ---------------------------------------------------------------------------

type EventSender = mpsc::UnboundedSender<forge_tui::PresenterEvent>;
type SharedEventSender = Arc<Mutex<Option<EventSender>>>;

// ---------------------------------------------------------------------------
// AgentPresenter: headless presenter that streams events to the MCP handler
// ---------------------------------------------------------------------------

struct AgentPresenter {
    event_tx: SharedEventSender,
    /// Shared with the MCP handler so `forge_set_mode` updates both sides atomically.
    mode: Arc<Mutex<PermissionMode>>,
}

impl forge_tui::Presenter for AgentPresenter {
    fn emit(&mut self, event: forge_tui::PresenterEvent) {
        if let Some(tx) = self.event_tx.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }
    }

    fn confirm(&mut self, _tool: &str, side_effect: SideEffect) -> bool {
        match *self.mode.lock().unwrap() {
            // Full auto: allow everything unconditionally.
            PermissionMode::Bypass => true,
            // Accept edits: allow all file mutations; defer shell to SideEffect check.
            PermissionMode::AcceptEdits => side_effect != SideEffect::External,
            // Default / Plan: only read-only is auto-allowed; writes need explicit permission.
            // In an agent context with no TTY, treat "ask" as allow so turns don't hang.
            PermissionMode::Default | PermissionMode::Plan => true,
        }
    }

    fn ask(
        &mut self,
        _question: &str,
        options: &[forge_tui::QChoice],
        _allow_other: bool,
    ) -> String {
        // Non-interactive: return the first option so the turn never blocks.
        // The orchestrating agent should use its own `ask_user` for genuine decisions.
        options.first().map(|o| o.label.clone()).unwrap_or_default()
    }

    fn read_line(&mut self) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// MCP server struct
// ---------------------------------------------------------------------------

struct ForgeAgentServer {
    session: Arc<tokio::sync::Mutex<forge_core::Session>>,
    event_tx: SharedEventSender,
    /// Shared with AgentPresenter — `forge_set_mode` writes here, presenter reads it.
    mode: Arc<Mutex<PermissionMode>>,
    /// Signaled by `forge_interrupt` to abort the in-flight `forge_chat` turn. Using a
    /// `Notify` (not the session lock) is deliberate: the interrupt handler must run while
    /// `forge_chat` still holds the session lock for the whole turn.
    interrupt: Arc<tokio::sync::Notify>,
    store: Arc<forge_store::Store>,
    session_id: String,
}

const TOOL_CHAT: &str = "forge_chat";
const TOOL_STATUS: &str = "forge_status";
const TOOL_SET_MODE: &str = "forge_set_mode";
const TOOL_INTERRUPT: &str = "forge_interrupt";

fn schema(obj: serde_json::Value) -> Arc<JsonObject> {
    Arc::new(obj.as_object().cloned().unwrap_or_default())
}

fn event_notification(
    event: &forge_tui::PresenterEvent,
) -> Option<LoggingMessageNotificationParam> {
    let (level, data) = match event {
        forge_tui::PresenterEvent::AssistantDelta(delta) => (
            LoggingLevel::Debug,
            serde_json::json!({ "event": "text", "delta": delta }),
        ),
        forge_tui::PresenterEvent::ToolStart { name, args } => (
            LoggingLevel::Info,
            serde_json::json!({ "event": "tool_start", "name": name, "args": args }),
        ),
        forge_tui::PresenterEvent::ToolResult { name, ok, summary } => (
            LoggingLevel::Info,
            serde_json::json!({
                "event": "tool_result",
                "name": name,
                "ok": ok,
                "summary": summary,
            }),
        ),
        forge_tui::PresenterEvent::Warning(msg) => (
            LoggingLevel::Warning,
            serde_json::json!({ "event": "warning", "msg": msg }),
        ),
        forge_tui::PresenterEvent::Routing { tier, .. } => (
            LoggingLevel::Debug,
            serde_json::json!({ "event": "routing", "tier": tier }),
        ),
        forge_tui::PresenterEvent::Cost {
            session_total_usd, ..
        } => (
            LoggingLevel::Debug,
            serde_json::json!({ "event": "cost", "usd": session_total_usd }),
        ),
        _ => return None,
    };
    Some(LoggingMessageNotificationParam::new(level, data).with_logger("forge"))
}

impl ServerHandler for ForgeAgentServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "A persistent Forge coding-agent session. Use `forge_chat` to send prompts — the \
             session retains full conversation history, mesh routing, skills, MCP tools, and \
             memory across calls. Treat it as a stateful senior engineer, not a one-shot \
             subprocess. `forge_status` inspects the session; `forge_set_mode` controls \
             permissions (default for careful work, accept_edits for file-heavy tasks, bypass \
             for fully autonomous runs)."
                .into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = vec![
            Tool::new(
                TOOL_CHAT.to_string(),
                "Send a prompt to this Forge session and receive the full response. The session \
                 retains conversation history, mesh-routed model selection, skills, MCP tools, \
                 memory, and the code graph (Lattice) across calls. For multi-step coding tasks, \
                 prefer multiple `forge_chat` calls over restarting the session — context \
                 accumulates and quality improves."
                    .to_string(),
                schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": {
                            "type": "string",
                            "description": "the prompt to send to the Forge session"
                        }
                    },
                    "required": ["message"]
                })),
            ),
            Tool::new(
                TOOL_STATUS.to_string(),
                "Return the current session state: session ID, permission mode, pinned model \
                 (if any), pending tasks, and whether the session is ready."
                    .to_string(),
                schema(serde_json::json!({
                    "type": "object",
                    "properties": {}
                })),
            ),
            Tool::new(
                TOOL_SET_MODE.to_string(),
                "Switch the session's permission mode. `bypass` = fully autonomous (all tools \
                 auto-allowed, no prompts); `accept_edits` = auto-allow file reads/writes, ask \
                 for external shell commands; `default` = ask before any write. Start with \
                 `accept_edits` for coding tasks, escalate to `bypass` only when you've \
                 established trust in the session's direction."
                    .to_string(),
                schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["bypass", "accept_edits", "default"],
                            "description": "permission mode to activate"
                        }
                    },
                    "required": ["mode"]
                })),
            ),
            Tool::new(
                TOOL_INTERRUPT.to_string(),
                "Abort the `forge_chat` turn that is currently running. The turn stops at its \
                 next await point and returns its partial result; the session stays intact, so \
                 you can immediately `forge_chat` again to redirect it. Safe to call anytime — \
                 a no-op if no turn is in flight. Use this when the streamed progress shows the \
                 turn going the wrong way, looping, or burning budget."
                    .to_string(),
                schema(serde_json::json!({
                    "type": "object",
                    "properties": {}
                })),
            ),
        ];
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name: &str = &request.name;
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);

        match name {
            TOOL_CHAT => {
                let message = match args.get("message").and_then(|v| v.as_str()) {
                    Some(m) if !m.trim().is_empty() => m.trim().to_string(),
                    _ => {
                        return Ok(CallToolResult::error(vec![Content::text(
                            "forge_chat requires a non-empty `message`",
                        )]));
                    }
                };

                let mut session = self.session.lock().await;
                let (tx, mut rx) = mpsc::unbounded_channel();
                *self.event_tx.lock().unwrap() = Some(tx);

                let peer = ctx.peer.clone();
                let store = Arc::clone(&self.store);
                let sid = self.session_id.clone();
                let notify_task = tokio::spawn(async move {
                    let mut tool_calls = Vec::new();
                    while let Some(event) = rx.recv().await {
                        if let forge_tui::PresenterEvent::ToolStart { name, .. } = &event {
                            tool_calls.push(name.clone());
                        }
                        if let Some(notification) = event_notification(&event) {
                            let _ = peer.notify_logging_message(notification).await;
                        }
                        if let Some(le) = crate::live_observer::to_live_event(&event) {
                            if let Ok(json) = serde_json::to_string(&le) {
                                let _ = store.append_live_event(&sid, &json);
                            }
                        }
                    }
                    tool_calls
                });

                // Race the turn against an interrupt signal. `notified()` registers its
                // waiter on first poll (inside `select!`), so a later `forge_interrupt` wakes
                // it; dropping the `run_turn_with` future on interrupt stops it at its current
                // await point and releases the `&mut session` borrow.
                let interrupt = Arc::clone(&self.interrupt);
                let outcome = tokio::select! {
                    r = session.run_turn_with(&message, &[], None) => Some(r),
                    _ = interrupt.notified() => None,
                };
                *self.event_tx.lock().unwrap() = None;
                let tool_calls = notify_task.await.unwrap_or_default();
                match outcome {
                    Some(Ok(response)) => {
                        let mut out = response.text;
                        if !tool_calls.is_empty() {
                            out.push_str(&format!(
                                "\n\n<!-- forge: tools used: {} -->",
                                tool_calls.join(", ")
                            ));
                        }
                        Ok(CallToolResult::success(vec![Content::text(out)]))
                    }
                    Some(Err(e)) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "turn failed: {e}"
                    ))])),
                    None => Ok(CallToolResult::success(vec![Content::text(format!(
                        "turn interrupted via forge_interrupt after {} tool call(s){}. Session \
                         state is preserved — send a new forge_chat to continue or redirect.",
                        tool_calls.len(),
                        if tool_calls.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", tool_calls.join(", "))
                        }
                    ))])),
                }
            }

            TOOL_STATUS => {
                let session = self.session.lock().await;
                let mode = match session.mode() {
                    PermissionMode::Bypass => "bypass",
                    PermissionMode::AcceptEdits => "accept_edits",
                    PermissionMode::Default => "default",
                    PermissionMode::Plan => "plan",
                };
                let status = serde_json::json!({
                    "session_id": session.id(),
                    "mode": mode,
                    "pinned_model": session.pinned_model(),
                    "ready": true,
                });
                Ok(CallToolResult::success(vec![Content::text(
                    serde_json::to_string_pretty(&status).unwrap_or_default(),
                )]))
            }

            TOOL_SET_MODE => {
                let mode_str = args
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");
                let mode = match mode_str {
                    "bypass" => PermissionMode::Bypass,
                    "accept_edits" => PermissionMode::AcceptEdits,
                    _ => PermissionMode::Default,
                };
                // Update both the session's internal mode and the presenter's mode so
                // permission checks on the next turn use the new value.
                *self.mode.lock().unwrap() = mode;
                let mut session = self.session.lock().await;
                session.set_mode(mode);
                Ok(CallToolResult::success(vec![Content::text(format!(
                    "permission mode → {mode_str}"
                ))]))
            }

            TOOL_INTERRUPT => {
                // Wake the in-flight turn's `notified()` waiter (if any). Does not touch the
                // session lock, so it runs even while `forge_chat` holds it for the whole turn.
                self.interrupt.notify_waiters();
                Ok(CallToolResult::success(vec![Content::text(
                    "interrupt signaled — the active forge_chat turn (if any) will stop at its \
                     next await point and return its partial result. No-op if no turn is running.",
                )]))
            }

            _ => Ok(CallToolResult::error(vec![Content::text(format!(
                "unknown tool '{name}'. Available: {TOOL_CHAT}, {TOOL_STATUS}, {TOOL_SET_MODE}, \
                 {TOOL_INTERRUPT}"
            ))])),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the Forge MCP agent server on stdio. Starts or resumes a Forge session (keyed by
/// `session_id` prefix, same as `forge chat --resume`) and serves it over stdio MCP until
/// the client disconnects. The session persists in the global store — reconnecting with the
/// same `--session` id resumes where it left off.
pub async fn run(session_id: Option<String>, cwd: Option<std::path::PathBuf>) -> Result<()> {
    if let Some(cwd) = cwd {
        std::env::set_current_dir(&cwd)?;
    }

    // Default to AcceptEdits: agent mode is assumed to be orchestrated, so file edits
    // auto-proceed without prompts. The orchestrating agent can escalate via forge_set_mode.
    let initial_mode = PermissionMode::AcceptEdits;
    let mode = Arc::new(Mutex::new(initial_mode));
    let event_tx: SharedEventSender = Arc::new(Mutex::new(None));

    let presenter = Box::new(AgentPresenter {
        event_tx: Arc::clone(&event_tx),
        mode: Arc::clone(&mode),
    });

    let session = build_session_with(presenter, false, None, session_id, None).await?;

    // Apply the initial mode to the session (it may have been loaded with a different mode
    // from its stored config; in agent mode we always start permissive).
    let mut session = session;
    session.set_mode(initial_mode);

    let store = Arc::new(crate::open_store()?);
    let sid = session.id().to_string();
    let _ = store.set_session_agent_active(&sid, true);

    struct ActiveGuard {
        store: Arc<forge_store::Store>,
        session_id: String,
    }
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            let _ = self.store.set_session_agent_active(&self.session_id, false);
        }
    }
    let _active_guard = ActiveGuard {
        store: Arc::clone(&store),
        session_id: sid.clone(),
    };

    let server = ForgeAgentServer {
        session: Arc::new(tokio::sync::Mutex::new(session)),
        event_tx,
        mode,
        interrupt: Arc::new(tokio::sync::Notify::new()),
        store,
        session_id: sid,
    };

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
