//! Keyboard input layer for the graphical splash.
//!
//! The splash renders to a DRM framebuffer (typically `/dev/dri/card0`)
//! while the kernel's `console=` directive may point stdin at a serial
//! line — so crossterm's `event::poll`, which reads stdin, never sees
//! the operator's keypresses. This module opens `/dev/tty0` directly,
//! puts it in raw mode, and synthesises [`crossterm::event::KeyEvent`]s
//! by parsing the VT escape sequences the kernel's keyboard driver
//! emits.
//!
//! Bytes come in via `rustix::event::poll` + `rustix::io::read` so no
//! new `unsafe` is introduced. The parser is split out as a pure
//! function ([`parse_event`]) so it is exercised by unit tests without
//! requiring a real tty fd.
//!
//! Recognised sequences (covers what the boot menu binds):
//! - Arrow keys, Home, End, Delete via the standard CSI forms.
//! - Plain Enter (CR/LF), Tab, Backspace (0x7f), Esc.
//! - Printable ASCII as `KeyCode::Char(c)`.
//! - C0 controls 0x01..=0x1a (minus the named ones above) as
//!   `KeyCode::Char((b | 0x60) as char)` with `CONTROL`.
//!
//! A bare `0x1b` is ambiguous (Esc vs. the lead byte of a CSI). The
//! poller resolves this by re-polling for ~10 ms; if nothing follows,
//! the byte was Esc.

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::termios::Termios;

use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::tty::{enter_raw, open_console, restore_termios};

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
        })
    }

    /// Poll for and parse a single key event. Returns `Ok(None)` if no
    /// input arrived within `timeout`.
    ///
    /// The kernel vt driver delivers escape sequences as separate
    /// reads in pathological cases, so this routine may issue a short
    /// follow-up poll to disambiguate a bare Esc from a CSI prefix.
    /// The `timeout` budgets only the *initial* wait for the first byte.
    pub fn poll(&mut self, timeout: Duration) -> Result<Option<KeyEvent>> {
        let mut buf = [0u8; 16];
        let n = poll_read(self.fd.as_fd(), &mut buf, duration_to_ms(timeout))?;
        if n == 0 {
            return Ok(None);
        }

        log_raw_bytes(buf.get(..n).unwrap_or(&[]));

        // Bare Esc disambiguation: if the first byte is 0x1b and that
        // was the only byte, give the kernel ~10 ms to deliver the
        // rest of a CSI; if nothing arrives, it's a real Esc.
        if n == 1 && buf.first() == Some(&0x1b) {
            let tail = buf.get_mut(1..).unwrap_or(&mut []);
            let extra = poll_read(self.fd.as_fd(), tail, ESC_FOLLOWUP_MS)?;
            if extra == 0 {
                let ev = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
                log_parsed_event(Some(&ev));
                return Ok(Some(ev));
            }
            let total = n.saturating_add(extra);
            let slice = buf.get(..total).unwrap_or(&[]);
            // The follow-up read may have grabbed more bytes; log the
            // continuation so the byte panel sees the full CSI.
            if extra > 0 {
                log_raw_bytes(buf.get(n..total).unwrap_or(&[]));
            }
            let parsed = parse_event(slice).0;
            log_parsed_event(parsed.as_ref());
            return Ok(parsed);
        }

        let parsed = parse_event(buf.get(..n).unwrap_or(&[])).0;
        log_parsed_event(parsed.as_ref());
        Ok(parsed)
    }
}

/// Emit a `nmbl_warn!` listing the bytes that just arrived from the VT
/// keyboard layer in hex, joined with single spaces (e.g. `1b 5b 41`).
/// Routed at `nmbl_warn!` rather than `nmbl_info!` so the line appears
/// even when the operator's config left verbosity at `Quiet` — the
/// whole point of this trace is to diagnose situations where the
/// operator can't see what they're doing.
fn log_raw_bytes(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut hex = String::with_capacity(bytes.len().saturating_mul(3));
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            hex.push(' ');
        }
        // Two-digit lowercase hex via `format!` — clippy's
        // indexing_slicing lint is denied at crate level, so a manual
        // 16-byte LUT lookup would need an explicit annotation; the
        // ergonomic cost of the per-byte allocation here is irrelevant
        // because this fires at human-keypress cadence at most.
        hex.push_str(&format!("{b:02x}"));
    }
    nmbl_warn!("SplashInput raw bytes: {hex}");
}

