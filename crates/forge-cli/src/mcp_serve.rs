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

use std::sync::Arc;

use anyhow::Result;
use forge_config::Config;
use forge_core::permission;
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

struct ForgeMcp {
    registry: ToolRegistry,
    mode: PermissionMode,
    rules: Vec<PermissionRule>,
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
        let tools = self
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

/// Run the Forge MCP server on stdio until the client disconnects. Loads config from the cwd
/// (so it shares the project's permission rules) and serves the core tool registry.
pub async fn run() -> Result<()> {
    let config = forge_config::load().unwrap_or_else(|_| Config::default());
    let server = ForgeMcp {
        registry: ToolRegistry::with_core_tools(),
        mode: config.permission_mode,
        rules: config.permission_rules(),
    };
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
