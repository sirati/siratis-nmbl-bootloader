//! [`MockConsole`] backend for the mocking harness.

use std::collections::VecDeque;
use std::io::{stdin, stdout};
use std::os::fd::AsFd;
use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Terminal;
use ratatui::backend::{Backend, TermwizBackend};
use rustix::termios::Termios;
use termwiz::caps::Capabilities;
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::unix::UnixTerminal;

use crate::error::{NmblError, Result};
use crate::sys::tty::{restore_termios, save_termios};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::parser::TermwizToCrossterm;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::render_current_screen;

/// Console backend for the mocking harness. termwiz drives both
/// reads (from stdin) and writes (to stdout); no `/dev/console`, no
/// KD ioctls. Raw mode is owned by `run()` so the harness can host
/// multiple scenarios in one process.
pub(super) struct MockConsole {
    terminal: Terminal<TermwizBackend>,
    /// Termwiz parser → crossterm `KeyEvent` translator.
    parser: TermwizToCrossterm,
    /// Keys produced by the parser, not yet drained.
    pending_keys: VecDeque<KeyEvent>,
    /// Scripted events injected by scenarios (e.g. `resize`).
    /// Drained ahead of stdin reads on the next `poll_event`.
    scripted: VecDeque<ConsoleEvent>,
    /// Latest grid size set by a `ConsoleEvent::Resize`. Overrides
    /// the backend's reported size so the next render lays out
    /// against the simulated geometry, mirroring `TtyConsole`.
    last_resize: Option<(u16, u16)>,
    /// Saved stdin termios so we can revert to blocking mode when
    /// the harness exits.
    saved_stdin_termios: Option<Termios>,
}

impl MockConsole {
    pub(super) fn new() -> Result<Self> {
        let stdin_fd = stdin();
        let stdout_fd = stdout();
        let saved = save_termios(stdin_fd.as_fd())?;

        let caps = caps_with_fallback()?;
        let unix_term = UnixTerminal::new_with(caps, &stdin_fd, &stdout_fd).map_err(tw_err)?;
        let buf = BufferedTerminal::new(unix_term).map_err(tw_err)?;
        let backend = TermwizBackend::with_buffered_terminal(buf);
        let terminal = Terminal::new(backend).map_err(io_err)?;
        Ok(Self {
            terminal,
            parser: TermwizToCrossterm::new(),
            pending_keys: VecDeque::new(),
            scripted: VecDeque::new(),
            last_resize: None,
            saved_stdin_termios: Some(saved),
        })
    }

    /// Inject a synthetic event into the queue. Drained ahead of any
    /// real input on the next `poll_event`. Used by the `resize`
    /// scenario.
    pub(super) fn script(&mut self, ev: ConsoleEvent) {
        self.scripted.push_back(ev);
    }

    pub(super) fn apply_resize(&mut self, ev: &ConsoleEvent) {
        let ConsoleEvent::Resize { rows, cols } = *ev else {
            return;
        };
        self.last_resize = Some((cols, rows));
        let _ = self
            .terminal
            .resize(ratatui::layout::Rect::new(0, 0, cols, rows));
    }

    /// Drain whatever stdin has ready and feed it through the
    /// termwiz parser. Bytes are read non-blockingly via a single
    /// `rustix::io::read` against the stdin fd; partial sequences
    /// stay buffered inside `self.parser` for the next call.
    fn refill_from_stdin(&mut self, timeout: Duration) -> Result<()> {
        use rustix::event::{PollFd, PollFlags, poll};
        let stdin_fd = stdin();
        let timeout_ms = duration_to_ms(timeout);
        let mut pfd = [PollFd::new(&stdin_fd, PollFlags::IN)];
        let ready = poll(&mut pfd, timeout_ms).map_err(rustix_err)?;
        if ready == 0 {
            // No new bytes — flush termwiz so a dangling ESC commits.
            let mut out = Vec::new();
            self.parser.feed(&[], false, &mut out);
            for k in out {
                self.pending_keys.push_back(k);
            }
            return Ok(());
        }
        let mut chunk = [0u8; 256];
        match rustix::io::read(&stdin_fd, &mut chunk) {
            Ok(0) => Ok(()),
            Ok(n) => {
                let mut out = Vec::new();
                self.parser
                    .feed(chunk.get(..n).unwrap_or(&[]), false, &mut out);
                for k in out {
                    self.pending_keys.push_back(k);
                }
                Ok(())
            }
            Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::WOULDBLOCK => Ok(()),
            Err(e) => Err(rustix_err(e)),
        }
    }
}

