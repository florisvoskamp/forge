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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forge_config::{AgentDef, Config};
use forge_mesh::pricing::Pricing;
use forge_mesh::{BudgetState, Router, RoutingDecision};
use forge_provider::{Provider, StreamEvent, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_types::{
    Message, PermissionDecision, PermissionMode, PermissionRule, Role, SideEffect, TaskTier, Usage,
};
use serde_json::Value;

use crate::{permission, worktree, CoreError};

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
    /// When worktree isolation is active for this child, the root of its isolated worktree.
    /// `None` for the top-level context and for read-only children.
    pub worktree_root: Option<PathBuf>,
    /// The git repo root for this session (used to create/merge worktrees).
    pub repo_root: PathBuf,
}

/// Returns `true` when any of the agent's resolved tools is write- or shell-capable, meaning
/// concurrent execution could corrupt the shared working tree.
pub fn is_write_capable(agent: &ResolvedAgent, registry: &ToolRegistry) -> bool {
    let tool_names: Vec<&str> = if agent.tools.is_empty() {
        SUBAGENT_TOOLS.to_vec()
    } else {
        agent.tools.iter().map(String::as_str).collect()
    };
    tool_names.iter().any(|name| {
        registry
            .get(name)
            .map(|t| matches!(t.side_effect(), SideEffect::Write | SideEffect::Shell))
            .unwrap_or(false)
    })
}

/// Rewrite tool call arguments so that relative or absent paths/cwd are rooted inside the
/// isolated `worktree_root`. Absolute paths that already point outside the root are left alone.
/// - For `path` args: if the value is relative, make it absolute under `worktree_root`.
/// - For `cwd` args on shell calls: if absent or relative, set it to `worktree_root`.
pub fn rewrite_args_for_worktree(args: &Value, worktree_root: &Path) -> Value {
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    let mut out = map.clone();

    // Rewrite "path" field.
    if let Some(Value::String(p)) = out.get("path") {
        let pb = Path::new(p);
        if pb.is_relative() {
            let abs = worktree_root.join(pb);
            out.insert(
                "path".into(),
                Value::String(abs.to_string_lossy().into_owned()),
            );
        }
    }

    // Rewrite "cwd" field (shell tool); inject worktree_root when absent.
    match out.get("cwd") {
        None => {
            out.insert(
                "cwd".into(),
                Value::String(worktree_root.to_string_lossy().into_owned()),
            );
        }
        Some(Value::String(cwd)) if Path::new(cwd).is_relative() => {
            let abs = worktree_root.join(cwd);
            out.insert(
                "cwd".into(),
                Value::String(abs.to_string_lossy().into_owned()),
            );
        }
        _ => {}
    }

    Value::Object(out)
}

/// The result of running one child agent.
pub struct SubagentOutcome {
    pub final_text: String,
    pub ok: bool,
}

/// Run one child agent to completion against `child_id` (a persisted child session): route the
/// task independently, run the model↔tool loop with read-only tools, persist messages + usage
/// to the child session (so its cost rolls into the shared budget), and return the answer.
/// Route one child's task through the mesh (deterministic, no model API call): an agent type may
/// pin a tier; otherwise the task is routed around currently-benched models. Used both inside
/// [`run_subagent`] and by [`orchestrate`] up front, so the live panel can show the child's model
/// the moment it starts (not only when it finishes).
pub async fn route_child(
    ctx: &AgentCtx,
    agent: &ResolvedAgent,
    budget: BudgetState,
) -> RoutingDecision {
    match agent
        .tier
        .and_then(|t| ctx.config.model_for(t).map(|m| (t, m)))
    {
        Some((tier, model)) => RoutingDecision {
            tier,
            model: model.to_string(),
            rationale: format!("pinned by agent type '{}'", agent.name),
            fallbacks: Vec::new(),
        },
        // Route around benched models too (model-health-failover): a child still avoids a
        // model the parent just rate-limited.
        None => {
            let health = ctx.store.current_benched().unwrap_or_default();
            let quota = ctx
                .store
                .current_quota()
                .unwrap_or_default()
                .with_plans(ctx.config.mesh.subscriptions.clone())
                .with_conserve(ctx.config.mesh.subscription_conserve);
            let project = crate::project_context::compute(&ctx.repo_root);
            ctx.router
                .route(&agent.task, budget, &health, &quota, None, &project)
                .await
        }
    }
}

