//! The dedicated full-screen workflow view (docs/rfcs/forge-workflow.md): a live, animated
//! dashboard for a running `run_workflow` / `/workflow run` script. Workflows do NOT render in
//! the sticky subagent activity panel — this view is their surface. It shows the phase tree with
//! per-phase progress meters, one row per agent with its streaming activity edge, the script's
//! `log()` narration feed, and live agent/cost totals.
//!
//! Lifecycle: [`PresenterEvent::WorkflowStarted`](crate::PresenterEvent::WorkflowStarted)
//! auto-opens the view and marks the run active; every `Subagent*` event until
//! `WorkflowFinished` folds in here (see `App::apply`). Esc backgrounds the view — the script
//! keeps running and a slim one-line status band stays above the input — and Ctrl+O reopens it.
//! Enter zooms into the selected agent's full transcript, reusing the same
//! [`transcript_lines`](crate::transcript::transcript_lines) renderer as the Ctrl+O viewer.

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TextLine, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{
    model_short, truncate, ActivityKind, ActivityStatus, App, TranscriptView, ACCENT, DIM, ERRRED,
    OKGREEN, ORANGE, SPINNER, TEXT, TOOLCYAN, WARNYEL,
};

/// Cap on retained narration lines (a chatty script can't grow the feed without bound).
const LOG_FEED_MAX: usize = 200;
/// Cap on retained agent rows — matches the activity panel's own retention bound; oldest
/// *finished* rows drop first, running rows are never dropped.
const MAX_ROWS: usize = 500;
/// Transcript-line cap per row, mirroring the activity panel's per-row bound.
const MAX_ROW_LOG: usize = 10_000;

/// One `phase(title)` group in the tree, in call order.
#[derive(Debug, Clone)]
pub struct WfPhase {
    pub title: String,
}

/// One agent's live row.
#[derive(Debug, Clone)]
pub struct WfRow {
    pub id: String,
    pub agent: String,
    pub task: String,
    pub model: Option<String>,
    /// Index into [`WorkflowView::phases`]; `None` for rows started outside any `phase()`.
    pub phase_idx: Option<usize>,
    /// Trailing edge of the streamed activity (one line, shown under the row while running).
    pub last: String,
    /// Assembled transcript lines for the Enter-zoom (same assembly as the activity panel).
    pub log: Vec<String>,
    pub done: bool,
    /// Only meaningful once `done`; `true` while running.
    pub ok: bool,
    pub cost: f64,
}

/// Scroll state of the Enter-zoom transcript, mirroring `ViewerState` semantics: the
/// `usize::MAX / 2` sentinel means "tail", clamped at render time.
#[derive(Debug, Clone)]
pub struct WfZoom {
    pub scroll: usize,
    pub follow: bool,
}

impl Default for WfZoom {
    fn default() -> Self {
        Self {
            scroll: usize::MAX / 2,
            follow: true,
        }
    }
}

/// All state of the workflow view. One workflow run per turn; reset by `App::on_turn_start`.
#[derive(Debug, Clone, Default)]
pub struct WorkflowView {
    /// Full-screen visible. Auto-set on `WorkflowStarted`; Esc clears it (the run continues).
    pub open: bool,
    /// A script is executing (between `WorkflowStarted` and `WorkflowFinished`).
    pub active: bool,
    /// Saved-script name (`/workflow run <name>`), `None` for a model-authored script.
    pub name: Option<String>,
    pub phases: Vec<WfPhase>,
    /// Agent rows in start order (render groups them by `phase_idx`).
    pub rows: Vec<WfRow>,
    /// `log()` narration, newest last, bounded to [`LOG_FEED_MAX`].
    pub logs: Vec<String>,
    /// Selection cursor into `rows`.
    pub selected: usize,
    /// `Some` while the Enter-zoom transcript is open over the dashboard.
    pub zoom: Option<WfZoom>,
    /// Set by `WorkflowFinished`: (every agent succeeded, one-line summary).
    pub finished: Option<(bool, String)>,
    /// Open-reveal animation tick — advanced by the render loop until [`Self::settle_tick`],
    /// then stops (no infinite redraw). Spinners use the global `App::tick` instead.
    pub anim_tick: u32,
    /// Zoom scroll geometry `(wrapped_len, body_h)` recorded by the render path, so a downward
    /// scroll can re-arm follow at the tail (same mechanism as `App::viewer_geom`).
    pub zoom_geom: std::cell::Cell<Option<(usize, u16)>>,
}

