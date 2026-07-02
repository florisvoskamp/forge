//! Workflow scripts (docs/rfcs/forge-workflow.md): `run_workflow` lets the top-level model author
//! a JS script that a deterministic runtime (the sandboxed `forge_workflow` rquickjs engine)
//! executes, fanning mesh-routed child agents out with real concurrency — the same efficiency
//! property a compiled script gives over re-invoking the model for every control-flow decision.
//!
//! Only ONE real execution primitive is implemented in Rust: `agent(prompt, opts)`, a single
//! mesh-routed child call (reusing `subagent::route_child`/`run_subagent` verbatim). `parallel()`
//! and `pipeline()` are NOT separate Rust primitives — they're pure JS compositions over `agent()`
//! (see [`PRELUDE`]), exactly mirroring how those same primitives work conceptually in the
//! reference Workflow-tool design: `parallel` is `Promise.all`, `pipeline` is "each item's own
//! async closure runs stage-by-stage, `Promise.all` just waits for all of them" — JS's own event
//! loop already gives the "no barrier between items" property for free. This is simpler and more
//! maintainable than reimplementing that concurrency shape in Rust, and is invisible to the
//! authoring model either way (it calls `pipeline(items, stage1, stage2)` exactly the same).
//!
//! Concurrency (the global `Semaphore` + per-provider `HashMap<String, Arc<Semaphore>>`) is built
//! ONCE per `run_workflow` call and shared across every `agent()` call the script makes — a
//! `parallel()` in phase 1 and a `pipeline()` in phase 2 draw from the same real budget, not two
//! independent ones.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use forge_mesh::BudgetState;
use forge_provider::{StreamEvent, ToolSpec};
use tokio::sync::{mpsc, Mutex, Semaphore};

use crate::subagent::{self, AgentCtx};

/// The virtual tool name the parent model calls to run a workflow script.
pub const RUN_WORKFLOW_TOOL: &str = "run_workflow";

/// Standing per-session guidance injected while `/effort whitehot` is pinned
/// (docs/features/whitehot-effort.md): maximum reasoning intensity plus automatic workflow
/// orchestration wherever the task has real multi-part structure.
pub const WHITEHOT_GUIDANCE: &str = "White-hot effort is active — work at maximum thoroughness. \
(1) For any task with three or more independent subtasks, several files/items to process, or \
distinct research → build → verify stages, orchestrate it as a `run_workflow` script: `phase()` \
per stage, `parallel()`/`pipeline()` for fan-out, and a final verification phase where separate \
agents adversarially check the important results before you rely on them. \
(2) Write workflow scripts defensively: ask discovery agents for one item per line with no \
commentary and no code fences; split on newlines (never JSON.parse free-form prose); filter out \
empty or 'null' items before fanning out; never interpolate a possibly-null value into an agent \
prompt; always end the script with `return <final result>`. \
(3) Single-step tasks are answered directly — don't orchestrate trivia. \
(4) Verify claims against the actual code/files instead of assuming; prefer evidence from a \
checking agent over your own recall.";

/// The `ToolSpec` advertised to the parent so the model can call `run_workflow`.
pub fn run_workflow_spec() -> ToolSpec {
    ToolSpec {
        name: RUN_WORKFLOW_TOOL.to_string(),
        description: "Run a JS workflow script that dynamically fans work out to mesh-routed \
            child agents. EVERY function below returns a Promise and must be awaited (a bare \
            `log(...)` without `await` can race with the script finishing before it takes \
            effect). Available inside the script: `await agent(prompt, opts?)` runs one child \
            agent and returns its answer as a string (opts: {agent?: named agent type, phase?: a \
            one-off label overriding the ambient phase() for this call only}); `await \
            parallel(thunks)` runs an array of `() => Promise` thunks concurrently and returns \
            their results; `await pipeline(items, stage1, stage2, ...)` runs each item through \
            every stage in order, independently (no barrier between items — item A can be on \
            stage 3 while item B is on stage 1), each stage called as `stage(prevResult, item, \
            index)`; `await phase(title)` labels every subsequent agent() call until the next \
            phase() call; `await log(message)` writes a note into the transcript; `await \
            workflow(name, args?)` runs a saved script from `.forge/workflows/<name>.js`. Write \
            the script as a sequence of statements (it runs inside an async function) using real \
            control flow — loops, conditionals, accumulation across rounds — for genuinely \
            dynamic multi-step work; use `agent()` directly for a single subtask, `spawn_agents` \
            for simple one-shot fan-out. ALWAYS end the script with `return <final result>` — \
            that value becomes this tool's result (a script that returns nothing falls back to \
            an auto-generated digest of the agents' summaries)."
            .to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "script": {
                    "type": "string",
                    "description": "the workflow script body (a sequence of JS statements, run inside an async function)"
                }
            },
            "required": ["script"]
        }),
    }
}