pub async fn run_subagent(
    ctx: &AgentCtx,
    child_id: &str,
    agent: &ResolvedAgent,
    // Routed ONCE by the caller (orchestrate) — that's the model its per-provider concurrency permit
    // was acquired for. Re-routing here could pick a DIFFERENT provider (if another child benched the
    // first in between), so the child would hold provider A's permit while hammering provider B,
    // silently bypassing B's cap. Take the decision as a parameter to keep route↔permit consistent.
    decision: RoutingDecision,
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

    // `decision` is now a parameter (routed once by the caller); no second route_child here.
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

    // Failover (model-health-failover): subagents fail over down the routed chain too, so a
    // child whose model rate-limits/stalls doesn't kill the whole spawn.
    let failover_enabled = ctx.config.mesh.failover;
    let default_cooldown = std::time::Duration::from_secs(ctx.config.mesh.failover_cooldown_secs);
    let mut chain = decision.fallbacks.clone().into_iter();
    let mut active_model = decision.model.clone();

    let max_steps = ctx.config.mesh.max_steps.max(1);
    for step in 0..max_steps {
        // Forward the child's streamed deltas so the orchestrator can show live per-child
        // activity (RFC subagent-orchestration Phase 3b), with transparent failover.
        let mut resp = loop {
            let mut sink = |ev: StreamEvent| on_delta(ev);
            match ctx
                .provider
                .complete(&active_model, &transcript, &specs, &mut sink)
                .await
            {
                Ok(r) => break r,
                Err(e) if failover_enabled && e.is_retryable() => {
                    let _ = ctx.store.bench_for(
                        &active_model,
                        e.cooldown(default_cooldown),
                        e.reason(),
                    );
                    match chain.next() {
                        Some(next) => {
                            tracing::debug!("subagent failover {active_model} -> {next}: {e}");
                            active_model = next;
                            continue;
                        }
                        None => return Err(e.into()),
                    }
                }
                Err(e) => return Err(e.into()),
            }
        };
        resp.usage.cost_usd = ctx.pricing.cost_for(
            &active_model,
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
            Some(&active_model),
            &resp.tool_calls,
            None,
        )?;
        if step == 0 {
            ctx.store
                .record_routing(&msg_id, decision.tier, &active_model, &decision.rationale)?;
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
            // Last outcome wins, not a permanent latch: a child that hits one failing tool call
            // but RECOVERS with a later successful one is not a failed child. (Seen live in a
            // workflow run: an agent's first read_file errored on a bad path, it recovered by
            // reading the right file and answering well — the old latch still marked it ✗, while
            // siblings that failed without ever touching a tool showed ✓.)
            ok = !(result.starts_with("error:") || result.starts_with("permission denied"));
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

    // The loop ended without the model producing a final answer (it kept calling tools until the
    // step cap). Don't report that as an empty SUCCESS — the parent model would assemble a blank
    // `[agent N]` block and proceed as if the child finished its task.
    if final_text.is_empty() {
        ok = false;
        final_text = format!(
            "error: subagent hit the {max_steps}-step limit without producing a final answer"
        );
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
            // Nested spawns start without a pre-allocated worktree; orchestrate will create
            // one per child if worktree_isolation is enabled.
            worktree_root: None,
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
        /// The model this child routed to (shown in the live panel from the moment it starts).
        model: &'a str,
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
    use tokio::sync::{mpsc, Mutex, Semaphore};

    let mode_label = format!("{:?}", ctx.mode);
    let n = requests.len();
    let sem = Arc::new(Semaphore::new(max_concurrency.max(1)));
    let (tx, mut rx) = mpsc::unbounded_channel::<ChildMsg>();
    // Indexed by request position (not push order) so a mid-batch skip below can't desync `ids`
    // from the `index` values already-spawned children send back over `tx`.
    let mut ids: Vec<String> = vec![String::new(); n];
    let mut all_ok = true;

    // Serialize merge-back across concurrently-finishing children so `git apply` doesn't race on
    // the index. One merge at a time is fine: the patch itself is generated from the branch diff,
    // not the index, so ordering is deterministic.
    let merge_lock: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

    let isolation_enabled = ctx.config.mesh.subagents.worktree_isolation;
    // Emit a one-time warning when isolation is requested but the cwd is not a git repo.
    let repo_is_git = if isolation_enabled {
        let ok = worktree::is_git_repo(&ctx.repo_root);
        if !ok {
            tracing::warn!(
                "worktree_isolation is enabled but {:?} is not a git repo — \
                 running without worktree isolation",
                ctx.repo_root
            );
        }
        ok
    } else {
        false
    };

    let full_registry = ToolRegistry::with_core_tools();

    // Per-provider concurrency sub-cap: a burst of children all routed to ONE provider (a
    // claude/codex subscription bridge, or a single metered key) would otherwise run in parallel up
    // to the global `max_concurrency` and hammer that one quota. Each child also acquires a permit
    // from its provider's semaphore (sized by `max_per_provider`), so same-provider fan-out is
    // throttled while different providers still run in parallel. `0` disables the sub-cap.
    let max_per_provider = ctx.config.mesh.subagents.max_per_provider;
    let mut provider_sems: std::collections::HashMap<String, Arc<Semaphore>> =
        std::collections::HashMap::new();

    // Create each child session + announce Start up front (so a UI shows the whole batch as
    // running immediately), then spawn the work bounded by a concurrency permit.
    for (i, req) in requests.into_iter().enumerate() {
        let resolved = resolve(&req, &ctx.agents);
        // A `?` here would abort the whole function — dropping `rx` and silently discarding the
        // results (and spend) of every sibling child already `tokio::spawn`ed above. Skip just
        // this request instead so the rest of the batch still runs and reports.
        let child_id = match ctx.store.create_child_session(".", &mode_label, parent_id) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    "create_child_session failed for subagent request {i}: {e} — skipping this \
                     agent, other children continue"
                );
                all_ok = false;
                continue;
            }
        };
        // Route up front (deterministic, no API call) so the panel shows the model immediately AND
        // the child runs against the SAME model its provider permit is acquired for (routed once).
        let decision = route_child(ctx, &resolved, budget).await;
        let child_model = decision.model.clone();
        on_event(Lifecycle::Start {
            id: &child_id,
            agent: &resolved.name,
            task: &resolved.task,
            model: &child_model,
        });
        ids[i] = child_id.clone();

        // Resolve this child's provider semaphore (get-or-create), sized by `max_per_provider`.
        // Done on the orchestrator (the routing model is already known) so the task just acquires it.
        let provider_sem = if max_per_provider > 0 {
            let provider = forge_config::provider_of(&child_model).to_string();
            Some(Arc::clone(provider_sems.entry(provider).or_insert_with(
                || Arc::new(Semaphore::new(max_per_provider)),
            )))
        } else {
            None
        };

        // Decide whether this child gets an isolated worktree. We create the WorktreeGuard here
        // (on the orchestrator task, before spawning) so any creation error is visible immediately
        // and doesn't kill an already-running child task.
        let maybe_guard =
            if isolation_enabled && repo_is_git && is_write_capable(&resolved, &full_registry) {
                match worktree::WorktreeGuard::create(&ctx.repo_root, &child_id) {
                    Ok(g) => Some(g),
                    Err(e) => {
                        tracing::warn!(
                        "worktree create failed for {child_id}: {e} — running without isolation"
                    );
                        None
                    }
                }
            } else {
                None
            };

        // Build the child context, injecting the worktree root when applicable.
        let child_ctx = AgentCtx {
            worktree_root: maybe_guard.as_ref().map(|g| g.path().to_path_buf()),
            ..ctx.clone()
        };

        let tx = tx.clone();
        let sem = Arc::clone(&sem);
        let merge_lock = Arc::clone(&merge_lock);
        let repo_root = ctx.repo_root.clone();
        tokio::spawn(async move {
            // Acquire the provider sub-cap FIRST (block here without holding a global permit, so a
            // saturated provider can't head-of-line-block children bound for OTHER providers), then
            // the global concurrency permit. Both are held for the child's lifetime.
            let _provider_permit = match provider_sem {
                Some(ps) => ps.acquire_owned().await.ok(),
                None => None,
            };
            let _permit = sem.acquire_owned().await;
            // Forward streamed text/reasoning as live progress for this child's UI row.
            let mut on_delta = |ev: StreamEvent| {
                let snippet = match ev {
                    StreamEvent::Text(t) | StreamEvent::Reasoning(t) => t,
                    _ => return,
                };
                let _ = tx.send(ChildMsg::Progress { index: i, snippet });
            };
            let outcome = run_subagent(
                &child_ctx,
                &child_id,
                &resolved,
                decision,
                budget,
                &mut on_delta,
            )
            .await;
            let (mut text, mut ok) = match outcome {
                Ok(out) => (out.final_text, out.ok),
                Err(e) => (format!("error: subagent failed: {e}"), false),
            };

            // Merge the child's worktree changes back into the main tree (serialized).
            if let Some(guard) = maybe_guard {
                let branch = guard.branch().to_string();
                // Hold the merge lock for the duration of the git apply so concurrent finishers
                // don't race on the index. Drop guard AFTER merge so the branch still exists.
                let _lock = merge_lock.lock().await;
                match worktree::merge_worktree_back(&repo_root, &branch) {
                    Ok(report) if report.conflicted_files.is_empty() => {
                        // Clean merge — nothing to add.
                    }
                    Ok(report) => {
                        let conflicts = report.conflicted_files.join(", ");
                        text.push_str(&format!("\n[worktree merge conflicts in: {conflicts}]"));
                        ok = false;
                    }
                    Err(e) => {
                        tracing::warn!("merge_worktree_back failed for {child_id}: {e}");
                        text.push_str(&format!("\n[worktree merge failed: {e}]"));
                        ok = false;
                    }
                }
                // Drop the guard now (removes worktree dir + branch).
                drop(guard);
            }

            let cost = child_ctx.store.session_cost(&child_id).unwrap_or(0.0);
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
        // When this child has an isolated worktree, rewrite path/cwd args for write/shell tools
        // so the child's operations stay inside its worktree rather than touching the shared tree.
        let effective_args = if let Some(root) = &ctx.worktree_root {
            if matches!(side_effect, SideEffect::Write | SideEffect::Shell) {
                rewrite_args_for_worktree(&call.args, root)
            } else {
                call.args.clone()
            }
        } else {
            call.args.clone()
        };
        match tool.run(&effective_args).await {
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

    // --- Subagent failover (model-health-failover): a child whose model rate-limits must
    // fail over down its chain, not die — the bug the user hit ("run a testing task" → the
    // spawned child 429'd). ---

    struct FlakyProvider {
        bad: std::collections::HashSet<String>,
    }
    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        async fn complete(
            &self,
            model: &str,
            _messages: &[forge_types::Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            if self.bad.contains(model) {
                return Err(forge_provider::ProviderError::RateLimited {
                    message: "429".into(),
                    retry_after: Some(std::time::Duration::from_secs(30)),
                });
            }
            Ok(forge_provider::ModelResponse {
                content: "child done".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    struct FixedRouter {
        model: String,
        fallbacks: Vec<String>,
    }
    #[async_trait::async_trait]
    impl Router for FixedRouter {
        async fn route(
            &self,
            _p: &str,
            _b: BudgetState,
            _h: &forge_types::ModelHealth,
            _q: &forge_types::SubscriptionQuota,
            _effort: Option<forge_types::EffortLevel>,
            _project: &forge_types::ProjectContext,
        ) -> RoutingDecision {
            RoutingDecision {
                tier: TaskTier::Standard,
                model: self.model.clone(),
                rationale: "test".into(),
                fallbacks: self.fallbacks.clone(),
            }
        }
    }

    fn ctx_with(
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
        store: Arc<Store>,
    ) -> AgentCtx {
        let config = Config::default();
        let pricing = Pricing::from_config(&config);
        AgentCtx {
            provider,
            router,
            store,
            config,
            pricing,
            mode: PermissionMode::default(),
            rules: Vec::new(),
            depth: 1,
            max_depth: 2,
            agents: Arc::new(HashMap::new()),
            worktree_root: None,
            repo_root: std::path::PathBuf::from("."),
        }
    }

    #[tokio::test]
    async fn subagent_fails_over_when_its_model_rate_limits() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let child = store
            .create_child_session(".", "default", "parent")
            .unwrap();
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec!["good::model".into()],
        });
        let ctx = ctx_with(provider, router, Arc::clone(&store));
        let agent = ResolvedAgent {
            name: "general".into(),
            task: "run a testing task".into(),
            system_prompt: "you are a subagent".into(),
            tools: Vec::new(),
            tier: None,
        };
        let mut sink = |_: StreamEvent| {};
        let decision = route_child(&ctx, &agent, BudgetState::default()).await;
        let out = run_subagent(
            &ctx,
            &child,
            &agent,
            decision,
            BudgetState::default(),
            &mut sink,
        )
        .await
        .expect("subagent must recover via failover, not error");
        assert!(out.ok, "subagent succeeded on the fallback");
        assert_eq!(out.final_text, "child done");
        assert!(
            store.current_benched().unwrap().is_benched("bad::model"),
            "the rate-limited model was benched"
        );
    }

    /// Emits: (0) a `read_file` on a nonexistent path → "error:" tool result, (1) a `read_file`
    /// on a real path → success, (2) a final answer — the recover-after-one-bad-call shape.
    struct RecoveringToolProvider {
        step: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for RecoveringToolProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[forge_types::Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            let step = self.step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let tool_calls = match step {
                0 => vec![forge_types::ToolCall {
                    id: "c0".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "/definitely/not/here-xyz"}),
                }],
                1 => vec![forge_types::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    args: serde_json::json!({"path": "Cargo.toml"}),
                }],
                _ => vec![],
            };
            let content = if tool_calls.is_empty() {
                "recovered fine".to_string()
            } else {
                String::new()
            };
            Ok(forge_provider::ModelResponse {
                content,
                tool_calls,
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    /// Regression (seen live in a workflow run): the old `ok` latch flipped false on ANY failing
    /// tool call and never reset, so a child whose first read errored on a bad path but which
    /// recovered with a successful read + good answer was still shown as ✗ — while siblings that
    /// failed without ever touching a tool showed ✓. Last outcome wins now.
    #[tokio::test]
    async fn a_child_that_recovers_from_one_failing_tool_call_is_not_marked_failed() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let child = store
            .create_child_session(".", "default", "parent")
            .unwrap();
        let provider = Arc::new(RecoveringToolProvider {
            step: std::sync::atomic::AtomicUsize::new(0),
        });
        let router = Arc::new(FixedRouter {
            model: "good::model".into(),
            fallbacks: vec![],
        });
        let ctx = ctx_with(provider, router, Arc::clone(&store));
        let agent = ResolvedAgent {
            name: "general".into(),
            task: "read the manifest".into(),
            system_prompt: "you are a subagent".into(),
            tools: Vec::new(),
            tier: None,
        };
        let mut sink = |_: StreamEvent| {};
        let decision = route_child(&ctx, &agent, BudgetState::default()).await;
        let out = run_subagent(
            &ctx,
            &child,
            &agent,
            decision,
            BudgetState::default(),
            &mut sink,
        )
        .await
        .unwrap();
        assert_eq!(out.final_text, "recovered fine");
        assert!(
            out.ok,
            "one failed tool call followed by a successful one is a recovery, not a failed child"
        );
    }

    #[test]
    fn is_write_capable_detects_write_tools() {
        let registry = ToolRegistry::with_core_tools();

        // An agent with write_file in its toolset is write-capable.
        let write_agent = ResolvedAgent {
            name: "writer".into(),
            task: "t".into(),
            system_prompt: "s".into(),
            tools: vec!["write_file".into()],
            tier: None,
        };
        assert!(is_write_capable(&write_agent, &registry));

        // An agent with only read_file is not.
        let read_agent = ResolvedAgent {
            name: "reader".into(),
            task: "t".into(),
            system_prompt: "s".into(),
            tools: vec!["read_file".into()],
            tier: None,
        };
        assert!(!is_write_capable(&read_agent, &registry));

        // An agent with shell is write-capable.
        let shell_agent = ResolvedAgent {
            name: "sheller".into(),
            task: "t".into(),
            system_prompt: "s".into(),
            tools: vec!["shell".into()],
            tier: None,
        };
        assert!(is_write_capable(&shell_agent, &registry));

        // An agent with empty tools falls back to SUBAGENT_TOOLS (read-only), so not write-capable.
        let default_agent = ResolvedAgent {
            name: "general".into(),
            task: "t".into(),
            system_prompt: "s".into(),
            tools: Vec::new(),
            tier: None,
        };
        assert!(!is_write_capable(&default_agent, &registry));
    }

    #[test]
    fn rewrite_args_for_worktree_rewrites_relative_path() {
        let root = std::path::Path::new("/work/tree");
        let args = json!({"path": "src/main.rs", "content": "fn main() {}"});
        let rewritten = rewrite_args_for_worktree(&args, root);
        // Compute the expected path the same way the impl joins it, so the assertion holds on both
        // unix ("/work/tree/src/main.rs") and Windows (backslash separators).
        let expected = root.join("src/main.rs").to_string_lossy().into_owned();
        assert_eq!(rewritten["path"].as_str().unwrap(), expected);
        // content field is untouched
        assert_eq!(rewritten["content"].as_str().unwrap(), "fn main() {}");
    }

    #[test]
    fn rewrite_args_for_worktree_leaves_absolute_path_alone() {
        let root = std::path::Path::new("/work/tree");
        let args = json!({"path": "/absolute/path/file.rs"});
        let rewritten = rewrite_args_for_worktree(&args, root);
        assert_eq!(
            rewritten["path"].as_str().unwrap(),
            "/absolute/path/file.rs"
        );
    }

    #[test]
    fn rewrite_args_for_worktree_injects_cwd_when_absent() {
        let root = std::path::Path::new("/work/tree");
        let args = json!({"cmd": "cargo test"});
        let rewritten = rewrite_args_for_worktree(&args, root);
        assert_eq!(rewritten["cwd"].as_str().unwrap(), "/work/tree");
    }

    #[test]
    fn rewrite_args_for_worktree_rewrites_relative_cwd() {
        let root = std::path::Path::new("/work/tree");
        let args = json!({"cmd": "ls", "cwd": "subdir"});
        let rewritten = rewrite_args_for_worktree(&args, root);
        let expected = root.join("subdir").to_string_lossy().into_owned();
        assert_eq!(rewritten["cwd"].as_str().unwrap(), expected);
    }

    #[test]
    fn rewrite_args_for_worktree_leaves_absolute_cwd_alone() {
        let root = std::path::Path::new("/work/tree");
        let args = json!({"cmd": "ls", "cwd": "/other/dir"});
        let rewritten = rewrite_args_for_worktree(&args, root);
        assert_eq!(rewritten["cwd"].as_str().unwrap(), "/other/dir");
    }

    // --- Provider-aware fan-out cap (competitor-gap #5): a burst of children all routed to ONE
    // provider must not run more than `max_per_provider` at once, so a single subscription/key
    // quota isn't hammered in parallel even when the global concurrency cap is higher. ---

    /// Records the peak number of `complete` calls in flight at once.
    struct ConcurrencyProbe {
        active: Arc<std::sync::atomic::AtomicUsize>,
        peak: Arc<std::sync::atomic::AtomicUsize>,
    }
    #[async_trait::async_trait]
    impl Provider for ConcurrencyProbe {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[forge_types::Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            // Hold the slot long enough that, without the cap, all children would overlap.
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            self.active.fetch_sub(1, SeqCst);
            Ok(forge_provider::ModelResponse {
                content: "child done".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    #[tokio::test]
    async fn per_provider_cap_throttles_same_provider_fanout() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let store = Arc::new(Store::open_in_memory().unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ConcurrencyProbe {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        });
        // Every child routes to the SAME provider (`openai::`), so they share one provider permit.
        let router = Arc::new(FixedRouter {
            model: "openai::gpt-test".into(),
            fallbacks: vec![],
        });
        let mut ctx = ctx_with(provider, router, store);
        ctx.config.mesh.subagents.max_per_provider = 1; // serialize same-provider children
        let requests: Vec<_> = (0..4)
            .map(|i| AgentRequest {
                agent: "general".into(),
                task: format!("t{i}"),
            })
            .collect();
        let mut sink = |_: Lifecycle| {};
        // Global cap 8 (>= the 4 children): only the PER-PROVIDER cap can hold them back.
        let (_out, ok) = orchestrate(
            &ctx,
            "parent",
            requests,
            BudgetState::default(),
            8,
            &mut sink,
        )
        .await
        .unwrap();
        assert!(ok, "all children should succeed");
        assert_eq!(
            peak.load(SeqCst),
            1,
            "max_per_provider=1 must serialize children sharing a provider (peak in-flight)"
        );
    }

    #[tokio::test]
    async fn per_provider_cap_disabled_lets_same_provider_fanout_run_parallel() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let store = Arc::new(Store::open_in_memory().unwrap());
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ConcurrencyProbe {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        });
        let router = Arc::new(FixedRouter {
            model: "openai::gpt-test".into(),
            fallbacks: vec![],
        });
        let mut ctx = ctx_with(provider, router, store);
        ctx.config.mesh.subagents.max_per_provider = 0; // disabled → only the global cap applies
        let requests: Vec<_> = (0..4)
            .map(|i| AgentRequest {
                agent: "general".into(),
                task: format!("t{i}"),
            })
            .collect();
        let mut sink = |_: Lifecycle| {};
        let (_out, ok) = orchestrate(
            &ctx,
            "parent",
            requests,
            BudgetState::default(),
            8,
            &mut sink,
        )
        .await
        .unwrap();
        assert!(ok);
        assert!(
            peak.load(SeqCst) > 1,
            "with the per-provider cap off and a high global cap, same-provider children run in parallel"
        );
    }
}