impl WorkflowView {
    /// A run exists this turn (live, or finished with its rows still viewable).
    pub fn exists(&self) -> bool {
        self.active || !self.rows.is_empty() || self.finished.is_some()
    }

    /// The one-line status band above the input is shown while a live run's view is backgrounded.
    pub fn band_visible(&self) -> bool {
        self.active && !self.open
    }

    /// The tick at which the open-reveal animation is fully settled.
    pub fn settle_tick(&self) -> u32 {
        (self.rows.len() as u32 + self.phases.len() as u32) * 2 + 12
    }

    pub fn begin(&mut self, name: Option<String>) {
        *self = WorkflowView {
            open: true,
            active: true,
            name,
            ..Default::default()
        };
    }

    pub fn on_phase(&mut self, title: String) {
        self.phases.push(WfPhase { title });
    }

    pub fn on_agent_start(
        &mut self,
        id: String,
        agent: String,
        task: String,
        model: Option<String>,
        phase: Option<String>,
    ) {
        // Map the row's phase label to a tree index. An `opts.phase` override that never went
        // through `phase()` still gets its own group, appended in first-seen order.
        let phase_idx = phase.map(|p| {
            self.phases
                .iter()
                .rposition(|ph| ph.title == p)
                .unwrap_or_else(|| {
                    self.phases.push(WfPhase { title: p });
                    self.phases.len() - 1
                })
        });
        self.rows.push(WfRow {
            id,
            agent,
            task,
            model,
            phase_idx,
            last: String::new(),
            log: Vec::new(),
            done: false,
            ok: true,
            cost: 0.0,
        });
        if self.rows.len() > MAX_ROWS {
            let mut excess = self.rows.len() - MAX_ROWS;
            let selected_id = self.rows.get(self.selected).map(|r| r.id.clone());
            self.rows.retain(|r| {
                let drop = excess > 0 && r.done;
                if drop {
                    excess -= 1;
                }
                !drop
            });
            // Keep the cursor on the same row across the drop when possible.
            self.selected = selected_id
                .and_then(|id| self.rows.iter().position(|r| r.id == id))
                .unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)));
        }
    }

    pub fn on_progress(&mut self, id: &str, snippet: &str) {
        let Some(row) = self.rows.iter_mut().find(|r| r.id == id && !r.done) else {
            return;
        };
        // Trailing edge for the live row (one line), same shape as the activity panel's rows.
        row.last.push_str(snippet.replace('\n', " ").as_str());
        let n = row.last.chars().count();
        if n > 120 {
            row.last = row.last.chars().skip(n - 120).collect();
        }
        // Assemble streamed fragments into coherent transcript lines for the Enter-zoom.
        if row.log.is_empty() {
            row.log.push(String::new());
        }
        for ch in snippet.chars() {
            if ch == '\n' {
                row.log.push(String::new());
            } else {
                row.log.last_mut().unwrap().push(ch);
            }
        }
        if row.log.len() > MAX_ROW_LOG {
            let drop = row.log.len() - MAX_ROW_LOG;
            row.log.drain(0..drop);
        }
    }

    pub fn on_result(&mut self, id: &str, ok: bool, summary: &str, cost_usd: f64) {
        let Some(row) = self.rows.iter_mut().find(|r| r.id == id) else {
            return;
        };
        row.done = true;
        row.ok = ok;
        row.cost = cost_usd;
        if row.log.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
            row.log.pop();
        }
        row.log.push(String::new());
        row.log.push(format!(
            "── result ({}) ──",
            if ok { "ok" } else { "failed" }
        ));
        for piece in summary.split('\n') {
            row.log.push(piece.to_string());
        }
    }

    pub fn on_log(&mut self, msg: String) {
        self.logs.push(msg);
        if self.logs.len() > LOG_FEED_MAX {
            let drop = self.logs.len() - LOG_FEED_MAX;
            self.logs.drain(0..drop);
        }
    }

    pub fn finish(&mut self, ok: bool, summary: String) {
        self.active = false;
        self.finished = Some((ok, summary));
    }

    /// (running, done, failed, total cost) across all rows.
    pub fn totals(&self) -> (usize, usize, usize, f64) {
        let running = self.rows.iter().filter(|r| !r.done).count();
        let done = self.rows.iter().filter(|r| r.done && r.ok).count();
        let failed = self.rows.iter().filter(|r| r.done && !r.ok).count();
        let cost = self.rows.iter().map(|r| r.cost).sum();
        (running, done, failed, cost)
    }

    /// Move the selection cursor, wrapping.
    pub fn move_selection(&mut self, delta: isize) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).rem_euclid(n as isize) as usize;
    }

    /// One `TranscriptView` per row for the Enter-zoom, rendered by the shared
    /// `transcript_lines` machinery (same header/entry-switching UX as the Ctrl+O viewer).
    pub fn zoom_views(&self) -> Vec<TranscriptView> {
        self.rows
            .iter()
            .map(|r| {
                let lines: Vec<TextLine<'static>> = if r.log.iter().all(|l| l.trim().is_empty()) {
                    vec![TextLine::from(Span::styled(
                        "(no activity captured yet)",
                        Style::default().fg(DIM),
                    ))]
                } else {
                    r.log
                        .iter()
                        .map(|l| {
                            let style = if l.starts_with("── result") {
                                Style::default().fg(TOOLCYAN)
                            } else {
                                Style::default().fg(TEXT)
                            };
                            TextLine::from(Span::styled(l.clone(), style))
                        })
                        .collect()
                };
                TranscriptView {
                    kind: ActivityKind::Subagent,
                    title: r.agent.clone(),
                    subtitle: r.task.clone(),
                    model: r.model.clone(),
                    status: row_status(r),
                    cost: r.cost,
                    lines,
                    line_count: r.log.len(),
                }
            })
            .collect()
    }

    /// Same tail-re-follow the Ctrl+O viewer uses: a downward scroll reaching the last full page
    /// clamps and re-arms follow, using the geometry the render path recorded.
    pub fn zoom_refollow_at_tail(&mut self) {
        let geom = self.zoom_geom.get();
        let Some(z) = self.zoom.as_mut() else {
            return;
        };
        if let Some((wrapped_len, body_h)) = geom {
            let max_scroll = wrapped_len.saturating_sub(body_h as usize);
            if z.scroll >= max_scroll {
                z.scroll = max_scroll;
                z.follow = true;
            }
        }
    }
}

