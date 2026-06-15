//! The `forge init` first-run setup wizard: a full-screen, animated ratatui flow to enable
//! providers (enter API keys, masked) and pick the subscription plan backing each installed CLI
//! bridge. Pure [`State`] + transitions are unit-tested; [`run`] is the thin terminal I/O shell.
//! Keys are returned to the caller to store in the OS keyring — never written to disk here.

use std::collections::HashMap;
use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};

const ORANGE: Color = Color::Rgb(255, 145, 60);
const USER: Color = Color::Rgb(125, 180, 255);
const DIM: Color = Color::Rgb(110, 110, 120);
const OKGREEN: Color = Color::Rgb(120, 210, 140);
const WARNYEL: Color = Color::Rgb(235, 200, 110);
const TOOLCYAN: Color = Color::Rgb(120, 200, 215);

/// A key-based provider offered in the wizard.
pub struct ProviderItem {
    pub id: String,
    pub label: String,
    pub had_key: bool,
}

/// An installed CLI bridge offered a plan choice: `plans` is `(human label, stored slug)`.
pub struct BridgeItem {
    pub prefix: String,
    pub plans: Vec<(String, String)>,
}

/// Everything the composition root (forge-cli) feeds the wizard.
pub struct WizardInput {
    pub providers: Vec<ProviderItem>,
    pub bridges: Vec<BridgeItem>,
}

/// What the wizard collected on finish.
#[derive(Debug, Default, PartialEq)]
pub struct WizardOutcome {
    /// `(provider, key)` to store in the OS keyring.
    pub keys: Vec<(String, String)>,
    /// Bridge prefix → chosen plan slug.
    pub plans: HashMap<String, String>,
    pub cancelled: bool,
}

/// A focusable row: a provider key field, a bridge plan chooser, or the Finish button.
#[derive(Clone, Copy, PartialEq)]
enum Row {
    Provider(usize),
    Bridge(usize),
    Finish,
}

/// Pure wizard state (no I/O), so navigation/editing is unit-testable.
struct State {
    input: WizardInput,
    /// Entered key per provider (empty = leave as-is / skip).
    keys: Vec<String>,
    /// Selected plan index per bridge.
    plan_sel: Vec<Option<usize>>,
    cursor: usize,
    /// True while typing into the focused provider's key field.
    editing: bool,
    /// Reveal animation 0.0→1.0.
    anim: f32,
    done: bool,
    cancelled: bool,
}

impl State {
    fn new(input: WizardInput) -> Self {
        let keys = vec![String::new(); input.providers.len()];
        let plan_sel = vec![None; input.bridges.len()];
        Self {
            input,
            keys,
            plan_sel,
            cursor: 0,
            editing: false,
            anim: 0.0,
            done: false,
            cancelled: false,
        }
    }

    /// The full focusable row list: providers, then installed bridges, then Finish.
    fn rows(&self) -> Vec<Row> {
        let mut r: Vec<Row> = (0..self.input.providers.len()).map(Row::Provider).collect();
        r.extend((0..self.input.bridges.len()).map(Row::Bridge));
        r.push(Row::Finish);
        r
    }

    fn focused(&self) -> Row {
        self.rows()[self.cursor.min(self.rows().len() - 1)]
    }

    fn move_up(&mut self) {
        if !self.editing {
            self.cursor = self.cursor.saturating_sub(1);
        }
    }

    fn move_down(&mut self) {
        if !self.editing {
            self.cursor = (self.cursor + 1).min(self.rows().len() - 1);
        }
    }

    /// Enter: start/stop editing a provider key, cycle nothing on a bridge, or finish.
    fn enter(&mut self) {
        match self.focused() {
            Row::Provider(_) => self.editing = !self.editing,
            Row::Bridge(i) => {
                // Enter advances the plan selection (wraps), a quick way to pick without digits.
                let n = self.input.bridges[i].plans.len();
                if n > 0 {
                    let next = self.plan_sel[i].map(|s| (s + 1) % n).unwrap_or(0);
                    self.plan_sel[i] = Some(next);
                }
            }
            Row::Finish => self.done = true,
        }
    }

    /// A digit selects the Nth plan when a bridge row is focused.
    fn digit(&mut self, d: u32) {
        if let Row::Bridge(i) = self.focused() {
            let idx = (d as usize).wrapping_sub(1);
            if idx < self.input.bridges[i].plans.len() {
                self.plan_sel[i] = Some(idx);
            }
        }
    }

