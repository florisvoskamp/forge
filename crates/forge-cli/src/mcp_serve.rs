//! `forge mcp-serve` — an MCP **server** (stdio) that exposes Forge's tool registry to an
//! external agent CLI (claude/codex) so the subscription model runs the **Forge harness**:
//! Forge's own tools, gated by Forge's permission engine (the builtin safety denylist +
//! configured rules). This is Phase 2 of RFC cli-bridge-full-harness — the CLI bridge spawns
//! `forge mcp-serve` and restricts the model to `mcp__forge__*`, so every tool call lands here.
//!
//! Permission: each call runs `permission::decide` before executing; a `Deny` (e.g. the
//! `rm -rf`/secret-read denylist) returns an MCP tool error the model sees and adapts to.
//! Interactive `Ask` is auto-allowed in this non-interactive context (the bridge can't prompt
//! mid-run) — the unoverridable denylist still protects. Forge never sees the CLI's auth.
//!
//! Subagents (RFC subagent-orchestration Phase 3): when subagents are enabled this server also
//! exposes the `spawn_agents` virtual tool, so a subscription model can fan work out to
//! mesh-routed child agents that run in *this* process (it builds its own provider/router/store).
//! Two cross-process mechanisms ride env vars inherited forge → claude/codex → mcp-serve:
//! - `FORGE_SUBAGENT_DEPTH` bounds recursion: this server advertises `spawn_agents` only while
//!   `depth < max_depth`, and bumps the var for anything it spawns (Phase 3c).
//! - `FORGE_SUBAGENT_SINK` names a JSONL file we append child lifecycle to, which the parent
//!   Forge process tails so bridge-spawned subagents are visible in the TUI (Phase 3c).

use std::sync::Arc;

use anyhow::Result;
use forge_config::Config;
use forge_core::hooks;
use forge_core::permission;
use forge_core::subagent::{self, AgentCtx};
use forge_mesh::pricing::Pricing;
use forge_mesh::BudgetState;
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_types::{PermissionDecision, PermissionMode, PermissionRule};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;

/// Append one JSON record to the out-of-band subagent sink the CLI bridge tails (if it gave us
/// one via `FORGE_SUBAGENT_SINK`). Used to surface bridge-turn activity (subagents, task-list
/// updates) in the parent Forge TUI live. Best-effort: no sink / write error is silently ignored.
fn report_to_sink(record: serde_json::Value) {
    let Ok(path) = std::env::var(forge_provider::SUBAGENT_SINK_ENV) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(f, "{record}");
        let _ = f.flush();
    }
}

/// Everything a `spawn_agents` call needs, built once if subagents are enabled here. `ctx`
/// already carries the loaded agent types, the nesting depth, and `max_depth`.
struct SubagentSupport {
    ctx: AgentCtx,
    parent_id: String,
    max_agents: usize,
    max_concurrency: usize,
    depth: usize,
}

struct ForgeMcp {
    registry: ToolRegistry,
    mode: PermissionMode,
    rules: Vec<PermissionRule>,
    config: Config,
    /// Present when subagents are enabled here (subagents on + `depth < max_depth`).
    subagents: Option<SubagentSupport>,
    /// Store used by the `update_tasks` virtual tool to persist the bridge turn's task list,
    /// keyed by the parent session id the parent's `run_turn` exported via `ENV_SESSION`.
    tasks_store: Arc<Store>,
    /// External MCP servers (mcp-client.md). On the CLI-bridge path the model's tool surface IS
    /// this server, so the MCP meta-tools must be advertised + handled here — otherwise a bridge
    /// model (claude/codex) can't see or call any external MCP tool. `None` when none configured.
    mcp: Option<Arc<forge_mcp::McpManager>>,
    /// Command/skill catalog so a bridged model can discover + load Forge's own skills via the
    /// `use_skill` tool (otherwise claude/codex hunt their own ~/.claude / ~/.codex skills).
    skills: Arc<forge_skills::Catalog>,
}

