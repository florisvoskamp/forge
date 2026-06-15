//! The animated-TUI driver pieces. `ChannelPresenter` forwards a session's presenter
//! events over a channel (so a turn can run on a background task), and `Tui` owns the
//! terminal for the render loop. The actual loop lives in the binary (it owns the
//! `Session`, which this crate must not depend on).

use std::io::{self, Stdout};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use forge_types::SideEffect;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line as TextLine;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::app::{self, App, KeyKind, LIVE_H};
use crate::{Presenter, PresenterEvent};

/// A message from a running turn to the render loop.
pub enum UiMsg {
    Event(PresenterEvent),
    Permission {
        tool: String,
        side_effect: SideEffect,
        reply: Sender<bool>,
    },
    /// An interactive question (AskUserQuestion): the loop shows it + the options and replies
    /// with the chosen label or a free-text answer.
    Question {
        question: String,
        options: Vec<crate::QChoice>,
        allow_other: bool,
        reply: Sender<String>,
    },
}

/// A presenter that forwards everything over a channel; safe to move onto a task.
pub struct ChannelPresenter {
    tx: Sender<UiMsg>,
}

impl ChannelPresenter {
    pub fn new(tx: Sender<UiMsg>) -> Self {
        Self { tx }
    }
}

impl Presenter for ChannelPresenter {
    fn emit(&mut self, event: PresenterEvent) {
        let _ = self.tx.send(UiMsg::Event(event));
    }

    fn confirm(&mut self, tool: &str, side_effect: SideEffect) -> bool {
        let (reply, answer) = std::sync::mpsc::channel();
        if self
            .tx
            .send(UiMsg::Permission {
                tool: tool.to_string(),
                side_effect,
                reply,
            })
            .is_err()
        {
            return false;
        }
        answer.recv().unwrap_or(false) // blocks this turn task until the loop answers
    }

    fn ask(&mut self, question: &str, options: &[crate::QChoice], allow_other: bool) -> String {
        let (reply, answer) = std::sync::mpsc::channel();
        if self
            .tx
            .send(UiMsg::Question {
                question: question.to_string(),
                options: options.to_vec(),
                allow_other,
                reply,
            })
            .is_err()
        {
            return crate::NO_ANSWER.to_string();
        }
        answer
            .recv()
            .unwrap_or_else(|_| crate::NO_ANSWER.to_string()) // blocks the turn task until answered
    }

    fn read_line(&mut self) -> Option<String> {
        None // input is handled by the render loop, not the presenter
    }
}

/// Owns the terminal for the render loop. Uses an *inline* viewport (no alternate screen):
/// finalized lines flow into the terminal's native scrollback via [`Tui::insert_lines`],
/// and only the small pinned live region is redrawn each frame.
pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(LIVE_H),
            },
        )?;
        Ok(Self { terminal })
    }

    /// Current terminal width (for building width-dependent scrollback like the banner).
    pub fn width(&self) -> u16 {
        self.terminal.size().map(|s| s.width).unwrap_or(80)
    }

    /// Push finalized lines into the terminal's native scrollback, above the live region.
    pub fn insert_lines(&mut self, lines: Vec<TextLine<'static>>) {
        if lines.is_empty() {
            return;
        }
        let width = self.width();
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let height = (para.line_count(width) as u16).max(1);
        let _ = self.terminal.insert_before(height, |buf| {
            para.render(buf.area, buf);
        });
    }

    pub fn draw(&mut self, app: &App) {
        let _ = self.terminal.draw(|f| app::render_live(f, app));
    }

    /// Non-blocking: returns a keystroke if one is pending, else `None`.
    pub fn poll_key(&self) -> io::Result<Option<KeyKind>> {
        if !event::poll(Duration::from_millis(0))? {
            return Ok(None);
        }
        if let Event::Key(k) = event::read()? {
            if k.kind == KeyEventKind::Press {
                let key = match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::Esc
                    }
                    KeyCode::Char(c) => KeyKind::Char(c),
                    KeyCode::Backspace => KeyKind::Backspace,
                    KeyCode::Enter => KeyKind::Enter,
                    KeyCode::Esc => KeyKind::Esc,
                    // Shift+Tab — crossterm reports it as BackTab — cycles the operating temper.
                    KeyCode::BackTab => KeyKind::CycleTemper,
                    _ => return Ok(None),
                };
                return Ok(Some(key));
            }
        }
        Ok(None)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        // No alternate screen to leave: the conversation stays in the user's scrollback.
        println!();
    }
}