    fn push_char(&mut self, c: char) {
        if self.editing {
            if let Row::Provider(i) = self.focused() {
                self.keys[i].push(c);
            }
        } else if c.is_ascii_digit() {
            self.digit(c.to_digit(10).unwrap());
        }
    }

    fn backspace(&mut self) {
        if self.editing {
            if let Row::Provider(i) = self.focused() {
                self.keys[i].pop();
            }
        }
    }

    /// Esc: stop editing if mid-key, else cancel the whole wizard.
    fn escape(&mut self) {
        if self.editing {
            self.editing = false;
        } else {
            self.cancelled = true;
            self.done = true;
        }
    }

    fn tick(&mut self) {
        if self.anim < 1.0 {
            self.anim = (self.anim + 0.12).min(1.0);
        }
    }

    fn outcome(&self) -> WizardOutcome {
        if self.cancelled {
            return WizardOutcome {
                cancelled: true,
                ..Default::default()
            };
        }
        let keys = self
            .input
            .providers
            .iter()
            .zip(&self.keys)
            .filter(|(_, k)| !k.is_empty())
            .map(|(p, k)| (p.id.clone(), k.clone()))
            .collect();
        let plans = self
            .input
            .bridges
            .iter()
            .zip(&self.plan_sel)
            .filter_map(|(b, sel)| sel.map(|i| (b.prefix.clone(), b.plans[i].1.clone())))
            .collect();
        WizardOutcome {
            keys,
            plans,
            cancelled: false,
        }
    }
}

/// Run the wizard against `input`, returning what the user chose. Enters the alternate screen +
/// raw mode and restores them on exit (and on panic, via the shared restore hook).
pub fn run(input: WizardInput) -> io::Result<WizardOutcome> {
    crate::driver::install_panic_restore();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen)?;
    let result = run_loop(input);
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
    result
}

fn run_loop(input: WizardInput) -> io::Result<WizardOutcome> {
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut state = State::new(input);
    loop {
        terminal.draw(|f| render(f, &state))?;
        if state.done {
            return Ok(state.outcome());
        }
        // Poll with a short timeout so the reveal animation advances even without input.
        if event::poll(Duration::from_millis(60))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Release {
                    match k.code {
                        KeyCode::Up => state.move_up(),
                        KeyCode::Down => state.move_down(),
                        KeyCode::Enter => state.enter(),
                        KeyCode::Esc => state.escape(),
                        KeyCode::Backspace => state.backspace(),
                        KeyCode::Char(c) => state.push_char(c),
                        _ => {}
                    }
                }
            }
        } else {
            state.tick();
        }
    }
}

fn render(f: &mut Frame, state: &State) {
    let area = f.area();
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ORANGE))
        .title(Span::styled(
            " ⚒ Forge setup ",
            Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "Enable providers (Enter to type a key, masked) · pick your bridge plan · then Finish.",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        "↑/↓ move · Enter edit/cycle · digits pick a plan · Esc cancel",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(""));

    let rows = state.rows();
    // Reveal rows progressively with the open animation.
    let revealed = ((state.anim * rows.len() as f32).ceil() as usize).clamp(1, rows.len());

    lines.push(section("Providers", USER));
    for (vi, row) in rows.iter().enumerate().take(revealed) {
        let selected = vi == state.cursor;
        match row {
            Row::Provider(i) => lines.push(provider_line(state, *i, selected)),
            Row::Bridge(_) => {}
            Row::Finish => {}
        }
    }
    if !state.input.bridges.is_empty() {
        lines.push(Line::from(""));
        lines.push(section("Subscription bridges", TOOLCYAN));
        for (vi, row) in rows.iter().enumerate().take(revealed) {
            if let Row::Bridge(i) = row {
                lines.push(bridge_line(state, *i, vi == state.cursor));
            }
        }
    }
    lines.push(Line::from(""));
    let finish_selected = matches!(state.focused(), Row::Finish);
    let finish_style = if finish_selected {
        Style::default().fg(OKGREEN).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(OKGREEN)
    };
    lines.push(Line::from(Span::styled(
        format!("{} Finish setup", marker(finish_selected)),
        finish_style,
    )));

    f.render_widget(Paragraph::new(lines).alignment(Alignment::Left), pad(inner));
}

fn section(title: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {title}"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn marker(selected: bool) -> &'static str {
    if selected {
        "▸"
    } else {
        " "
    }
}

