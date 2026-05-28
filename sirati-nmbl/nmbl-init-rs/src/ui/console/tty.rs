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
//! ## VT graphics mode
//!
//! When `/dev/console` is bound to a kernel VT (the framebuffer case,
//! not a serial line), the kernel keeps writing printk output to the
//! same framebuffer the TUI is drawing to. The result is a screen full
//! of kernel messages with stray TUI escape fragments (`[?25l`,
//! colour resets) wedged between them — verifier BUG #2.
//!
//! The standard remedy is `ioctl(KDSETMODE, KD_GRAPHICS)`: this tells
//! the VT subsystem that userspace is rendering directly to the
//! framebuffer and suppresses printk to that VT until `KD_TEXT` is
//! restored. We do this in [`TtyConsole::open_path`] and undo it in
//! [`Drop`]. On non-VT lines (serial console) the ioctl returns
//! `ENOTTY`; we tolerate that and proceed without changing the mode.
//!
//! rustix 0.38 does not expose a wrapper for the kd ioctls, so this
//! file contains one tightly-scoped `unsafe { libc::ioctl(...) }` per
//! direction, each documented with a SAFETY comment naming the kernel
//! contract (linux/kd.h).

use std::io::Stdout;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use rustix::termios::Termios;

use crate::error::{NmblError, Result};
use crate::log;
use crate::nmbl_warn;
use crate::sys::printk::PrintkQuiet;
use crate::sys::tty::{enter_raw, open_console as open_console_fd, restore_termios, save_termios};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleKind};
use crate::ui::render_current_screen;

/// Default tty path the orchestrator opens at boot.
const CONSOLE_PATH: &str = "/dev/console";

/// `linux/kd.h` ioctl numbers. Stable kernel ABI.
const KDGETMODE: libc::Ioctl = 0x4B3B;
const KDSETMODE: libc::Ioctl = 0x4B3A;
/// VT in graphics mode: kernel stops painting printk to the framebuffer.
const KD_GRAPHICS: libc::c_long = 0x01;
/// VT in text mode (the default). Only referenced by tests; production
/// code never hard-codes `KD_TEXT` — it always restores the mode value
/// captured by `KDGETMODE` so we don't clobber a pre-graphics setup.
#[cfg(test)]
const KD_TEXT: libc::c_long = 0x00;

/// Raw-mode tty backend. See module docs for the lifetime story.
pub struct TtyConsole {
    /// Owns the `/dev/console` fd for the lifetime of the console; the
    /// crossterm backend writes through stdout (which the kernel
    /// pointed at the same device).
    fd: OwnedFd,
    /// Termios snapshot to restore on drop. `Option` so [`Drop`] can
    /// take it without leaving a dangling clone.
    saved_termios: Option<Termios>,
    /// Previous KD VT mode, captured iff we successfully switched the
    /// VT into `KD_GRAPHICS`. `None` means we never changed the mode
    /// (e.g. the fd is a serial line and `KDGETMODE` returned ENOTTY),
    /// so [`Drop`] must leave it alone.
    previous_kd_mode: Option<libc::c_long>,
    /// Serial-console mitigation for the "kernel printk echoes through
    /// /dev/console while the TUI is also painting through it" smear
    /// (see [`crate::sys::printk`]). `None` after `suspend()` or on
    /// non-serial consoles where `KD_GRAPHICS` already handles it.
    printk_quiet: Option<PrintkQuiet>,
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
        let previous_kd_mode = enter_kd_graphics(fd.as_fd());

        let backend = CrosstermBackend::new(std::io::stdout());
        let terminal = Terminal::new(backend).map_err(tui_err)?;

        // Silence kernel-printk to console while we own the screen.
        // KD_GRAPHICS already handles this on a framebuffer VT; on a
        // serial console it returns ENOTTY (see `enter_kd_graphics`) so
        // PrintkQuiet is the only mitigation for the smear that would
        // otherwise duplicate every `[nmbl] phase N` line with a
        // `[ N.xxx] [nmbl] phase N` kernel echo on the UART.
        let printk_quiet = Some(PrintkQuiet::engage());

