//! Raw-mode tty backend for the [`Console`] abstraction.
//!
//! Opens `/dev/console`, enters raw mode, and drives a
//! [`ratatui::Terminal`] over a [`CrosstermBackend`] writing to
//! [`std::io::stdout`]. The early-userspace contract is that the kernel
//! already pointed stdout (fd 1) at `/dev/console`, so writing through
//! `stdout()` reaches the operator's screen without a `dup2`.
//!
//! ## Why we don't reuse [`RawModeGuard`]
//!
//! [`RawModeGuard`] holds a [`BorrowedFd`] with an explicit lifetime,
//! which doesn't compose with self-referential storage inside this
//! struct: the guard would need to borrow from the same struct that
//! owns the fd. Instead we mirror [`crate::splash::input::SplashInput`]:
//! own the [`OwnedFd`] plus a saved [`Termios`] snapshot and restore it
//! manually on [`Drop`]. The behaviour is identical (TCSAFLUSH on
//! restore); the only difference is that the lifetime invariant lives
//! at construction time rather than in the type system.
//!
//! No new `unsafe` is introduced.

use std::io::Stdout;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use rustix::termios::Termios;

use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::tty::{enter_raw, open_console as open_console_fd, restore_termios};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleKind};
use crate::ui::render_current_screen;

/// Default tty path the orchestrator opens at boot.
const CONSOLE_PATH: &str = "/dev/console";

/// Raw-mode tty backend. See module docs for the lifetime story.
pub struct TtyConsole {
    /// Owns the `/dev/console` fd for the lifetime of the console; the
    /// crossterm backend writes through stdout (which the kernel
    /// pointed at the same device).
    fd: OwnedFd,
    /// Termios snapshot to restore on drop. `Option` so [`Drop`] can
    /// take it without leaving a dangling clone.
    saved_termios: Option<Termios>,
    /// Ratatui terminal over the crossterm backend wrapping stdout.
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TtyConsole {
    /// Open the default console path (`/dev/console`).
    pub fn open() -> Result<TtyConsole> {
        Self::open_path(Path::new(CONSOLE_PATH))
    }

    /// Open a caller-specified tty path. Used by tests and any future
    /// caller that wants to drive a non-default console node.
    pub fn open_path(path: &Path) -> Result<TtyConsole> {
        let fd = open_console_fd(path)?;
        let saved = enter_raw(fd.as_fd())?;

        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend).map_err(tui_err)?;

        Ok(TtyConsole {
            fd,
            saved_termios: Some(saved),
            terminal,
        })
    }
}

impl Console for TtyConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.terminal
            .draw(|f| render_current_screen(f, app))
            .map(|_| ())
            .map_err(tui_err)
    }

    fn poll_key(&mut self, timeout: Duration) -> Result<Option<KeyEvent>> {
        // Cap the poll slice the same way the rest of the UI does so
        // backends are responsive to ticking countdowns uniformly. The
        // caller-supplied timeout is honoured but never longer than
        // POLL_SLICE per call.
        let slice = timeout.min(POLL_SLICE);
        if !event::poll(slice).map_err(tui_err)? {
            return Ok(None);
        }
        match event::read().map_err(tui_err)? {
            Event::Key(k) => Ok(Some(k)),
            _ => Ok(None),
        }
    }

    fn size(&self) -> (u16, u16) {
        match self.terminal.backend().size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (0, 0),
        }
    }

    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }
}

impl Drop for TtyConsole {
    fn drop(&mut self) {
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            // Drop MUST NOT panic. Logging is all we can do; an
            // operator can `stty sane` to recover.
            use std::os::fd::AsRawFd as _;
            nmbl_warn!(
                "failed to restore termios on tty console fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
    }
}

fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    /// `/dev/null` is not a tty, so opening it as a [`TtyConsole`]
    /// must fail at the `enter_raw` step (ENOTTY). Confirms the
    /// constructor short-circuits and doesn't leak a half-constructed
    /// terminal.
    #[test]
    fn open_path_on_non_tty_errors() {
        // Skip if /dev/null isn't available (extremely sandboxed env).
        if std::fs::metadata("/dev/null").is_err() {
            return;
        }
        let res = TtyConsole::open_path(Path::new("/dev/null"));
        assert!(res.is_err(), "expected ENOTTY-style failure on /dev/null");
    }
}
