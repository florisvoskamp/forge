//! The session orchestrator: it runs the agent loop (the walking skeleton's spine) and
//! owns the permission broker — the one component that must be central (ADR-0002). It
//! wires the Mesh (routing), a Provider (model calls), the tool registry, the store
//! (persistence) and a presenter (UI) together, depending on each only through its trait.

use std::sync::Arc;

use forge_config::Config;
use forge_mesh::pricing::Pricing;
use forge_mesh::{BudgetState, BudgetStatus, Router};
use forge_provider::{Provider, StreamEvent, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::{Presenter, PresenterEvent};
use forge_types::{Message, PermissionDecision, PermissionMode, PermissionRule, Role};

pub mod llm_router;
pub mod permission;
pub mod subagent;

pub use llm_router::LlmRouter;

/// Hard cap on model<->tool round trips within a single turn.
pub(crate) const MAX_STEPS: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error(transparent)]
    Provider(#[from] forge_provider::ProviderError),
    #[error(transparent)]
    Store(#[from] forge_store::StoreError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("session not found: {0}")]
    SessionNotFound(String),
}

/// One interactive session. Construct with [`Session::start`], then drive [`Session::run_turn`].
pub struct Session {
    id: String,
    store: Arc<Store>,
    provider: Arc<dyn Provider>,
    router: Arc<dyn Router>,
    tools: ToolRegistry,
    presenter: Box<dyn Presenter>,
    config: Config,
    pricing: Pricing,
    mode: PermissionMode,
    /// Resolved permission rules (built-in safety denies + configured), consulted per call.
    rules: Vec<PermissionRule>,
    transcript: Vec<Message>,
    seq: i64,
}

impl Session {
    pub fn start(
        store: Arc<Store>,
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
        tools: ToolRegistry,
        presenter: Box<dyn Presenter>,
        config: Config,
        cwd: &str,
    ) -> Result<Self, CoreError> {
        let mode = config.permission_mode;
        let id = store.create_session(cwd, format!("{mode:?}").as_str())?;
        Ok(Self::build(
            id,
            store,
            provider,
            router,
            tools,
            presenter,
            config,
            Vec::new(),
            0,
        ))
    }

    /// Resume an existing session: rehydrate its transcript and continue the same row.
    #[allow(clippy::too_many_arguments)]
    pub fn resume(
        store: Arc<Store>,
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
        tools: ToolRegistry,
        presenter: Box<dyn Presenter>,
        config: Config,
        session_id: &str,
    ) -> Result<Self, CoreError> {
        if !store.session_exists(session_id)? {
            return Err(CoreError::SessionNotFound(session_id.to_string()));
        }
        let stored = store.load_messages(session_id)?;
        let seq = stored.len() as i64;
        let transcript = stored
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
                tool_calls: m.tool_calls,
                tool_call_id: m.tool_call_id,
            })
            .collect();
        Ok(Self::build(
            session_id.to_string(),
            store,
            provider,
            router,
            tools,
            presenter,
            config,
            transcript,
            seq,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        id: String,
        store: Arc<Store>,
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
        tools: ToolRegistry,
        presenter: Box<dyn Presenter>,
        config: Config,
        transcript: Vec<Message>,
        seq: i64,
    ) -> Self {
        let mode = config.permission_mode;
        let pricing = Pricing::from_config(&config);
        let rules = config.permission_rules();
        let mut s = Self {
            id,
            store,
            provider,
            router,
            tools,
            presenter,
            config,
            pricing,
            mode,
            rules,
            transcript,
            seq,
        };
        let id = s.id.clone();
        s.presenter.emit(PresenterEvent::SessionStarted { id });
        s
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Read the next user prompt from the attached surface. `None` ends the session.
    pub fn read_line(&mut self) -> Option<String> {
        self.presenter.read_line()
    }

    /// Surface a turn-level failure to the UI (a warning + a Done marker) so the caller's
    /// loop ends the turn cleanly instead of leaving it hanging.
    pub fn notify_error(&mut self, msg: &str) {
        self.presenter
            .emit(PresenterEvent::Warning(msg.to_string()));
        self.presenter.emit(PresenterEvent::Done {
            final_text: String::new(),
        });
    }

    fn next_seq(&mut self) -> i64 {
        let n = self.seq;
        self.seq += 1;
        n
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .names()
            .filter_map(|name| self.tools.get(name))
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                schema: t.schema(),
            })
            .collect();
        // Advertise the subagent virtual tool to the top-level model only (RFC
        // subagent-orchestration). Children build their own registry without it, so the
        // depth-1 recursion guard is structural.
        if self.config.mesh.subagents.enabled {
            specs.push(subagent::spawn_agents_spec(
                self.config.mesh.subagents.max_agents,
            ));
        }
        specs
    }

    /// Run one full turn: route -> (model -> tools)* -> final answer. Returns the answer.
    pub async fn run_turn(&mut self, prompt: &str) -> Result<String, CoreError> {
        // 1. Route the task (deterministic, no model call) and record why. The budget is
        // aggregated across ALL sessions for the current local day + month (FR-5), not one
        // session's running total.
        let budget = BudgetState {
            spent_today_usd: self.store.spend_today_usd()?,
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd()?,
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        };
        let status = budget.status();

        // Hard stop: once a cap is exceeded, refuse the call before any provider request
        // (the cap is never silently exceeded). Overridable per process via
        // FORGE_BUDGET_OVERRIDE=1.
        if status == BudgetStatus::Exhausted
            && self.config.mesh.budget.hard_stop
            && !budget_override_active()
        {
            let msg = over_budget_message(&budget);
            self.presenter.emit(PresenterEvent::Warning(msg.clone()));
            // Persist the prompt + a system note, make NO provider call, write NO usage row.
            let seq = self.next_seq();
            self.store
                .add_message(&self.id, seq, Role::User, prompt, None)?;
            self.transcript.push(Message::user(prompt));
            let seq = self.next_seq();
            self.store
                .add_message(&self.id, seq, Role::System, &msg, None)?;
            self.transcript.push(Message::system(&msg));
            self.presenter.emit(PresenterEvent::Done {
                final_text: msg.clone(),
            });
            return Ok(msg);
        }

        // Surface budget pressure before routing (FR-5).
        match status {
            BudgetStatus::Warning => self.presenter.emit(PresenterEvent::Warning(format!(
                "approaching budget cap (today ${:.4}, month ${:.4})",
                budget.spent_today_usd, budget.spent_month_usd
            ))),
            BudgetStatus::Exhausted => self.presenter.emit(PresenterEvent::Warning(format!(
                "budget cap reached (today ${:.4}) — routing to the cheapest tier",
                budget.spent_today_usd
            ))),
            BudgetStatus::Ok => {}
        }

        let decision = self.router.route(prompt, budget).await;
        self.presenter.emit(PresenterEvent::Routing {
            tier: decision.tier.as_str().to_string(),
            model: decision.model.clone(),
            rationale: decision.rationale.clone(),
        });

        // 2. Persist + record the user message.
        let seq = self.next_seq();
        self.store
            .add_message(&self.id, seq, Role::User, prompt, None)?;
        self.transcript.push(Message::user(prompt));

        let specs = self.tool_specs();
        let mut final_text = String::new();

        // 3. Model <-> tool loop.
        for step in 0..MAX_STEPS {
            // Stream the reply: deltas flow to the presenter as the model produces them.
            let mut resp = {
                let provider = &self.provider;
                let presenter = &mut self.presenter;
                let r = provider
                    .complete(
                        &decision.model,
                        &self.transcript,
                        &specs,
                        &mut |ev: StreamEvent| match ev {
                            StreamEvent::Text(t) => {
                                presenter.emit(PresenterEvent::AssistantDelta(t))
                            }
                            StreamEvent::Reasoning(t) => {
                                presenter.emit(PresenterEvent::Reasoning(t))
                            }
                            StreamEvent::ToolStarted { name, args } => {
                                presenter.emit(PresenterEvent::ToolStart { name, args })
                            }
                            StreamEvent::ToolFinished { name, ok, summary } => {
                                presenter.emit(PresenterEvent::ToolResult { name, ok, summary })
                            }
                        },
                    )
                    .await?;
                if !r.content.is_empty() {
                    presenter.emit(PresenterEvent::AssistantDone);
                }
                r
            };

            // Compute the real cost from token counts and the model's price (FR-5, A-7).
            resp.usage.cost_usd = self.pricing.cost_for(
                &decision.model,
                resp.usage.input_tokens,
                resp.usage.output_tokens,
            );

            self.transcript.push(Message::assistant_tool_calls(
                &resp.content,
                resp.tool_calls.clone(),
            ));

            let seq = self.next_seq();
            let msg_id = self.store.add_message_full(
                &self.id,
                seq,
                Role::Assistant,
                &resp.content,
                Some(&decision.model),
                &resp.tool_calls,
                None,
            )?;
            if step == 0 {
                self.store.record_routing(
                    &msg_id,
                    decision.tier,
                    &decision.model,
                    &decision.rationale,
                )?;
            }
            self.store.record_usage(&self.id, &msg_id, &resp.usage)?;

            if !resp.wants_tools() {
                final_text = resp.content;
                break;
            }

            // Execute each requested tool through the permission broker.
            for call in &resp.tool_calls {
                let result = self.invoke_tool(&msg_id, call).await?;
                let seq = self.next_seq();
                self.store.add_message_full(
                    &self.id,
                    seq,
                    Role::Tool,
                    &result,
                    None,
                    &[],
                    Some(&call.id),
                )?;
                self.transcript.push(Message::tool_result(&call.id, result));
            }
        }

        self.presenter.emit(PresenterEvent::Cost {
            session_total_usd: self.store.session_cost(&self.id)?,
        });
        self.presenter.emit(PresenterEvent::Done {
            final_text: final_text.clone(),
        });
        Ok(final_text)
    }

    /// Run a single tool call, applying the permission policy, and return its result text.
    async fn invoke_tool(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        // The subagent virtual tool is owned by core (it needs provider/router/store), not the
        // registry — intercept before the registry lookup (RFC subagent-orchestration).
        if call.name == subagent::SPAWN_AGENTS_TOOL {
            return self.spawn_agents(msg_id, call).await;
        }

        let args_json = serde_json::to_string(&call.args)?;

        let Some(tool) = self.tools.get(&call.name) else {
            let result = format!("error: unknown tool '{}'", call.name);
            self.presenter.emit(PresenterEvent::ToolResult {
                name: call.name.clone(),
                ok: false,
                summary: "unknown tool".to_string(),
            });
            self.store
                .record_tool_call(msg_id, &call.name, &args_json, &result, "n/a", "error")?;
            return Ok(result);
        };

        let side_effect = tool.side_effect();
        self.presenter.emit(PresenterEvent::ToolStart {
            name: call.name.clone(),
            args: args_json.clone(),
        });

        // For a file-mutating tool, show the proposed change BEFORE the permission gate so
        // the user reviews a diff instead of approving a blind write.
        if side_effect == forge_types::SideEffect::Write {
            if let Some(diff) = tool.preview(&call.args).await {
                self.presenter.emit(PresenterEvent::Diff(diff));
            }
        }

        let allowed =
            match permission::decide(self.mode, side_effect, &call.name, &call.args, &self.rules) {
                PermissionDecision::Allow => true,
                PermissionDecision::Deny => false,
                PermissionDecision::Ask => self.presenter.confirm(&call.name, side_effect),
            };
        let permission_label = if allowed { "allowed" } else { "denied" };

        let (result, ok) = if allowed {
            match tool.run(&call.args).await {
                Ok(out) => (out, true),
                Err(e) => (format!("error: {e}"), false),
            }
        } else {
            ("permission denied by policy".to_string(), false)
        };

        self.presenter.emit(PresenterEvent::ToolResult {
            name: call.name.clone(),
            ok,
            summary: summarize(&result),
        });
        self.store.record_tool_call(
            msg_id,
            &call.name,
            &args_json,
            &result,
            permission_label,
            if ok { "ok" } else { "error" },
        )?;

        Ok(result)
    }

    /// Handle a `spawn_agents` call: resolve each requested child against the loaded agent
    /// types, then run them **concurrently** (bounded by `max_concurrency`), each in its own
    /// mesh-routed, persisted child session. Children run on tokio tasks (they share the
    /// parent's `Arc` backends); since the presenter is single-threaded, each child reports its
    /// lifecycle over a channel that this method drains on the main task — so `SubagentResult`
    /// events surface live as children finish (RFC subagent-orchestration, Phase 2).
    async fn spawn_agents(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let args_json = serde_json::to_string(&call.args)?;
        let max = self.config.mesh.subagents.max_agents;
        let requests = match subagent::parse_requests(&call.args, max) {
            Ok(r) => r,
            Err(msg) => {
                let result = format!("error: {msg}");
                self.store.record_tool_call(
                    msg_id, &call.name, &args_json, &result, "allowed", "error",
                )?;
                return Ok(result);
            }
        };

        // Budget snapshot so children also down-tier when the day/month is under pressure.
        let budget = BudgetState {
            spent_today_usd: self.store.spend_today_usd()?,
            daily_cap_usd: self.config.mesh.daily_budget_usd,
            spent_month_usd: self.store.spend_this_month_usd()?,
            monthly_cap_usd: self.config.mesh.monthly_cap_usd,
            warn_fraction: self.config.mesh.warn_threshold,
        };

        let agents =
            forge_config::load_agents(std::path::Path::new(&self.config.mesh.subagents.agents_dir));
        let ctx = subagent::AgentCtx {
            provider: Arc::clone(&self.provider),
            router: Arc::clone(&self.router),
            store: Arc::clone(&self.store),
            config: self.config.clone(),
            pricing: self.pricing.clone(),
            mode: self.mode,
            rules: self.rules.clone(),
        };
        let parent_id = self.id.clone();
        let max_concurrency = self.config.mesh.subagents.max_concurrency;

        // Drive the shared orchestrator, turning each child lifecycle into a presenter event
        // (running children animate live; completed ones fold into the scrollback box).
        let presenter = &mut self.presenter;
        let mut on_event = |ev: subagent::Lifecycle| match ev {
            subagent::Lifecycle::Start { id, agent, task } => {
                presenter.emit(PresenterEvent::SubagentStart {
                    id: id.to_string(),
                    agent: agent.to_string(),
                    task: task.to_string(),
                })
            }
            subagent::Lifecycle::Progress { id, snippet } => {
                presenter.emit(PresenterEvent::SubagentProgress {
                    id: id.to_string(),
                    snippet: snippet.to_string(),
                })
            }
            subagent::Lifecycle::Done {
                id,
                agent,
                ok,
                summary,
                cost_usd,
            } => presenter.emit(PresenterEvent::SubagentResult {
                id: id.to_string(),
                agent: agent.to_string(),
                ok,
                summary: summary.to_string(),
                cost_usd,
            }),
        };
        let (combined, all_ok) = subagent::orchestrate(
            &ctx,
            &parent_id,
            requests,
            &agents,
            budget,
            max_concurrency,
            &mut on_event,
        )
        .await?;

        self.store.record_tool_call(
            msg_id,
            &call.name,
            &args_json,
            &combined,
            "allowed",
            if all_ok { "ok" } else { "error" },
        )?;
        Ok(combined)
    }
}

