//! The animated-TUI driver pieces. `ChannelPresenter` forwards a session's presenter
//! events over a channel (so a turn can run on a background task), and `Tui` owns the
//! terminal for the render loop. The actual loop lives in the binary (it owns the
//! `Session`, which this crate must not depend on).

use std::io::{self, Stdout};
use std::sync::mpsc::Sender;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use forge_types::SideEffect;
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line as TextLine;
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::{Terminal, TerminalOptions, Viewport};

use crate::app::{self, App, KeyKind, LIVE_H};
use crate::{Presenter, PresenterEvent};

/// An input event from the terminal — either a keystroke or a bracketed paste.
pub enum InputEvent {
    Key(KeyKind),
    /// A bracketed paste: the terminal wrapped the content in `\x1b[200~…\x1b[201~` and
    /// crossterm decoded it as a single string (EnableBracketedPaste must be active).
    Paste(String),
    /// The terminal window gained (`true`) or lost (`false`) focus. Drives the input cursor's
    /// focused/hollow appearance (EnableFocusChange must be active).
    Focus(bool),
}

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

/// Block the current async task on a sync channel WITHOUT starving the tokio runtime. The
/// presenter's `confirm`/`ask` are sync trait methods called from inside the async turn task;
/// a bare `recv()` parks a worker and can stall the runtime — including the render loop's timer
/// — so the whole TUI freezes (Ctrl-C dead). `block_in_place` hands the worker back to the
/// runtime so the render loop keeps running and can deliver the user's answer.
fn recv_blocking<T>(rx: &std::sync::mpsc::Receiver<T>) -> Result<T, std::sync::mpsc::RecvError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| rx.recv())
    } else {
        rx.recv()
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
        recv_blocking(&answer).unwrap_or(false) // blocks this turn task until the loop answers
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
        recv_blocking(&answer).unwrap_or_else(|_| crate::NO_ANSWER.to_string())
    }

    fn read_line(&mut self) -> Option<String> {
        None // input is handled by the render loop, not the presenter
    }
}

/// Owns the terminal for the render loop. Two modes:
/// - **inline** (`fullscreen = false`): an inline viewport (no alternate screen). Finalized lines
///   flow into the terminal's native scrollback via [`Tui::insert_lines`]; only the small pinned
///   live region is redrawn each frame.
/// - **fullscreen** (`fullscreen = true`, the default): an alternate-screen viewport spanning the
///   whole terminal. There is no native scrollback to corrupt — the transcript is rendered from
///   `App::main_log` into a scrollable region, so [`Tui::insert_lines`] is a no-op (the caller
///   folds lines into the app log instead). Round-tripping into the full-screen activity viewer
///   can't disturb the conversation.
pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    fullscreen: bool,
}

/// Set while a full-screen (alternate-screen) TUI is active, so the panic hook knows to leave the
/// alternate screen before printing — otherwise a panic message would be lost on the alt buffer.
static IN_ALT_SCREEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install (once) a panic hook that disables raw mode (and leaves the alternate screen, if active)
/// before the default hook prints, so a panic anywhere can never leave the terminal stuck or
/// swallow the message. Idempotent across `Tui`/`TuiPresenter`.
pub fn install_panic_restore() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            if IN_ALT_SCREEN.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
            }
            let _ = crossterm::execute!(io::stdout(), crossterm::cursor::Show);
            prev(info);
        }));
    });
}