impl Console for MockConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.terminal
            .draw(|f| render_current_screen(f, app))
            .map(|_| ())
            .map_err(io_err)
    }

    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>>
    {
        Box::pin(async move {
            // Scripted / buffered events are ready immediately; only go
            // to stdin (via the reactor) when nothing is queued.
            if !self.scripted.is_empty() || !self.pending_keys.is_empty() {
                return self.poll_event_blocking(Duration::from_millis(0));
            }
            let slice = timeout.min(POLL_SLICE);
            crate::ui::console::await_fd_readable(stdin().as_fd(), slice).await?;
            self.poll_event_blocking(Duration::from_millis(0))
        })
    }

    fn poll_event_blocking(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
        if let Some(ev) = self.scripted.pop_front() {
            self.apply_resize(&ev);
            return Ok(Some(ev));
        }
        if let Some(k) = self.pending_keys.pop_front() {
            return Ok(Some(ConsoleEvent::Key(k)));
        }
        let slice = timeout.min(POLL_SLICE);
        self.refill_from_stdin(slice)?;
        Ok(self.pending_keys.pop_front().map(ConsoleEvent::Key))
    }

    fn size(&self) -> (u16, u16) {
        if let Some((cols, rows)) = self.last_resize {
            return (cols, rows);
        }
        match self.terminal.backend().size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (0, 0),
        }
    }

    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal.draw(|f| body(f)).map(|_| ()).map_err(io_err)
    }

    fn suspend(&mut self) -> Result<()> {
        // No-op for the mocking harness; we don't host external shells.
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        // Force a full repaint so the next render produces a clean frame.
        self.terminal.clear().map_err(io_err)
    }
}

impl Drop for MockConsole {
    fn drop(&mut self) {
        // Best-effort: restore the stdin termios we snapshotted at
        // construction. The outer `run()` guard does this too — both
        // paths are idempotent because `enter_raw` accepts being
        // applied to an already-cooked tty.
        if let Some(saved) = self.saved_stdin_termios.take() {
            let _ = restore_termios(stdin().as_fd(), &saved);
        }
    }
}

pub(super) fn caps_with_fallback() -> Result<Capabilities> {
    if let Ok(c) = Capabilities::new_from_env() {
        return Ok(c);
    }
    let hints = termwiz::caps::ProbeHints::new_from_env().term(Some("xterm-256color".to_owned()));
    if let Ok(c) = Capabilities::new_with_hints(hints) {
        return Ok(c);
    }
    Capabilities::new_with_hints(termwiz::caps::ProbeHints::new_from_env()).map_err(tw_err)
}

pub(super) fn tw_err(e: termwiz::Error) -> NmblError {
    NmblError::Io {
        source: std::io::Error::other(format!("termwiz: {e}")),
        context: "mocking harness".to_string(),
    }
}

pub(super) fn rustix_err(e: rustix::io::Errno) -> NmblError {
    NmblError::Io {
        source: std::io::Error::from(e),
        context: "mocking harness".to_string(),
    }
}

pub(super) fn duration_to_ms(d: Duration) -> i32 {
    let ms = d.as_millis();
    if ms > i32::MAX as u128 {
        i32::MAX
    } else {
        i32::try_from(ms).unwrap_or(i32::MAX)
    }
}

pub(super) fn io_err(source: std::io::Error) -> NmblError {
    NmblError::Io {
        source,
        context: "mocking harness".to_string(),
    }
}
