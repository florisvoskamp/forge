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
//! mesh-routed child agents. The children run in *this* process (it builds its own
//! provider/router/store), and we set `FORGE_NO_SPAWN_AGENTS=1` before running them so any
//! nested CLI-bridge child inherits it and does NOT re-expose `spawn_agents` — a depth-1
//! recursion guard that holds across the process boundary via env inheritance.

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

/// Env var that disables the `spawn_agents` tool. Set in a subagent's environment so a nested
/// CLI-bridge child inherits it and can't recurse (RFC subagent-orchestration depth-1 guard).
const NO_SPAWN_ENV: &str = "FORGE_NO_SPAWN_AGENTS";

/// Everything a `spawn_agents` call needs, built once if subagents are enabled here.
struct SubagentSupport {
    ctx: AgentCtx,
    agents: std::collections::HashMap<String, forge_config::AgentDef>,
    parent_id: String,
    max_agents: usize,
    max_concurrency: usize,
}

struct ForgeMcp {
    registry: ToolRegistry,
    mode: PermissionMode,
    rules: Vec<PermissionRule>,
    config: Config,
    /// Present when subagents are enabled and not suppressed by [`NO_SPAWN_ENV`].
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

        match tool.run(&args).await {
            Ok(out) => Ok(CallToolResult::success(vec![Content::text(out)])),
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

        // Mark this process (and anything it spawns) as a subagent context so a nested CLI
        // bridge won't re-expose spawn_agents — the depth-1 guard across the process boundary.
        std::env::set_var(NO_SPAWN_ENV, "1");

        let mut on_event = |ev: subagent::Lifecycle| match ev {
            subagent::Lifecycle::Start { agent, task, .. } => {
                tracing::info!(agent, task, "subagent started")
            }
            // Live deltas have nowhere to go over the MCP boundary; the bridge sees the final
            // combined result. (Streaming them to the parent TUI is Phase 3b-bridge, deferred.)
            subagent::Lifecycle::Progress { .. } => {}
            subagent::Lifecycle::Done {
                agent,
                ok,
                cost_usd,
                ..
            } => tracing::info!(agent, ok, cost_usd, "subagent done"),
        };

        match subagent::orchestrate(
            &s.ctx,
            &s.parent_id,
            requests,
            &s.agents,
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

    // Subagents are exposed here only when enabled AND not suppressed (a nested bridge child
    // inherits FORGE_NO_SPAWN_AGENTS=1 and must not re-expose the tool).
    let subagents = if config.mesh.subagents.enabled && std::env::var(NO_SPAWN_ENV).is_err() {
        let store = Arc::new(Store::open(std::path::Path::new(".forge/forge.db"))?);
        let (provider, router) = crate::build_provider_and_router(&config, false, None);
        let parent_id = store.create_session(".", &format!("{:?}", config.permission_mode))?;
        let ctx = AgentCtx {
            provider,
            router,
            store,
            config: config.clone(),
            pricing: Pricing::from_config(&config),
            mode: config.permission_mode,
            rules: config.permission_rules(),
        };
        Some(SubagentSupport {
            ctx,
            agents: forge_config::load_agents(std::path::Path::new(
                &config.mesh.subagents.agents_dir,
            )),
            parent_id,
            max_agents: config.mesh.subagents.max_agents,
            max_concurrency: config.mesh.subagents.max_concurrency,
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