impl ServerHandler for ForgeMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some(
            "Forge's own tools (read_file, write_file, edit_file, list_dir, search, shell), \
             gated by Forge's permission engine."
                .into(),
        );
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut tools: Vec<Tool> = self
            .registry
            .names()
            .filter_map(|name| self.registry.get(name))
            .map(|t| {
                let schema: JsonObject = t.schema().as_object().cloned().unwrap_or_default();
                Tool::new(
                    t.name().to_string(),
                    t.description().to_string(),
                    Arc::new(schema),
                )
            })
            .collect();
        // Advertise the subagent virtual tool only when enabled here.
        if let Some(s) = &self.subagents {
            let spec = subagent::spawn_agents_spec(s.max_agents);
            let schema: JsonObject = spec.schema.as_object().cloned().unwrap_or_default();
            tools.push(Tool::new(spec.name, spec.description, Arc::new(schema)));
        }
        // Always advertise task tracking so a bridge model can maintain a visible todo list.
        let ts = forge_core::update_tasks_spec();
        let ts_schema: JsonObject = ts.schema.as_object().cloned().unwrap_or_default();
        tools.push(Tool::new(ts.name, ts.description, Arc::new(ts_schema)));
        // Advertise skill loading (with the available-skills list) so a bridge model finds + uses
        // Forge's own skills instead of its native ones.
        if !self.skills.skill_listing().is_empty() {
            let us = forge_core::use_skill_spec(&self.skills);
            let us_schema: JsonObject = us.schema.as_object().cloned().unwrap_or_default();
            tools.push(Tool::new(us.name, us.description, Arc::new(us_schema)));
        }
        // External MCP meta-tools (mcp_search_tools / mcp_call / resources / prompt) so a bridge
        // model can discover + call the connected servers' tools (e.g. helm). Empty if none.
        if let Some(m) = &self.mcp {
            for spec in m.advertised_specs() {
                let schema: JsonObject = spec.schema.as_object().cloned().unwrap_or_default();
                tools.push(Tool::new(spec.name, spec.description, Arc::new(schema)));
            }
        }
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let name = request.name.to_string();
        let args = request.arguments.map(Value::Object).unwrap_or(Value::Null);

        // The subagent virtual tool — fan out to mesh-routed children in this process.
        if name == subagent::SPAWN_AGENTS_TOOL {
            return Ok(self.handle_spawn_agents(&args).await);
        }

        // Task tracking — persist the list to the parent session (id from ENV_SESSION) so the
        // parent's run_turn reloads it. Also report it to the out-of-band sink so the parent TUI's
        // sticky task panel updates LIVE during the bridge turn (not just on completion).
        if name == forge_core::UPDATE_TASKS_TOOL {
            let tasks = forge_core::parse_tasks(&args);
            let done = tasks
                .iter()
                .filter(|t| t.status == forge_types::TodoStatus::Done)
                .count();
            if let Ok(session_id) = std::env::var(forge_core::snapshot::ENV_SESSION) {
                let _ = self.tasks_store.set_tasks(&session_id, &tasks);
            }
            report_to_sink(serde_json::json!({ "k": "tasks", "tasks": tasks }));
            return Ok(CallToolResult::success(vec![Content::text(format!(
                "task list updated: {} task(s) — {done} done",
                tasks.len()
            ))]));
        }

        // Skill loading — return the named Forge skill's methodology so the bridge model applies
        // it (parity with the direct path's use_skill handler).
        if name == forge_core::USE_SKILL_TOOL {
            let skill = args
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            return Ok(match self.skills.skill_guidance(skill) {
                Some(g) => CallToolResult::success(vec![Content::text(format!(
                    "Loaded the '{skill}' skill. Apply this methodology now:\n\n{g}"
                ))]),
                None => {
                    let available = self
                        .skills
                        .skill_listing()
                        .into_iter()
                        .map(|(n, _)| n)
                        .collect::<Vec<_>>()
                        .join(", ");
                    CallToolResult::error(vec![Content::text(format!(
                        "no Forge skill named '{skill}'. Available: {available}"
                    ))])
                }
            });
        }

        // External MCP meta-tools — gate (External/ReadOnly) then route to the manager. Server
        // tools are invoked via `mcp_call`, so this covers the whole external surface.
        if let Some(m) = &self.mcp {
            if m.knows_tool(&name) {
                let side_effect = m.side_effect_of(&name);
                let decision =
                    permission::decide(self.mode, side_effect, &name, &args, &self.rules);
                if decision == PermissionDecision::Deny {
                    return Ok(CallToolResult::error(vec![Content::text(format!(
                        "denied by Forge permission policy: {name}"
                    ))]));
                }
                let out = m.call(&name, &args).await;
                let content = vec![Content::text(out.text)];
                return Ok(if out.ok {
                    CallToolResult::success(content)
                } else {
                    CallToolResult::error(content)
                });
            }
        }

        let Some(tool) = self.registry.get(&name) else {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "unknown tool: {name}"
            ))]));
        };

        // PreToolUse hooks (hooks.md): fire before the permission gate so user hooks can block
        // calls on the CLI-bridge path the same way they do on the direct path.
        if !self.config.hooks.is_empty() {
            let payload = serde_json::json!({ "tool": name, "args": args }).to_string();
            let outcome = hooks::run_hooks(
                &self.config.hooks,
                forge_config::HookEvent::PreToolUse,
                &name,
                &payload,
            )
            .await;
            if let Some(reason) = outcome.blocked {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "blocked by hook: {reason}"
                ))]));
            }
        }

        // Forge's permission gate — the unoverridable denylist always applies here.
        let decision = permission::decide(self.mode, tool.side_effect(), &name, &args, &self.rules);
        if decision == PermissionDecision::Deny {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "denied by Forge permission policy: {name}"
            ))]));
        }

        // Snapshot the target's pre-edit bytes into the parent turn's checkpoint dir (read from
        // the env the parent's run_turn exported), so files a bridge model edits are restorable by
        // `/undo` exactly like in-process edits (RFC PR3, cross-process). No-op if unset.
        let write_path = (tool.side_effect() == forge_types::SideEffect::Write)
            .then(|| args.get("path").and_then(|v| v.as_str()))
            .flatten()
            .map(std::path::PathBuf::from);
        if let Some(path) = &write_path {
            let _ = forge_core::snapshot::snapshot_from_env_before_write(path);
        }

        let (out, ok) = match tool.run(&args).await {
            Ok(out) => {
                if let Some(path) = &write_path {
                    let _ = forge_core::snapshot::record_from_env_after_write(path);
                }
                (out, true)
            }
            Err(e) => (format!("error: {e}"), false),
        };

        // PostToolUse hooks (hooks.md): observe the completed call. Notes are surfaced as a
        // prefix on the result the bridge model sees (no presenter available on this path).
        let mut note_prefix = String::new();
        if !self.config.hooks.is_empty() {
            let payload =
                serde_json::json!({ "tool": name, "args": args, "result": out, "ok": ok })
                    .to_string();
            let outcome = hooks::run_hooks(
                &self.config.hooks,
                forge_config::HookEvent::PostToolUse,
                &name,
                &payload,
            )
            .await;
            for note in outcome.notes {
                note_prefix.push_str(&format!("[hook note] {note}\n"));
            }
        }

        let result_text = if note_prefix.is_empty() {
            out
        } else {
            format!("{note_prefix}{out}")
        };
        Ok(if ok {
            CallToolResult::success(vec![Content::text(result_text)])
        } else {
            CallToolResult::error(vec![Content::text(result_text)])
        })
    }
}