fn row_status(r: &WfRow) -> ActivityStatus {
    if !r.done {
        ActivityStatus::Running
    } else if r.ok {
        ActivityStatus::Done
    } else {
        ActivityStatus::Failed
    }
}

/// The one-line status band shown above the input while a live run's view is backgrounded.
pub(crate) fn workflow_band_line(app: &App) -> TextLine<'static> {
    let wf = &app.workflow;
    let (running, done, failed, cost) = wf.totals();
    let spin = SPINNER[app.tick % SPINNER.len()];
    let phase = wf
        .phases
        .last()
        .map(|p| format!(" · ▶ {}", truncate(&p.title, 24)))
        .unwrap_or_default();
    let failed_part = if failed > 0 {
        format!(" · {failed} failed")
    } else {
        String::new()
    };
    TextLine::from(vec![
        Span::styled(
            format!("  ⛓ {spin} workflow"),
            Style::default().fg(ORANGE).bold(),
        ),
        Span::styled(
            format!("{phase} · {running} running · {done} done{failed_part} · ${cost:.4}  "),
            Style::default().fg(TEXT),
        ),
        Span::styled("^O view", Style::default().fg(DIM)),
    ])
}

/// An eased meter like the mesh inspector's quota gauges: `frac` filled of `width` cells.
fn meter(frac: f64, ease: f32, width: usize, color: ratatui::style::Color) -> Vec<Span<'static>> {
    let frac = (frac * ease as f64).clamp(0.0, 1.0);
    let filled = (frac * width as f64).round() as usize;
    vec![
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled("░".repeat(width - filled), Style::default().fg(DIM)),
    ]
}