fn provider_line(state: &State, i: usize, selected: bool) -> Line<'static> {
    let p = &state.input.providers[i];
    let editing = selected && state.editing;
    let key = &state.keys[i];
    let value = if editing {
        format!("{}▌", "•".repeat(key.len()))
    } else if !key.is_empty() {
        format!("{} set", "•".repeat(key.len().min(8)))
    } else if p.had_key {
        "[already configured]".to_string()
    } else {
        "(Enter to add a key)".to_string()
    };
    let name_color = if selected { ORANGE } else { USER };
    let val_color = if editing {
        WARNYEL
    } else if !key.is_empty() || p.had_key {
        OKGREEN
    } else {
        DIM
    };
    Line::from(vec![
        Span::styled(
            format!("  {} {:<14}", marker(selected), p.id),
            Style::default().fg(name_color),
        ),
        Span::styled(format!("{:<42} ", p.label), Style::default().fg(DIM)),
        Span::styled(value, Style::default().fg(val_color)),
    ])
}

fn bridge_line(state: &State, i: usize, selected: bool) -> Line<'static> {
    let b = &state.input.bridges[i];
    let chosen = state.plan_sel[i]
        .map(|s| b.plans[s].0.as_str())
        .unwrap_or("(choose)");
    let opts = b
        .plans
        .iter()
        .enumerate()
        .map(|(n, (label, _))| format!("{})_{}", n + 1, label).replace('_', " "))
        .collect::<Vec<_>>()
        .join("  ");
    let name_color = if selected { ORANGE } else { TOOLCYAN };
    Line::from(vec![
        Span::styled(
            format!("  {} {:<12}", marker(selected), b.prefix),
            Style::default().fg(name_color),
        ),
        Span::styled(
            format!("{:<14}", chosen),
            Style::default().fg(OKGREEN).add_modifier(Modifier::BOLD),
        ),
        Span::styled(opts, Style::default().fg(DIM)),
    ])
}

/// Inset the content one column/row inside the border.
fn pad(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> WizardInput {
        WizardInput {
            providers: vec![
                ProviderItem {
                    id: "anthropic".into(),
                    label: "Claude API".into(),
                    had_key: false,
                },
                ProviderItem {
                    id: "groq".into(),
                    label: "Groq free".into(),
                    had_key: true,
                },
            ],
            bridges: vec![BridgeItem {
                prefix: "claude-cli".into(),
                plans: vec![
                    ("Pro".into(), "pro".into()),
                    ("Max 20×".into(), "max-20x".into()),
                ],
            }],
        }
    }

    #[test]
    fn rows_are_providers_then_bridges_then_finish() {
        let s = State::new(input());
        let rows = s.rows();
        assert!(matches!(rows[0], Row::Provider(0)));
        assert!(matches!(rows[2], Row::Bridge(0)));
        assert!(matches!(rows[3], Row::Finish));
    }

    #[test]
    fn typing_a_key_collects_it_in_the_outcome() {
        let mut s = State::new(input());
        // Focus provider 0, start editing, type a key, commit.
        s.enter();
        assert!(s.editing);
        for c in "sk-test".chars() {
            s.push_char(c);
        }
        s.enter(); // commit
        assert!(!s.editing);
        let out = s.outcome();
        assert_eq!(
            out.keys,
            vec![("anthropic".to_string(), "sk-test".to_string())]
        );
    }

    #[test]
    fn digit_selects_a_bridge_plan() {
        let mut s = State::new(input());
        // Move to the bridge row (index 2).
        s.move_down();
        s.move_down();
        assert!(matches!(s.focused(), Row::Bridge(0)));
        s.push_char('2'); // pick "Max 20×"
        let out = s.outcome();
        assert_eq!(
            out.plans.get("claude-cli").map(String::as_str),
            Some("max-20x")
        );
    }

    #[test]
    fn esc_cancels_and_yields_no_keys() {
        let mut s = State::new(input());
        s.escape();
        assert!(s.done);
        let out = s.outcome();
        assert!(out.cancelled && out.keys.is_empty() && out.plans.is_empty());
    }

    #[test]
    fn navigation_clamps_at_the_ends() {
        let mut s = State::new(input());
        s.move_up(); // already at top
        assert_eq!(s.cursor, 0);
        for _ in 0..20 {
            s.move_down();
        }
        assert!(matches!(s.focused(), Row::Finish));
    }
}
