//! `TuiPresenter`: the interactive ratatui+crossterm renderer. It owns the terminal,
//! folds each [`PresenterEvent`] into [`app::App`], and repaints — so the UI updates live
//! as a turn progresses. `confirm` shows a permission prompt and blocks on a key. All the
//! rendering logic lives in `app` (pure, TestBackend-tested); this module is the I/O shell.

use std::io::{self, Stdout};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use forge_types::SideEffect;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{self, handle_key, App, InputOutcome, KeyKind, Line};
use crate::{Presenter, PresenterEvent};

pub struct TuiPresenter {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    app: App,
}

impl TuiPresenter {
    /// Enter raw mode + the alternate screen and take over the terminal.
    pub fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        // From here on, any failure must undo raw mode — Drop won't run because the
        // struct isn't constructed yet, which would otherwise leave the shell broken.
        Self::enter().inspect_err(|_| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        })
    }

    fn enter() -> io::Result<Self> {
        let mut out = io::stdout();
        execute!(out, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(out))?;
        Ok(Self {
            terminal,
            app: App::default(),
        })
    }

    fn draw(&mut self) {
        let app = &self.app;
        let _ = self.terminal.draw(|f| app::render(f, app));
    }

    fn restore(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
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
                            // Echo the user's message into the transcript.
                            self.app.lines.push(Line::User(line.clone()));
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