/// The JS control-flow compositions over `agent()` — see the module doc for why these are pure
/// JS, not Rust primitives. Prepended to every script before it runs.
const PRELUDE: &str = r#"
function parallel(thunks) { return Promise.all(thunks.map((fn) => fn())); }
async function pipeline(items, ...stages) {
    return Promise.all(items.map(async (item, index) => {
        let prev = null;
        for (const stage of stages) {
            prev = await stage(prev, item, index);
        }
        return prev;
    }));
}
"#;

/// A lifecycle event from the script's execution, surfaced the same way `subagent::Lifecycle`
/// is — converted to `PresenterEvent`s by the caller (`Session::run_workflow`), which owns the
/// presenter and drains these on its own task while the script runs concurrently on another.
#[derive(Debug)]
pub enum WorkflowEvent {
    AgentStart {
        id: String,
        agent: String,
        task: String,
        model: String,
        /// The active `phase()` label (or `opts.phase` override) at the time this agent started,
        /// if any — a real field (not embedded in `task`) so the TUI can group by it.
        phase: Option<String>,
    },
    AgentProgress {
        id: String,
        snippet: String,
    },
    AgentDone {
        id: String,
        agent: String,
        ok: bool,
        summary: String,
        cost_usd: f64,
    },
    /// A `phase()` transition — the ambient label for subsequent agents changed. A dedicated
    /// variant (not a `Log` line) so the workflow view can build its phase tree from it directly,
    /// showing a phase the moment it starts rather than only once its first agent spawns.
    Phase(String),
    /// A `log()` call — the raw message; presenters add their own glyph/styling.
    Log(String),
}

/// Shared state for one `run_workflow` call — closed over by every registered host function.
struct WorkflowState {
    ctx: AgentCtx,
    parent_id: String,
    budget: BudgetState,
    sem: Arc<Semaphore>,
    provider_sems: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    max_per_provider: usize,
    current_phase: Arc<StdMutex<Option<String>>>,
    tx: mpsc::UnboundedSender<WorkflowEvent>,
    agent_counter: Arc<AtomicUsize>,
    max_total_agents: usize,
    workflows_dir: PathBuf,
}

impl WorkflowState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ctx: AgentCtx,
        parent_id: String,
        budget: BudgetState,
        max_concurrency: usize,
        max_per_provider: usize,
        max_total_agents: usize,
        workflows_dir: PathBuf,
        tx: mpsc::UnboundedSender<WorkflowEvent>,
    ) -> Self {
        WorkflowState {
            ctx,
            parent_id,
            budget,
            sem: Arc::new(Semaphore::new(max_concurrency.max(1))),
            provider_sems: Arc::new(Mutex::new(HashMap::new())),
            max_per_provider,
            current_phase: Arc::new(StdMutex::new(None)),
            tx,
            agent_counter: Arc::new(AtomicUsize::new(0)),
            max_total_agents,
            workflows_dir,
        }
    }

    /// A nested `workflow()` call shares every Arc'd resource (same concurrency budget, same
    /// event channel) with the parent, but runs one level deeper (bounded by `max_depth`, same
    /// structural guard `subagent.rs` uses for `spawn_agents` recursion).
    fn nested(&self) -> Self {
        WorkflowState {
            ctx: AgentCtx {
                depth: self.ctx.depth + 1,
                ..self.ctx.clone()
            },
            parent_id: self.parent_id.clone(),
            budget: self.budget,
            sem: Arc::clone(&self.sem),
            provider_sems: Arc::clone(&self.provider_sems),
            max_per_provider: self.max_per_provider,
            current_phase: Arc::clone(&self.current_phase),
            tx: self.tx.clone(),
            agent_counter: Arc::clone(&self.agent_counter),
            max_total_agents: self.max_total_agents,
            workflows_dir: self.workflows_dir.clone(),
        }
    }
}

/// First non-empty line of a result, truncated — mirrors `subagent::summary`'s one-line style.
pub(crate) fn summary(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    line.chars().take(120).collect()
}