impl Tui {
    pub fn new(fullscreen: bool) -> io::Result<Self> {
        // Belt-and-suspenders: if *anything* panics while the terminal is in raw mode, restore
        // it before the panic prints — otherwise a panic would leave the shell wedged (no echo,
        // Ctrl-C inert). `Drop` covers the normal/unwind path; this covers the print itself.
        install_panic_restore();
        enable_raw_mode()?;
        crossterm::execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange)?;
        if fullscreen {
            crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
            IN_ALT_SCREEN.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: viewport(fullscreen),
            },
        )?;
        Ok(Self {
            terminal,
            fullscreen,
        })
    }

    /// Whether this TUI renders on the alternate screen (full-screen) rather than inline.
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// Current terminal width (for building width-dependent scrollback like the banner).
    pub fn width(&self) -> u16 {
        self.terminal.size().map(|s| s.width).unwrap_or(80)
    }

    /// Current terminal height (for sizing the full-screen transcript page when paging).
    pub fn height(&self) -> u16 {
        self.terminal.size().map(|s| s.height).unwrap_or(24)
    }

    /// Push plain multi-line text into the scrollback (convenience over [`insert_lines`]).
    pub fn print_text(&mut self, text: &str) {
        let lines: Vec<TextLine<'static>> =
            text.lines().map(|s| TextLine::from(s.to_owned())).collect();
        self.insert_lines(lines);
    }

    /// Push finalized lines into the terminal's native scrollback, above the live region.
    /// In full-screen mode there is no native scrollback — the transcript is rendered from the
    /// app's `main_log` — so this is a no-op and the caller must fold the lines into the app log.
    pub fn insert_lines(&mut self, lines: Vec<TextLine<'static>>) {
        if lines.is_empty() || self.fullscreen {
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

    /// Clear the visible screen (used by `/clear`). Native scrollback above is wiped from view;
    /// the session/transcript are untouched. `terminal.clear()` forces a full viewport redraw.
    pub fn clear_screen(&mut self) {
        use crossterm::terminal::{Clear, ClearType};
        let _ = crossterm::execute!(io::stdout(), Clear(ClearType::All));
        let _ = self.terminal.clear();
    }

    /// Non-blocking: returns the next input event (key or paste) if one is pending, else `None`.
    pub fn poll_event(&self) -> io::Result<Option<InputEvent>> {
        if !event::poll(Duration::from_millis(0))? {
            return Ok(None);
        }
        match event::read()? {
            Event::Paste(s) => return Ok(Some(InputEvent::Paste(s))),
            Event::FocusGained => return Ok(Some(InputEvent::Focus(true))),
            Event::FocusLost => return Ok(Some(InputEvent::Focus(false))),
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                let key = match k.code {
                    KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::Esc
                    }
                    KeyCode::Char('o') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::ToggleSubagentDetail
                    }
                    KeyCode::Char('j') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::InsertNewline
                    }
                    KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::DeleteWordBack
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::KillLineBack
                    }
                    KeyCode::Char('k') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::KillLineForward
                    }
                    KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::Home
                    }
                    KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::End
                    }
                    KeyCode::Char(c) => KeyKind::Char(c),
                    KeyCode::Backspace => KeyKind::Backspace,
                    KeyCode::Delete => KeyKind::DeleteForward,
                    KeyCode::Enter => KeyKind::Enter,
                    KeyCode::Esc => KeyKind::Esc,
                    KeyCode::BackTab => KeyKind::CycleTemper,
                    KeyCode::Tab if k.modifiers.contains(KeyModifiers::SHIFT) => {
                        KeyKind::CycleTemper
                    }
                    KeyCode::Up => KeyKind::Up,
                    KeyCode::Down => KeyKind::Down,
                    KeyCode::Left if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::WordLeft
                    }
                    KeyCode::Left => KeyKind::Left,
                    KeyCode::Right if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        KeyKind::WordRight
                    }
                    KeyCode::Right => KeyKind::Right,
                    KeyCode::Home => KeyKind::Home,
                    KeyCode::End => KeyKind::End,
                    KeyCode::PageUp => KeyKind::PageUp,
                    KeyCode::PageDown => KeyKind::PageDown,
                    KeyCode::Tab => KeyKind::Tab,
                    _ => return Ok(None),
                };
                return Ok(Some(InputEvent::Key(key)));
            }
            _ => {}
        }
        Ok(None)
    }

    /// Run a full-screen takeover (e.g. the `/config` wizard or the activity viewer) that owns its
    /// own alternate screen + raw mode and restores them on exit. Afterwards the chat's raw mode is
    /// back off and the alt-screen excursion left our cursor/viewport stale, so re-enter raw mode
    /// and rebuild the viewport. In inline mode this re-anchors the small live region above the
    /// untouched scrollback; in full-screen mode we re-enter our own alternate screen and force a
    /// full redraw, so the excursion can never duplicate panels or the input box.
    pub fn run_fullscreen<T>(&mut self, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let out = f();
        enable_raw_mode()?;
        if self.fullscreen {
            crossterm::execute!(io::stdout(), EnterAlternateScreen)?;
        }
        let backend = CrosstermBackend::new(io::stdout());
        self.terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: viewport(self.fullscreen),
            },
        )?;
        let _ = self.terminal.clear();
        out
    }
}

/// The ratatui viewport for a given mode: an inline pinned region, or the whole alternate screen.
fn viewport(fullscreen: bool) -> Viewport {
    if fullscreen {
        Viewport::Fullscreen
    } else {
        Viewport::Inline(LIVE_H)
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste, DisableFocusChange);
        if self.fullscreen {
            IN_ALT_SCREEN.store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
        }
        let _ = disable_raw_mode();
        let _ = self.terminal.show_cursor();
        // Inline mode: the conversation stays in the user's scrollback. Full-screen mode: we just
        // left the alternate screen, restoring whatever was on the terminal before.
        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::QChoice;

    /// Regression: `ask`/`confirm` block on a sync channel from inside the async turn task. On a
    /// single-worker multi-thread runtime a bare `recv()` parks the only worker → the answering
    /// task can never run → deadlock (the real "frozen TUI, Ctrl-C dead"). `block_in_place` must
    /// hand the worker back so the answer is delivered and `ask` returns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn ask_does_not_starve_the_single_worker_runtime() {
        let (tx, rx) = std::sync::mpsc::channel::<UiMsg>();

        // The "turn" task: calls the blocking `ask()` (sync method) from within an async task.
        let turn = tokio::spawn(async move {
            let mut p = ChannelPresenter::new(tx);
            p.ask(
                "pick one",
                &[QChoice {
                    label: "A".into(),
                    description: String::new(),
                }],
                false,
            )
        });

        // The "render loop" task: must get CPU despite the turn blocking, receive the question,
        // and reply. If the worker were starved this would never run.
        let render = tokio::spawn(async move {
            loop {
                if let Ok(UiMsg::Question { reply, .. }) = rx.try_recv() {
                    let _ = reply.send("A".to_string());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let answer = tokio::time::timeout(Duration::from_secs(5), turn)
            .await
            .expect("ask() must not deadlock the runtime")
            .unwrap();
        render.await.unwrap();
        assert_eq!(answer, "A");
    }
}
