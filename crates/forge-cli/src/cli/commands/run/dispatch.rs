use super::*;

/// What a line typed at the chat prompt means.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChatAction {
    Quit,
    Skip,
    Run(String),
}

pub(crate) fn chat_action(line: &str) -> ChatAction {
    let t = line.trim();
    match t {
        "" => ChatAction::Skip,
        "/quit" | "/exit" | "/q" => ChatAction::Quit,
        // `//foo` escapes to a literal `/foo` prompt — mirrors the TUI behaviour.
        _ if t.starts_with("//") => ChatAction::Run(format!("/{}", &t[2..])),
        // Slash commands are TUI-only in plain mode; print a hint and skip.
        _ if t.starts_with('/') => {
            let cmd = t.split_whitespace().next().unwrap_or(t);
            eprintln!("⚒ '{cmd}' is a TUI-only command — run `forge chat` (without --plain) for the interactive TUI.");
            ChatAction::Skip
        }
        task => ChatAction::Run(task.to_string()),
    }
}

/// What the render loop must do after [`dispatch_command`].
pub(crate) enum DispatchOutcome {
    /// Command fully handled in-loop (palette, picker, note, …) — keep going.
    Handled,
    /// `/quit` — exit the TUI.
    Quit,
    /// A file command/skill expanded into a model turn the caller should spawn.
    RunTurn {
        prompt: String,
        guidance: Vec<String>,
        tier: Option<forge_types::TaskTier>,
    },
    /// `/compact` — summarize older messages in a background task (it makes a model call).
    RunCompact,
    /// `/loop <task>` — run the task, then re-run each turn until the model signals completion.
    StartLoop { prompt: String },
    /// `/mesh` — overlay opened immediately; receiver delivers the computed `MeshOverlay` (None =
    /// no catalog).
    PendingMesh(tokio::sync::oneshot::Receiver<Option<forge_tui::MeshOverlay>>),
    /// `/usage` — overlay opened immediately; receiver delivers `BridgeStats` when ready.
    PendingUsage(tokio::sync::oneshot::Receiver<bridge_stats::BridgeStats>),
    /// `/remote [--lan|--local|--anywhere]` — toggle remote control on (start the server) or off
    /// (stop it). The [`remote::Exposure`] selects bind address / public-tunnel mode.
    ToggleRemote { exposure: remote::Exposure },
    /// `/copy [N]` — write the resolved assistant response text to the clipboard. The driver loop
    /// owns the `arboard::Clipboard`, so dispatch resolves the text and hands it back to copy.
    CopyToClipboard(String),
}

/// Build a fully-populated [`forge_tui::MeshOverlay`] from a routing explanation.
/// Extracted so both the sync path and the background-task path can share the logic.
pub(crate) fn build_mesh_overlay(
    e: forge_mesh::RoutingExplanation,
    prompt: &str,
) -> forge_tui::MeshOverlay {
    let conserve_line = if !e.conserve.enabled {
        "off".to_string()
    } else if !e.conserve.eligible {
        "no frontier alternative → not applied".to_string()
    } else if e.conserve.fired {
        format!(
            "FIRED (roll {:.2} < P {:.2}) → spread to free frontier",
            e.conserve.roll, e.conserve.probability
        )
    } else {
        format!(
            "not fired (roll {:.2} ≥ P {:.2}) → subscription kept",
            e.conserve.roll, e.conserve.probability
        )
    };
    forge_tui::MeshOverlay {
        open: true,
        loading: false,
        prompt: prompt.to_string(),
        classified: e.classified_tier.as_str().to_string(),
        classifier: e.classifier_label.clone(),
        routed: e.routed_tier.as_str().to_string(),
        code_heavy: e.code_heavy,
        reasons: e.classify_reasons.join(", "),
        conserve_fired: e.conserve.fired,
        conserve_line,
        quota: e
            .quota
            .iter()
            .map(|q| forge_tui::MeshQuotaRow {
                provider: q.provider.clone(),
                fraction: q.fraction,
                plan: q.plan.clone(),
                status: format!("{:?}", q.status),
                spread_complex: q.spread_probability,
            })
            .collect(),
        candidates: e
            .candidates
            .iter()
            .take(12)
            .map(|c| forge_tui::MeshCandRow {
                rank: c.rank,
                model: c.row.model.clone(),
                score: c.row.final_score,
                cost_tag: match c.row.cost_class {
                    0 => "free",
                    1 => "subscription",
                    _ => "paid",
                }
                .to_string(),
                frontier: c.row.frontier,
                usable: c.usable,
                selected: c.selected,
                penalty: c.row.conserve_penalty,
            })
            .collect(),
        pick: e.pick.clone(),
        fallbacks: e.fallbacks.clone(),
        rationale: e.rationale.clone(),
        anim_tick: 0,
        scroll: 0,
    }
}

