//! Subagent orchestration (RFC subagent-orchestration): the `spawn_agents` tool lets the
//! top-level model delegate subtasks to **child agents**, each in its own isolated context
//! and **routed independently through the Model Mesh** — so a Complex parent can fan out
//! cheap Trivial children.
//!
//! `spawn_agents` is a *virtual tool*: it is advertised to the parent model but is not a
//! `forge_tools::Tool` (it needs the provider/router/store, which ordinary tools can't reach).
//! [`Session`](crate::Session) intercepts it and calls [`run_subagent`] here.
//!
//! Phase 1 (this module): one or more **inline, read-only** children run **sequentially**,
//! each as a persisted child session linked to the parent. Children get only read-only tools
//! and **never** the `spawn_agents` tool itself — a structural depth-1 guard against recursion.
//! Parallel fan-out and `.forge/agents/*.md` agent types are Phase 2.

use std::sync::Arc;

use forge_config::Config;
use forge_mesh::pricing::Pricing;
use forge_mesh::{BudgetState, Router};
use forge_provider::{Provider, StreamEvent, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_types::{Message, PermissionDecision, PermissionMode, PermissionRule, Role, Usage};

use crate::{permission, CoreError, MAX_STEPS};

/// The virtual tool name the parent model calls to delegate subtasks.
pub const SPAWN_AGENTS_TOOL: &str = "spawn_agents";

/// Tools a subagent may use in Phase 1: read-only investigation only. Writes/shell require an
/// agent-type opt-in (Phase 2) plus a permitting mode; the safety denylist always applies.
const SUBAGENT_TOOLS: &[&str] = &["read_file", "list_dir", "search"];

const SUBAGENT_SYSTEM: &str = "You are a focused Forge subagent working on ONE delegated \
    subtask. Investigate with the tools you have and return a concise, self-contained answer \
    to the task — just the result the parent agent needs, no preamble.";

/// The `ToolSpec` advertised to the parent so the model can call `spawn_agents`.
pub fn spawn_agents_spec(max_agents: usize) -> ToolSpec {
    ToolSpec {
        name: SPAWN_AGENTS_TOOL.to_string(),
        description: format!(
            "Delegate one or more independent subtasks to child agents that work in their own \
             isolated context and are routed to the cheapest capable model. Use this to fan out \
             research/search/review across files instead of doing it all yourself. Up to \
             {max_agents} agents per call. Each agent gets read-only tools and returns a concise \
             result. Returns all results, labeled."
        ),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": max_agents,
                    "items": {
                        "type": "object",
                        "properties": {
                            "agent": {
                                "type": "string",
                                "description": "optional named agent type; omit for a general read-only agent"
                            },
                            "task": {
                                "type": "string",
                                "description": "the self-contained subtask for this agent"
                            }
                        },
                        "required": ["task"]
                    }
                }
            },
            "required": ["agents"]
        }),
    }
}

/// One requested child agent, parsed from the `spawn_agents` arguments.
pub struct AgentRequest {
    pub agent: String,
    pub task: String,
}

/// Parse `spawn_agents` arguments into child requests, capped at `max_agents`. Returns an
/// `Err(message)` describing the problem in model-readable form if the shape is wrong.
pub fn parse_requests(
    args: &serde_json::Value,
    max_agents: usize,
) -> Result<Vec<AgentRequest>, String> {
    let arr = args
        .get("agents")
        .and_then(|a| a.as_array())
        .ok_or("spawn_agents requires an `agents` array")?;
    if arr.is_empty() {
        return Err("spawn_agents `agents` must not be empty".into());
    }
    let mut out = Vec::new();
    for entry in arr.iter().take(max_agents) {
        let task = entry
            .get("task")
            .and_then(|t| t.as_str())
            .filter(|t| !t.trim().is_empty())
            .ok_or("each agent needs a non-empty `task`")?;
        let agent = entry
            .get("agent")
            .and_then(|a| a.as_str())
            .filter(|a| !a.trim().is_empty())
            .unwrap_or("general")
            .to_string();
        out.push(AgentRequest {
            agent,
            task: task.to_string(),
        });
    }
    Ok(out)
}

/// Shared, cheaply-cloneable machinery a subagent needs — the same backends the parent uses.
#[derive(Clone)]
pub struct AgentCtx {
    pub provider: Arc<dyn Provider>,
    pub router: Arc<dyn Router>,
    pub store: Arc<Store>,
    pub config: Config,
    pub pricing: Pricing,
    pub mode: PermissionMode,
    pub rules: Vec<PermissionRule>,
}

/// The result of running one child agent.
pub struct SubagentOutcome {
    pub final_text: String,
    pub ok: bool,
}

