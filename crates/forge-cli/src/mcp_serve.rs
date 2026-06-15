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

        let Some(tool) = self.registry.get(&name) else {
            return Ok(CallToolResult::error(vec![Content::text(format!(
                "unknown tool: {name}"
            ))]));
        };

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

        match tool.run(&args).await {
            Ok(out) => {
                if let Some(path) = &write_path {
                    let _ = forge_core::snapshot::record_from_env_after_write(path);
                }
                Ok(CallToolResult::success(vec![Content::text(out)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(e.to_string())])),
        }
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

    let server = ForgeMcp {
        registry: ToolRegistry::with_core_tools(),
        mode: config.permission_mode,
        rules: config.permission_rules(),
        config,
        subagents,
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
