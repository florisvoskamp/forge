//! The session orchestrator: it runs the agent loop (the walking skeleton's spine) and
//! owns the permission broker — the one component that must be central (ADR-0002). It
//! wires the Mesh (routing), a Provider (model calls), the tool registry, the store
//! (persistence) and a presenter (UI) together, depending on each only through its trait.

use forge_config::Config;
use forge_mesh::pricing::Pricing;
use forge_mesh::{BudgetState, BudgetStatus, Router};
use forge_provider::{Provider, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::{Presenter, PresenterEvent};
use forge_types::{Message, PermissionDecision, PermissionMode, PermissionRule, Role};

pub mod permission;

/// Hard cap on model<->tool round trips within a single turn.
const MAX_STEPS: usize = 8;

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
    store: Store,
    provider: Box<dyn Provider>,
    router: Box<dyn Router>,
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
        store: Store,
        provider: Box<dyn Provider>,
        router: Box<dyn Router>,
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
        store: Store,
        provider: Box<dyn Provider>,
        router: Box<dyn Router>,
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
        store: Store,
        provider: Box<dyn Provider>,
        router: Box<dyn Router>,
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

    fn next_seq(&mut self) -> i64 {
        let n = self.seq;
        self.seq += 1;
        n
    }

    fn tool_specs(&self) -> Vec<ToolSpec> {
        self.tools
            .names()
            .filter_map(|name| self.tools.get(name))
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                schema: t.schema(),
            })
            .collect()
    }

    /// Run one full turn: route -> (model -> tools)* -> final answer. Returns the answer.
    pub async fn run_turn(&mut self, prompt: &str) -> Result<String, CoreError> {
        // 1. Route the task (deterministic, no model call) and record why.
        let budget = BudgetState {
            spent_today_usd: self.store.session_cost(&self.id)?,
            daily_budget_usd: self.config.mesh.daily_budget_usd,
        };

        // Surface budget pressure before routing (FR-5).
        match budget.status() {
            BudgetStatus::Warning => self.presenter.emit(PresenterEvent::Warning(format!(
                "approaching daily budget cap (spent ${:.4})",
                budget.spent_today_usd
            ))),
            BudgetStatus::Exhausted => self.presenter.emit(PresenterEvent::Warning(format!(
                "daily budget cap reached (spent ${:.4}) — routing to the cheapest tier",
                budget.spent_today_usd
            ))),
            BudgetStatus::Ok => {}
        }

        let decision = self.router.route(prompt, budget);
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
                        &mut |delta: &str| {
                            presenter.emit(PresenterEvent::AssistantDelta(delta.to_string()));
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
}

fn summarize(s: &str) -> String {
    let first = s.lines().next().unwrap_or("").trim();
    if first.len() > 80 {
        format!("{}…", &first[..80])
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
        let store = Store::open_in_memory().unwrap();
        let config = Config::default();
        let mut session = Session::start(
            store,
            Box::new(MockProvider),
            Box::new(HeuristicRouter::new(config.clone())),
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
        let store = Store::open_in_memory().unwrap();
        let config = Config::default();
        let mut session = Session::start(
            store,
            Box::new(MockProvider),
            Box::new(HeuristicRouter::new(config.clone())),
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
        // Pin the complex model's price so the test is independent of bundled rates:
        // each complex turn costs (30+12)/1k + (42+18)/1k = 0.102 USD.
        let mut config = Config::default();
        config.mesh.daily_budget_usd = Some(0.12); // 80% = 0.096
        config.mesh.pricing.insert(
            "anthropic::claude-opus-4-8".to_string(),
            forge_config::PriceOverride {
                input_per_1k: 1.0,
                output_per_1k: 1.0,
            },
        );

        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Store::open_in_memory().unwrap(),
            Box::new(MockProvider),
            Box::new(HeuristicRouter::new(config.clone())),
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

    fn fresh_session(store: Store, config: Config) -> Session {
        Session::start(
            store,
            Box::new(MockProvider),
            Box::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn resume_rehydrates_transcript_and_continues_same_session() {
        let path = std::env::temp_dir().join(format!("forge-resume-{}.db", forge_types::new_id()));
        let config = Config::default();

        // First run on a file-backed store, then drop it.
        let (id, cost1, msgs1) = {
            let mut s = fresh_session(Store::open(&path).unwrap(), config.clone());
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
            Store::open(&path).unwrap(),
            Box::new(MockProvider),
            Box::new(HeuristicRouter::new(config.clone())),
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
            Store::open_in_memory().unwrap(),
            Box::new(MockProvider),
            Box::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            "ghost-id",
        )
        .err()
        .unwrap();
        assert!(matches!(err, CoreError::SessionNotFound(_)));
    }
}