fn agent_host_fn(state: Arc<WorkflowState>) -> forge_workflow::HostFunction {
    forge_workflow::HostFunction::new("agent", move |args| {
        let state = Arc::clone(&state);
        async move {
            let prompt = args
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if prompt.is_empty() {
                return Err("agent() requires a non-empty prompt string".to_string());
            }
            let opts = args.get(1);
            let agent_name = opts
                .and_then(|o| o.get("agent"))
                .and_then(|a| a.as_str())
                .unwrap_or("general")
                .to_string();

            let n = state.agent_counter.fetch_add(1, Ordering::SeqCst) + 1;
            if n > state.max_total_agents {
                return Err(format!(
                    "workflow exceeded its {}-agent safety cap",
                    state.max_total_agents
                ));
            }

            let req = subagent::AgentRequest {
                agent: agent_name,
                task: prompt,
            };
            let resolved = subagent::resolve(&req, &state.ctx.agents);

            let mode_label = format!("{:?}", state.ctx.mode);
            let child_id = state
                .ctx
                .store
                .create_child_session(".", &mode_label, &state.parent_id)
                .map_err(|e| format!("failed to create child session: {e}"))?;

            let decision = subagent::route_child(&state.ctx, &resolved, state.budget).await;
            let model = decision.model.clone();

            // `opts.phase` overrides the ambient `phase()` label for this one call only.
            let phase = opts
                .and_then(|o| o.get("phase"))
                .and_then(|p| p.as_str())
                .map(str::to_string)
                .or_else(|| state.current_phase.lock().unwrap().clone())
                .filter(|p| !p.is_empty());
            let _ = state.tx.send(WorkflowEvent::AgentStart {
                id: child_id.clone(),
                agent: resolved.name.clone(),
                task: resolved.task.clone(),
                model: model.clone(),
                phase: phase.clone(),
            });

            // Same ordering as `orchestrate()`: acquire the provider sub-cap FIRST (without
            // holding the global permit), then the global cap — a saturated provider can't
            // head-of-line-block agent() calls bound for other providers.
            let provider_permit = if state.max_per_provider > 0 {
                let provider = forge_config::provider_of(&model).to_string();
                let sem = {
                    let mut sems = state.provider_sems.lock().await;
                    Arc::clone(
                        sems.entry(provider)
                            .or_insert_with(|| Arc::new(Semaphore::new(state.max_per_provider))),
                    )
                };
                sem.acquire_owned().await.ok()
            } else {
                None
            };
            let global_permit = state.sem.clone().acquire_owned().await;

            let tx = state.tx.clone();
            let id_for_progress = child_id.clone();
            let mut on_delta = |ev: StreamEvent| {
                let snippet = match ev {
                    StreamEvent::Text(t) | StreamEvent::Reasoning(t) => t,
                    _ => return,
                };
                let _ = tx.send(WorkflowEvent::AgentProgress {
                    id: id_for_progress.clone(),
                    snippet,
                });
            };
            let outcome = subagent::run_subagent(
                &state.ctx,
                &child_id,
                &resolved,
                decision,
                state.budget,
                &mut on_delta,
            )
            .await;
            drop(provider_permit);
            drop(global_permit);

            let (text, ok) = match outcome {
                Ok(out) => (out.final_text, out.ok),
                Err(e) => (format!("error: subagent failed: {e}"), false),
            };
            let cost = state.ctx.store.session_cost(&child_id).unwrap_or(0.0);
            let _ = state.tx.send(WorkflowEvent::AgentDone {
                id: child_id,
                agent: resolved.name,
                ok,
                summary: summary(&text),
                cost_usd: cost,
            });
            Ok(serde_json::Value::String(text))
        }
    })
}

fn log_host_fn(state: Arc<WorkflowState>) -> forge_workflow::HostFunction {
    forge_workflow::HostFunction::new("log", move |args| {
        let state = Arc::clone(&state);
        async move {
            let msg = args.first().and_then(|v| v.as_str()).unwrap_or_default();
            let _ = state.tx.send(WorkflowEvent::Log(msg.to_string()));
            Ok(serde_json::Value::Null)
        }
    })
}

fn phase_host_fn(state: Arc<WorkflowState>) -> forge_workflow::HostFunction {
    forge_workflow::HostFunction::new("phase", move |args| {
        let state = Arc::clone(&state);
        async move {
            let title = args
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            *state.current_phase.lock().unwrap() = if title.is_empty() {
                None
            } else {
                Some(title.clone())
            };
            if !title.is_empty() {
                let _ = state.tx.send(WorkflowEvent::Phase(title));
            }
            Ok(serde_json::Value::Null)
        }
    })
}