/// Run one child agent to completion against `child_id` (a persisted child session): route the
/// task independently, run the model↔tool loop with read-only tools, persist messages + usage
/// to the child session (so its cost rolls into the shared budget), and return the answer.
pub async fn run_subagent(
    ctx: &AgentCtx,
    child_id: &str,
    task: &str,
    budget: BudgetState,
) -> Result<SubagentOutcome, CoreError> {
    // Read-only registry, WITHOUT spawn_agents — children can't recurse (depth-1 guard).
    let full = ToolRegistry::with_core_tools();
    let specs: Vec<ToolSpec> = SUBAGENT_TOOLS
        .iter()
        .filter_map(|name| full.get(name))
        .map(|t| ToolSpec {
            name: t.name().to_string(),
            description: t.description().to_string(),
            schema: t.schema(),
        })
        .collect();

    let decision = ctx.router.route(task, budget).await;

    let mut transcript = vec![Message::system(SUBAGENT_SYSTEM), Message::user(task)];
    let mut seq: i64 = 0;
    let mut next_seq = || {
        let n = seq;
        seq += 1;
        n
    };
    ctx.store
        .add_message(child_id, next_seq(), Role::User, task, None)?;

    let mut final_text = String::new();
    let mut ok = true;

    for step in 0..MAX_STEPS {
        // Subagents don't stream inner tokens to the UI in Phase 1 (coarse events only).
        let mut sink = |_: StreamEvent| {};
        let mut resp = ctx
            .provider
            .complete(&decision.model, &transcript, &specs, &mut sink)
            .await?;
        resp.usage.cost_usd = ctx.pricing.cost_for(
            &decision.model,
            resp.usage.input_tokens,
            resp.usage.output_tokens,
        );

        transcript.push(Message::assistant_tool_calls(
            &resp.content,
            resp.tool_calls.clone(),
        ));
        let msg_id = ctx.store.add_message_full(
            child_id,
            next_seq(),
            Role::Assistant,
            &resp.content,
            Some(&decision.model),
            &resp.tool_calls,
            None,
        )?;
        if step == 0 {
            ctx.store.record_routing(
                &msg_id,
                decision.tier,
                &decision.model,
                &decision.rationale,
            )?;
        }
        ctx.store.record_usage(child_id, &msg_id, &resp.usage)?;

        if !resp.wants_tools() {
            final_text = resp.content;
            break;
        }

        for call in &resp.tool_calls {
            let result = execute_tool(ctx, &full, &msg_id, call).await?;
            if result.starts_with("error:") || result.starts_with("permission denied") {
                ok = false;
            }
            ctx.store.add_message_full(
                child_id,
                next_seq(),
                Role::Tool,
                &result,
                None,
                &[],
                Some(&call.id),
            )?;
            transcript.push(Message::tool_result(&call.id, result));
        }
    }

    Ok(SubagentOutcome { final_text, ok })
}

/// Run one subagent tool call through the permission gate (headless). Differs from the parent's
/// `invoke_tool` in that there is no interactive surface: an `Ask` decision resolves to **Deny**
/// (a parallel/headless child can't prompt), and no presenter events are emitted. The safety
/// denylist always applies. Unknown / non-read-only tools are refused.
async fn execute_tool(
    ctx: &AgentCtx,
    registry: &ToolRegistry,
    msg_id: &str,
    call: &forge_types::ToolCall,
) -> Result<String, CoreError> {
    let args_json = serde_json::to_string(&call.args)?;
    let Some(tool) = registry.get(&call.name) else {
        let result = format!("error: tool '{}' is not available to subagents", call.name);
        ctx.store
            .record_tool_call(msg_id, &call.name, &args_json, &result, "n/a", "error")?;
        return Ok(result);
    };
    let side_effect = tool.side_effect();
    let allowed =
        match permission::decide(ctx.mode, side_effect, &call.name, &call.args, &ctx.rules) {
            PermissionDecision::Allow => true,
            // No interactive surface in a subagent → Ask becomes Deny (safe default).
            PermissionDecision::Deny | PermissionDecision::Ask => false,
        };
    let (result, status) = if allowed {
        match tool.run(&call.args).await {
            Ok(out) => (out, "ok"),
            Err(e) => (format!("error: {e}"), "error"),
        }
    } else {
        ("permission denied by policy".to_string(), "error")
    };
    ctx.store.record_tool_call(
        msg_id,
        &call.name,
        &args_json,
        &result,
        if allowed { "allowed" } else { "denied" },
        status,
    )?;
    Ok(result)
}

/// Sum the token/cost usage of a list of usages (helper for rollups).
pub fn sum_usage(items: impl IntoIterator<Item = Usage>) -> Usage {
    items.into_iter().fold(Usage::default(), |mut acc, u| {
        acc.input_tokens += u.input_tokens;
        acc.output_tokens += u.output_tokens;
        acc.cost_usd += u.cost_usd;
        acc
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_requests_reads_agent_and_task_with_inline_default() {
        let args = json!({"agents": [
            {"agent": "reviewer", "task": "review the diff"},
            {"task": "find all call sites of foo"}
        ]});
        let reqs = parse_requests(&args, 8).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].agent, "reviewer");
        assert_eq!(reqs[1].agent, "general"); // omitted agent → general inline
        assert_eq!(reqs[1].task, "find all call sites of foo");
    }

    #[test]
    fn parse_requests_caps_at_max_agents() {
        let agents: Vec<_> = (0..20).map(|i| json!({"task": format!("t{i}")})).collect();
        let reqs = parse_requests(&json!({ "agents": agents }), 3).unwrap();
        assert_eq!(reqs.len(), 3, "must not exceed max_agents");
    }

    #[test]
    fn parse_requests_rejects_empty_or_taskless() {
        assert!(parse_requests(&json!({"agents": []}), 8).is_err());
        assert!(parse_requests(&json!({"agents": [{"agent": "x"}]}), 8).is_err());
        assert!(parse_requests(&json!({"nope": 1}), 8).is_err());
    }

    #[test]
    fn subagents_never_get_the_spawn_tool_depth_guard() {
        // Structural depth-1 guard: a child's toolset excludes spawn_agents, so it cannot recurse.
        assert!(!SUBAGENT_TOOLS.contains(&SPAWN_AGENTS_TOOL));
        // And the read-only set is exactly investigation tools (no write/shell).
        assert_eq!(SUBAGENT_TOOLS, &["read_file", "list_dir", "search"]);
    }
}
