//! Keyboard input layer for the graphical splash.
//!
//! The splash renders to a DRM framebuffer (typically `/dev/dri/card0`)
//! while the kernel's `console=` directive may point stdin at a serial
//! line. This module opens `/dev/tty0` directly, puts it in raw mode,
//! and synthesises [`crossterm::event::KeyEvent`]s by feeding the raw
//! byte stream into [`TermwizToCrossterm`] (a thin translator over
//! `termwiz::input::InputParser`).
//!
//! Bytes come in via `rustix::event::poll` + `rustix::io::read` so no
//! new `unsafe` is introduced; the byte parser itself lives in
//! [`crate::ui::console::parser`] so the tty and splash backends share
//! one parsing path (and one translation surface to crossterm's key
//! types — see the module docs for the rationale on keeping
//! crossterm as a leaf data-type dependency).
//!
//! Recognised key set is whatever termwiz's `InputParser` produces;
//! [`TermwizToCrossterm`] maps it onto the subset of
//! [`crossterm::event::KeyCode`] the App state machine matches
//! against.
//!
//! Bare `0x1b` is ambiguous (Esc vs. the lead byte of a CSI). The
//! poller resolves this by re-polling for ~10 ms; if nothing follows,
//! we commit the parser with `maybe_more = false` so termwiz emits
//! `KeyCode::Escape`.

use std::collections::VecDeque;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crossterm::event::KeyEvent;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::termios::Termios;

use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::tty::{enter_raw, open_console, restore_termios};
use crate::ui::console::parser::TermwizToCrossterm;

/// Short follow-up wait used to disambiguate a bare Esc from the start
/// of a CSI sequence. 10 ms is comfortably above any realistic inter-
/// byte gap inside a single escape sequence delivered by the kernel
/// vt driver, yet short enough that the operator perceives Esc as
/// instant.
const ESC_FOLLOWUP_MS: i32 = 10;

/// Raw-mode keyboard reader bound to a tty fd (typically `/dev/tty0`).
///
/// Owns its own `OwnedFd` plus the saved termios snapshot; on drop we
/// restore the snapshot ourselves rather than going through
/// [`crate::sys::tty::RawModeGuard`] — that type holds a `BorrowedFd`
/// with an explicit lifetime which doesn't compose with self-referential
/// storage. Inlining the restore keeps the impl `unsafe`-free and lets
/// us own the fd in the same struct.
pub struct SplashInput {
    fd: OwnedFd,
    saved_termios: Option<Termios>,
    /// Termwiz-backed byte parser → crossterm `KeyEvent` translator.
    /// Same path the tty backend uses; we don't need a CSI 8t resize
    /// pre-filter here because the splash grid is fixed by the DRM
    /// mode (the framebuffer doesn't resize at runtime).
    parser: TermwizToCrossterm,
    /// Keys emitted by `parser` but not yet returned to the caller.
    /// `poll` pops one per call, mirroring the original 1-event-per-
    /// poll contract.
    pending: VecDeque<KeyEvent>,
}

impl SplashInput {
    /// Open the given tty path read/write, enter raw mode, return a
    /// reader. The saved termios is restored on drop.
    ///
    /// Also calls VT_ACTIVATE + VT_WAITACTIVE on the fd to force the VT
    /// into the foreground *and wait for the switch to complete* before
    /// returning. Without the activate, the kernel routes PS/2 / VNC
    /// keypresses to whichever VT was foreground at boot (the kernel
    /// console VT, typically 0/1 depending on `console=`); without the
    /// wait, the activate is asynchronous and the first reads race the
    /// switch — keys land on the *previous* foreground VT and never
    /// surface here. With both, reads from this fd reliably see every
    /// keystroke from the moment `open` returns.
    ///
    /// We also pin the keyboard layer to `K_XLATE` (the default mode
    /// that emits ANSI escape sequences for arrow / function keys) so
    /// the parser in [`parse_event`] gets the byte stream it expects.
    /// If a previous boot stage left the line in `K_RAW`/`K_MEDIUMRAW`,
    /// the parser would see raw scancodes and silently drop everything.
    pub fn open(path: &Path) -> Result<SplashInput> {
        let fd = open_console(path)?;
        let saved = enter_raw(fd.as_fd())?;
        activate_vt(&fd);
        set_kbd_xlate(&fd);
        Ok(SplashInput {
            fd,
            saved_termios: Some(saved),
            parser: TermwizToCrossterm::new(),
            pending: VecDeque::new(),
        })
    }