impl ForgeMcp {
    async fn handle_spawn_agents(&self, args: &Value) -> CallToolResult {
        let Some(s) = &self.subagents else {
            return CallToolResult::error(vec![Content::text(
                "spawn_agents is not available here",
            )]);
        };
        let requests = match subagent::parse_requests(args, s.max_agents) {
            Ok(r) => r,
            Err(msg) => return CallToolResult::error(vec![Content::text(format!("error: {msg}"))]),
        };

        let budget = BudgetState {
            spent_today_usd: s.ctx.store.spend_today_usd().unwrap_or(0.0),
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_month_usd: s.ctx.store.spend_this_month_usd().unwrap_or(0.0),
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        };

        // Mark anything we spawn as a deeper subagent level so recursion is bounded (Phase 3c).
        std::env::set_var(crate::FORGE_SUBAGENT_DEPTH_ENV, (s.depth + 1).to_string());

        // Report subagent lifecycle to the out-of-band sink (if the bridge gave us one) so the
        // parent Forge TUI shows these children natively (RFC subagent-orchestration Phase 3c).
        let mut sink = std::env::var(forge_provider::SUBAGENT_SINK_ENV)
            .ok()
            .and_then(|p| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .ok()
            });
        let mut write = move |v: serde_json::Value| {
            if let Some(f) = sink.as_mut() {
                use std::io::Write;
                let _ = writeln!(f, "{v}");
                let _ = f.flush();
            }
        };
        let mut on_event = |ev: subagent::Lifecycle| match ev {
            subagent::Lifecycle::Start { id, agent, task } => {
                write(serde_json::json!({"k":"start","id":id,"agent":agent,"task":task}));
            }
            subagent::Lifecycle::Progress { id, snippet } => {
                write(serde_json::json!({"k":"progress","id":id,"snippet":snippet}));
            }
            subagent::Lifecycle::Done {
                id,
                agent,
                ok,
                summary,
                cost_usd,
            } => {
                write(
                    serde_json::json!({"k":"done","id":id,"agent":agent,"ok":ok,"summary":summary,"cost":cost_usd}),
                );
            }
        };

