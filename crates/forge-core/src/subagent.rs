//! Subagent orchestration (RFC subagent-orchestration): the `spawn_agents` tool lets the
//! top-level model delegate subtasks to **child agents**, each in its own isolated context
//! and **routed independently through the Model Mesh** — so a Complex parent can fan out
//! cheap Trivial children.
//!
//! `spawn_agents` is a *virtual tool*: it is advertised to the parent model but is not a
//! `forge_tools::Tool` (it needs the provider/router/store, which ordinary tools can't reach).
//! [`orchestrate`] is the presenter-agnostic driver; [`Session`](crate::Session) calls it for a
//! native API turn (lifecycle → TUI events) and `forge mcp-serve` calls it for a CLI-bridge
//! (claude/codex) turn (RFC subagent-orchestration Phase 3).
//!
//! Children run **concurrently** (bounded by `max_concurrency`), each as a persisted child
//! session linked to the parent. A child's toolset comes from its agent type (default:
//! read-only `read_file`/`list_dir`/`search`) and **never** includes `spawn_agents` — a
//! structural depth-1 guard against recursion. Named agent types load from `.forge/agents/*.md`
//! (system prompt + optional tool subset + optional pinned tier); unknown/inline agents use the
//! default read-only investigator and are mesh-routed.

use std::collections::HashMap;
use std::sync::Arc;