fn workflow_host_fn(state: Arc<WorkflowState>) -> forge_workflow::HostFunction {
    forge_workflow::HostFunction::new("workflow", move |args| {
        let state = Arc::clone(&state);
        async move {
            let name = args.first().and_then(|v| v.as_str()).unwrap_or_default();
            if name.is_empty() {
                return Err("workflow() requires a non-empty name".to_string());
            }
            // Sandboxed strictly to `.forge/workflows/<name>.js` — no path traversal, no
            // absolute paths, no arbitrary filesystem access from inside a script.
            if name.contains(['/', '\\']) || name.contains("..") {
                return Err(format!(
                    "workflow name '{name}' must be a plain filename with no path separators"
                ));
            }
            if state.ctx.depth >= state.ctx.max_depth {
                return Err("workflow() recursion depth limit reached".to_string());
            }
            let path = state.workflows_dir.join(format!("{name}.js"));
            let script = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("saved workflow '{name}' not found: {e}"))?;
            let saved_args = args.get(1).cloned().unwrap_or(serde_json::Value::Null);
            let script = inject_args(&script, &saved_args);

            let nested = Arc::new(state.nested());
            let host_fns = build_host_functions(nested);
            forge_workflow::run_script(host_fns, &wrap_with_prelude(&script)).await
        }
    })
}

/// Prepends `const args = <json>;` before a SAVED script's body, so it can reference the `args`
/// global if the author wants — used for `workflow(name, args)` and `/workflow run <name>
/// [args]`. A top-level `run_workflow` script (authored fresh by the model each turn) has no such
/// concept and never goes through this.
fn inject_args(script_body: &str, args: &serde_json::Value) -> String {
    let json = serde_json::to_string(args).unwrap_or_else(|_| "null".to_string());
    format!("const args = {json};\n{script_body}")
}

fn build_host_functions(state: Arc<WorkflowState>) -> Vec<forge_workflow::HostFunction> {
    vec![
        agent_host_fn(Arc::clone(&state)),
        log_host_fn(Arc::clone(&state)),
        phase_host_fn(Arc::clone(&state)),
        workflow_host_fn(state),
    ]
}

fn wrap_with_prelude(script_body: &str) -> String {
    format!("{PRELUDE}\n(async () => {{\n{script_body}\n}})")
}

/// Runs a workflow script end to end: builds the shared concurrency/event state, registers the
/// host functions, and executes the (prelude-wrapped) script. Returns the script's own return
/// value (JSON) and whether every `agent()` call it made succeeded.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    ctx: AgentCtx,
    parent_id: String,
    budget: BudgetState,
    max_concurrency: usize,
    max_per_provider: usize,
    max_total_agents: usize,
    workflows_dir: PathBuf,
    script_body: &str,
    mut on_event: impl FnMut(WorkflowEvent) + Send,
) -> Result<(serde_json::Value, bool), String> {
    let (tx, mut rx) = mpsc::unbounded_channel::<WorkflowEvent>();
    let state = Arc::new(WorkflowState::new(
        ctx,
        parent_id,
        budget,
        max_concurrency,
        max_per_provider,
        max_total_agents,
        workflows_dir,
        tx,
    ));
    let host_fns = build_host_functions(state);
    let script = wrap_with_prelude(script_body);

    let script_fut = forge_workflow::run_script(host_fns, &script);
    tokio::pin!(script_fut);

    let mut all_ok = true;
    // Every agent's (name, ok, one-line summary), kept so a script that returns nothing still
    // yields a real result (see `result_digest`).
    let mut agent_results: Vec<(String, bool, String)> = Vec::new();
    let mut note = |ev: &WorkflowEvent| {
        if let WorkflowEvent::AgentDone {
            agent, ok, summary, ..
        } = ev
        {
            if !*ok {
                all_ok = false;
            }
            agent_results.push((agent.clone(), *ok, summary.clone()));
        }
    };
    let result = loop {
        tokio::select! {
            biased;
            msg = rx.recv() => {
                if let Some(ev) = msg {
                    note(&ev);
                    on_event(ev);
                }
            }
            res = &mut script_fut => {
                break res;
            }
        }
    };
    // Drain anything buffered between the script's last event and its future resolving.
    while let Ok(ev) = rx.try_recv() {
        note(&ev);
        on_event(ev);
    }

    match result {
        // A script that ends without `return` resolves to `undefined` (JSON null) — seen live:
        // the authored script did all its work, the tool result became the literal string
        // "null", and the parent model relayed an empty answer. Substitute a digest of the
        // agents' own summaries so the run ALWAYS produces something relayable.
        Ok(value) if returned_nothing(&value) => Ok((
            serde_json::Value::String(result_digest(&agent_results)),
            all_ok,
        )),
        Ok(value) => Ok((value, all_ok)),
        Err(e) => {
            let mut msg = format!("error: {e}");
            // Seen live: a model authored a script calling `glob(...)` — a host function that
            // doesn't exist — and got a bare "glob is not defined". Spell out the real surface
            // so the model's retry uses it instead of guessing another phantom function.
            if msg.contains("is not defined") {
                msg.push_str(
                    "\nhint: the ONLY functions available inside a workflow script are \
                     agent(prompt, opts?), parallel(thunks), pipeline(items, ...stages), \
                     phase(title), log(message), and workflow(name, args?) — there is no \
                     glob/fs/require/fetch or any other API. Do discovery, file reading, and \
                     shell work through agent() calls.",
                );
            }
            Ok((serde_json::Value::String(msg), false))
        }
    }
}