        // Tell the `nmbl_*!` macros to stop writing to stderr. The
        // BootReporter renders log lines through the TUI from the
        // in-memory ring, so userspace duplicates would only smear the
        // ratatui repaint. Cleared again on suspend / Drop.
        log::set_tui_active();

        Ok(TtyConsole {
            fd,
            saved_termios: Some(saved),
            previous_kd_mode,
            printk_quiet,
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

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal
            .draw(|f| body(f))
            .map(|_| ())
            .map_err(tui_err)
    }

    /// Restore the tty so the kernel VT and any foreign userspace
    /// writer can paint without our raw-mode termios fighting them.
    /// Releases:
    /// - VT graphics mode (back to KD_TEXT) so the kernel resumes
    ///   printk to the framebuffer behind a VT.
    /// - Raw-mode termios (back to the snapshot we captured at open).
    ///
    /// The owned `fd` and `terminal` stay alive so [`resume`] can
    /// re-apply the same flags without re-opening anything. Repeated
    /// calls are tolerated: each `suspend` is paired with the next
    /// `resume`.
    ///
    /// [`resume`]: TtyConsole::resume
    fn suspend(&mut self) -> Result<()> {
        // Restore printk loglevel and re-enable the eprintln side of
        // `nmbl_*!` first so any warnings emitted by the rest of the
        // restore sequence go to the relay's pre-shell screen.
        if let Some(mut q) = self.printk_quiet.take() {
            q.restore();
        }
        log::clear_tui_active();
        // KD_TEXT next so the kernel framebuffer reclaim happens
        // before the shell starts writing to the same fd; if termios
        // restoration fails the operator still ends up on a sane VT.
        if let Some(previous) = self.previous_kd_mode.take() {
            restore_kd_mode(self.fd.as_fd(), previous);
        }
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            // Suspend MUST stay non-fatal: the caller will continue
            // into the relay loop anyway. Operator can `stty sane`.
            nmbl_warn!(
                "TtyConsole::suspend: failed to restore termios on fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
        Ok(())
    }

    /// Re-acquire the tty for raw-mode TUI rendering. Re-snapshots the
    /// current termios (the shell that just ran almost certainly
    /// poked at it), re-enters raw mode, re-enters KD_GRAPHICS if the
    /// underlying device is a VT, and clears the ratatui terminal so
    /// the next render produces a full frame.
    fn resume(&mut self) -> Result<()> {
        // Capture whatever state the foreign writer (shell) left the
        // termios in. We use this snapshot for the next `suspend`'s
        // restore so a chain of suspend/resume rounds doesn't lose
        // the shell's tweaks (e.g. `stty rows`).
        let saved = save_termios(self.fd.as_fd())?;
        // enter_raw also returns the original; we already captured it.
        let _ = enter_raw(self.fd.as_fd())?;
        self.saved_termios = Some(saved);

        // Re-enter KD_GRAPHICS (no-op on serial; the helper handles
        // ENOTTY itself).
        self.previous_kd_mode = enter_kd_graphics(self.fd.as_fd());

        // Re-engage the printk-quiet guard so the post-shell screen
        // doesn't get kernel printk smear, and re-arm the macro gate.
        self.printk_quiet = Some(PrintkQuiet::engage());
        log::set_tui_active();

        // Force a full repaint on the next render: any kernel printk
        // or shell output that landed on the framebuffer while we
        // were suspended would otherwise bleed under the TUI.
        self.terminal.clear().map_err(tui_err)?;
        Ok(())
    }
}

impl Drop for TtyConsole {
    fn drop(&mut self) {
        // Re-raise the printk loglevel and re-enable the eprintln side
        // of `nmbl_*!` before any final warning so the post-NMBL kernel
        // (kexec) or post-execve shell sees a normal console policy.
        // PrintkQuiet's own Drop covers the case where the explicit
        // `restore()` is skipped on the panic-unwind path.
        if let Some(mut q) = self.printk_quiet.take() {
            q.restore();
        }
        log::clear_tui_active();
        // Restore VT text mode first so the kernel can resume printk to
        // the framebuffer if the operator ends up in a recovery shell.
        // Best-effort: a failure here just means the VT stays in
        // graphics until the next mode-set, which is recoverable.
        if let Some(previous) = self.previous_kd_mode.take() {
            restore_kd_mode(self.fd.as_fd(), previous);
        }
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            // Drop MUST NOT panic. Logging is all we can do; an
            // operator can `stty sane` to recover.
            nmbl_warn!(
                "failed to restore termios on tty console fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
    }
}

/// Try to switch `fd`'s VT into `KD_GRAPHICS` so the kernel stops
/// painting printk over the TUI. Returns the previous mode iff we
/// actually changed it, so [`Drop`] knows whether and what to restore.
///
/// Failure is non-fatal in every direction: if `fd` is not a VT (serial
/// console, ENOTTY) or the kernel refuses the mode change for any other
/// reason, we log and proceed with the TUI exactly as before. The
/// worst-case visual outcome is the pre-fix behaviour (printk
/// fragments).
fn enter_kd_graphics(fd: BorrowedFd<'_>) -> Option<libc::c_long> {
    let mut mode: libc::c_long = 0;
    // SAFETY: KDGETMODE (linux/kd.h) reads an `unsigned long` through
    // the pointer in the third ioctl argument. `&mut mode` is a valid,
    // properly-aligned pointer to a live `c_long` that outlives the
    // call. The kernel writes at most `sizeof(unsigned long)` bytes.
    // The fd is a live open file descriptor by the function contract.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDGETMODE, &mut mode) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOTTY) {
            nmbl_warn!(
                "KDGETMODE on console fd {} failed: {err}; \
                 leaving VT in current mode (printk may bleed into TUI)",
                fd.as_raw_fd()
            );
        }
        // ENOTTY just means this isn't a VT (e.g. serial), which is
        // expected on non-framebuffer consoles. Silent skip.
        return None;
    }

    if mode == KD_GRAPHICS {
        // Already in graphics (something else got here first); don't
        // claim ownership of the previous mode so Drop won't flip it.
        return None;
    }

    // SAFETY: KDSETMODE (linux/kd.h) takes its third argument as an
    // `unsigned long` value (not a pointer). The kernel validates the
    // mode against {KD_TEXT, KD_GRAPHICS}. The fd is a live open VT
    // (we just successfully read its mode above).
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSETMODE, KD_GRAPHICS) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "KDSETMODE(KD_GRAPHICS) on console fd {} failed: {err}; \
             printk may bleed into TUI",
            fd.as_raw_fd()
        );
        return None;
    }
    Some(mode)
}