/// Render the full-screen workflow view over the whole frame. Called by `render_live` when
/// [`WorkflowView::open`]; the Enter-zoom transcript takes over the frame while `zoom` is set.
pub(crate) fn render_workflow_view(f: &mut Frame, app: &App) {
    let wf = &app.workflow;
    let area = f.area();
    f.render_widget(Clear, area);

    // ── Enter-zoom: the selected agent's transcript, via the shared viewer renderer. ──
    if let Some(z) = &wf.zoom {
        let views = wf.zoom_views();
        if !views.is_empty() {
            let selected = wf.selected.min(views.len() - 1);
            let scroll = if z.follow { usize::MAX / 2 } else { z.scroll };
            let wrapped_len = crate::transcript::wrap_lines(
                &views[selected].lines,
                area.width.saturating_sub(1) as usize,
            )
            .len();
            wf.zoom_geom
                .set(Some((wrapped_len, area.height.saturating_sub(3).max(1))));
            f.render_widget(
                Paragraph::new(crate::transcript::transcript_lines(
                    &views,
                    selected,
                    scroll,
                    area.height,
                    area.width,
                )),
                area,
            );
            return;
        }
    }
    wf.zoom_geom.set(None);

    let spin = SPINNER[app.tick % SPINNER.len()];
    let (running, done, failed, cost) = wf.totals();
    let total = wf.rows.len();

    // Header block: state glyph + name + elapsed, bordered in the forge ember accent.
    let state = match &wf.finished {
        None => format!("{spin} running"),
        Some((true, _)) => "✓ finished".to_string(),
        Some((false, _)) => "✗ finished with errors".to_string(),
    };
    let name = wf
        .name
        .as_deref()
        .map(|n| format!("'{n}' "))
        .unwrap_or_default();
    let title = format!(
        " ⛓ workflow {name}· {state} · ⧖ {}s ",
        app.turn_elapsed_secs
    );
    let border_color = match &wf.finished {
        None => ORANGE,
        Some((true, _)) => OKGREEN,
        Some((false, _)) => ERRRED,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::default().fg(border_color).bold(),
        ))
        .title_bottom(Span::styled(
            " ↑↓ select · ⏎ transcript · esc background (^O reopens) ",
            Style::default().fg(DIM),
        ))
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 24 || inner.height < 4 {
        return;
    }

    let ease = ((wf.anim_tick as f32) / 6.0).min(1.0);

    // ── Top: overall progress meter + totals. ──
    let mut top: Vec<TextLine> = Vec::new();
    let frac = if total == 0 {
        0.0
    } else {
        (done + failed) as f64 / total as f64
    };
    let meter_w = (inner.width as usize).saturating_sub(46).clamp(10, 40);
    let mut spans = vec![Span::styled("  ", Style::default())];
    spans.extend(meter(
        frac,
        ease,
        meter_w,
        if failed > 0 { WARNYEL } else { OKGREEN },
    ));
    let failed_part = if failed > 0 {
        format!(" · {failed} failed")
    } else {
        String::new()
    };
    spans.push(Span::styled(
        format!("  {done}/{total} agents · {running} running{failed_part} · ${cost:.4}"),
        Style::default().fg(TEXT),
    ));
    top.push(TextLine::from(spans));
    if let Some((ok, summary)) = &wf.finished {
        let (glyph, color) = if *ok {
            ("✓", OKGREEN)
        } else {
            ("⚠", WARNYEL)
        };
        top.push(TextLine::from(Span::styled(
            format!(
                "  {glyph} {}",
                truncate(summary, inner.width.saturating_sub(6) as usize)
            ),
            Style::default().fg(color),
        )));
    }
    top.push(TextLine::from(""));

    // ── Body: phase tree with per-phase meters + agent rows, revealed row-by-row. ──
    // Group order: rows outside any phase first (they started before the first `phase()`),
    // then phases in call order. `body` collects (line, Some(row_idx)) so the cursor's line is
    // known for autoscroll.
    let mut body: Vec<(TextLine, Option<usize>)> = Vec::new();
    let groups: Vec<Option<usize>> = std::iter::once(None)
        .chain((0..wf.phases.len()).map(Some))
        .collect();
    for g in groups {
        let members: Vec<usize> = wf
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.phase_idx == g)
            .map(|(i, _)| i)
            .collect();
        if let Some(pi) = g {
            // A phase header renders even before its first agent spawns (live feedback that the
            // script advanced), with a mini-meter once it has members.
            let p = &wf.phases[pi];
            let p_done = members.iter().filter(|&&i| wf.rows[i].done).count();
            let p_failed = members
                .iter()
                .filter(|&&i| wf.rows[i].done && !wf.rows[i].ok)
                .count();
            let glyph = if members.is_empty() || p_done < members.len() {
                Span::styled(format!("{spin} "), Style::default().fg(ACCENT))
            } else if p_failed > 0 {
                Span::styled("✗ ", Style::default().fg(ERRRED))
            } else {
                Span::styled("✓ ", Style::default().fg(OKGREEN))
            };
            let mut spans = vec![
                Span::styled("  ▶ ", Style::default().fg(WARNYEL).bold()),
                glyph,
                Span::styled(
                    format!("{}  ", truncate(&p.title, 40)),
                    Style::default().fg(WARNYEL).bold(),
                ),
            ];
            if !members.is_empty() {
                spans.extend(meter(
                    p_done as f64 / members.len() as f64,
                    ease,
                    12,
                    if p_failed > 0 { WARNYEL } else { OKGREEN },
                ));
                spans.push(Span::styled(
                    format!(" {p_done}/{}", members.len()),
                    Style::default().fg(DIM),
                ));
            }
            body.push((TextLine::from(spans), None));
        } else if members.is_empty() {
            continue;
        }
        for i in members {
            let r = &wf.rows[i];
            let selected = i == wf.selected;
            let marker = if selected { "▸" } else { " " };
            let status = match row_status(r) {
                ActivityStatus::Running => {
                    Span::styled(format!("{spin} "), Style::default().fg(ACCENT))
                }
                ActivityStatus::Done => Span::styled("✓ ", Style::default().fg(OKGREEN)),
                _ => Span::styled("✗ ", Style::default().fg(ERRRED)),
            };
            let mut base = if selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            if r.done && !r.ok {
                base = base.fg(ERRRED);
            }
            let cost = if r.cost > 0.0 {
                format!("  ${:.4}", r.cost)
            } else {
                String::new()
            };
            let model = model_short(r.model.as_deref());
            let task_max = (inner.width as usize)
                .saturating_sub(16 + r.agent.chars().count() + model.chars().count() + cost.len())
                .max(8);
            body.push((
                TextLine::from(vec![
                    Span::styled(format!("   {marker} "), Style::default().fg(ACCENT)),
                    status,
                    Span::styled(format!("{} ", r.agent), base.fg(TOOLCYAN)),
                    Span::styled(format!("[{model}] "), base.fg(DIM)),
                    Span::styled(truncate(&r.task, task_max), base),
                    Span::styled(cost, Style::default().fg(DIM)),
                ]),
                Some(i),
            ));
            // Live activity edge under every running row — the "what is it doing" pulse.
            if !r.done && !r.last.is_empty() {
                body.push((
                    TextLine::from(Span::styled(
                        format!(
                            "        ▏{}",
                            truncate(&r.last, (inner.width as usize).saturating_sub(10).max(8))
                        ),
                        Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
                    )),
                    None,
                ));
            }
        }
    }
    if wf.rows.is_empty() && wf.phases.is_empty() {
        body.push((
            TextLine::from(Span::styled(
                format!("  {spin} authoring… waiting for the first agent to spawn"),
                Style::default().fg(DIM),
            )),
            None,
        ));
    }

    // ── Bottom: `log()` narration feed (tail), when the script has narrated anything. ──
    let feed_h = if wf.logs.is_empty() {
        0
    } else {
        (wf.logs.len().min(4) + 1) as u16
    };
    let top_h = (top.len() as u16).min(inner.height.saturating_sub(2));
    let chunks = Layout::vertical([
        Constraint::Length(top_h),
        Constraint::Min(1),
        Constraint::Length(feed_h),
    ])
    .split(inner);
    f.render_widget(Paragraph::new(top), chunks[0]);

    // Reveal rows over the first frames (ease-in like the palette), then keep the selected row
    // visible: stay at the top until the cursor passes the viewport, then follow it.
    let revealed = (((wf.anim_tick as usize) * 2).max(1)).min(body.len());
    let cursor_line = body
        .iter()
        .take(revealed)
        .position(|(_, idx)| *idx == Some(wf.selected.min(wf.rows.len().saturating_sub(1))))
        .unwrap_or(0) as u16;
    let body_h = chunks[1].height;
    let max_scroll = (revealed as u16).saturating_sub(body_h);
    let scroll = if cursor_line < body_h {
        0
    } else {
        (cursor_line + 1).saturating_sub(body_h)
    }
    .min(max_scroll);
    let body_lines: Vec<TextLine> = body.into_iter().take(revealed).map(|(l, _)| l).collect();
    f.render_widget(Paragraph::new(body_lines).scroll((scroll, 0)), chunks[1]);

    if feed_h > 0 {
        let mut feed: Vec<TextLine> = vec![TextLine::from(Span::styled(
            "  ── narration ──",
            Style::default().fg(DIM),
        ))];
        for msg in wf.logs.iter().rev().take(feed_h as usize - 1).rev() {
            feed.push(TextLine::from(vec![
                Span::styled("  💬 ", Style::default().fg(ACCENT)),
                Span::styled(
                    truncate(msg, (inner.width as usize).saturating_sub(8).max(8)),
                    Style::default().fg(TEXT),
                ),
            ]));
        }
        f.render_widget(Paragraph::new(feed), chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(wf: &mut WorkflowView, id: &str, phase: Option<&str>) {
        wf.on_agent_start(
            id.to_string(),
            "general".into(),
            format!("task {id}"),
            Some("prov::model".into()),
            phase.map(str::to_string),
        );
    }

    #[test]
    fn begin_resets_state_and_auto_opens() {
        let mut wf = WorkflowView::default();
        wf.on_log("stale".into());
        wf.begin(Some("audit".into()));
        assert!(wf.open && wf.active);
        assert_eq!(wf.name.as_deref(), Some("audit"));
        assert!(wf.logs.is_empty() && wf.rows.is_empty());
    }

    #[test]
    fn rows_group_under_their_phase_and_ad_hoc_phases_get_their_own_group() {
        let mut wf = WorkflowView::default();
        wf.begin(None);
        wf.on_phase("research".into());
        started(&mut wf, "a", Some("research"));
        // An opts.phase override never announced via phase() still forms a group.
        started(&mut wf, "b", Some("verify"));
        assert_eq!(wf.phases.len(), 2);
        assert_eq!(wf.rows[0].phase_idx, Some(0));
        assert_eq!(wf.rows[1].phase_idx, Some(1));
    }

    #[test]
    fn progress_assembles_lines_and_result_marks_the_row() {
        let mut wf = WorkflowView::default();
        wf.begin(None);
        started(&mut wf, "a", None);
        wf.on_progress("a", "hello\nworld");
        wf.on_result("a", false, "boom", 0.01);
        let r = &wf.rows[0];
        assert!(r.done && !r.ok);
        assert!(r.log.iter().any(|l| l == "hello"));
        assert!(r.log.iter().any(|l| l.contains("result (failed)")));
        let (_, _, failed, cost) = wf.totals();
        assert_eq!(failed, 1);
        assert!(cost > 0.0);
    }

    #[test]
    fn finish_deactivates_but_keeps_rows_viewable() {
        let mut wf = WorkflowView::default();
        wf.begin(None);
        started(&mut wf, "a", None);
        wf.on_result("a", true, "done", 0.0);
        wf.finish(true, "all good".into());
        assert!(!wf.active);
        assert!(wf.exists(), "rows stay browsable after the run ends");
        assert!(!wf.band_visible(), "no band once the run is over");
    }

    #[test]
    fn zoom_views_mirror_row_status() {
        let mut wf = WorkflowView::default();
        wf.begin(None);
        started(&mut wf, "a", None);
        started(&mut wf, "b", None);
        wf.on_result("b", false, "nope", 0.0);
        let views = wf.zoom_views();
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].status, ActivityStatus::Running);
        assert_eq!(views[1].status, ActivityStatus::Failed);
    }

    #[test]
    fn selection_wraps_both_directions() {
        let mut wf = WorkflowView::default();
        wf.begin(None);
        started(&mut wf, "a", None);
        started(&mut wf, "b", None);
        started(&mut wf, "c", None);
        wf.move_selection(-1);
        assert_eq!(wf.selected, 2);
        wf.move_selection(1);
        assert_eq!(wf.selected, 0);
    }
}
