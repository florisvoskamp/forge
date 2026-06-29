use super::*;

/// Offer, on resuming a previously-compacted session, whether the MODEL should continue with the
/// compacted context (fast, fits) or re-read the full original history. Either way the user already
/// sees the full conversation in scrollback. Resolved in `picker_accept`.
pub(crate) fn open_resume_choice_picker(app: &mut forge_tui::App) {
    let rows = vec![
        forge_tui::PickerRow {
            id: "compacted".into(),
            title: "Continue with the compacted context (recommended)".into(),
            subtitle: "the model reads a summary of earlier turns — fast, fits the window".into(),
        },
        forge_tui::PickerRow {
            id: "full".into(),
            title: "Reload the FULL history into context (uncompacted)".into(),
            subtitle: "the model re-reads the entire conversation — may auto-compact again".into(),
        },
    ];
    app.picker.open_with(
        forge_tui::PickerKind::ResumeMode,
        "this session was compacted — how should the model continue?",
        rows,
    );
}

pub(crate) fn open_sessions_picker(app: &mut forge_tui::App, query: &str) -> Result<()> {
    let store = open_store()?;
    let list = store.list_sessions().context("listing sessions")?;
    if list.is_empty() {
        app.note("no past sessions yet");
        return Ok(());
    }
    let active_ids = store.active_agent_session_ids().unwrap_or_default();
    let rows = list
        .into_iter()
        .take(50)
        .map(|s| {
            let is_active = active_ids.contains(&s.id);
            let id8: String = s.id.chars().take(8).collect();
            // Title = a clean one-line snippet of the first user prompt (newlines/extra spaces
            // collapsed), so each row reads as a recognizable conversation rather than a hash.
            let mut title = session_title(s.preview.as_deref());
            if is_active {
                title = format!("⚡ {}", title);
            }
            let subtitle = if is_active {
                format!(
                    "[LIVE] {id8} · {} · {} msgs · ${:.4}",
                    fmt_age(s.last_activity),
                    s.message_count,
                    s.total_cost_usd,
                )
            } else {
                format!(
                    "{id8} · {} · {} msgs · ${:.4}",
                    fmt_age(s.last_activity),
                    s.message_count,
                    s.total_cost_usd,
                )
            };
            let id = if is_active {
                format!("observe:{}", s.id)
            } else {
                s.id
            };
            forge_tui::PickerRow {
                title,
                subtitle,
                id,
            }
        })
        .collect();
    app.picker
        .open_with(forge_tui::PickerKind::Sessions, "resume a session", rows);
    app.picker.query = query.to_string();
    app.picker.clamp();
    Ok(())
}

/// Read the session's checkpoints (one per turn, newest first) and open the rewind picker.
pub(crate) async fn open_checkpoint_picker(
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
    heading: &str,
) -> Result<()> {
    let rows = {
        let s = session.lock().await;
        checkpoint_rows(&s.checkpoints().map_err(|e| anyhow::anyhow!("{e}"))?)
    };
    if rows.is_empty() {
        app.note("nothing to undo yet");
    } else {
        app.picker
            .open_with(forge_tui::PickerKind::Checkpoints, heading, rows);
    }
    Ok(())
}

/// One picker row per checkpoint, reading as a message list: the prompt preview is the title,
/// with the turn index + age as the subtitle.
pub(crate) fn checkpoint_rows(cps: &[forge_store::CheckpointRow]) -> Vec<forge_tui::PickerRow> {
    cps.iter()
        .map(|c| forge_tui::PickerRow {
            id: c.seq.to_string(),
            title: c
                .label
                .clone()
                .unwrap_or_else(|| format!("turn @ {}", c.seq)),
            subtitle: format!("#{} · {}", c.seq, fmt_age(c.created_at)),
        })
        .collect()
}

/// Build the top-level provider list for the `/models` browser, with a stats heading.
pub(crate) fn models_provider_view(
    cat: &forge_mesh::ModelCatalog,
    pricing: &forge_mesh::pricing::Pricing,
    benched: &forge_types::ModelHealth,
) -> (String, Vec<forge_tui::PickerRow>) {
    let s = cat.stats(pricing);
    let heading = format!(
        "⊞ models — {} total · {} frontier · {} free · {} subscription · {} providers",
        s.total, s.frontier, s.free, s.subscription, s.providers
    );
    let rows = cat
        .by_provider(pricing)
        .into_iter()
        .map(|g| {
            let benched_n = g
                .models
                .iter()
                .filter(|m| benched.is_benched(&m.id))
                .count();
            let mut parts = vec![format!("{} models", g.total())];
            if g.frontier() > 0 {
                parts.push(format!("{} frontier", g.frontier()));
            }
            if g.free() > 0 {
                parts.push(format!("{} free", g.free()));
            }
            if g.paid() > 0 {
                parts.push(format!("{} paid", g.paid()));
            }
            if benched_n > 0 {
                parts.push(format!("{benched_n} benched"));
            }
            forge_tui::PickerRow {
                id: g.provider.clone(),
                title: g.provider.clone(),
                subtitle: parts.join(" · "),
            }
        })
        .collect();
    (heading, rows)
}