/// `undefined`/`null`/whitespace-only — nothing a model (or the `/workflow run` finish note)
/// could relay.
fn returned_nothing(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.trim().is_empty(),
        _ => false,
    }
}

/// Fallback tool result for a script that returned nothing: a digest built from the agents'
/// own one-line summaries, so the parent model still has real content to relay.
fn result_digest(results: &[(String, bool, String)]) -> String {
    if results.is_empty() {
        return "The workflow script finished but returned no value and ran no agents.".to_string();
    }
    let failed = results.iter().filter(|(_, ok, _)| !ok).count();
    const MAX: usize = 30;
    let mut out = format!(
        "The workflow script finished but returned no value (end scripts with `return <result>`). \
         Digest of its {} agent result(s) ({} ok, {failed} failed):",
        results.len(),
        results.len() - failed,
    );
    for (i, (agent, ok, summary)) in results.iter().take(MAX).enumerate() {
        let mark = if *ok { "✓" } else { "✗" };
        out.push_str(&format!("\n{}. {mark} [{agent}] {summary}", i + 1));
    }
    if results.len() > MAX {
        out.push_str(&format!("\n… and {} more", results.len() - MAX));
    }
    out
}

/// Runs a SAVED `.forge/workflows/<name>.js` script directly — the `/workflow run <name> [args]`
/// path, which skips the authoring turn entirely (no LLM call to decide the script). Sandboxed
/// against path traversal exactly like the `workflow()` host function.
#[allow(clippy::too_many_arguments)]
pub async fn run_saved(
    ctx: AgentCtx,
    parent_id: String,
    budget: BudgetState,
    max_concurrency: usize,
    max_per_provider: usize,
    max_total_agents: usize,
    workflows_dir: PathBuf,
    name: &str,
    args: serde_json::Value,
    on_event: impl FnMut(WorkflowEvent) + Send,
) -> Result<(serde_json::Value, bool), String> {
    if name.is_empty() || name.contains(['/', '\\']) || name.contains("..") {
        return Err(format!(
            "workflow name '{name}' must be a non-empty plain filename with no path separators"
        ));
    }
    let path = workflows_dir.join(format!("{name}.js"));
    let script = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("saved workflow '{name}' not found: {e}"))?;
    let script = inject_args(&script, &args);
    run(
        ctx,
        parent_id,
        budget,
        max_concurrency,
        max_per_provider,
        max_total_agents,
        workflows_dir,
        &script,
        on_event,
    )
    .await
}

