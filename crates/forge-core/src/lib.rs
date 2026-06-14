//! The session orchestrator: it runs the agent loop (the walking skeleton's spine) and
//! owns the permission broker — the one component that must be central (ADR-0002). It
//! wires the Mesh (routing), a Provider (model calls), the tool registry, the store
//! (persistence) and a presenter (UI) together, depending on each only through its trait.

use forge_config::Config;
use forge_mesh::{BudgetState, Router};
use forge_provider::{Provider, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::{Presenter, PresenterEvent};
use forge_types::{Message, PermissionDecision, PermissionMode, Role};

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
    mode: PermissionMode,
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
        let mut s = Self {
            id,
            store,
            provider,
            router,
            tools,
            presenter,
            config,
            mode,
            transcript: Vec::new(),
            seq: 0,
        };
        let id = s.id.clone();
        s.presenter.emit(PresenterEvent::SessionStarted { id });
        Ok(s)
    }

    pub fn id(&self) -> &str {
        &self.id
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
            let resp = self
                .provider
                .complete(&decision.model, &self.transcript, &specs)
                .await?;

            if !resp.content.is_empty() {
                self.presenter
                    .emit(PresenterEvent::AssistantText(resp.content.clone()));
            }
            self.transcript.push(Message::assistant(&resp.content));

            let seq = self.next_seq();
            let msg_id = self.store.add_message(
                &self.id,
                seq,
                Role::Assistant,
                &resp.content,
                Some(&decision.model),
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
                self.store
                    .add_message(&self.id, seq, Role::Tool, &result, None)?;
                self.transcript.push(Message::new(Role::Tool, result));
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

        let allowed = match permission::decide(self.mode, side_effect) {
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
}
