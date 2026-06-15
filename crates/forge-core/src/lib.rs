//! The session orchestrator: it runs the agent loop (the walking skeleton's spine) and
//! owns the permission broker — the one component that must be central (ADR-0002). It
//! wires the Mesh (routing), a Provider (model calls), the tool registry, the store
//! (persistence) and a presenter (UI) together, depending on each only through its trait.

use std::sync::Arc;

use forge_config::Config;
use forge_mesh::pricing::Pricing;
use forge_mesh::{BudgetState, BudgetStatus, ModelCatalog, Router};
use forge_provider::{Provider, StreamEvent, ToolSpec};
use forge_store::Store;
use forge_tools::ToolRegistry;
use forge_tui::{Presenter, PresenterEvent};
use forge_types::{Message, PermissionDecision, PermissionMode, PermissionRule, Role};

pub mod assay;
pub mod llm_router;
pub mod permission;
pub mod snapshot;
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
    #[error("no healthy model available: every routed/fallback model is rate-limited or down")]
    NoHealthyModel,
}

/// Result of a [`Session::rewind_to`] / [`Session::undo`]: what the file-restore did, plus the
/// prompt that began the rewound-to turn (the UI re-offers it in the input box).
#[derive(Debug, Default, Clone)]
pub struct RewindOutcome {
    pub restore: snapshot::RestoreReport,
    pub rewound_prompt: Option<String>,
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
    /// Where code shadow-snapshots live (RFC PR3); defaults to `.forge/checkpoints`.
    checkpoint_root: std::path::PathBuf,
    /// The seq that began the current turn (its user message), keying this turn's snapshot dir.
    current_turn_seq: i64,
    /// The discovered model catalog (auto-discovery mesh), kept so the TUI `/models` browser can
    /// classify + group what's available without re-running discovery. `None` for mock/offline.
    catalog: Option<ModelCatalog>,
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
            checkpoint_root: std::path::PathBuf::from(".forge/checkpoints"),
            current_turn_seq: 0,
            catalog: None,
        };
        let id = s.id.clone();
        s.presenter.emit(PresenterEvent::SessionStarted { id });
        s
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Attach the discovered catalog so the `/models` browser can read it (composition root).
    pub fn set_catalog(&mut self, catalog: Option<ModelCatalog>) {
        self.catalog = catalog;
    }

    /// The discovered model catalog, if auto-discovery ran for this session.
    pub fn catalog(&self) -> Option<&ModelCatalog> {
        self.catalog.as_ref()
    }

    /// The pricing table in effect (bundled defaults + config overrides), for cost display.
    pub fn pricing(&self) -> &Pricing {
        &self.pricing
    }

    /// Override where code shadow-snapshots are stored (default `.forge/checkpoints`). Used by the
    /// composition root to anchor them under the project `.forge/`, and by tests for isolation.
    pub fn set_checkpoint_root(&mut self, root: impl Into<std::path::PathBuf>) {
        self.checkpoint_root = root.into();
    }

    /// Rewind the conversation to a transcript boundary (`seq`): soft-delete the messages at/after
    /// it, restore any files those turns wrote (PR3 shadow snapshots), and truncate the live
    /// transcript. Returns the file-restore result plus the prompt that started the rewound-to turn
    /// (so the UI can put it back in the input box). Powers `/undo` and `/checkpoints`.
    pub fn rewind_to(&mut self, boundary: i64) -> Result<RewindOutcome, CoreError> {
        let boundary = boundary.max(0);
        // The message AT the boundary is the user prompt of the rewound-to turn; capture it before
        // truncation so the UI can re-offer it for editing/resubmitting.
        let rewound_prompt = self
            .transcript
            .get(boundary as usize)
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.clone());
        let mut restore = snapshot::RestoreReport::default();
        // Turns are keyed by their user-message seq. Restore every snapshotted turn at/after the
        // boundary, newest first so an earlier turn's blob (pre-turn bytes) wins the final state.
        for seq in (boundary..self.seq).rev() {
            if let Ok(r) = snapshot::restore_turn(&self.checkpoint_root, &self.id, seq) {
                restore.restored.extend(r.restored);
                restore.warnings.extend(r.warnings);
            }
        }
        self.store.deactivate_messages_from(&self.id, boundary)?;
        self.transcript.truncate(boundary as usize);
        self.seq = boundary;
        Ok(RewindOutcome {
            restore,
            rewound_prompt,
        })
    }

    /// Undo the last user turn: rewind to (and including) the most recent user message, dropping
    /// that prompt and everything after it. `Ok(None)` if there's nothing to undo.
    pub fn undo(&mut self) -> Result<Option<RewindOutcome>, CoreError> {
        let Some(idx) = self.transcript.iter().rposition(|m| m.role == Role::User) else {
            return Ok(None);
        };
        Ok(Some(self.rewind_to(idx as i64)?))
    }

    /// Publish the current turn's snapshot context (session id, seq, absolute root) to the
    /// environment so the CLI bridge's `forge mcp-serve` snapshots its writes into this turn's dir.
    fn export_checkpoint_env(&self, seq: i64) {
        let root = std::path::absolute(&self.checkpoint_root)
            .unwrap_or_else(|_| self.checkpoint_root.clone());
        std::env::set_var(snapshot::ENV_SESSION, &self.id);
        std::env::set_var(snapshot::ENV_SEQ, seq.to_string());
        std::env::set_var(snapshot::ENV_ROOT, root);
    }

    /// Save a conversation checkpoint at the current boundary. `label` None = an auto checkpoint.
    pub fn checkpoint(&mut self, label: Option<&str>) -> Result<(), CoreError> {
        self.store.add_checkpoint(&self.id, label, self.seq)?;
        Ok(())
    }

    /// This session's saved checkpoints, newest first.
    pub fn checkpoints(&self) -> Result<Vec<forge_store::CheckpointRow>, CoreError> {
        Ok(self.store.list_checkpoints(&self.id)?)
    }

    /// Visible conversation history (user + non-empty assistant messages), oldest first, for
    /// redrawing the transcript into the TUI scrollback after a `/resume` swap.
    pub fn history(&self) -> Vec<(Role, String)> {
        self.transcript
            .iter()
            .filter(|m| {
                matches!(m.role, Role::User | Role::Assistant) && !m.content.trim().is_empty()
            })
            .map(|m| (m.role, m.content.clone()))
            .collect()
    }

    /// Reconfigure this session in place as a **fresh** one (new id, empty transcript), keeping
    /// the same backends + live presenter so events keep flowing to the running TUI. Powers
    /// `/new` — no process restart, no Session move (it lives behind the loop's `Mutex`).
    pub fn reset_fresh(&mut self, cwd: &str) -> Result<(), CoreError> {
        let id = self
            .store
            .create_session(cwd, format!("{:?}", self.mode).as_str())?;
        self.id = id.clone();
        self.transcript.clear();
        self.seq = 0;
        self.presenter.emit(PresenterEvent::SessionStarted { id });
        Ok(())
    }

    /// Reconfigure this session in place, **resumed** from `session_id`: rehydrate the stored
    /// transcript, keep the same backends + live presenter. Powers `/resume`.
    pub fn reset_resumed(&mut self, session_id: &str) -> Result<(), CoreError> {
        if !self.store.session_exists(session_id)? {
            return Err(CoreError::SessionNotFound(session_id.to_string()));
        }
        let stored = self.store.load_messages(session_id)?;
        self.seq = stored.len() as i64;
        self.transcript = stored
            .into_iter()
            .map(|m| Message {
                role: m.role,
                content: m.content,
                tool_calls: m.tool_calls,
                tool_call_id: m.tool_call_id,
            })
            .collect();
        self.id = session_id.to_string();
        self.presenter.emit(PresenterEvent::SessionStarted {
            id: session_id.to_string(),
        });
        Ok(())
    }

    /// The session's current temper (permission mode).
    pub fn temper(&self) -> PermissionMode {
        self.mode
    }

    /// Advance the temper through the SHIFT+TAB cycle, persist it, and return the new temper
    /// (RFC/temper-modes). Takes effect on the next turn's permission decisions.
    pub fn cycle_temper(&mut self) -> PermissionMode {
        self.set_temper(self.mode.cycle_next())
    }

    /// Set the temper to a specific mode (the `/mode` picker), persist it, and return it. Unlike
    /// the cycle this can reach `Bypass`/Full, since the picker is an explicit, deliberate choice.
    pub fn set_temper(&mut self, mode: PermissionMode) -> PermissionMode {
        self.mode = mode;
        let _ = self
            .store
            .update_session_mode(&self.id, &format!("{:?}", self.mode));
        self.mode
    }

    /// Run an Assay analysis over `source` (the bundled scope content), emit + persist the report,
    /// and — when `cleanup` — run a permission-gated, **undoable** fix turn (Refine) over the
    /// findings. The crew is read-only; Refine reuses the normal agent loop so its edits go through
    /// the permission broker and are shadow-snapshotted (so `/undo` reverts them).
    pub async fn assay(
        &mut self,
        source: Arc<str>,
        models: assay::TierModels,
        cleanup: bool,
    ) -> Result<(), CoreError> {
        let pricing = Arc::new(self.pricing.clone());
        let lenses = forge_types::FindingCategory::crew().to_vec();
        let cooldown = std::time::Duration::from_secs(self.config.mesh.failover_cooldown_secs);
        let provider = Arc::clone(&self.provider);
        let store = Arc::clone(&self.store);
        // Surface each critic/verifier as it finishes so the run shows live activity.
        let presenter = &mut self.presenter;
        let mut on_progress = |p: assay::AssayProgress| {
            presenter.emit(PresenterEvent::AssayProgress(assay::progress_line(&p)));
        };
        let mut report = assay::run_assay(
            forge_types::AssayScope::Repo,
            source,
            lenses,
            models,
            provider,
            pricing,
            store,
            cooldown,
            &mut on_progress,
        )
        .await;
        if let Ok(run_id) = self
            .store
            .create_assay_run(&report.scope.label(), report.cost_usd)
        {
            report.run_id = run_id.clone();
            for f in &report.findings {
                let _ = self.store.add_finding(&run_id, f);
            }
        }
        self.presenter
            .emit(PresenterEvent::AssayReport(report.clone()));

        if cleanup && !report.findings.is_empty() {
            self.presenter.emit(PresenterEvent::Warning(format!(
                "⚒ Refine — fixing {} finding(s); edits are permission-gated, /undo to revert",
                report.findings.len()
            )));
            let prompt = refine_prompt(&report);
            self.run_turn(&prompt).await?; // emits its own Done
        } else {
            if cleanup {
                self.presenter.emit(PresenterEvent::Warning(
                    "nothing to clean up — no findings".into(),
                ));
            }
            self.presenter.emit(PresenterEvent::Done {
                final_text: String::new(),
            });
        }
        Ok(())
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
        // The interactive question tool (AskUserQuestion) — always advertised so the model can
        // ask the user a focused question with suggested answers (docs/features/ask-user-question.md).
        specs.push(ask_user_spec());
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

        // Route around any currently-benched models (failover): the snapshot excludes models
        // whose cooldown hasn't elapsed, even across restarts (model-health-failover).
        let health = self.store.current_benched().unwrap_or_default();
        // Quota-aware routing (L3): demote/skip a subscription that the bridge reported is near or
        // over its plan limit (recorded after earlier turns from the CLI's rate-limit events).
        let quota = self.store.current_quota().unwrap_or_default();
        let decision = self.router.route(prompt, budget, &health, &quota).await;
        self.presenter.emit(PresenterEvent::Routing {
            tier: decision.tier.as_str().to_string(),
            model: decision.model.clone(),
            rationale: decision.rationale.clone(),
        });

        // 2. Persist + record the user message. Its seq keys this turn's code-snapshot dir
        // (PR3): files written during the turn are restorable by rewinding to this boundary.
        let seq = self.next_seq();
        self.current_turn_seq = seq;
        self.store
            .add_message(&self.id, seq, Role::User, prompt, None)?;
        self.transcript.push(Message::user(prompt));
        // Auto-checkpoint at the turn boundary, labeled with the prompt preview, so `/undo` can
        // offer a list of past messages to rewind to (no manual /checkpoint needed).
        let _ = self
            .store
            .add_checkpoint(&self.id, Some(&checkpoint_preview(prompt)), seq);
        // Export this turn's snapshot context so a CLI-bridge model's file edits (which run in
        // `forge mcp-serve`, a separate process) get snapshotted into THIS turn's dir and are
        // restorable by `/undo` (the in-process tool path snapshots directly in `invoke_tool`).
        self.export_checkpoint_env(seq);

        let specs = self.tool_specs();
        let mut final_text = String::new();

        // Failover state (model-health-failover): try `active_model`, and on a *retryable*
        // provider error (rate-limit / unavailable / auth) bench it and advance down the
        // routed decision's fallback chain. `active_model` is the model that actually answered,
        // so cost / usage / routing are recorded against it (not the original pick).
        let failover_enabled = self.config.mesh.failover;
        let default_cooldown =
            std::time::Duration::from_secs(self.config.mesh.failover_cooldown_secs);
        let mut chain = decision.fallbacks.clone().into_iter();
        let mut active_model = decision.model.clone();

        // 3. Model <-> tool loop.
        for step in 0..MAX_STEPS {
            // Stream the reply, with transparent failover for this step's completion.
            let mut resp = loop {
                // Tight scope: borrow provider + presenter only for the streamed call, so the
                // failover branch below has full `&mut self` for benching + warnings.
                let result =
                    {
                        let provider = &self.provider;
                        let presenter = &mut self.presenter;
                        provider
                            .complete(
                                &active_model,
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
                                    StreamEvent::ToolFinished { name, ok, summary } => presenter
                                        .emit(PresenterEvent::ToolResult { name, ok, summary }),
                                    StreamEvent::SubagentStarted { id, agent, task } => presenter
                                        .emit(PresenterEvent::SubagentStart { id, agent, task }),
                                    StreamEvent::SubagentProgress { id, snippet } => presenter
                                        .emit(PresenterEvent::SubagentProgress { id, snippet }),
                                    StreamEvent::SubagentFinished {
                                        id,
                                        agent,
                                        ok,
                                        summary,
                                        cost_usd,
                                    } => presenter.emit(PresenterEvent::SubagentResult {
                                        id,
                                        agent,
                                        ok,
                                        summary,
                                        cost_usd,
                                    }),
                                },
                            )
                            .await
                    };
                match result {
                    Ok(r) => {
                        if !r.content.is_empty() {
                            self.presenter.emit(PresenterEvent::AssistantDone);
                        }
                        break r;
                    }
                    Err(e) if failover_enabled && e.is_retryable() => {
                        let reason = e.reason();
                        let _ = self.store.bench_for(
                            &active_model,
                            e.cooldown(default_cooldown),
                            reason,
                        );
                        self.presenter.emit(PresenterEvent::Warning(format!(
                            "{active_model} {reason} — failing over"
                        )));
                        match chain.next() {
                            Some(next) => {
                                self.presenter.emit(PresenterEvent::Routing {
                                    tier: decision.tier.as_str().to_string(),
                                    model: next.clone(),
                                    rationale: format!("failover from {active_model}"),
                                });
                                active_model = next;
                                continue;
                            }
                            // Nothing healthy left to try (AC-6).
                            None => return Err(CoreError::NoHealthyModel),
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            };

            // Compute the real cost from token counts and the model's price (FR-5, A-7).
            resp.usage.cost_usd = self.pricing.cost_for(
                &active_model,
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
                Some(&active_model),
                &resp.tool_calls,
                None,
            )?;
            if step == 0 {
                self.store.record_routing(
                    &msg_id,
                    decision.tier,
                    &active_model,
                    &decision.rationale,
                )?;
            }
            self.store.record_usage(&self.id, &msg_id, &resp.usage)?;
            // Quota-aware routing (L3): if a CLI bridge reported its subscription window this turn,
            // persist it so the next route() can demote/skip a near-limit subscription.
            if let Some(hint) = &resp.quota {
                let _ = self.store.record_quota(hint);
            }

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
        // The interactive question tool is core-owned too (it needs the presenter).
        if call.name == ASK_USER_TOOL {
            return self.ask_user(msg_id, call);
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

        // Snapshot the target's pre-edit bytes BEFORE a permitted write, so `/undo` can restore
        // it (PR3 shadow snapshots; first touch per path per turn wins).
        let write_path = (allowed && side_effect == forge_types::SideEffect::Write)
            .then(|| call.args.get("path").and_then(|v| v.as_str()))
            .flatten()
            .map(std::path::PathBuf::from);
        if let Some(path) = &write_path {
            let _ = snapshot::snapshot_before_write(
                &self.checkpoint_root,
                &self.id,
                self.current_turn_seq,
                path,
            );
        }

        let (result, ok) = if allowed {
            match tool.run(&call.args).await {
                Ok(out) => {
                    // Record what we wrote, so a later restore can warn on a manual edit.
                    if let Some(path) = &write_path {
                        let _ = snapshot::record_post_write(
                            &self.checkpoint_root,
                            &self.id,
                            self.current_turn_seq,
                            path,
                        );
                    }
                    (out, true)
                }
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

        let agents = Arc::new(forge_config::load_agents(std::path::Path::new(
            &self.config.mesh.subagents.agents_dir,
        )));
        let ctx = subagent::AgentCtx {
            provider: Arc::clone(&self.provider),
            router: Arc::clone(&self.router),
            store: Arc::clone(&self.store),
            config: self.config.clone(),
            pricing: self.pricing.clone(),
            mode: self.mode,
            rules: self.rules.clone(),
            depth: 0,
            max_depth: self.config.mesh.subagents.max_depth,
            agents,
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

    /// Handle an `ask_user` call: parse the question + options, ask the user through the
    /// presenter (interactive multi-choice / open-ended), and return their answer as the tool
    /// result (docs/features/ask-user-question.md).
    fn ask_user(
        &mut self,
        msg_id: &str,
        call: &forge_types::ToolCall,
    ) -> Result<String, CoreError> {
        let args_json = serde_json::to_string(&call.args)?;
        let question = call
            .args
            .get("question")
            .and_then(|q| q.as_str())
            .unwrap_or("")
            .to_string();
        if question.trim().is_empty() {
            let result = "error: ask_user requires a non-empty `question`".to_string();
            self.store
                .record_tool_call(msg_id, &call.name, &args_json, &result, "allowed", "error")?;
            return Ok(result);
        }
        let options: Vec<forge_tui::QChoice> = call
            .args
            .get("options")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        let label = o.get("label").and_then(|l| l.as_str())?;
                        Some(forge_tui::QChoice {
                            label: label.to_string(),
                            description: o
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Default to allowing a free-text answer (and force it when there are no options).
        let allow_other = call
            .args
            .get("allow_other")
            .and_then(|a| a.as_bool())
            .unwrap_or(true)
            || options.is_empty();

        let answer = self.presenter.ask(&question, &options, allow_other);
        self.store
            .record_tool_call(msg_id, &call.name, &args_json, &answer, "allowed", "ok")?;
        Ok(answer)
    }
}

/// The interactive-question virtual tool name (AskUserQuestion).
const ASK_USER_TOOL: &str = "ask_user";

/// The `ToolSpec` advertised to the model for [`ASK_USER_TOOL`].
fn ask_user_spec() -> ToolSpec {
    ToolSpec {
        name: ASK_USER_TOOL.to_string(),
        description: "Ask the user a single focused question when you hit a real decision only \
            they can make (a value choice, a missing requirement). Provide 2–4 suggested \
            `options` with short labels (+ optional descriptions); set `allow_other` (default \
            true) to also accept a free-text answer. Returns the user's choice. Don't use it for \
            things you can decide yourself."
            .to_string(),
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "the question to ask" },
                "options": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": { "type": "string" },
                            "description": { "type": "string" }
                        },
                        "required": ["label"]
                    }
                },
                "allow_other": {
                    "type": "boolean",
                    "description": "allow a free-text answer beyond the options (default true)"
                }
            },
            "required": ["question"]
        }),
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

/// Build the Refine (cleanup) task prompt from an assay report: instruct the agent loop to fix
/// each finding by editing files (gated + snapshotted via the normal turn path).
fn refine_prompt(report: &forge_types::AssayReport) -> String {
    let mut s = String::from(
        "You are Refine, a cleanup crew. An Assay analysis found the issues below in this \
         codebase. Fix each one by editing the relevant files (edit_file/write_file). Be surgical \
         — fix exactly the issue without breaking working code or changing unrelated behavior. If \
         a finding is a false positive, skip it and briefly say why.\n\nIssues:\n",
    );
    for (i, f) in report.findings.iter().enumerate() {
        let loc = match f.line {
            Some(l) => format!("{}:{l}", f.file),
            None => f.file.clone(),
        };
        s.push_str(&format!(
            "{}. [{}] {} — {}\n   why: {}\n   suggested fix: {}\n",
            i + 1,
            f.severity.as_str(),
            loc,
            f.title,
            f.rationale,
            f.suggested_fix
        ));
    }
    s
}

/// A short single-line label for an auto-checkpoint: the prompt's first line, char-truncated.
fn checkpoint_preview(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    if first.chars().count() > 60 {
        format!("{}…", first.chars().take(60).collect::<String>())
    } else {
        first.to_string()
    }
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
        fn ask(&mut self, _q: &str, options: &[forge_tui::QChoice], _allow_other: bool) -> String {
            // Deterministic: pick the first option (or empty) so tests don't block on input.
            options.first().map(|o| o.label.clone()).unwrap_or_default()
        }
        fn read_line(&mut self) -> Option<String> {
            None
        }
    }

    /// A provider that calls `ask_user` once, then answers using whatever came back.
    #[derive(Default)]
    struct AskingProvider;

    #[async_trait::async_trait]
    impl Provider for AskingProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let usage = Usage::default();
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage,
                    quota: None,
                });
            }
            Ok(ModelResponse {
                content: "asking".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "ask_user".into(),
                    args: serde_json::json!({
                        "question": "which database?",
                        "options": [{"label": "Postgres"}, {"label": "SQLite"}]
                    }),
                }],
                usage,
                quota: None,
            })
        }
    }

    #[tokio::test]
    async fn ask_user_round_trips_the_answer_into_the_turn() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(AskingProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            // CapturePresenter::ask returns the first option ("Postgres").
            Box::new(CapturePresenter::default()),
            Config::default(),
            ".",
        )
        .unwrap();
        let id = session.id().to_string();
        let answer = session.run_turn("set up the db").await.unwrap();
        assert_eq!(
            answer, "done",
            "turn completes after the question is answered"
        );
        // The chosen answer was fed back as the tool result.
        let tool_msgs: Vec<_> = store
            .load_messages(&id)
            .unwrap()
            .into_iter()
            .filter(|m| m.role == Role::Tool)
            .collect();
        assert!(
            tool_msgs.iter().any(|m| m.content == "Postgres"),
            "ask_user answer fed back as tool result: {tool_msgs:?}"
        );
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
                        quota: None,
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
                    quota: None,
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
                    quota: None,
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
                quota: None,
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

    /// A provider where EVERY agent (top or subagent) tries to `spawn_agents` once, then answers.
    /// Used to prove recursion is bounded by `max_depth` (the registry refuses `spawn_agents`
    /// once depth is exhausted, so the chain terminates).
    #[derive(Default)]
    struct AlwaysRecurseProvider;

    #[async_trait::async_trait]
    impl Provider for AlwaysRecurseProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let used_tool = messages.iter().any(|m| m.role == Role::Tool);
            let usage = Usage {
                input_tokens: 5,
                output_tokens: 2,
                cost_usd: 0.0,
            };
            if used_tool {
                return Ok(ModelResponse {
                    content: "leaf answer".into(),
                    tool_calls: vec![],
                    usage,
                    quota: None,
                });
            }
            Ok(ModelResponse {
                content: "delegating deeper".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "spawn_agents".into(),
                    args: serde_json::json!({"agents": [{"task": "go deeper"}]}),
                }],
                usage,
                quota: None,
            })
        }
    }

    #[test]
    fn cycle_temper_advances_wraps_and_persists() {
        use forge_types::PermissionMode;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = fresh_session(Arc::clone(&store), Config::default());
        let id = session.id().to_string();
        let mut session = session;

        assert_eq!(session.temper(), PermissionMode::Default); // Guarded
        assert_eq!(session.cycle_temper(), PermissionMode::AcceptEdits); // → Smith
        assert_eq!(
            store.session_mode(&id).unwrap(),
            "AcceptEdits",
            "switch persisted"
        );
        assert_eq!(session.cycle_temper(), PermissionMode::Plan); // → Survey
        assert_eq!(session.cycle_temper(), PermissionMode::Default); // wraps → Guarded
                                                                     // Cycling never lands on the dangerous Unfettered temper.
        for _ in 0..6 {
            assert_ne!(session.cycle_temper(), PermissionMode::Bypass);
        }
    }

    #[tokio::test]
    async fn recursion_is_bounded_by_max_depth() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut config = tiered_config();
        config.mesh.subagents.max_depth = 2;
        config.mesh.subagents.max_concurrency = 2;
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(AlwaysRecurseProvider),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        let parent_id = session.id().to_string();

        // Must terminate (not infinite-recurse / stack-overflow).
        session
            .run_turn("kick off a delegating turn")
            .await
            .unwrap();

        // Walk the parent→child tree; with max_depth=2 the chain is child→grandchild→
        // great-grandchild (depths 0,1,2) and stops — never a 4th generation.
        fn max_gen(store: &Store, id: &str) -> usize {
            let kids = store.child_sessions(id).unwrap();
            1 + kids.iter().map(|k| max_gen(store, k)).max().unwrap_or(0)
        }
        let generations = max_gen(&store, &parent_id);
        assert_eq!(
            generations, 4,
            "parent + 3 nested generations (depths 0,1,2), bounded by max_depth"
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

    // --- Model health / failover (model-health-failover) ---

    /// A router that returns a fixed model + fallback chain, so the failover loop is testable
    /// without depending on discovery/availability.
    struct FixedRouter {
        model: String,
        fallbacks: Vec<String>,
    }
    #[async_trait::async_trait]
    impl Router for FixedRouter {
        async fn route(
            &self,
            _prompt: &str,
            _budget: BudgetState,
            _health: &forge_types::ModelHealth,
            _quota: &forge_types::SubscriptionQuota,
        ) -> forge_mesh::RoutingDecision {
            forge_mesh::RoutingDecision {
                tier: forge_types::TaskTier::Trivial,
                model: self.model.clone(),
                rationale: "test".into(),
                fallbacks: self.fallbacks.clone(),
            }
        }
    }

    /// A provider that fails for `bad` models (with a chosen error) and answers for any other.
    struct FlakyProvider {
        bad: std::collections::HashSet<String>,
        err: fn(&str) -> forge_provider::ProviderError,
    }
    #[async_trait::async_trait]
    impl Provider for FlakyProvider {
        async fn complete(
            &self,
            model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            if self.bad.contains(model) {
                return Err((self.err)(model));
            }
            on_event(StreamEvent::Text("recovered".into()));
            Ok(forge_provider::ModelResponse {
                content: "recovered".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quota: None,
            })
        }
    }

    fn rate_limited(_m: &str) -> forge_provider::ProviderError {
        forge_provider::ProviderError::RateLimited {
            message: "429".into(),
            retry_after: Some(std::time::Duration::from_secs(42)),
        }
    }

    fn fixed_session(
        provider: Arc<dyn Provider>,
        router: Arc<dyn Router>,
    ) -> (Arc<Store>, Session) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = Session::start(
            Arc::clone(&store),
            provider,
            router,
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();
        (store, session)
    }

    #[tokio::test]
    async fn retryable_error_benches_the_model_and_fails_over() {
        // AC-1 + AC-2: the primary 429s → benched (with the server's 42s cooldown) → the turn
        // retries on the fallback and succeeds.
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec!["good::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        let answer = session.run_turn("hi").await.unwrap();
        assert_eq!(answer, "recovered");
        // The bad model is benched; the cooldown reflects the server's 42s (not the default).
        let report = store.current_benched_report().unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].0, "bad::model");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (report[0].1 - now - 42).abs() <= 2,
            "cooldown ~42s: {report:?}"
        );
    }

    #[tokio::test]
    async fn non_retryable_error_does_not_fail_over_or_bench() {
        // AC-5: a 400-style error fails the turn as before; the model is NOT benched.
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
            err: |_| forge_provider::ProviderError::Request("bad request".into()),
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec!["good::model".into()],
        });
        let (store, mut session) = fixed_session(provider, router);
        assert!(session.run_turn("hi").await.is_err());
        assert!(store.current_benched().unwrap().is_empty());
    }

    #[tokio::test]
    async fn exhausting_the_chain_returns_no_healthy_model() {
        // AC-6: primary 429s, no fallbacks → a clear error, not a hang.
        let provider = Arc::new(FlakyProvider {
            bad: ["bad::model".to_string()].into_iter().collect(),
            err: rate_limited,
        });
        let router = Arc::new(FixedRouter {
            model: "bad::model".into(),
            fallbacks: vec![],
        });
        let (_store, mut session) = fixed_session(provider, router);
        assert!(matches!(
            session.run_turn("hi").await,
            Err(CoreError::NoHealthyModel)
        ));
    }

    // --- Conversation checkpoints + /undo (RFC session-management-and-commands, PR2) ---

    #[tokio::test]
    async fn undo_rewinds_the_last_user_turn() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = fresh_session(Arc::clone(&store), Config::default());
        let id = session.id().to_string();

        session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        assert!(
            store.load_messages(&id).unwrap().len() >= 2,
            "the turn persisted messages"
        );

        // Undo drops the whole turn (the user prompt + its replies/tools).
        assert!(session.undo().unwrap().is_some(), "a turn was undone");
        assert!(
            store.load_messages(&id).unwrap().is_empty(),
            "rewound turn is excluded from the active transcript"
        );
        assert!(session.undo().unwrap().is_none(), "nothing left to undo");
    }

    #[tokio::test]
    async fn every_turn_auto_checkpoints_with_a_prompt_preview() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = fresh_session(Arc::clone(&store), Config::default());

        session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        session.run_turn("now check it again please").await.unwrap();

        let cps = session.checkpoints().unwrap();
        assert_eq!(cps.len(), 2, "one auto checkpoint per turn");
        // Newest first, labeled with the prompt preview (so /undo can show the message).
        assert_eq!(cps[0].label.as_deref(), Some("now check it again please"));
        assert_eq!(cps[1].label.as_deref(), Some("check the project manifest"));
        // Each checkpoint's boundary is its turn's start, so rewinding there undoes that turn.
        assert!(cps[0].seq > cps[1].seq);
    }

    #[tokio::test]
    async fn checkpoint_then_turn_then_rewind_to_it() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut session = fresh_session(Arc::clone(&store), Config::default());
        let id = session.id().to_string();

        session
            .run_turn("check the project manifest")
            .await
            .unwrap();
        session.checkpoint(Some("after first turn")).unwrap();
        let boundary = session.checkpoints().unwrap()[0].seq;
        session.run_turn("check the manifest again").await.unwrap();
        let after_two = store.load_messages(&id).unwrap().len();

        session.rewind_to(boundary).unwrap();
        let after_rewind = store.load_messages(&id).unwrap().len();
        assert!(
            after_rewind < after_two && after_rewind == boundary as usize,
            "rewind drops the second turn back to the checkpoint boundary"
        );
    }

    /// A provider that writes a file once (via `write_file`), then answers.
    struct WritingProvider {
        path: String,
        content: String,
    }
    #[async_trait::async_trait]
    impl Provider for WritingProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            use forge_types::{new_id, ToolCall, Usage};
            let usage = Usage::default();
            if messages.iter().any(|m| m.role == Role::Tool) {
                return Ok(ModelResponse {
                    content: "done".into(),
                    tool_calls: vec![],
                    usage,
                    quota: None,
                });
            }
            Ok(ModelResponse {
                content: "writing".into(),
                tool_calls: vec![ToolCall {
                    id: new_id(),
                    name: "write_file".into(),
                    args: serde_json::json!({ "path": self.path, "content": self.content }),
                }],
                usage,
                quota: None,
            })
        }
    }

    #[tokio::test]
    async fn picker_rewind_to_an_earlier_turn_reverts_files() {
        // Mirrors the /undo picker path: two turns edit a file, then rewind to the FIRST turn's
        // checkpoint seq (as the picker does) — the file must return to its pre-turn-1 bytes.
        let dir = std::env::temp_dir().join(format!("forge-rew-{}", forge_types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "ORIGINAL").unwrap();

        let config = Config {
            permission_mode: PermissionMode::Bypass,
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(WritingProvider {
                path: file.to_string_lossy().to_string(),
                content: "MODEL-EDIT".into(),
            }),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        session.set_checkpoint_root(dir.join("snaps"));

        session.run_turn("turn one edits the file").await.unwrap();
        session.run_turn("turn two edits it again").await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "MODEL-EDIT");

        // Picker uses the checkpoint's seq; pick the OLDEST (first turn).
        let cps = session.checkpoints().unwrap();
        let first_turn_seq = cps.last().unwrap().seq;
        let report = session.rewind_to(first_turn_seq).unwrap().restore;

        assert!(
            !report.restored.is_empty(),
            "files were restored: {report:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "ORIGINAL",
            "rewinding to turn 1 reverts the file to its pre-turn-1 bytes"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn undo_restores_files_written_during_the_turn() {
        let dir = std::env::temp_dir().join(format!("forge-undo-{}", forge_types::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("edited.txt");
        std::fs::write(&file, "original bytes").unwrap();

        let config = Config {
            permission_mode: PermissionMode::Bypass, // allow the write without a prompt
            ..Config::default()
        };
        let mut session = Session::start(
            Arc::new(Store::open_in_memory().unwrap()),
            Arc::new(WritingProvider {
                path: file.to_string_lossy().to_string(),
                content: "the model overwrote this".into(),
            }),
            Arc::new(HeuristicRouter::new(config.clone())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            config,
            ".",
        )
        .unwrap();
        session.set_checkpoint_root(dir.join("snaps"));

        session.run_turn("rewrite the file").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "the model overwrote this",
            "the turn wrote the file"
        );

        let report = session.undo().unwrap().unwrap().restore;
        assert!(
            report.restored.iter().any(|p| p.contains("edited.txt")),
            "the written file was restored: {report:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "original bytes",
            "undo restored the pre-turn bytes"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A provider that blocks for a long time, so a turn can be interrupted mid-flight.
    struct SlowProvider;
    #[async_trait::async_trait]
    impl Provider for SlowProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(forge_provider::ModelResponse {
                content: "too late".into(),
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quota: None,
            })
        }
    }

    #[tokio::test]
    async fn aborting_a_running_turn_releases_the_session_lock() {
        // The interrupt feature aborts the turn task; this proves the invariant it relies on —
        // cancelling a task that holds the session Mutex across an await frees the lock, so the
        // session stays usable (no deadlock / frozen UI).
        use std::time::Duration;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let session = Arc::new(tokio::sync::Mutex::new(
            Session::start(
                store,
                Arc::new(SlowProvider),
                Arc::new(HeuristicRouter::new(Config::default())),
                ToolRegistry::with_core_tools(),
                Box::new(HeadlessPresenter::new(false)),
                Config::default(),
                ".",
            )
            .unwrap(),
        ));

        let s = session.clone();
        let handle = tokio::spawn(async move {
            let mut g = s.lock().await;
            let _ = g.run_turn("a slow request").await;
        });
        // Let the task acquire the lock and enter the 30s provider sleep, then interrupt it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.abort();
        let _ = handle.await;

        // The lock must be free immediately (the aborted task dropped its guard).
        let guard = tokio::time::timeout(Duration::from_secs(2), session.lock())
            .await
            .expect("abort released the session lock");
        assert!(
            guard
                .history()
                .iter()
                .any(|(r, c)| matches!(r, Role::User) && c == "a slow request"),
            "the interrupted turn's prompt was recorded before the abort"
        );
    }

    // --- Assay mode (docs/features/analysis-mode.md) ---

    /// A provider that plays the critic + verifier roles for an in-session assay run.
    struct AssayProvider;
    #[async_trait::async_trait]
    impl Provider for AssayProvider {
        async fn complete(
            &self,
            _model: &str,
            messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut forge_provider::EventSink<'_>,
        ) -> Result<forge_provider::ModelResponse, forge_provider::ProviderError> {
            use forge_provider::ModelResponse;
            let sys = messages
                .iter()
                .find(|m| m.role == Role::System)
                .map(|m| m.content.as_str())
                .unwrap_or("");
            let content = if sys.contains("ASSAY-VERIFIER") {
                r#"{"verdict":"uphold","confidence":"high"}"#.to_string()
            } else if sys.contains("ASSAY-CRITIC") && sys.contains("'correctness'") {
                r#"[{"severity":"high","file":"a.rs","line":1,"title":"bug","why":"w","fix":"f","effort":"small"}]"#.to_string()
            } else {
                "[]".to_string()
            };
            Ok(ModelResponse {
                content,
                tool_calls: vec![],
                usage: forge_types::Usage::default(),
                quota: None,
            })
        }
    }

    #[tokio::test]
    async fn assay_analysis_emits_a_report_and_persists_the_run() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let capture = CapturePresenter::default();
        let events = capture.events.clone();
        let mut session = Session::start(
            Arc::clone(&store),
            Arc::new(AssayProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(capture),
            Config::default(),
            ".",
        )
        .unwrap();

        session
            .assay(
                Arc::from("fn main() {}"),
                assay::TierModels {
                    trivial: vec!["m".into()],
                    complex: vec!["m".into()],
                },
                false, // analysis-only
            )
            .await
            .unwrap();

        let ev = events.lock().unwrap();
        let report = ev.iter().find_map(|e| match e {
            PresenterEvent::AssayReport(r) => Some(r.clone()),
            _ => None,
        });
        let report = report.expect("an AssayReport was emitted");
        assert_eq!(report.findings.len(), 1, "the upheld finding is reported");
        assert!(!report.run_id.is_empty(), "the run was persisted");
        assert_eq!(store.list_assay_runs().unwrap().len(), 1);
        assert_eq!(store.load_findings(&report.run_id).unwrap().len(), 1);
    }

    // --- In-TUI session swap (RFC session-management-and-commands, PR1) ---

    #[tokio::test]
    async fn reset_resumed_and_fresh_swap_the_live_session() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        // Seed a past session A with a user+assistant exchange.
        let a = store.create_session(".", "default").unwrap();
        store.add_message(&a, 0, Role::User, "hello", None).unwrap();
        store
            .add_message(&a, 1, Role::Assistant, "hi there", Some("m"))
            .unwrap();
        // A live session B (what the TUI is holding).
        let mut b = Session::start(
            Arc::clone(&store),
            Arc::new(MockProvider),
            Arc::new(HeuristicRouter::new(Config::default())),
            ToolRegistry::with_core_tools(),
            Box::new(HeadlessPresenter::new(false)),
            Config::default(),
            ".",
        )
        .unwrap();
        let b_id = b.id().to_string();

        // /resume A: B becomes A, rehydrating A's transcript.
        b.reset_resumed(&a).unwrap();
        assert_eq!(b.id(), a);
        assert_ne!(b.id(), b_id);
        assert_eq!(
            b.history(),
            vec![
                (Role::User, "hello".to_string()),
                (Role::Assistant, "hi there".to_string()),
            ]
        );

        // /new: a fresh empty session, new id.
        b.reset_fresh(".").unwrap();
        assert!(b.history().is_empty());
        assert_ne!(b.id(), a);
    }
}