/// Execute a slash command (command-skill-system.md). Builtins are matched first; an unrecognised
/// `/name` falls through to the file-based command/skill [`forge_skills::Catalog`]. Returns
/// [`DispatchOutcome`]. Session-mutating commands (`/new`, `/resume`, `/clear`) and file
/// commands/skills are gated while a turn holds the session `Mutex`. All session access is
/// `lock().await` — no blocking on the render-loop thread (the #45 invariant).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch_command(
    line: &str,
    session: &Arc<tokio::sync::Mutex<Session>>,
    tui: &mut forge_tui::Tui,
    app: &mut forge_tui::App,
    catalog: &forge_skills::Catalog,
    armed: &mut std::collections::HashSet<String>,
    trust_project: bool,
    busy: bool,
    assay_lenses: &mut Vec<forge_types::FindingCategory>,
    assay_scope: &mut forge_types::AssayScope,
) -> Result<DispatchOutcome> {
    use forge_tui::CommandAction;
    let action = forge_tui::parse_command(line);
    // Everything that touches the live `Session` (lock().await) or swaps it is gated while a turn
    // holds the Mutex — opening the read-only `/sessions` picker is the one exception.
    let mutates = !matches!(
        action,
        CommandAction::Help
            | CommandAction::Quit
            | CommandAction::Unknown(_)
            | CommandAction::ListSessions
            | CommandAction::Resume(_)
            | CommandAction::ClearScreen
            | CommandAction::PinModel(_)
            | CommandAction::SetEffort(_)
            | CommandAction::Replay(_, _)
            | CommandAction::Usage
            | CommandAction::Remote { .. }
    );
    if busy && mutates {
        app.note("⚠ finish or Esc the current turn first");
        return Ok(DispatchOutcome::Handled);
    }
    match action {
        CommandAction::Help => app.palette.open_with(""),
        CommandAction::Quit => return Ok(DispatchOutcome::Quit),
        CommandAction::ClearScreen => {
            tui.clear_screen();
            app.clear_transcript();
            app.note("— screen cleared —");
        }
        CommandAction::New => {
            let cwd = std::env::current_dir()?.display().to_string();
            {
                let mut s = session.lock().await;
                s.reset_fresh(&cwd).map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            tui.clear_screen();
            app.clear_transcript();
            app.note("● new session");
        }
        // `/mode` opens the operating-mode (temper) picker — a reliable, discoverable alternative
        // to SHIFT+TAB. Enter sets the chosen temper in picker_accept.
        CommandAction::Mode => {
            let current = {
                let s = session.lock().await;
                s.temper().label()
            };
            let rows = forge_types::PermissionMode::all()
                .iter()
                .map(|m| {
                    let mark = if m.label() == current {
                        "   ● current"
                    } else {
                        ""
                    };
                    forge_tui::PickerRow {
                        id: m.label().to_string(),
                        title: m.label().to_string(),
                        subtitle: format!("{}{mark}", m.description()),
                    }
                })
                .collect();
            app.picker.open_with(
                forge_tui::PickerKind::Tempers,
                "switch operating mode",
                rows,
            );
        }
        // `/assay` enters Assay mode: pick analysis-only vs full cleanup; the crew then runs as a
        // background task (spawned in the picker-Enter handler so the spinner ticks).
        CommandAction::Assay { only, skip, scope } => {
            // Compute the lens set from --only/--skip and store for picker resolution.
            let crew = forge_types::FindingCategory::crew();
            *assay_lenses = if !only.is_empty() {
                crew.iter()
                    .filter(|l| only.iter().any(|o| o == l.as_str()))
                    .copied()
                    .collect()
            } else if !skip.is_empty() {
                crew.iter()
                    .filter(|l| !skip.iter().any(|s| s == l.as_str()))
                    .copied()
                    .collect()
            } else {
                Vec::new() // empty = use full crew (default)
            };
            // Resolve the scope string into a typed AssayScope.
            *assay_scope = if scope == "--diff" {
                forge_types::AssayScope::Diff
            } else if let Some(b) = scope.strip_prefix("--branch ") {
                forge_types::AssayScope::Branch(b.to_string())
            } else if let Some(r) = scope.strip_prefix("--since ") {
                forge_types::AssayScope::Since(r.to_string())
            } else if !scope.is_empty() {
                forge_types::AssayScope::Path(scope)
            } else {
                forge_types::AssayScope::Repo
            };
            let rows = vec![
                forge_tui::PickerRow {
                    id: "analysis".into(),
                    title: "Analysis only".into(),
                    subtitle: "review & ranked report — no edits".into(),
                },
                forge_tui::PickerRow {
                    id: "cleanup".into(),
                    title: "Full cleanup (Refine)".into(),
                    subtitle: "analyze, then auto-fix findings — permission-gated, /undo to revert"
                        .into(),
                },
            ];
            app.picker
                .open_with(forge_tui::PickerKind::AssayChoice, "⚒ assay — choose", rows);
        }
        // `/resume [prefix]` and `/sessions` both open the interactive picker; a prefix pre-fills
        // its filter. Resolving + swapping the session happens on Enter (picker_accept).
        CommandAction::Resume(prefix) => open_sessions_picker(app, &prefix)?,
        CommandAction::ListSessions => open_sessions_picker(app, "")?,
        // `/model <id>` pins a specific model for the rest of this session.
        // `/model` with no arg opens the interactive model browser — selecting a model pins it.
        // Works while a turn is running (pin takes effect on the NEXT turn).
        CommandAction::PinModel(Some(model_id)) => {
            // `/model <full-id>` (contains `::`) → pin immediately, no picker.
            // `/model <partial>` → open the animated ModelPin picker pre-filtered.
            if model_id.contains("::") {
                let model_id = forge_provider::normalize_model_id(&model_id).into_owned();
                let mut s = session.lock().await;
                s.pin_model(Some(model_id.clone()));
                app.note(&format!("⊕ model pinned: {model_id} (clear with /model)"));
            } else {
                open_model_pin_picker(session, app, &model_id).await?;
            }
        }
        CommandAction::PinModel(None) => {
            // Bare `/model` clears the model pin and returns to mesh auto-routing.
            session.lock().await.pin_model(None);
            app.note("⊕ model pin cleared — mesh auto-routing restored");
        }
        // `/effort <level>` pins the reasoning-effort level for subsequent turns.
        // `/effort` (bare) opens the interactive effort slider above the input bar.
        CommandAction::SetEffort(level) => match level {
            Some(ref s) => match forge_types::EffortLevel::parse(s) {
                Some(e) => {
                    session.lock().await.set_effort(Some(e));
                    app.apply(forge_tui::PresenterEvent::Effort(Some(e)));
                    app.note(&format!(
                        "◎ effort pinned: {} — use /effort to adjust",
                        e.as_str()
                    ));
                }
                None => {
                    app.note(&format!(
                        "⚠ unknown effort level '{s}' — use low/medium/high/xhigh"
                    ));
                }
            },
            None => {
                // Bare /effort → open the slider (same as Ctrl+R).
                app.effort_slider = true;
            }
        },
        // `/models` opens the interactive model browser: a provider list (with global counts in
        // the heading) that drills into each provider's models on Enter; Esc steps back.
        CommandAction::ListModels => open_models_root(session, app).await?,
        // `/config` launches the animated setup wizard full-screen (reconfigure mode): set
        // provider + search API keys, bridge plans, permission mode, and credit conservation.
        // Keys go to the OS keyring; all other settings are written to the user config file.
        // `/config` opens the dynamic settings editor (every scalar setting, fuzzy-searchable).
        // The guided provider/plan wizard now lives at `forge setup`.
        CommandAction::Config => {
            app.config_editor.open_with(config_editor_rows());
        }
        // `/thinking` toggles model reasoning/thinking block display for this session.
        CommandAction::Thinking => {
            app.show_thinking = !app.show_thinking;
            let state = if app.show_thinking { "on" } else { "off" };
            app.note(&format!("thinking display: {state}"));
        }
        // `/image <path>` attaches an image file to the next prompt as an input block.
        CommandAction::Image(path) => {
            let path = path.trim();
            if path.is_empty() {
                app.note("usage: /image <path>");
            } else {
                match crate::image_input::load_image_file(path) {
                    Ok((att, label)) => app.attach_image(att, &label),
                    Err(e) => app.note(&format!("⚠ {e}")),
                }
            }
        }
        CommandAction::Mcp(server) => {
            let s = session.lock().await;
            match server {
                Some(srv) => {
                    let tools = s.mcp_tool_lines(&srv);
                    if tools.is_empty() {
                        app.note(&format!("no tools for MCP server '{srv}' (not connected?)"));
                    } else {
                        app.note(&format!("{} tool(s) on '{srv}':", tools.len()));
                        for (name, desc) in tools {
                            app.note(&format!("  {name} — {desc}"));
                        }
                    }
                }
                None => app.apply(forge_tui::PresenterEvent::McpStatus(s.mcp_status())),
            }
        }
        // `/undo` and `/checkpoints` both open the same interactive picker over the per-turn
        // checkpoints — pick any past message to rewind (chat + files) to. Enter acts in
        // picker_accept.
        CommandAction::Undo => open_checkpoint_picker(session, app, "rewind to a message").await?,
        CommandAction::ListCheckpoints => {
            open_checkpoint_picker(session, app, "restore a checkpoint").await?
        }
        CommandAction::Checkpoint(name) => {
            {
                let mut s = session.lock().await;
                s.checkpoint(name.as_deref())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            match name {
                Some(n) => app.note(&format!("✓ checkpoint saved: {n}")),
                None => app.note("✓ checkpoint saved"),
            }
        }
        // `/compact` makes a model call → run it as a background task so the spinner ticks.
        CommandAction::Compact => return Ok(DispatchOutcome::RunCompact),
        // `/copy [N]` — resolve the Nth-latest assistant response and hand it to the loop to copy
        // (the loop owns the clipboard). N is 1-based from the most recent (1 = last response).
        CommandAction::Copy { nth } => {
            let history = { session.lock().await.history() };
            match nth_assistant_response(&history, nth) {
                Some(text) => {
                    let blocks = extract_code_blocks(&text);
                    if blocks.is_empty() {
                        // No code → copy the whole response straight away (no picker needed).
                        return Ok(DispatchOutcome::CopyToClipboard(text));
                    }
                    // Code present → offer a picker: the full response, or any individual block.
                    // Row `id` is the index into `app.copy_candidates`; 0 = full response.
                    let mut rows = vec![forge_tui::PickerRow {
                        id: "0".into(),
                        title: "Full response".into(),
                        subtitle: format!("{} chars", text.chars().count()),
                    }];
                    let mut candidates: Vec<(String, String)> = vec![(String::new(), text.clone())];
                    for (i, (lang, code)) in blocks.into_iter().enumerate() {
                        let label = if lang.is_empty() {
                            format!("Code block {}", i + 1)
                        } else {
                            format!("Code block {} · {lang}", i + 1)
                        };
                        rows.push(forge_tui::PickerRow {
                            id: (i + 1).to_string(),
                            title: label,
                            subtitle: format!("{} lines", code.lines().count()),
                        });
                        candidates.push((lang, code));
                    }
                    app.copy_candidates = candidates;
                    app.picker.open_with(
                        forge_tui::PickerKind::CopyBlocks,
                        "copy — Enter: clipboard · w: write to file · Esc: cancel",
                        rows,
                    );
                }
                None => {
                    let n = history
                        .iter()
                        .filter(|(role, _)| matches!(role, forge_types::Role::Assistant))
                        .count();
                    app.note(&format!(
                        "no assistant response #{nth} to copy (only {n} so far)"
                    ));
                }
            }
        }
        CommandAction::Lattice(symbol) => {
            if symbol.is_empty() {
                app.note("usage: /lattice <symbol>");
            } else {
                let view = { session.lock().await.lattice_view(&symbol)? };
                match view {
                    None => app.note("lattice is disabled (set [lattice] enabled = true)"),
                    Some(v) => {
                        let rows = |hits: &[forge_index::NodeHit]| {
                            hits.iter()
                                .map(|h| {
                                    (h.kind.clone(), h.name.clone(), h.rel_path.clone(), h.line)
                                })
                                .collect::<Vec<_>>()
                        };
                        let why = v.why.map(|p| (p.author, p.date, p.commit, p.subject));
                        let lines = forge_tui::lattice_view_lines(
                            &v.query,
                            &rows(&v.roots),
                            &rows(&v.dependents),
                            why,
                        );
                        emit_scrollback(tui, app, lines);
                    }
                }
            }
        }
        // `/init` — scan the repo and write `.forge/AGENTS.md`, the project memory the agent
        // auto-loads as a standing system prompt on future sessions.
        CommandAction::Init => {
            app.note("📝 scanning the repo to write .forge/AGENTS.md …");
            return Ok(DispatchOutcome::RunTurn {
                prompt: "Analyze this codebase and write a concise `.forge/AGENTS.md` capturing \
what a new contributor (human or agent) needs: a one-paragraph project overview; how to build, \
test, lint, and run it; the source layout and architecture; and the project's code conventions. \
Inspect the real files first (README, package/build manifests, CI config, the main source dirs) \
using your tools — do not guess. Then create `.forge/AGENTS.md` with the WriteFile tool. Keep it \
tight and accurate; omit anything you could not verify."
                    .to_string(),
                guidance: Vec::new(),
                tier: Some(forge_types::TaskTier::Complex),
            });
        }
        // `/plan <task>` — planning mode: switch to read-only (Plan) temper and run a turn that
        // investigates and proposes a plan without making any edits. Approved with `/execute`.
        CommandAction::Plan(task) => {
            let task = task.trim().to_string();
            if task.is_empty() {
                app.note("usage: /plan <task> — investigate read-only and propose a plan");
                return Ok(DispatchOutcome::Handled);
            }
            let label = {
                let mut s = session.lock().await;
                s.set_temper(forge_types::PermissionMode::Plan).label()
            };
            app.set_temper(label);
            app.note(
                "🗺 planning mode — read-only. I'll investigate, then present a plan to approve.",
            );
            return Ok(DispatchOutcome::RunTurn {
                prompt: format!(
                    "Investigate the codebase as needed, then produce a concrete, ordered, \
step-by-step plan to accomplish the task below. Do NOT make any edits or run state-changing \
commands — this is planning only. When the plan is ready, call the `present_plan` tool with a \
short title and the ordered steps (each a title + optional one-line detail, plus any notes) so the \
user can review and approve it interactively. Do not just describe the plan in prose — present it \
with the tool.\n\nTask: {task}"
                ),
                guidance: Vec::new(),
                tier: Some(forge_types::TaskTier::Complex),
            });
        }
        // `/execute` — approve the proposed plan: switch to Auto-edit (AcceptEdits) and carry it out.
        CommandAction::Execute => {
            let label = {
                let mut s = session.lock().await;
                s.set_temper(forge_types::PermissionMode::AcceptEdits)
                    .label()
            };
            app.set_temper(label);
            app.note("⚒ executing the approved plan (Auto-edit)");
            return Ok(DispatchOutcome::RunTurn {
                prompt: "Implement the plan you just proposed, step by step — make the edits and \
run the commands needed to carry it out. If something forces a deviation from the plan, say so \
and keep going."
                    .to_string(),
                guidance: Vec::new(),
                tier: Some(forge_types::TaskTier::Complex),
            });
        }
        // `/goal <objective>` — pin a persisted north-star, then run a turn that decomposes it
        // into a tracked task plan (update_tasks).
        CommandAction::Goal(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                app.note("usage: /goal <objective> — sets the goal and breaks it into tasks");
                return Ok(DispatchOutcome::Handled);
            }
            {
                let mut s = session.lock().await;
                s.prime_guidance(&[format!(
                    "Session goal: {text}\nKeep every step aligned to this goal until it is fully met."
                )])
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            app.note(&format!("🎯 goal set — {text}"));
            return Ok(DispatchOutcome::RunTurn {
                prompt: format!(
                    "Break this goal into a concrete, ordered plan and record it with the \
                     update_tasks tool, then start on the first step.\n\nGoal: {text}"
                ),
                guidance: Vec::new(),
                tier: Some(forge_types::TaskTier::Complex),
            });
        }
        // `/loop <task>` — autonomous re-run until the model signals completion.
        CommandAction::Loop(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                app.note("usage: /loop <task> — re-runs until the model signals it's complete");
                return Ok(DispatchOutcome::Handled);
            }
            return Ok(DispatchOutcome::StartLoop { prompt: text });
        }
        // `/replay <id>` — show a transcript inline; `/replay <a> <b>` diffs two sessions.
        CommandAction::Replay(id_a, id_b) => {
            if id_a.is_empty() {
                app.note("usage: /replay <id>  or  /replay <id-a> <id-b>");
                return Ok(DispatchOutcome::Handled);
            }
            let text = {
                let s = session.lock().await;
                match id_b {
                    None => {
                        // resolve prefix → full id, load, render
                        let ids = s
                            .matching_session_ids(&id_a)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        match ids.first() {
                            None => format!("no session matching '{id_a}'"),
                            Some(full) => {
                                let entries =
                                    s.load_replay(full).map_err(|e| anyhow::anyhow!("{e}"))?;
                                crate::replay::render_transcript(
                                    &full[..full.len().min(8)],
                                    &entries,
                                )
                            }
                        }
                    }
                    Some(id_b) => {
                        let ids_a = s
                            .matching_session_ids(&id_a)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        let ids_b = s
                            .matching_session_ids(&id_b)
                            .map_err(|e| anyhow::anyhow!("{e}"))?;
                        match (ids_a.first(), ids_b.first()) {
                            (Some(fa), Some(fb)) => {
                                let ea = s.load_replay(fa).map_err(|e| anyhow::anyhow!("{e}"))?;
                                let eb = s.load_replay(fb).map_err(|e| anyhow::anyhow!("{e}"))?;
                                let d = crate::replay::diff(&ea, &eb);
                                let fa8 = &fa[..fa.len().min(8)];
                                let fb8 = &fb[..fb.len().min(8)];
                                let mut out = crate::replay::render_diff(fa8, fb8, &d);
                                out.push('\n');
                                out.push_str(&crate::replay::render_turn_diff(fa8, fb8, &ea, &eb));
                                out
                            }
                            (None, _) => format!("no session matching '{id_a}'"),
                            (_, None) => format!("no session matching '{id_b}'"),
                        }
                    }
                }
            };
            emit_text(tui, app, &text);
        }
        CommandAction::Usage => {
            // Open immediately with fast session data; bridge stats load in background.
            let (
                (
                    month_usd,
                    by_model_5h,
                    by_model,
                    by_model_week,
                    (daily_cap, monthly_cap, weekly_cap),
                    _,
                ),
                (session_in, session_out, session_usd),
            ) = {
                let s = session.lock().await;
                (
                    (
                        s.spend_this_month_usd(),
                        s.spend_by_model_5h(),
                        s.spend_by_model_today(),
                        s.spend_by_model_week(),
                        s.budget_caps(),
                        s.bridge_fractions(),
                    ),
                    s.session_usage_db(),
                )
            };
            app.usage_overlay.open = true;
            app.usage_overlay.loading = true;
            app.usage_overlay.month_usd = month_usd;
            app.usage_overlay.session_usd = session_usd;
            app.usage_overlay.session_in = session_in;
            app.usage_overlay.session_out = session_out;
            app.usage_overlay.by_model_5h = by_model_5h;
            app.usage_overlay.by_model = by_model;
            app.usage_overlay.by_model_week = by_model_week;
            app.usage_overlay.daily_cap = daily_cap;
            app.usage_overlay.weekly_cap = weekly_cap;
            app.usage_overlay.monthly_cap = monthly_cap;
            // Bridge stats (subscription %s) fill in via the PendingUsage receiver.
            let (tx, rx) = tokio::sync::oneshot::channel::<bridge_stats::BridgeStats>();
            tokio::spawn(async move {
                let bstats = tokio::task::spawn_blocking(bridge_stats::fetch)
                    .await
                    .unwrap_or_default();
                let _ = tx.send(bstats);
            });
            // Claude quota refresh is fire-and-forget; tick-based auto-refresh picks it up.
            if claude_quota_is_stale(session, 300).await {
                let s = session.clone();
                tokio::spawn(async move { refresh_claude_quota(&s).await });
            }
            return Ok(DispatchOutcome::PendingUsage(rx));
        }
        CommandAction::Mesh(arg) => {
            let prompt = arg.unwrap_or_default();
            let to_explain = if prompt.trim().is_empty() {
                "design and prove correct a concurrent lock-free algorithm".to_string()
            } else {
                prompt.clone()
            };
            // Open immediately with loading spinner; bridge stats + routing compute in background.
            app.mesh_overlay = forge_tui::MeshOverlay {
                open: true,
                loading: true,
                prompt: prompt.trim().to_string(),
                ..Default::default()
            };
            let (tx, rx) = tokio::sync::oneshot::channel::<Option<forge_tui::MeshOverlay>>();
            let session_c = session.clone();
            let prompt_str = prompt.trim().to_string();
            tokio::spawn(async move {
                let bstats = tokio::task::spawn_blocking(bridge_stats::fetch)
                    .await
                    .unwrap_or_default();
                if claude_quota_is_stale(&session_c, 300).await {
                    let sc = session_c.clone();
                    tokio::spawn(async move { refresh_claude_quota(&sc).await });
                }
                let exp = {
                    let s = session_c.lock().await;
                    s.seed_subscription_quota("codex-cli", "five_hour", bstats.codex_5h_pct);
                    s.seed_subscription_quota("codex-cli", "weekly", bstats.codex_weekly_pct);
                    s.explain_routing(&to_explain)
                };
                let _ = tx.send(exp.map(|e| build_mesh_overlay(e, &prompt_str)));
            });
            return Ok(DispatchOutcome::PendingMesh(rx));
        }
        // `/remote` toggles remote control. The render loop owns the `RemoteControl` handle (it
        // needs the presenter channel + App state to broadcast snapshots + drain inputs), so the
        // command just signals the desired bind mode; the loop starts/stops the server there.
        CommandAction::Remote { mode } => {
            let exposure = match mode {
                forge_tui::RemoteMode::Lan => remote::Exposure::Lan,
                forge_tui::RemoteMode::Local => remote::Exposure::Local,
                forge_tui::RemoteMode::Anywhere => remote::Exposure::Anywhere,
            };
            return Ok(DispatchOutcome::ToggleRemote { exposure });
        }
        // `/remember <text>` — save a durable memory for this project.
        CommandAction::Remember(text) => {
            let text = text.trim().to_string();
            if text.is_empty() {
                app.note("usage: /remember <text>");
            } else {
                let scope = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "global".to_string());
                let session_id = {
                    let s = session.lock().await;
                    s.id().to_string()
                };
                let s = session.lock().await;
                match s.store.add_memory(&scope, "fact", &text, &session_id) {
                    Ok(_) => app.note(&format!("💭 remembered: {text}")),
                    Err(e) => app.note(&format!("⚠ failed to remember: {e}")),
                }
            }
        }
        // `/memories` — list this project's memories.
        CommandAction::Memories => {
            let scope = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "global".to_string());
            let s = session.lock().await;
            match s.store.list_memories(&scope) {
                Ok(mems) if mems.is_empty() => app.note("no memories yet"),
                Ok(mems) => {
                    app.note(&format!("{} memories:", mems.len()));
                    for m in mems {
                        app.note(&format!(
                            "  {}  [{}] {}",
                            &m.id[..m.id.len().min(8)],
                            m.kind,
                            m.text
                        ));
                    }
                }
                Err(e) => app.note(&format!("⚠ failed to list memories: {e}")),
            }
        }
        // `/self-mcp [enable|disable]` — toggle or set the self-MCP sub-agent live.
        CommandAction::SelfMcp(explicit) => {
            let current = forge_config::load().map(|c| c.self_mcp).unwrap_or(false);
            let enable = explicit.unwrap_or(!current);
            match forge_config::write_self_mcp(enable) {
                Err(e) => app.note(&format!("⚠ self-mcp: failed to write config: {e}")),
                Ok(_) => {
                    if enable {
                        let exe = std::env::current_exe()
                            .unwrap_or_else(|_| std::path::PathBuf::from("forge"));
                        let server = forge_config::McpServerConfig {
                            name: "forge".to_string(),
                            transport: forge_config::McpTransport::Stdio {
                                command: exe.to_string_lossy().into_owned(),
                                args: vec!["mcp".to_string(), "agent".to_string()],
                                env: std::collections::HashMap::new(),
                            },
                            auth: None,
                            enabled: true,
                        };
                        let mut s = session.lock().await;
                        match s.add_mcp_server(server).await {
                            Ok(()) => app.note(
                                "self-MCP enabled — forge_chat / forge_assay now available \
                                 as tools in this session",
                            ),
                            Err(e) => app.note(&format!(
                                "self-MCP enabled in config but live connect failed: {e}"
                            )),
                        }
                    } else {
                        session.lock().await.remove_mcp_server("forge");
                        app.note("self-MCP disabled — sub-Forge MCP server disconnected");
                    }
                }
            }
        }
        // Not a builtin → try the file-based command/skill catalog.
        CommandAction::Unknown(_) => {
            return dispatch_catalog(line, catalog, session, app, armed, trust_project, busy).await
        }
    }
    Ok(DispatchOutcome::Handled)
}

