//! Controlling-terminal handling for the TUI.
//!
//! Owns `/dev/console` (or any caller-supplied tty path), snapshots the
//! current `termios` so we can put the line into raw mode for the
//! ratatui menu, and restores it on drop. This is the foundation Phase
//! D's UI builds on: every code path that draws to the screen must do
//! so under a live `RawModeGuard`, and every exit path (success,
//! error, panic) must restore the saved state so the operator's shell
//! still works after we hand off.
//!
//! No external processes (`stty(1)` is forbidden); everything is done
//! via direct termios syscalls through `rustix`/`nix`.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr};

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

/// Open `/dev/console` (or another tty path) read/write.
///
/// `O_NOCTTY` keeps us from accidentally acquiring the tty as a
/// controlling terminal — PID 1 already inherits whatever the kernel
/// handed it, and we don't want the open() to clobber that.
///
/// `rustix::fs::open` returns an `OwnedFd` directly, so no `unsafe`
/// `from_raw_fd` wrap is needed. We hand back the `OwnedFd` (rather
/// than `std::fs::File`) because the numeric fd may legitimately be
/// 0/1/2 in early boot, and `File` makes no guarantees there.
pub fn open_console(path: &Path) -> Result<OwnedFd> {
    rustix::fs::open(
        path,
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io_tui)
}

/// Snapshot the current termios so it can be restored on exit.
pub fn save_termios<F: AsFd>(fd: F) -> Result<Termios> {
    tcgetattr(fd).map_err(io_tui)
}

/// Apply a termios snapshot. Uses `TCSAFLUSH` so any pending input is
/// discarded — important when restoring after raw-mode key handling
/// so stray scancodes don't leak into the post-handover shell.
pub fn restore_termios<F: AsFd>(fd: F, saved: &Termios) -> Result<()> {
    tcsetattr(fd, OptionalActions::Flush, saved).map_err(io_tui)
}

/// Put the tty into "raw" mode for the TUI. Returns the previous
/// termios so the caller can restore it on drop.
///
/// "Raw" here is the POSIX `cfmakeraw` set: disables canonical mode,
/// echo, signal generation, input/output translation, and 8-bit
/// stripping. After this the kernel hands us bytes as they arrive
/// with no line buffering — exactly what ratatui's crossterm backend
/// expects.
pub fn enter_raw<F: AsFd>(fd: F) -> Result<Termios> {
    let borrowed = fd.as_fd();
    let original = save_termios(borrowed)?;

    let mut raw = original.clone();
    raw.make_raw();
    tcsetattr(borrowed, OptionalActions::Flush, &raw).map_err(io_tui)?;

    Ok(original)
}

/// RAII guard: enters raw mode on construction, restores on drop.
///
/// The constructor takes a `BorrowedFd<'a>` directly, so the lifetime
/// invariant (the underlying fd must outlive the guard) is enforced at
/// the type level — there is no `unsafe` borrow synthesis. Construct
/// the guard from `owned_fd.as_fd()` in the surrounding scope.
pub struct RawModeGuard<'a> {
    fd: BorrowedFd<'a>,
    previous: Termios,
}

impl<'a> RawModeGuard<'a> {
    pub fn new(fd: BorrowedFd<'a>) -> Result<RawModeGuard<'a>> {
        let previous = enter_raw(fd)?;
        Ok(RawModeGuard { fd, previous })
    }
}

impl Drop for RawModeGuard<'_> {
    fn drop(&mut self) {
        if let Err(e) = restore_termios(self.fd, &self.previous) {
            // Drop MUST NOT panic. The terminal is now in an unknown
            // state, but the operator's emergency shell can `stty
            // sane` to recover; logging is the most we can do.
            use std::os::fd::AsRawFd as _;
            nmbl_warn!(
                "failed to restore termios on fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
    }
}

/// Wrap a `rustix::io::Errno` into our `NmblError::Tui` variant.
/// `Errno` implements `From<Errno> for io::Error`, so the translation
/// is lossless.
fn io_tui(e: rustix::io::Errno) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::from(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    /// `/dev/null` is not a tty, so `tcgetattr` (and therefore
    /// `enter_raw`) must fail with ENOTTY. This is the only
    /// terminal-related path we can exercise in a unit test — we
    /// can't get a real pty in CI without extra setup.
    #[test]
    fn enter_raw_on_non_tty_errors() {
        let file = match OpenOptions::new().read(true).write(true).open("/dev/null") {
            Ok(f) => f,
            // No /dev/null available (extremely sandboxed test env);
            // skip rather than fail.
            Err(_) => return,
        };

        let res = enter_raw(&file);
        assert!(res.is_err(), "expected ENOTTY on /dev/null, got Ok");
    }
}