/// Best-effort restore of the saved VT mode on drop. Never panics; a
/// failure here at worst leaves the VT in graphics mode, which an
/// operator can recover from with `chvt` or `kbd_mode`.
fn restore_kd_mode(fd: BorrowedFd<'_>, previous: libc::c_long) {
    // SAFETY: same contract as the KDSETMODE call in
    // `enter_kd_graphics`: third arg is an `unsigned long` mode value,
    // fd is a live VT char device for the lifetime of `TtyConsole`.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSETMODE, previous) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "KDSETMODE restore on console fd {} failed: {err}; \
             VT may remain in graphics mode (try `kbd_mode -a`)",
            fd.as_raw_fd()
        );
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

    /// `enter_kd_graphics` must gracefully tolerate fds that aren't
    /// VTs: the ioctl returns ENOTTY and the helper must return
    /// `None` (no previous mode captured) without erroring out. This
    /// is what protects serial-console boots from breaking when the
    /// TUI tries to claim graphics mode.
    #[test]
    fn enter_kd_graphics_on_non_vt_returns_none() {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            Ok(f) => f,
            // No /dev/null available (extremely sandboxed env); skip.
            Err(_) => return,
        };
        let result = enter_kd_graphics(file.as_fd());
        assert!(
            result.is_none(),
            "expected None on non-VT fd (KDGETMODE→ENOTTY), got {result:?}"
        );
    }

    /// `restore_kd_mode` must be a no-op-with-warning on a non-VT fd
    /// and, critically, must not panic. This mirrors the Drop-time
    /// safety contract: even if the fd has degraded between open and
    /// drop, we walk away cleanly.
    #[test]
    fn restore_kd_mode_on_non_vt_does_not_panic() {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            Ok(f) => f,
            Err(_) => return,
        };
        // Should log a warning internally and return normally.
        restore_kd_mode(file.as_fd(), KD_TEXT);
    }
}
