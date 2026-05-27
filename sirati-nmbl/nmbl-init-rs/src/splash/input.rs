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
    pub fn open(path: &Path) -> Result<SplashInput> {
        let fd = open_console(path)?;
        let saved = enter_raw(fd.as_fd())?;
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

        // Bare Esc disambiguation: if the first byte is 0x1b and that
        // was the only byte, give the kernel ~10 ms to deliver the
        // rest of a CSI; if nothing arrives, it's a real Esc.
        if n == 1 && buf.first() == Some(&0x1b) {
            let tail = buf.get_mut(1..).unwrap_or(&mut []);
            let extra = poll_read(self.fd.as_fd(), tail, ESC_FOLLOWUP_MS)?;
            if extra == 0 {
                return Ok(Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            }
            let total = n.saturating_add(extra);
            let slice = buf.get(..total).unwrap_or(&[]);
            return Ok(parse_event(slice).0);
        }

        Ok(parse_event(buf.get(..n).unwrap_or(&[])).0)
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