    /// Release the raw-mode termios so a foreign writer (the
    /// multiplexed emergency shell on `/dev/tty1`) can drive the same
    /// fd without our raw-mode flags eating its bytes. Pairs with
    /// [`resume`].
    ///
    /// The fd stays open: we only restore the saved termios snapshot.
    /// If the snapshot was already taken (a previous `suspend` without
    /// a matching `resume`) the call is a no-op.
    ///
    /// [`resume`]: SplashInput::resume
    pub fn suspend(&mut self) -> Result<()> {
        if let Some(saved) = self.saved_termios.take() {
            restore_termios(self.fd.as_fd(), &saved)?;
        }
        Ok(())
    }

    /// Re-enter raw mode after [`suspend`]. Captures the current
    /// termios snapshot (the shell may have left them changed) so the
    /// next `suspend` restores back to a coherent state.
    pub fn resume(&mut self) -> Result<()> {
        if self.saved_termios.is_some() {
            // Already raw; nothing to do.
            return Ok(());
        }
        let saved = enter_raw(self.fd.as_fd())?;
        self.saved_termios = Some(saved);
        Ok(())
    }

    /// Best-effort Caps-Lock state of this VT keyboard, for the
    /// passphrase prompt's warning. Delegates to
    /// [`crate::sys::vt::caps_lock_active`] on the owned input fd;
    /// returns `None` when the line is not a VT. Decoupled from the
    /// input-parsing path on purpose — it only reads a keyboard LED
    /// flag and never touches the byte stream.
    #[must_use]
    pub fn caps_lock_active(&self) -> Option<bool> {
        crate::sys::vt::caps_lock_active(self.fd.as_fd())
    }

    /// Poll for and parse a single key event. Returns `Ok(None)` if no
    /// input arrived within `timeout`.
    ///
    /// The kernel vt driver delivers escape sequences as separate
    /// reads in pathological cases, so this routine may issue a short
    /// follow-up poll to disambiguate a bare Esc from a CSI prefix.
    /// The `timeout` budgets only the *initial* wait for the first byte.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<KeyEvent>> {
        // Serve anything already classified from a previous call.
        if let Some(k) = self.pending.pop_front() {
            return Ok(Some(k));
        }

        let mut buf = [0u8; 64];
        let n = poll_read(self.fd.as_fd(), &mut buf, duration_to_ms(timeout))?;
        if n == 0 {
            // Nothing arrived. Flush termwiz so any dangling ESC
            // commits as a real Esc on the next call.
            let mut out = Vec::new();
            self.parser.feed(&[], /*maybe_more=*/ false, &mut out);
            for k in out {
                self.pending.push_back(k);
            }
            return Ok(self.pending.pop_front());
        }

        let mut maybe_more = false;

        // Bare Esc disambiguation: if the first byte is 0x1b and that
        // was the only byte, give the kernel ~10 ms to deliver the
        // rest of a CSI; if nothing arrives, it's a real Esc.
        let total = if n == 1 && buf.first() == Some(&0x1b) {
            let tail = buf.get_mut(1..).unwrap_or(&mut []);
            let extra = poll_read(self.fd.as_fd(), tail, ESC_FOLLOWUP_MS)?;
            if extra == 0 {
                // Treat as a final byte for termwiz so it commits Esc.
                n
            } else {
                maybe_more = false;
                n.saturating_add(extra)
            }
        } else {
            // Plain reads — termwiz needs to know more bytes might
            // arrive so it doesn't prematurely commit a dangling ESC.
            maybe_more = false;
            n
        };

        let slice = buf.get(..total).unwrap_or(&[]);
        let mut out = Vec::new();
        self.parser.feed(slice, maybe_more, &mut out);
        for k in out {
            self.pending.push_back(k);
        }
        Ok(self.pending.pop_front())
    }
}