/// Build the drill-in model list for one provider (Enter on a provider row).
pub(crate) fn models_for_provider(
    cat: &forge_mesh::ModelCatalog,
    pricing: &forge_mesh::pricing::Pricing,
    benched: &forge_types::ModelHealth,
    provider: &str,
) -> (String, Vec<forge_tui::PickerRow>) {
    let rows: Vec<forge_tui::PickerRow> = cat
        .by_provider(pricing)
        .into_iter()
        .find(|g| g.provider == provider)
        .map(|g| {
            g.models
                .iter()
                .map(|m| {
                    let name = if m.name.is_empty() {
                        "(default model)".to_string()
                    } else {
                        m.name.clone()
                    };
                    let mut badges: Vec<String> = Vec::new();
                    if m.subscription {
                        badges.push("subscription".into());
                    }
                    if m.frontier {
                        badges.push("frontier".into());
                    }
                    if m.free {
                        badges.push("free".into());
                    }
                    if m.cost > f64::EPSILON {
                        badges.push(format!("paid ~${:.4}/turn", m.cost));
                    } else if m.paid {
                        badges.push("paid".into()); // metered gateway model, price unknown
                    }
                    if benched.is_benched(&m.id) {
                        badges.push("benched".into());
                    }
                    forge_tui::PickerRow {
                        id: m.id.clone(),
                        title: name,
                        subtitle: badges.join(" · "),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let heading = format!("⊞ {provider} — {} model(s)  ·  esc: back", rows.len());
    (heading, rows)
}

/// Open the flat ranked model pin-picker (`/model <partial>` or Ctrl+M).
/// Shows "mesh (auto)" at top, then all known models ranked by tier:
/// subscription → frontier → paid → free. Each row's subtitle encodes the tier so the
/// render loop can color-code it. An optional pre-filled `query` narrows the list.
pub(crate) async fn open_model_pin_picker(
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
    query: &str,
) -> Result<()> {
    let benched = open_store()?.current_benched().unwrap_or_default();
    let rows_opt: Option<Vec<forge_tui::PickerRow>> = {
        let s = session.lock().await;
        s.catalog().map(|cat| {
            let pricing = s.pricing();
            // Collect all models flat from all providers.
            let mut models: Vec<forge_mesh::ModelInfo> = cat
                .by_provider(pricing)
                .into_iter()
                .flat_map(|g| g.models)
                .collect();
            // Sort by tier priority: subscription first, then frontier, then paid, then free.
            // Within same tier, alphabetical by id.
            models.sort_by(|a, b| {
                tier_priority(a)
                    .cmp(&tier_priority(b))
                    .then(a.id.cmp(&b.id))
            });
            let mut rows = vec![forge_tui::PickerRow {
                id: "mesh".into(),
                title: "⊞ mesh (auto-route)".into(),
                subtitle: "let forge pick the best model for each task".into(),
            }];
            for m in models {
                let display_name = if m.name.is_empty() {
                    m.id.clone()
                } else {
                    format!("{} ({})", m.name, m.provider)
                };
                let mut badges: Vec<&str> = Vec::new();
                if m.subscription {
                    badges.push("subscription");
                }
                if m.frontier {
                    badges.push("frontier");
                }
                if m.free {
                    badges.push("free");
                }
                if m.paid && !m.free {
                    badges.push("paid");
                }
                if benched.is_benched(&m.id) {
                    badges.push("benched");
                }
                let cost_str;
                let mut sub = badges.join(" · ");
                if m.cost > 1e-9 {
                    cost_str = format!("~${:.4}/turn", m.cost);
                    if !sub.is_empty() {
                        sub.push_str(" · ");
                    }
                    sub.push_str(&cost_str);
                }
                rows.push(forge_tui::PickerRow {
                    id: m.id,
                    title: display_name,
                    subtitle: sub,
                });
            }
            rows
        })
    };
    match rows_opt {
        Some(rows) if !rows.is_empty() => {
            app.picker.open_with(
                forge_tui::PickerKind::ModelPin,
                "⊕ pin model — Enter to select · Esc cancel · /model clears pin",
                rows,
            );
            if !query.is_empty() {
                app.picker.query = query.to_string();
                app.picker.clamp();
            }
        }
        Some(_) => {
            app.note(
                "no models discovered — set a provider key (`forge auth <provider>`) or run ollama",
            );
        }
        None => app.note("model discovery is off (mock/offline) — nothing to pick"),
    }
    Ok(())
}

/// Tier sort priority: lower = shown first.
fn tier_priority(m: &forge_mesh::ModelInfo) -> u8 {
    if m.subscription {
        0
    } else if m.frontier {
        1
    } else if m.paid {
        2
    } else {
        3
    } // free
}

/// Open the `/models` browser at the top-level provider list (also the Esc target from a drill-in).
pub(crate) async fn open_models_root(
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
) -> Result<()> {
    let benched = open_store()?.current_benched().unwrap_or_default();
    let view = {
        let s = session.lock().await;
        s.catalog()
            .map(|c| models_provider_view(c, s.pricing(), &benched))
    };
    match view {
        Some((heading, rows)) if !rows.is_empty() => {
            app.models_drilled = None;
            app.picker
                .open_with(forge_tui::PickerKind::Models, &heading, rows);
        }
        Some(_) => app.note(
            "no models discovered — set a provider key (`forge auth <provider>`) or run ollama",
        ),
        None => app.note("model discovery is off (mock/offline) — nothing to browse"),
    }
    Ok(())
}

/// Open the model picker for `/model` (bare): shows the same provider browser as `/models`,
/// but selecting a leaf model row pins it (closes the picker + shows a confirmation note).
/// Drill the `/models` browser into one provider's models.
pub(crate) async fn open_models_provider(
    session: &Arc<tokio::sync::Mutex<Session>>,
    app: &mut forge_tui::App,
    provider: &str,
) -> Result<()> {
    let benched = open_store()?.current_benched().unwrap_or_default();
    let view = {
        let s = session.lock().await;
        s.catalog()
            .map(|c| models_for_provider(c, s.pricing(), &benched, provider))
    };
    if let Some((heading, rows)) = view {
        app.models_drilled = Some(provider.to_string());
        app.picker
            .open_with(forge_tui::PickerKind::Models, &heading, rows);
    }
    Ok(())
}

/// Act on the picker's selected row: resume the chosen session, or rewind to the chosen
/// checkpoint — then redraw the surviving transcript into scrollback.
pub(crate) async fn picker_accept(
    kind: forge_tui::PickerKind,
    row: &forge_tui::PickerRow,
    session: &Arc<tokio::sync::Mutex<Session>>,
    tui: &mut forge_tui::Tui,
    app: &mut forge_tui::App,
) -> Result<()> {
    match kind {
        forge_tui::PickerKind::Sessions => {
            let (items, compacted, view) = {
                let mut s = session.lock().await;
                s.reset_resumed(&row.id)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                (s.replay_items_full(), s.was_compacted(), s.view_snapshot())
            };
            tui.clear_screen();
            app.clear_transcript();
            app.note(&format!(
                "● resumed {}",
                row.id.chars().take(8).collect::<String>()
            ));
            app.replay_history(&items);
            // Restore the saved on-screen view (activity panel, viewer, scroll) for this session.
            if let Some(json) = view {
                app.restore_view_json(&json);
            }
            // If it was compacted, immediately offer compacted-vs-full for the model's context.
            if compacted {
                open_resume_choice_picker(app);
            }
        }
        forge_tui::PickerKind::Checkpoints => {
            let seq: i64 = row.id.parse().unwrap_or(0);
            let (items, outcome) = {
                let mut s = session.lock().await;
                let outcome = s.rewind_to(seq).map_err(|e| anyhow::anyhow!("{e}"))?;
                (s.replay_items(), outcome)
            };
            tui.clear_screen();
            app.clear_transcript();
            app.note("● rewound to that point");
            app.replay_history(&items);
            note_restore(app, &outcome.restore);
            // Put the rewound-to message back in the input box so it can be edited/resubmitted.
            if let Some(prompt) = outcome.rewound_prompt {
                app.input = prompt;
            }
        }
        forge_tui::PickerKind::Tempers => {
            if let Some(mode) = forge_types::PermissionMode::from_label(&row.id) {
                let label = {
                    let mut s = session.lock().await;
                    s.set_temper(mode).label()
                };
                app.set_temper(label);
                app.note(&format!("◆ mode → {label}"));
                // Persist as the default for the next session (best-effort).
                let _ = forge_config::write_permission_mode(mode);
            }
        }
        // Assay's choice is handled in the render loop (it spawns a background task), never here.
        forge_tui::PickerKind::AssayChoice => {}
        // The models browser drills/steps within the render loop; Enter never resolves here.
        forge_tui::PickerKind::Models => {}
        // The copy picker resolves in the render loop (it needs the clipboard); never here.
        forge_tui::PickerKind::CopyBlocks => {}
        // Flat model pin picker: "mesh" → clear pin, any other row → pin it.
        forge_tui::PickerKind::ModelPin => {
            if row.id == "mesh" {
                session.lock().await.pin_model(None);
                app.note("⊕ model pin cleared — mesh auto-routing restored");
            } else {
                let model_id = forge_provider::normalize_model_id(&row.id).into_owned();
                session.lock().await.pin_model(Some(model_id.clone()));
                app.note(&format!("⊕ model pinned: {model_id} (clear with /model)"));
            }
        }
        forge_tui::PickerKind::ResumeMode => {
            if row.id == "full" {
                let n = {
                    let mut s = session.lock().await;
                    s.reload_full_context()
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    s.history().len()
                };
                app.note(&format!(
                    "● reloaded the full history into context ({n} messages, uncompacted)"
                ));
            } else {
                app.note("● continuing with the compacted context");
            }
        }
    }
    Ok(())
}