/// Emit a `nmbl_warn!` describing the parsed `KeyEvent` (or the fact
/// that no event was produced — i.e. the parser dropped the bytes).
/// Same routing rationale as [`log_raw_bytes`].
fn log_parsed_event(ev: Option<&KeyEvent>) {
    match ev {
        Some(k) => nmbl_warn!(
            "SplashInput parsed: code={:?} mods={:?}",
            k.code,
            k.modifiers
        ),
        None => nmbl_warn!("SplashInput parsed: <no event> (parser dropped bytes)"),
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

/// Parse a VT byte stream into the first complete key event.
///
/// Returns `(Some(event), consumed_bytes)` when a full sequence is
/// recognised, or `(None, n)` to skip `n` bytes the parser could not
/// classify (so callers can advance and re-try on the next chunk).
///
/// This function is intentionally pure: it has no fd or syscall
/// dependencies and is unit-tested on canned byte sequences.
pub(crate) fn parse_event(bytes: &[u8]) -> (Option<KeyEvent>, usize) {
    let Some(&first) = bytes.first() else {
        return (None, 0);
    };

    match first {
        0x1b => parse_escape(bytes),
        0x0d | 0x0a => (Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), 1),
        0x7f | 0x08 => (
            Some(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            1,
        ),
        0x09 => (Some(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)), 1),
        0x20..=0x7e => (
            Some(KeyEvent::new(
                KeyCode::Char(first as char),
                KeyModifiers::NONE,
            )),
            1,
        ),
        0x01..=0x1a => {
            // Ctrl+letter. 0x01 = Ctrl-A, 0x02 = Ctrl-B, …
            let letter = (first | 0x60) as char;
            (
                Some(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL)),
                1,
            )
        }
        _ => (None, 1),
    }
}

/// Parse a byte sequence that begins with `0x1b`. Caller has already
/// matched the lead byte.
fn parse_escape(bytes: &[u8]) -> (Option<KeyEvent>, usize) {
    // `bytes[0] == 0x1b`. We need at least 0x1b 0x5b X for any of the
    // CSI forms; a bare Esc is handled by the poll layer.
    let Some(&b1) = bytes.get(1) else {
        return (None, 1);
    };
    if b1 != b'[' {
        // ESC + non-CSI: treat as Esc and let the next call classify
        // the leftover byte.
        return (Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), 1);
    }

    let Some(&b2) = bytes.get(2) else {
        return (None, 2);
    };
    let key = match b2 {
        b'A' => Some(KeyCode::Up),
        b'B' => Some(KeyCode::Down),
        b'C' => Some(KeyCode::Right),
        b'D' => Some(KeyCode::Left),
        b'H' => Some(KeyCode::Home),
        b'F' => Some(KeyCode::End),
        b'3' => {
            // CSI 3 ~ → Delete
            if bytes.get(3) == Some(&b'~') {
                return (Some(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE)), 4);
            }
            return (None, 3);
        }
        _ => None,
    };
    match key {
        Some(code) => (Some(KeyEvent::new(code, KeyModifiers::NONE)), 3),
        None => (None, 3),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> KeyEvent {
        parse_event(bytes).0.expect("expected a key event")
    }

    #[test]
    fn arrows() {
        assert_eq!(parse(b"\x1b[A").code, KeyCode::Up);
        assert_eq!(parse(b"\x1b[B").code, KeyCode::Down);
        assert_eq!(parse(b"\x1b[C").code, KeyCode::Right);
        assert_eq!(parse(b"\x1b[D").code, KeyCode::Left);
    }

    #[test]
    fn home_end_delete() {
        assert_eq!(parse(b"\x1b[H").code, KeyCode::Home);
        assert_eq!(parse(b"\x1b[F").code, KeyCode::End);
        assert_eq!(parse(b"\x1b[3~").code, KeyCode::Delete);
    }

    #[test]
    fn enter_tab_backspace() {
        assert_eq!(parse(b"\r").code, KeyCode::Enter);
        assert_eq!(parse(b"\n").code, KeyCode::Enter);
        assert_eq!(parse(b"\t").code, KeyCode::Tab);
        assert_eq!(parse(&[0x7f]).code, KeyCode::Backspace);
    }

    #[test]
    fn printables() {
        let e = parse(b"a");
        assert_eq!(e.code, KeyCode::Char('a'));
        assert_eq!(e.modifiers, KeyModifiers::NONE);

        let space = parse(b" ");
        assert_eq!(space.code, KeyCode::Char(' '));

        let z = parse(b"Z");
        assert_eq!(z.code, KeyCode::Char('Z'));
    }

    #[test]
    fn ctrl_letters() {
        let c = parse(&[0x03]);
        assert_eq!(c.code, KeyCode::Char('c'));
        assert!(c.modifiers.contains(KeyModifiers::CONTROL));

        let l = parse(&[0x0c]);
        assert_eq!(l.code, KeyCode::Char('l'));
        assert!(l.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn esc_alone_via_parser_returns_none_on_short_buffer() {
        // The parser refuses to commit to Esc on a bare 0x1b because
        // it can't see the future; the poll layer disambiguates.
        let (ev, n) = parse_event(&[0x1b]);
        assert!(ev.is_none(), "bare 0x1b in parser must defer");
        assert_eq!(n, 1);
    }

    #[test]
    fn esc_plus_non_csi_commits_to_esc() {
        let (ev, n) = parse_event(&[0x1b, b'x']);
        let ev = ev.expect("esc + x → Esc, leftover x");
        assert_eq!(ev.code, KeyCode::Esc);
        assert_eq!(n, 1);
    }

    #[test]
    fn empty_buffer_is_none() {
        let (ev, n) = parse_event(&[]);
        assert!(ev.is_none());
        assert_eq!(n, 0);
    }
}
