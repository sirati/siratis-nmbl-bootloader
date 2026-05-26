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
//! via direct termios syscalls through `nix`.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use nix::sys::termios::{SetArg, Termios, cfmakeraw, tcgetattr, tcsetattr};

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

/// Open `/dev/console` (or another tty path) read/write.
///
/// `O_NOCTTY` keeps us from accidentally acquiring the tty as a
/// controlling terminal — PID 1 already inherits whatever the kernel
/// handed it, and we don't want the open() to clobber that.
///
/// We wrap the raw fd into an `OwnedFd` (rather than `std::fs::File`)
/// because the numeric fd may legitimately be 0/1/2 — in early boot
/// the kernel hands PID 1 a non-traditional fd setup, and `File` makes
/// no guarantees there. `OwnedFd` closes via `close(2)` on drop and is
/// happy with any value.
pub fn open_console(path: &Path) -> Result<OwnedFd> {
    let raw = open(
        path,
        OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(io_tui)?;

    // SAFETY: `open` succeeded and returned a fresh fd we exclusively own.
    // Wrapping it in `OwnedFd` transfers ownership; the underlying fd will
    // be closed exactly once when the OwnedFd is dropped.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Snapshot the current termios so it can be restored on exit.
pub fn save_termios<F: AsFd>(fd: F) -> Result<Termios> {
    tcgetattr(fd).map_err(io_tui)
}

/// Apply a termios snapshot. Uses `TCSAFLUSH` so any pending input is
/// discarded — important when restoring after raw-mode key handling
/// so stray scancodes don't leak into the post-handover shell.
pub fn restore_termios<F: AsFd>(fd: F, saved: &Termios) -> Result<()> {
    tcsetattr(fd, SetArg::TCSAFLUSH, saved).map_err(io_tui)
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
    cfmakeraw(&mut raw);
    tcsetattr(borrowed, SetArg::TCSAFLUSH, &raw).map_err(io_tui)?;

    Ok(original)
}

/// RAII guard: enters raw mode on construction, restores on drop.
///
/// We store the fd as a `RawFd` and re-borrow it inside `Drop` using
/// `BorrowedFd::borrow_raw`. The safety invariant is: **the caller
/// must keep the underlying fd open for the lifetime of the guard.**
/// In practice the guard is constructed from an `&OwnedFd` held by
/// the surrounding scope, so this falls out naturally; the `'a`
/// lifetime parameter is what enforces it at compile time.
pub struct RawModeGuard<'a> {
    fd: RawFd,
    previous: Termios,
    _borrow: BorrowedFd<'a>,
}

impl<'a> RawModeGuard<'a> {
    pub fn new<F: AsFd + 'a>(fd: F) -> Result<RawModeGuard<'a>> {
        let raw_fd = fd.as_fd().as_raw_fd();
        // SAFETY: `fd: F` is borrowed for `'a` (the trait bound `F: 'a`
        // forces it), so the underlying fd is guaranteed to remain open
        // for at least `'a`. We synthesize a `BorrowedFd<'a>` from the
        // raw fd to store it in the guard without holding `F` itself.
        let borrow = unsafe { BorrowedFd::borrow_raw(raw_fd) };
        let previous = enter_raw(borrow)?;
        Ok(RawModeGuard {
            fd: raw_fd,
            previous,
            _borrow: borrow,
        })
    }
}

impl Drop for RawModeGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: see `RawModeGuard::new` — the `_borrow` field's
        // lifetime keeps the fd alive for the whole guard, so it is
        // still valid here in `Drop`.
        let borrow = unsafe { BorrowedFd::borrow_raw(self.fd) };
        if let Err(e) = restore_termios(borrow, &self.previous) {
            // Drop MUST NOT panic. The terminal is now in an unknown
            // state, but the operator's emergency shell can `stty
            // sane` to recover; logging is the most we can do.
            nmbl_warn!("failed to restore termios on fd {}: {e}", self.fd);
        }
    }
}

/// Wrap a `nix::Error` (alias for `Errno`) into our `NmblError::Tui`
/// variant. `Errno` implements `From<Errno> for io::Error`, so the
/// translation is lossless.
fn io_tui(e: nix::Error) -> NmblError {
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