use forge_config::{AgentDef, Config};
use forge_mesh::pricing::Pricing;
use forge_mesh::{BudgetState, Router, RoutingDecision};
use forge_provider::{Provider, StreamEvent, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_types::{
    Message, PermissionDecision, PermissionMode, PermissionRule, Role, TaskTier, Usage,
};

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

/// A request resolved against the loaded agent types — owned so it can move into a spawned
/// task. A named agent supplies its system prompt / tool subset / pinned tier; an unknown or
/// inline (`general`) agent falls back to the default read-only investigator.
#[derive(Clone)]
pub struct ResolvedAgent {
    pub name: String,
    pub task: String,
    pub system_prompt: String,
    pub tools: Vec<String>,
    pub tier: Option<TaskTier>,
}

/// Resolve a parsed request against the loaded agent-type map (RFC subagent-orchestration Ph2).
pub fn resolve(req: &AgentRequest, agents: &HashMap<String, AgentDef>) -> ResolvedAgent {
    match agents.get(&req.agent) {
        Some(def) => ResolvedAgent {
            name: def.name.clone(),
            task: req.task.clone(),
            system_prompt: if def.system_prompt.is_empty() {
                SUBAGENT_SYSTEM.to_string()
            } else {
                def.system_prompt.clone()
            },
            tools: def.tools.clone(),
            tier: def.tier,
        },
        None => ResolvedAgent {
            name: req.agent.clone(),
            task: req.task.clone(),
            system_prompt: SUBAGENT_SYSTEM.to_string(),
            tools: Vec::new(),
            tier: None,
        },
    }
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
    /// Nesting level of *this* context's children (0 = the top-level turn's children). Children
    /// may themselves spawn iff `depth < max_depth` (RFC subagent-orchestration Phase 3c).
    pub depth: usize,
    pub max_depth: usize,
    /// Loaded agent types, shared so a recursing child can resolve named agents too.
    pub agents: Arc<HashMap<String, AgentDef>>,
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
    agent: &ResolvedAgent,
    budget: BudgetState,
    on_delta: &mut (dyn FnMut(StreamEvent) + Send),
) -> Result<SubagentOutcome, CoreError> {
    let task = agent.task.as_str();
    // The agent type may widen the toolset beyond read-only; writes/shell still pass the
    // permission gate (where Ask→Deny in a child). The child can itself spawn subagents only
    // while there is depth budget left (RFC subagent-orchestration Phase 3c).
    let full = ToolRegistry::with_core_tools();
    let can_recurse = ctx.depth < ctx.max_depth;
    let allowed: Vec<&str> = if agent.tools.is_empty() {
        SUBAGENT_TOOLS.to_vec()
    } else {
        agent
            .tools
            .iter()
            .map(String::as_str)
            .filter(|t| *t != SPAWN_AGENTS_TOOL)
            .collect()
    };
    let mut specs: Vec<ToolSpec> = allowed
        .iter()
        .filter_map(|name| full.get(name))
        .map(|t| ToolSpec {
            name: t.name().to_string(),
            description: t.description().to_string(),
            schema: t.schema(),
        })
        .collect();
    if can_recurse {
        specs.push(spawn_agents_spec(ctx.config.mesh.subagents.max_agents));
    }

    // Routing: an agent type may pin a tier; otherwise route the task through the mesh.
    let decision = match agent
        .tier
        .and_then(|t| ctx.config.model_for(t).map(|m| (t, m)))
    {
        Some((tier, model)) => RoutingDecision {
            tier,
            model: model.to_string(),
            rationale: format!("pinned by agent type '{}'", agent.name),
        },
        None => ctx.router.route(task, budget).await,
    };

    let mut transcript = vec![Message::system(&agent.system_prompt), Message::user(task)];
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
        // Forward the child's streamed deltas so the orchestrator can show live per-child
        // activity (RFC subagent-orchestration Phase 3b).
        let mut sink = |ev: StreamEvent| on_delta(ev);
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
            let result = if call.name == SPAWN_AGENTS_TOOL && can_recurse {
                // The child delegates further: recurse one level deeper (bounded by max_depth).
                // Grandchildren aren't shown in the live panel — they roll up into this result.
                run_nested_spawn(ctx.clone(), child_id.to_string(), call.args.clone(), budget).await
            } else {
                execute_tool(ctx, &full, &msg_id, call).await?
            };
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

/// A child agent delegating further: recurse one level deeper. Grandchildren are not surfaced
/// to the live UI (no-op lifecycle) — their results roll up into this child's tool result.
/// `Box::pin` breaks the orchestrate→run_subagent→orchestrate async-recursion type cycle.
/// Returns a **boxed** (concrete, non-opaque) future so it does not participate in the
/// orchestrate→run_subagent async-`impl Future` opaque-type cycle; `+ Send` asserts Send at the
/// boundary. Owned args so the future is `'static`.
fn run_nested_spawn(
    ctx: AgentCtx,
    parent_id: String,
    args: serde_json::Value,
    budget: BudgetState,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>> {
    Box::pin(async move {
        let max = ctx.config.mesh.subagents.max_agents;
        let requests = match parse_requests(&args, max) {
            Ok(r) => r,
            Err(e) => return format!("error: {e}"),
        };
        let deeper = AgentCtx {
            depth: ctx.depth + 1,
            ..ctx.clone()
        };
        let concurrency = ctx.config.mesh.subagents.max_concurrency;
        let mut noop = |_: Lifecycle| {};
        match orchestrate(
            &deeper,
            &parent_id,
            requests,
            budget,
            concurrency,
            &mut noop,
        )
        .await
        {
            Ok((combined, _)) => combined,
            Err(e) => format!("error: nested subagents failed: {e}"),
        }
    })
}

/// A message from a child task to the orchestrator's drain loop: a live activity delta, or the
/// final completion.
enum ChildMsg {
    Progress { index: usize, snippet: String },
    Done(ChildDone),
}

/// One subagent's completion, sent from its task back to the orchestrator's drain loop.
struct ChildDone {
    index: usize,
    id: String,
    agent: String,
    text: String,
    ok: bool,
    cost: f64,
}

/// A subagent lifecycle event, surfaced to whatever is driving the orchestration (the TUI
/// presenter for a native turn, or a headless logger for the CLI-bridge `mcp-serve` path).
pub enum Lifecycle<'a> {
    Start {
        id: &'a str,
        agent: &'a str,
        task: &'a str,
    },
    /// A live activity snippet (streamed text/reasoning) from a still-running child.
    Progress { id: &'a str, snippet: &'a str },
    Done {
        id: &'a str,
        agent: &'a str,
        ok: bool,
        summary: &'a str,
        cost_usd: f64,
    },
}

/// Run a batch of subagents concurrently (bounded by `max_concurrency`) under `parent_id`,
/// reporting each child's lifecycle through `on_event`, and return the combined labeled result
/// plus whether all succeeded. Presenter-agnostic so both [`crate::Session`] (TUI events) and
/// `forge mcp-serve` (the CLI-bridge path) reuse one orchestrator (RFC subagent-orchestration).
#[allow(clippy::too_many_arguments)]
pub async fn orchestrate(
    ctx: &AgentCtx,
    parent_id: &str,
    requests: Vec<AgentRequest>,
    budget: BudgetState,
    max_concurrency: usize,
    on_event: &mut (dyn FnMut(Lifecycle) + Send),
) -> Result<(String, bool), CoreError> {
    use tokio::sync::{mpsc, Semaphore};

    let mode_label = format!("{:?}", ctx.mode);
    let n = requests.len();
    let sem = Arc::new(Semaphore::new(max_concurrency.max(1)));
    let (tx, mut rx) = mpsc::unbounded_channel::<ChildMsg>();
    let mut ids: Vec<String> = Vec::with_capacity(n);

    // Create each child session + announce Start up front (so a UI shows the whole batch as
    // running immediately), then spawn the work bounded by a concurrency permit.
    for (i, req) in requests.into_iter().enumerate() {
        let resolved = resolve(&req, &ctx.agents);
        let child_id = ctx
            .store
            .create_child_session(".", &mode_label, parent_id)?;
        on_event(Lifecycle::Start {
            id: &child_id,
            agent: &resolved.name,
            task: &resolved.task,
        });
        ids.push(child_id.clone());

        let ctx = ctx.clone();
        let tx = tx.clone();
        let sem = Arc::clone(&sem);
        tokio::spawn(async move {
            let _permit = sem.acquire_owned().await;
            // Forward streamed text/reasoning as live progress for this child's UI row.
            let mut on_delta = |ev: StreamEvent| {
                let snippet = match ev {
                    StreamEvent::Text(t) | StreamEvent::Reasoning(t) => t,
                    _ => return,
                };
                let _ = tx.send(ChildMsg::Progress { index: i, snippet });
            };
            let outcome = run_subagent(&ctx, &child_id, &resolved, budget, &mut on_delta).await;
            let (text, ok) = match outcome {
                Ok(out) => (out.final_text, out.ok),
                Err(e) => (format!("error: subagent failed: {e}"), false),
            };
            let cost = ctx.store.session_cost(&child_id).unwrap_or(0.0);
            let _ = tx.send(ChildMsg::Done(ChildDone {
                index: i,
                id: child_id,
                agent: resolved.name,
                text,
                ok,
                cost,
            }));
        });
    }
    drop(tx); // close the channel once every task holds its own clone

    let mut slots: Vec<Option<(String, String)>> = vec![None; n];
    let mut all_ok = true;
    while let Some(msg) = rx.recv().await {
        match msg {
            ChildMsg::Progress { index, snippet } => {
                on_event(Lifecycle::Progress {
                    id: &ids[index],
                    snippet: &snippet,
                });
            }
            ChildMsg::Done(done) => {
                all_ok &= done.ok;
                on_event(Lifecycle::Done {
                    id: &done.id,
                    agent: &done.agent,
                    ok: done.ok,
                    summary: &summary(&done.text),
                    cost_usd: done.cost,
                });
                slots[done.index] = Some((done.agent, done.text));
            }
        }
    }

    let mut combined = String::new();
    for (i, slot) in slots.into_iter().enumerate() {
        let (agent, text) = slot.unwrap_or_else(|| ("?".into(), "error: no result".into()));
        combined.push_str(&format!("[agent {}: {}]\n{}\n\n", i + 1, agent, text));
    }
    Ok((combined.trim_end().to_string(), all_ok))
}

/// First non-empty line of a result, truncated — a one-line summary for lifecycle events.
fn summary(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.chars().take(120).collect()
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
    fn resolve_applies_named_agent_type_and_defaults_unknown() {
        let mut agents = HashMap::new();
        agents.insert(
            "reviewer".to_string(),
            AgentDef {
                name: "reviewer".into(),
                description: "d".into(),
                tools: vec!["read_file".into()],
                tier: Some(TaskTier::Complex),
                system_prompt: "You review.".into(),
            },
        );
        let named = resolve(
            &AgentRequest {
                agent: "reviewer".into(),
                task: "look at the diff".into(),
            },
            &agents,
        );
        assert_eq!(named.system_prompt, "You review.");
        assert_eq!(named.tools, vec!["read_file"]);
        assert_eq!(named.tier, Some(TaskTier::Complex));

        let unknown = resolve(
            &AgentRequest {
                agent: "general".into(),
                task: "t".into(),
            },
            &agents,
        );
        assert!(unknown.tools.is_empty()); // → default read-only set
        assert_eq!(unknown.tier, None); // → mesh-routed
        assert_eq!(unknown.system_prompt, SUBAGENT_SYSTEM);
    }

    #[test]
    fn subagents_never_get_the_spawn_tool_depth_guard() {
        // Structural depth-1 guard: a child's toolset excludes spawn_agents, so it cannot recurse.
        assert!(!SUBAGENT_TOOLS.contains(&SPAWN_AGENTS_TOOL));
        // And the read-only set is exactly investigation tools (no write/shell).
        assert_eq!(SUBAGENT_TOOLS, &["read_file", "list_dir", "search"]);
    }
}