/// Resolve a `/line` that isn't a builtin against the file catalog: expand a command, load a
/// skill's methodology, or report a missing-arg / unknown error. A project-scope definition is
/// gated on first use (re-run confirms) unless `trust_project`.
pub(crate) async fn dispatch_catalog(
    line: &str,
    catalog: &forge_skills::Catalog,
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
    armed: &mut std::collections::HashSet<String>,
    trust_project: bool,
    busy: bool,
) -> Result<DispatchOutcome> {
    use forge_skills::Resolved;
    match catalog.resolve(line) {
        Resolved::Command {
            cmd,
            prompt,
            guidance,
        } => {
            if busy {
                app.note("⚠ finish or Esc the current turn first");
                return Ok(DispatchOutcome::Handled);
            }
            if !project_trust_ok(&cmd.name, cmd.scope, trust_project, armed, app) {
                return Ok(DispatchOutcome::Handled);
            }
            app.note(&format!(
                "⚒ command · /{} ({})",
                cmd.name,
                cmd.scope.label()
            ));
            Ok(DispatchOutcome::RunTurn {
                prompt,
                guidance,
                tier: cmd.tier,
            })
        }
        Resolved::Skill { meta, prompt } => {
            if busy {
                app.note("⚠ finish or Esc the current turn first");
                return Ok(DispatchOutcome::Handled);
            }
            if !project_trust_ok(&meta.name, meta.scope, trust_project, armed, app) {
                return Ok(DispatchOutcome::Handled);
            }
            let skill = forge_skills::Skill::load(&meta);
            for w in &skill.warnings {
                app.note(&format!("⚠ {w}"));
            }
            app.note(&format!("⚒ skill · {} ({})", meta.name, meta.scope.label()));
            if !skill.resources.is_empty() {
                app.note(&format!(
                    "↳ loaded methodology + {} resource(s)",
                    skill.resources.len()
                ));
            }
            let guidance = vec![skill.guidance()];
            if prompt.trim().is_empty() {
                // No task given: prime the methodology into the transcript (no model call) so it
                // shapes the next turn the user types.
                {
                    let mut s = session.lock().await;
                    s.prime_guidance(&guidance)
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                }
                app.note("↳ methodology primed — type your task");
                Ok(DispatchOutcome::Handled)
            } else {
                Ok(DispatchOutcome::RunTurn {
                    prompt,
                    guidance,
                    tier: meta.tier,
                })
            }
        }
        Resolved::MissingArgs { name, missing } => {
            let need = missing
                .iter()
                .map(|m| format!("<{m}>"))
                .collect::<Vec<_>>()
                .join(" ");
            app.note(&format!("/{name} requires {need}"));
            Ok(DispatchOutcome::Handled)
        }
        Resolved::Unknown(x) => {
            app.note(&format!("unknown command: /{x} — try /help"));
            Ok(DispatchOutcome::Handled)
        }
        // A `/`-line never resolves to Plain, but stay safe rather than silently submit it.
        Resolved::Plain(_) => {
            app.note("unknown command — try /help");
            Ok(DispatchOutcome::Handled)
        }
    }
}