impl Drop for SplashInput {
    fn drop(&mut self) {
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            // Drop MUST NOT panic. Mirror RawModeGuard's behaviour:
            // log and move on; `stty sane` recovers an operator shell.
            use std::os::fd::AsRawFd as _;
            nmbl_warn!(
                "failed to restore termios on splash input fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
    }
}

/// Force the VT bound to `fd` into the foreground via `VT_ACTIVATE`,
/// then block until the switch completes via `VT_WAITACTIVE`.
///
/// `VT_ACTIVATE` is asynchronous: the kernel schedules the switch but
/// returns immediately. PS/2 / VNC keystrokes get demultiplexed to the
/// *currently foreground* VT at delivery time, so the first reads on
/// the splash fd race the switch and the early keys land on whichever
/// VT was foreground before us. `VT_WAITACTIVE` blocks until VT 1 is
/// actually the active VT, after which every subsequent keystroke
/// arrives on this fd.
///
/// On x86 the constants are `VT_ACTIVATE = 0x5606` and `VT_WAITACTIVE
/// = 0x5607`, with the third ioctl arg the 1-based VT number. Both
/// failures are non-fatal: we log and continue — the worst case is the
/// pre-fix behaviour where the operator sees the splash but can't drive
/// it. The two unsafe calls are documented in docs/architecture.md
/// alongside the other accepted ioctls (finit_module, kexec_file_load).
fn activate_vt(fd: &OwnedFd) {
    use std::os::fd::AsRawFd as _;
    const VT_ACTIVATE: libc::Ioctl = 0x5606;
    const VT_WAITACTIVE: libc::Ioctl = 0x5607;
    // /dev/tty1 → VT 1. We always open VT1 (see splash::INPUT_TTY_PATH)
    // so the VT number is fixed.
    let vt_number: libc::c_int = 1;
    // SAFETY: VT_ACTIVATE takes an integer argument as the third ioctl
    // parameter; the kernel reads `vt_number` by value. The fd is a
    // live, open tty char device per the contract on this function.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), VT_ACTIVATE, vt_number) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "VT_ACTIVATE({vt_number}) on splash input fd failed: {err}; \
             keystrokes may not reach the splash"
        );
        // No point waiting for a switch we couldn't schedule.
        return;
    }
    // SAFETY: VT_WAITACTIVE has the same ABI as VT_ACTIVATE — third arg
    // is the target VT number as an integer value. The kernel blocks
    // until that VT is the foreground console (or returns EINTR on a
    // pending signal — early userspace has no async signal sources we
    // care about, but a stray EINTR is non-fatal and just collapses to
    // the warning path below).
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), VT_WAITACTIVE, vt_number) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "VT_WAITACTIVE({vt_number}) on splash input fd failed: {err}; \
             early keystrokes may race the VT switch"
        );
    }
}

/// Pin the VT keyboard layer to `K_XLATE` (the default mode that
/// translates scancodes to ANSI escape sequences). Defensive: an
/// earlier boot stage that left the line in `K_RAW` / `K_MEDIUMRAW`
/// would feed raw scancodes to our parser, which expects the ANSI
/// CSI forms (see [`parse_event`]) and would silently drop them.
///
/// Failure is non-fatal: on a non-VT fd `KDSKBMODE` returns `ENOTTY`,
/// which is the expected behaviour on serial consoles — log and move
/// on. Other failures (EPERM, EINVAL) are likewise tolerated because
/// the most common state is already-K_XLATE.
fn set_kbd_xlate(fd: &OwnedFd) {
    use std::os::fd::AsRawFd as _;
    const KDSKBMODE: libc::Ioctl = 0x4B45;
    const K_XLATE: libc::c_long = 0x01;
    // SAFETY: KDSKBMODE (linux/kd.h) takes its third argument as an
    // `unsigned long` value (not a pointer). The kernel validates the
    // mode against the K_* set. The fd is a live open tty char device
    // by the function contract; non-VT fds return ENOTTY which we
    // tolerate below.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSKBMODE, K_XLATE) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOTTY) {
            nmbl_warn!(
                "KDSKBMODE(K_XLATE) on splash input fd failed: {err}; \
                 keystrokes may arrive as raw scancodes the parser ignores"
            );
        }
    }
}

/// Wrap a rustix poll/read into a single call that returns the number
/// of bytes read (0 on timeout).
fn poll_read(fd: std::os::fd::BorrowedFd<'_>, buf: &mut [u8], timeout_ms: i32) -> Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let mut pfd = [PollFd::new(&fd, PollFlags::IN)];
    let ready = poll(&mut pfd, timeout_ms).map_err(errno_to_tui)?;
    if ready == 0 {
        return Ok(0);
    }
    let revents = pfd
        .first()
        .map(PollFd::revents)
        .unwrap_or_else(PollFlags::empty);
    if !revents.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
        return Ok(0);
    }
    let n = rustix::io::read(fd, buf).map_err(errno_to_tui)?;
    Ok(n)
}

/// Saturating cast of a `Duration` to the `i32` millisecond timeout
/// `poll(2)` expects. Out-of-range values clamp to `i32::MAX` (an
/// effectively-infinite wait, which is fine because callers always
/// pass a bounded slice).
fn duration_to_ms(d: Duration) -> i32 {
    let ms = d.as_millis();
    if ms > i32::MAX as u128 {
        i32::MAX
    } else {
        ms as i32
    }
}

fn errno_to_tui(e: rustix::io::Errno) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::from(e),
    }
}

// The byte parser and its unit tests now live in
// `crate::ui::console::parser` (`TermwizToCrossterm`) so the tty and
// splash backends share one parsing path. See that module for the
// state machine and the canned-input tests.

// The byte parser tests live in `crate::ui::console::parser`
// (`TermwizToCrossterm`) — the splash backend reuses that translator
// verbatim, so there is nothing splash-specific left to unit-test here.