/// Lists saved workflow scripts (`.forge/workflows/*.js`, name without the `.js` extension),
/// sorted. Empty (not an error) if the directory doesn't exist yet.
pub async fn list_saved(workflows_dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(workflows_dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("js") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_mesh::{Router, RoutingDecision};
    use forge_provider::{EventSink, ModelResponse, Provider, ProviderError};
    use forge_types::{ModelHealth, PermissionMode, ProjectContext, SubscriptionQuota};

    /// Always answers with a fixed reply, regardless of prompt/model.
    struct EchoProvider;
    #[async_trait::async_trait]
    impl Provider for EchoProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[forge_types::Message],
            _tools: &[ToolSpec],
            _on_event: &mut EventSink<'_>,
        ) -> Result<ModelResponse, ProviderError> {
            Ok(ModelResponse {
                content: "child done".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    /// Records the peak number of concurrent `complete` calls, like `subagent`'s own
    /// `ConcurrencyProbe` test fixture.
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
            _on_event: &mut EventSink<'_>,
        ) -> Result<ModelResponse, ProviderError> {
            use std::sync::atomic::Ordering::SeqCst;
            let now = self.active.fetch_add(1, SeqCst) + 1;
            self.peak.fetch_max(now, SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            self.active.fetch_sub(1, SeqCst);
            Ok(ModelResponse {
                content: "child done".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quotas: Vec::new(),
            })
        }
    }

    struct FixedRouter {
        model: String,
    }
    #[async_trait::async_trait]
    impl Router for FixedRouter {
        async fn route(
            &self,
            _p: &str,
            _b: BudgetState,
            _h: &ModelHealth,
            _q: &SubscriptionQuota,
            _effort: Option<forge_types::EffortLevel>,
            _project: &ProjectContext,
        ) -> RoutingDecision {
            RoutingDecision {
                tier: forge_types::TaskTier::Standard,
                model: self.model.clone(),
                rationale: "test".into(),
                fallbacks: Vec::new(),
            }
        }
    }

    fn ctx_with(provider: Arc<dyn forge_provider::Provider>, model: &str) -> AgentCtx {
        let config = forge_config::Config::default();
        let pricing = forge_mesh::pricing::Pricing::from_config(&config);
        AgentCtx {
            provider,
            router: Arc::new(FixedRouter {
                model: model.to_string(),
            }),
            store: Arc::new(forge_store::Store::open_in_memory().unwrap()),
            config,
            pricing,
            mode: PermissionMode::default(),
            rules: Vec::new(),
            depth: 0,
            max_depth: 2,
            agents: Arc::new(HashMap::new()),
            worktree_root: None,
            repo_root: std::path::PathBuf::from("."),
        }
    }

    fn events(rx: &mut std::sync::mpsc::Receiver<WorkflowEvent>) -> Vec<WorkflowEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Runs a workflow script against a fresh in-memory `AgentCtx`, collecting every
    /// `WorkflowEvent` into a plain `Vec` (via a std channel bridged from the async `on_event`
    /// callback) so assertions can inspect them after the fact.
    async fn run_test_workflow(
        provider: Arc<dyn forge_provider::Provider>,
        model: &str,
        script: &str,
    ) -> (serde_json::Value, bool, Vec<WorkflowEvent>) {
        run_test_workflow_with(ctx_with(provider, model), script).await
    }

    async fn run_test_workflow_with(
        ctx: AgentCtx,
        script: &str,
    ) -> (serde_json::Value, bool, Vec<WorkflowEvent>) {
        let (etx, erx) = std::sync::mpsc::channel::<WorkflowEvent>();
        let (value, ok) = run(
            ctx,
            "parent".to_string(),
            BudgetState::default(),
            8,
            0,
            200,
            std::path::PathBuf::from(".forge/workflows"),
            script,
            move |ev| {
                let _ = etx.send(ev);
            },
        )
        .await
        .unwrap();
        let mut erx = erx;
        (value, ok, events(&mut erx))
    }

    #[tokio::test]
    async fn agent_call_runs_a_mesh_routed_child_and_returns_its_text() {
        let (value, ok, evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"return await agent("do the thing");"#,
        )
        .await;

        assert!(ok);
        assert_eq!(value, serde_json::Value::String("child done".to_string()));
        assert_eq!(evs.len(), 2, "one Start + one Done event");
        assert!(matches!(evs[0], WorkflowEvent::AgentStart { .. }));
        assert!(matches!(evs[1], WorkflowEvent::AgentDone { ok: true, .. }));
    }

    #[tokio::test]
    async fn parallel_runs_agent_calls_concurrently_not_serially() {
        use std::sync::atomic::AtomicUsize;
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(ConcurrencyProbe {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
        });

        let start = std::time::Instant::now();
        let (_value, ok, evs) = run_test_workflow(
            provider,
            "openai::gpt-test",
            r#"
            const results = await parallel([
                () => agent("a"),
                () => agent("b"),
                () => agent("c"),
            ]);
            return results.join(",");
            "#,
        )
        .await;
        let elapsed = start.elapsed();

        assert!(ok);
        assert_eq!(
            peak.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "all 3 ran concurrently"
        );
        // Same rationale as forge-workflow's own concurrency test (see its comment): a slow/shared
        // CI runner needs a wide margin. Serialized would take 150ms+ (3×50ms); concurrent should
        // land well under that even accounting for CI scheduling overhead.
        assert!(
            elapsed < std::time::Duration::from_millis(120),
            "3×50ms serialized would take ~150ms+; concurrent should take ~50-90ms, took {elapsed:?}"
        );
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, WorkflowEvent::AgentDone { .. }))
                .count(),
            3
        );
    }

    #[tokio::test]
    async fn pipeline_runs_each_item_through_every_stage_independently() {
        let (value, ok, _evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"
            const results = await pipeline(
                ["x", "y"],
                (prev, item) => agent("stage1:" + item),
                (prev, item) => agent("stage2:" + item + ":" + prev),
            );
            return results.join("|");
            "#,
        )
        .await;
        assert!(ok);
        // EchoProvider always answers "child done", so both items' final stage output is identical
        // — the point of this test is that it completes without error across 2 items x 2 stages.
        assert_eq!(
            value,
            serde_json::Value::String("child done|child done".to_string())
        );
    }

    #[tokio::test]
    async fn phase_labels_subsequent_agent_calls() {
        let (_value, ok, evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"
            phase("research");
            await agent("look into it");
            "#,
        )
        .await;
        assert!(ok);
        assert!(
            evs.iter()
                .any(|e| matches!(e, WorkflowEvent::Phase(t) if t == "research")),
            "phase() emits a dedicated Phase event for the workflow view's phase tree"
        );
        let (task, phase) = evs
            .iter()
            .find_map(|e| match e {
                WorkflowEvent::AgentStart { task, phase, .. } => {
                    Some((task.clone(), phase.clone()))
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(task, "look into it", "task text is unprefixed");
        assert_eq!(phase.as_deref(), Some("research"));
    }

    #[tokio::test]
    async fn log_emits_a_log_event() {
        // `log()` returns a real Promise like every other host function — an unawaited call can
        // race with the enclosing script's own completion, so a real script (like this test) must
        // `await` it for reliable sequencing, same as any other JS async call.
        let (_value, ok, evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"await log("hello from the script");"#,
        )
        .await;
        assert!(ok);
        assert!(evs
            .iter()
            .any(|e| matches!(e, WorkflowEvent::Log(m) if m.contains("hello from the script"))));
    }

    /// Regression: seen live — an authored script did all its work but had no `return`, so the
    /// tool result was the literal string "null" and the parent model relayed an empty answer.
    #[tokio::test]
    async fn a_script_with_no_return_still_yields_an_agent_digest_result() {
        let (value, ok, _evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"await agent("do the thing");"#, // no `return`
        )
        .await;
        assert!(ok);
        let text = value.as_str().expect("digest is a string");
        assert!(
            text.contains("returned no value"),
            "explains itself: {text}"
        );
        assert!(
            text.contains("child done"),
            "carries the agent's own summary: {text}"
        );
        assert!(text.contains("1 ok, 0 failed"), "counts: {text}");
    }

    #[tokio::test]
    async fn an_empty_string_return_also_falls_back_to_the_digest() {
        let (value, ok, _evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"await agent("do the thing"); return "  ";"#,
        )
        .await;
        assert!(ok);
        assert!(value.as_str().unwrap().contains("child done"));
    }

    #[tokio::test]
    async fn an_explicit_return_value_is_untouched() {
        let (value, ok, _evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"await agent("x"); return "my own answer";"#,
        )
        .await;
        assert!(ok);
        assert_eq!(
            value,
            serde_json::Value::String("my own answer".to_string())
        );
    }

    /// Regression: seen live — the model authored a script calling `glob(...)` (not a host
    /// function) and got only "glob is not defined", so its retry guessed another phantom API.
    #[tokio::test]
    async fn an_undefined_function_error_lists_the_real_host_functions() {
        let (value, ok, _evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"const files = await glob("**/*.rs"); return files;"#,
        )
        .await;
        assert!(!ok);
        let text = value.as_str().unwrap();
        assert!(
            text.contains("glob is not defined"),
            "original error: {text}"
        );
        assert!(
            text.contains("agent(prompt, opts?)") && text.contains("no glob/fs/require/fetch"),
            "hint lists the real surface: {text}"
        );
    }

    #[tokio::test]
    async fn a_returnless_script_with_no_agents_says_so() {
        let (value, ok, _evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"const x = 1;"#,
        )
        .await;
        assert!(ok);
        assert!(value.as_str().unwrap().contains("ran no agents"));
    }

    #[tokio::test]
    async fn total_agent_cap_stops_a_runaway_script() {
        let (value, ok, _evs) = run(
            ctx_with(Arc::new(EchoProvider), "openai::gpt-test"),
            "parent".to_string(),
            BudgetState::default(),
            8,
            0,
            2, // max_total_agents — deliberately tiny
            std::path::PathBuf::from(".forge/workflows"),
            r#"
            for (let i = 0; i < 5; i++) {
                await agent("call " + i);
            }
            return "done";
            "#,
            |_| {},
        )
        .await
        .map(|(v, ok)| (v, ok, ()))
        .unwrap();
        assert!(!ok, "must fail once the cap is exceeded");
        let text = value.as_str().unwrap_or_default();
        assert!(
            text.contains("agent safety cap"),
            "cap error surfaced: {text}"
        );
    }

    #[tokio::test]
    async fn workflow_rejects_path_traversal_in_saved_script_name() {
        let (value, ok, _evs) = run_test_workflow(
            Arc::new(EchoProvider),
            "openai::gpt-test",
            r#"return await workflow("../../etc/passwd");"#,
        )
        .await;
        assert!(!ok);
        let text = value.as_str().unwrap_or_default();
        assert!(
            text.contains("path separators") || text.contains("error"),
            "path traversal rejected: {text}"
        );
    }

    #[tokio::test]
    async fn workflow_depth_guard_stops_recursive_saved_workflows() {
        let mut ctx = ctx_with(Arc::new(EchoProvider), "openai::gpt-test");
        ctx.depth = ctx.max_depth; // already at the limit
        let (value, ok, _evs) =
            run_test_workflow_with(ctx, r#"return await workflow("anything");"#).await;
        assert!(!ok);
        let text = value.as_str().unwrap_or_default();
        assert!(
            text.contains("depth"),
            "depth guard message surfaced: {text}"
        );
    }

    /// A unique scratch dir per test run (no `tempfile` dependency needed) — cleaned up on drop.
    struct ScratchDir(std::path::PathBuf);
    impl ScratchDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "forge-workflow-test-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            ScratchDir(dir)
        }
    }
    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn run_saved_loads_a_real_file_and_injects_args() {
        let dir = ScratchDir::new("run-saved");
        std::fs::write(
            dir.0.join("greet.js"),
            r#"return "hello " + args.name + " via " + await agent("x");"#,
        )
        .unwrap();

        let (value, ok) = run_saved(
            ctx_with(Arc::new(EchoProvider), "openai::gpt-test"),
            "parent".to_string(),
            BudgetState::default(),
            8,
            0,
            200,
            dir.0.clone(),
            "greet",
            serde_json::json!({"name": "world"}),
            |_| {},
        )
        .await
        .unwrap();

        assert!(ok);
        assert_eq!(
            value,
            serde_json::Value::String("hello world via child done".to_string())
        );
    }

    #[tokio::test]
    async fn run_saved_rejects_path_traversal_in_name() {
        let dir = ScratchDir::new("run-saved-traversal");
        // Unlike a script's own `workflow()` calls (whose sandbox rejection surfaces as
        // `Ok((error_text, false))`, so the model sees why), `run_saved`'s own upfront validation
        // is a direct `Err` — there's no model in the loop to hand a text explanation to here.
        let err = run_saved(
            ctx_with(Arc::new(EchoProvider), "openai::gpt-test"),
            "parent".to_string(),
            BudgetState::default(),
            8,
            0,
            200,
            dir.0.clone(),
            "../../etc/passwd",
            serde_json::Value::Null,
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.contains("path separators"), "{err}");
    }

    #[tokio::test]
    async fn list_saved_lists_js_files_sorted_ignoring_others() {
        let dir = ScratchDir::new("list-saved");
        std::fs::write(dir.0.join("b.js"), "").unwrap();
        std::fs::write(dir.0.join("a.js"), "").unwrap();
        std::fs::write(dir.0.join("readme.md"), "").unwrap();

        let names = list_saved(&dir.0).await;
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[tokio::test]
    async fn list_saved_is_empty_not_an_error_when_dir_is_missing() {
        let missing = std::env::temp_dir().join("forge-workflow-test-does-not-exist-anywhere");
        assert!(list_saved(&missing).await.is_empty());
    }
}