/// True if the per-process budget override is set (lets one over-budget run proceed).
fn budget_override_active() -> bool {
    matches!(
        std::env::var("FORGE_BUDGET_OVERRIDE").as_deref(),
        Ok("1") | Ok("true")
    )
}

fn over_budget_message(b: &BudgetState) -> String {
    let cap = |c: Option<f64>| c.map(|v| format!("${v:.2}")).unwrap_or_else(|| "∞".into());
    format!(
        "budget cap reached — today ${:.4}/{}, month ${:.4}/{}. Refusing further model calls. \
         Set FORGE_BUDGET_OVERRIDE=1 to proceed.",
        b.spent_today_usd,
        cap(b.daily_cap_usd),
        b.spent_month_usd,
        cap(b.monthly_cap_usd)
    )
}

fn summarize(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    // Truncate by *characters*, not bytes — a byte slice (`&first[..80]`) panics when the
    // cut falls inside a multi-byte UTF-8 char, which real tool output (file contents, shell
    // output, accents/emoji) routinely contains.
    if first.chars().count() > 80 {
        let head: String = first.chars().take(80).collect();
        format!("{head}…")
    } else {
        first.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_mesh::HeuristicRouter;
    use forge_provider::MockProvider;
    use forge_tui::HeadlessPresenter;
    use forge_types::SideEffect;
    use std::sync::{Arc, Mutex};

    /// A presenter that records every event so tests can assert on what was shown.
    #[derive(Clone, Default)]
    struct CapturePresenter {
        events: Arc<Mutex<Vec<PresenterEvent>>>,
    }
    impl Presenter for CapturePresenter {
        fn emit(&mut self, event: PresenterEvent) {
            self.events.lock().unwrap().push(event);
        }
        fn confirm(&mut self, _tool: &str, _side_effect: SideEffect) -> bool {
            false
        }
        fn read_line(&mut self) -> Option<String> {
            None
        }
    }

    #[tokio::test]
    async fn full_turn_routes_calls_tool_and_persists() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = Config::default();
        let mut session = Session::start(
            store,
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            // non-interactive: side-effect tools would be denied, but the mock uses read_file
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();

        let answer = session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        assert!(answer.contains("healthy"));

        // user + assistant + tool(read) + assistant(final) = 4 messages persisted.
        let count = session_message_count(&session);
        assert!(count >= 4, "expected >=4 messages, got {count}");
    }

    fn session_message_count(s: &Session) -> i64 {
        s.store.message_count(s.id()).unwrap()
    }

    #[tokio::test]
    async fn cost_accumulates_for_a_priced_model() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = priced_complex_config();
        let mut session = Session::start(
            store,
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();

        // "refactor ... concurrency" routes to the complex tier (a priced model),
        // so the mock's token counts must turn into a non-zero session cost.
        session
            .run_turn("refactor the architecture for concurrency")
            .await
            .unwrap();
        let cost = session.store.session_cost(session.id()).unwrap();
        assert!(cost > 0.0, "expected a non-zero cost, got {cost}");
    }

    #[tokio::test]
    async fn warns_when_budget_threshold_reached() {
        // Complex turn costs (30+12)/1k + (42+18)/1k = 0.102 USD (keyless priced model, so
        // provider-fallback can't re-route and change the cost).
        let mut config = priced_complex_config();
        config.mesh.daily_budget_usd = Some(0.12); // 80% = 0.096

        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();

        // Turn 1 spends ~0.102 -> into the warning band (>= 0.096, < 0.12).
        session
            .run_turn("refactor the architecture for concurrency")
            .await
            .unwrap();
        // Turn 2 starts already in the warning band, so it must warn.
        session
            .run_turn("refactor the concurrency design again")
            .await
            .unwrap();

        let warned = events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, PresenterEvent::Warning(_)));
        assert!(warned, "expected a budget Warning event");
    }

    /// A config whose complex tier points at a keyless (always-available) model with a fixed
    /// 1.0/1k price, so budget/cost tests are deterministic regardless of which API keys the
    /// host happens to have — otherwise provider-fallback would re-route to an available model
    /// and change the cost out from under the test.
    fn priced_complex_config() -> Config {
        let mut config = Config::default();
        config.mesh.models.insert(
            "complex".to_string(),
            forge_config::OneOrMany::One("ollama::opus-sim".to_string()),
        );
        config.mesh.pricing.insert(
            "ollama::opus-sim".to_string(),
            forge_config::PriceOverride {
                input_per_1k: 1.0,
                output_per_1k: 1.0,
            },
        );
        config
    }

    fn fresh_session(store: Arc<Store>, config: Config) -> Session {
        Session::start(
            store,
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap()
    }

    #[test]
    fn summarize_does_not_panic_on_multibyte_boundary() {
        // Byte 80 lands inside the multi-byte 'é' — `&first[..80]` would panic here.
        let line = format!(
            "{}éééééé, and a tail to push well past eighty bytes",
            "a".repeat(78)
        );
        let s = summarize(&line);
        assert!(s.ends_with('…'), "long line is truncated with an ellipsis");
        assert!(s.chars().count() <= 81);
    }

    #[test]
    fn summarize_passes_short_lines_through() {
        assert_eq!(summarize("ok: [workspace]"), "ok: [workspace]");
        assert_eq!(summarize("line one\nline two"), "line one");
    }

    #[tokio::test]
    async fn hard_stop_refuses_once_over_cap() {
        // AC-7: once the day total exceeds the cap, the next turn is refused before any
        // provider call and records no further spend.
        let mut config = priced_complex_config();
        config.mesh.daily_budget_usd = Some(0.05);
        let mut session = fresh_session(Arc::new(Store::open_in_memory().unwrap()), config);

        // Turn 1 sees $0 spent -> proceeds, spends ~$0.102 (over the $0.05 cap).
        session
            .run_turn("refactor the architecture for concurrency")
            .await
            .unwrap();
        let cost_after_1 = session.store.session_cost(session.id()).unwrap();
        assert!(
            cost_after_1 > 0.05,
            "turn 1 should exceed the cap: {cost_after_1}"
        );

        // Turn 2 is over budget -> hard stop.
        let answer = session
            .run_turn("refactor the concurrency design again")
            .await
            .unwrap();
        assert!(
            answer.contains("budget cap reached"),
            "turn 2 refused: {answer}"
        );
        let cost_after_2 = session.store.session_cost(session.id()).unwrap();
        assert!(
            (cost_after_2 - cost_after_1).abs() < 1e-9,
            "no spend after a hard stop"
        );
    }

    #[tokio::test]
    async fn daily_spend_aggregates_across_sessions() {
        // AC-1/AC-2: a second session sees the first session's spend in the day total.
        let path = std::env::temp_dir().join(format!("forge-budget-{}.db", forge_types::new_id()));
        let config = priced_complex_config(); // no cap -> both proceed; complex tier is priced

        let day_total_after_a = {
            let mut a = fresh_session(Arc::new(Store::open(&path).unwrap()), config.clone());
            a.run_turn("refactor the architecture for concurrency")
                .await
                .unwrap();
            a.store.spend_today_usd().unwrap()
        };
        assert!(day_total_after_a > 0.0, "session A recorded spend today");

        // A brand-new session on the same DB must see A's spend (the bug was a per-session reset).
        let b = fresh_session(Arc::new(Store::open(&path).unwrap()), config.clone());
        let seen_by_b = b.store.spend_today_usd().unwrap();
        assert!(
            (seen_by_b - day_total_after_a).abs() < 1e-9,
            "B sees the cross-session day total: {seen_by_b} vs {day_total_after_a}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_rehydrates_transcript_and_continues_same_session() {
        let path = std::env::temp_dir().join(format!("forge-resume-{}.db", forge_types::new_id()));
        let config = Config::default();

        // First run on a file-backed store, then drop it.
        let (id, cost1, msgs1) = {
            let mut s = fresh_session(Arc::new(Store::open(&path).unwrap()), config.clone());
            s.run_turn("refactor the architecture for concurrency")
                .await
                .unwrap();
            let id = s.id().to_string();
            (
                id.clone(),
                s.store.session_cost(&id).unwrap(),
                s.store.message_count(&id).unwrap(),
            )
        };

        // Resume on a fresh connection to the same file.
        let mut s2 = Session::resume(
            Arc::new(Store::open(&path).unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            &id,
        )
        .unwrap();

        assert_eq!(s2.id(), id, "must continue the same session row");
        assert_eq!(
            s2.transcript.len() as i64,
            msgs1,
            "transcript should be rehydrated"
        );
        let cost_after_resume = s2.store.session_cost(&id).unwrap();
        assert!(
            (cost_after_resume - cost1).abs() < 1e-9,
            "prior cost preserved"
        );

        // Continuing appends to the same session.
        s2.run_turn("another complex refactor of the design")
            .await
            .unwrap();
        assert!(
            s2.store.message_count(&id).unwrap() > msgs1,
            "new turn appended"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resume_missing_session_errors() {
        let err = Session::resume(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            "ghost-id",
        )
        .err()
        .unwrap();
        assert!(matches!(err, CoreError::SessionNotFound(_)));
    }

    // --- Subagent orchestration (RFC subagent-orchestration) ---

    /// A test provider that, for the TOP-LEVEL agent, calls `spawn_agents` with two inline
    /// subtasks then synthesizes; for a SUBAGENT (its transcript opens with the subagent system
    /// prompt) it behaves like the normal mock (read_file → done). Shared via `Arc` by parent
    /// and children, exactly as in production.
    #[derive(Default)]
    struct SpawnThenSynthProvider;

    #[async_trait::async_trait]
    impl Provider for SpawnThenSynthProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let is_subagent = messages
                .iter()
                .any(|m| m.role == Role::System && m.content.contains("subagent"));
            let used_tool = messages.iter().any(|m| m.role == Role::Tool);
            let usage = Usage {
                input_tokens: 30,
                output_tokens: 12,
                cost_usd: 0.0,
            };
            if is_subagent {
                // Child: read a file once, then answer.
                if used_tool {
                    let content = "child finding: ok";
                    on_event(StreamEvent::Text(content.into()));
                    return Ok(ModelResponse {
                        content: content.into(),
                        tool_calls: vec![],
                        usage,
                    });
                }
                return Ok(ModelResponse {
                    content: "reading".into(),
                    tool_calls: vec![ToolCall {
                        id: new_id(),
                        name: "read_file".into(),
                        args: serde_json::json!({"path": "Cargo.toml"}),
                    }],
                    usage,
                });
            }
            // Parent: fan out, then synthesize once results return.
            if used_tool {
                let content = "synthesized from subagents";
                on_event(StreamEvent::Text(content.into()));
                return Ok(ModelResponse {
                    content: content.into(),
                    tool_calls: vec![],
                    usage,
                });
            }
            Ok(ModelResponse {
                content: "delegating".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "spawn_agents".into(),
                    args: serde_json::json!({"agents": [
                        {"agent": "reviewer", "task": "review the change"},
                        {"task": "fix the typo in the readme"}
                    ]}),
                }],
                usage,
            })
        }
    }

    /// A config with three distinct, keyless, priced tiers so routing is deterministic and a
    /// Trivial child routes to a cheaper model than a Complex parent.
    fn tiered_config() -> Config {
        use forge_config::{OneOrMany, PriceOverride};
        let mut config = Config::default();
        for (tier, model, price) in [
            ("trivial", "ollama::small", 0.001),
            ("standard", "ollama::mid", 0.05),
            ("complex", "ollama::big", 1.0),
        ] {
            config
                .mesh
                .models
                .insert(tier.into(), OneOrMany::One(model.into()));
            config.mesh.pricing.insert(
                model.into(),
                PriceOverride {
                    input_per_1k: price,
                    output_per_1k: price,
                },
            );
        }
        config
    }

    #[tokio::test]
    async fn spawn_agents_creates_linked_children_and_returns_results() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = tiered_config();
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SpawnThenSynthProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        let answer = session
            .run_turn("design and architect a complex concurrency refactor across modules")
            .await
            .unwrap();

        assert!(
            answer.contains("synthesized"),
            "parent synthesizes: {answer}"
        );

        // Two child sessions, both linked to the parent.
        let children = store.child_sessions(&parent_id).unwrap();
        assert_eq!(children.len(), 2, "two children persisted with parent link");

        // Coarse lifecycle events surfaced for each child.
        let ev = events.lock().unwrap();
        let starts = ev
            .iter()
            .filter(|e| matches!(e, PresenterEvent::SubagentStart { .. }))
            .count();
        let results = ev
            .iter()
            .filter(|e| matches!(e, PresenterEvent::SubagentResult { .. }))
            .count();
        assert_eq!((starts, results), (2, 2), "start+result per child");

        // Children stream their activity → live progress events surface (Phase 3b).
        let progress = ev
            .iter()
            .filter(|e| matches!(e, PresenterEvent::SubagentProgress { .. }))
            .count();
        assert!(progress > 0, "at least one live progress delta surfaced");

        // Child usage rolled into the shared day budget (children did real model work).
        assert!(store.spend_today_usd().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn subagents_route_independently_via_the_mesh() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let config = tiered_config();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SpawnThenSynthProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        session
            .run_turn("design and architect a complex concurrency refactor across modules")
            .await
            .unwrap();

        // Parent routed Complex; the "fix the typo" child routed Trivial → different model.
        let parent_models = store.session_models(&parent_id).unwrap();
        assert_eq!(
            parent_models.first().map(String::as_str),
            Some("ollama::big")
        );

        let children = store.child_sessions(&parent_id).unwrap();
        let child_models: Vec<String> = children
            .iter()
            .flat_map(|c| store.session_models(c).unwrap())
            .collect();
        assert!(
            child_models.iter().any(|m| m == "ollama::small"),
            "a trivial child routed to the cheap tier independently: {child_models:?}"
        );
    }

    #[tokio::test]
    async fn agent_type_file_pins_tier_alongside_mesh_routed_inline_child() {
        // A `.forge/agents/reviewer.md` pins tier=complex; the inline "fix the typo" child has
        // no pin and mesh-routes to trivial. Both must coexist in one spawn_agents call.
        let dir = std::env::temp_dir().join(format!("forge-agents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("reviewer.md"),
            "---\nname: reviewer\ntier: complex\ntools: [read_file]\n---\nYou review code.",
        )
        .unwrap();

        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut config = tiered_config();
        config.mesh.subagents.agents_dir = dir.to_string_lossy().to_string();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(SpawnThenSynthProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        session
            .run_turn("design and architect a complex concurrency refactor across modules")
            .await
            .unwrap();

        let children = store.child_sessions(&parent_id).unwrap();
        let child_models: Vec<String> = children
            .iter()
            .flat_map(|c| store.session_models(c).unwrap())
            .collect();
        // reviewer pinned → complex tier model; the inline "fix typo" → trivial tier model.
        assert!(
            child_models.iter().any(|m| m == "ollama::big"),
            "pinned reviewer routed to its tier: {child_models:?}"
        );
        assert!(
            child_models.iter().any(|m| m == "ollama::small"),
            "inline child still mesh-routed cheaply: {child_models:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