        match subagent::orchestrate(
            &s.ctx,
            &s.parent_id,
            requests,
            budget,
            s.max_concurrency,
            &mut on_event,
        )
        .await
        {
            Ok((combined, _ok)) => CallToolResult::success(vec![Content::text(combined)]),
            Err(e) => CallToolResult::error(vec![Content::text(format!("subagents failed: {e}"))]),
        }
    }
}

/// Run the Forge MCP server on stdio until the client disconnects. Loads config from the cwd
/// (so it shares the project's permission rules) and serves the core tool registry.
pub async fn run() -> Result<()> {
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_else(|_| Config::default());

    // Current nesting depth, carried across the process boundary. Subagents are exposed here
    // only when enabled AND there is depth budget left — a nested bridge child inherits a
    // higher FORGE_SUBAGENT_DEPTH and stops advertising the tool once it reaches max_depth.
    let depth: usize = std::env::var(crate::FORGE_SUBAGENT_DEPTH_ENV)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let subagents = if config.mesh.subagents.enabled && depth < config.mesh.subagents.max_depth {
        let store = Arc::new(Store::open(std::path::Path::new(".forge/forge.db"))?);
        let (provider, router) = crate::build_provider_and_router(&config, false, None, None);
        let parent_id = store.create_session(".", &format!("{:?}", config.permission_mode))?;
        let agents = Arc::new(forge_config::load_agents(std::path::Path::new(
            &config.mesh.subagents.agents_dir,
        )));
        let ctx = AgentCtx {
            provider,
            router,
            store,
            config: config.clone(),
            pricing: Pricing::from_config(&config),
            mode: config.permission_mode,
            rules: config.permission_rules(),
            depth,
            max_depth: config.mesh.subagents.max_depth,
            agents,
        };
        Some(SubagentSupport {
            ctx,
            parent_id,
            max_agents: config.mesh.subagents.max_agents,
            max_concurrency: config.mesh.subagents.max_concurrency,
            depth,
        })
    } else {
        None
    };

    // Reuse the subagent store if present, else open the project store for task persistence.
    let tasks_store = match &subagents {
        Some(s) => Arc::clone(&s.ctx.store),
        None => Arc::new(Store::open(std::path::Path::new(".forge/forge.db"))?),
    };
    // Connect the external MCP servers in THIS process so the bridge model can drive them — the
    // bridge's whole tool surface is this server. Skipped (None) when none are configured.
    // Connect external MCP servers in the BACKGROUND so the bridge serves Forge's own tools
    // (update_tasks, spawn_agents, file/shell, …) IMMEDIATELY. Awaiting connect here previously
    // stalled the whole tool list behind a slow/auth-gated server (e.g. helm), so the spawned
    // claude/codex CLI timed out waiting for the MCP server and fell back to its native tools
    // ("update_tasks not in my toolset"). The meta-tools are advertised right away via
    // `connecting`; the first `mcp_call` lazily connects on demand.
    let mcp = if config.mcp.active_servers().next().is_some() {
        let mgr = Arc::new(forge_mcp::McpManager::connecting(&config.mcp));
        let bg = Arc::clone(&mgr);
        tokio::spawn(async move { bg.connect_active().await });
        Some(mgr)
    } else {
        None
    };
    let skills = Arc::new(forge_skills::Catalog::load(&forge_config::command_sources()));
    let server = ForgeMcp {
        registry: ToolRegistry::with_core_tools(),
        mode: config.permission_mode,
        rules: config.permission_rules(),
        config,
        subagents,
        tasks_store,
        mcp,
        skills,
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
