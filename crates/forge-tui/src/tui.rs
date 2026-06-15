//! `TuiPresenter`: the synchronous ratatui+crossterm renderer for `forge run --tui`. It
//! owns an *inline* terminal viewport (no alternate screen), folds each [`PresenterEvent`]
//! into [`app::App`], flushes finalized lines into the terminal's native scrollback, and
//! redraws the small pinned live region. `confirm` shows a permission prompt and blocks on
//! a key. All rendering lives in `app` (pure, TestBackend-tested); this is the I/O shell.

use std::io::{self, Stdout};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use forge_types::SideEffect;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line as TextLine;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::app::{self, banner_lines, handle_key, App, InputOutcome, KeyKind, LIVE_H};
use crate::{Presenter, PresenterEvent};

pub struct TuiPresenter {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    app: App,
}

impl TuiPresenter {
    /// Enter raw mode + an inline viewport and take over the bottom of the terminal.
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        // From here on, any failure must undo raw mode — Drop won't run because the
        // struct isn't constructed yet, which would otherwise leave the shell broken.
        Self::enter().inspect_err(|_| {
            let _ = disable_raw_mode();
        })
    }

    fn enter() -> io::Result<Self> {
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(LIVE_H),
            },
        )?;
        let width = terminal.size().map(|s| s.width).unwrap_or(80);
        let banner = banner_lines(width);
        let mut me = Self {
            terminal,
            app: App::default(),
        };
        me.insert_lines(banner);
        Ok(me)
    }

    fn insert_lines(&mut self, lines: Vec<TextLine<'static>>) {
        if lines.is_empty() {
            return;
        }
        let width = self.terminal.size().map(|s| s.width).unwrap_or(80);
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let height = (para.line_count(width) as u16).max(1);
        let _ = self.terminal.insert_before(height, |buf| {
            para.render(buf.area, buf);
        });
    }

    fn flush(&mut self) {
        let lines = self.app.drain_flush();
        self.insert_lines(lines);
    }

    fn draw(&mut self) {
        let app = &self.app;
        let _ = self.terminal.draw(|f| app::render_live(f, app));
    }

    fn restore(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        self.terminal.show_cursor()?;
        println!();
        Ok(())
    }
}

impl Drop for TuiPresenter {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl Presenter for TuiPresenter {
    fn emit(&mut self, event: PresenterEvent) {
        self.app.apply(event);
        self.flush();
        self.draw();
    }

    fn confirm(&mut self, tool: &str, side_effect: SideEffect) -> bool {
        self.app.prompt = Some(format!("allow {tool} ({side_effect:?})?"));
        self.draw();

        let allowed = loop {
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => break true,
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => break false,
                    _ => {}
                },
                Ok(_) => {}
                Err(_) => break false, // can't read input -> deny (safe)
            }
        };

        self.app.prompt = None;
        self.draw();
        allowed
    }

    fn ask(&mut self, question: &str, options: &[crate::QChoice], allow_other: bool) -> String {
        self.app.set_question(question, options, allow_other);
        self.app.input.clear();
        self.flush();
        self.draw();
        loop {
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let key = match k.code {
                        KeyCode::Char(c) => KeyKind::Char(c),
                        KeyCode::Backspace => KeyKind::Backspace,
                        KeyCode::Enter => KeyKind::Enter,
                        _ => continue,
                    };
                    match handle_key(&mut self.app.input, key) {
                        InputOutcome::Submit(line) => {
                            if let Some(ans) = self.app.resolve_question(&line) {
                                self.flush();
                                self.draw();
                                return ans;
                            }
                            // Invalid → keep the question open, clear the line, re-prompt.
                            self.app.input.clear();
                            self.draw();
                        }
                        InputOutcome::Quit => return crate::NO_ANSWER.to_string(),
                        InputOutcome::Editing => self.draw(),
                    }
                }
                Ok(_) => {}
                Err(_) => return crate::NO_ANSWER.to_string(),
            }
        }
    }

    fn read_line(&mut self) -> Option<String> {
        self.app.input.clear();
        self.draw();
        loop {
            match event::read() {
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let key = match k.code {
                        // Ctrl-C quits, like a shell.
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            KeyKind::Esc
                        }
                        KeyCode::Char(c) => KeyKind::Char(c),
                        KeyCode::Backspace => KeyKind::Backspace,
                        KeyCode::Enter => KeyKind::Enter,
                        KeyCode::Esc => KeyKind::Esc,
                        _ => continue,
                    };
                    match handle_key(&mut self.app.input, key) {
                        InputOutcome::Editing => self.draw(),
                        InputOutcome::Submit(line) => {
                            // Echo the user's message into scrollback.
                            self.app.submit_user(&line);
                            self.flush();
                            self.draw();
                            return Some(line);
                        }
                        InputOutcome::Quit => return None,
                    }
                }
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
}
