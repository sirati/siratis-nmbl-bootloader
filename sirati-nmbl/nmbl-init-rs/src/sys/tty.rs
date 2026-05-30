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
use std::path::{Path, PathBuf};

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

/// Read `/sys/class/tty/console/active` and return the kernel-elected
/// primary interactive console as an absolute `/dev/<name>` path.
///
/// The sysfs file lists every active console driver, space-separated,
/// in registration order. The *last* `console=` argument on the kernel
/// cmdline becomes the primary interactive console (the one
/// `register_console` calls `CON_CONSDEV` on); that entry appears
/// **first** in the file's contents. We therefore return the first
/// word, mapped to `/dev/<word>` (e.g. `tty0` → `/dev/tty0`,
/// `ttyS0` → `/dev/ttyS0`).
///
/// Pure I/O: the function does not open the resulting device, so a
/// caller can decide whether opening is appropriate (the picker dialog
/// uses the path purely as a label and a target identity, not a fd).
///
/// `path` is parameterised so unit tests can point the helper at a
/// fixture in a temp directory; production callers should use
/// [`read_active_console`].
pub fn read_active_console_from(path: &Path) -> Result<PathBuf> {
    let text = std::fs::read_to_string(path).map_err(|source| NmblError::Io {
        source,
        context: format!("reading active console listing {}", path.display()),
    })?;
    parse_active_console(&text).ok_or_else(|| NmblError::Tui {
        source: std::io::Error::other(format!("{} contains no console names", path.display())),
    })
}

/// Production wrapper around [`read_active_console_from`] pinned to
/// the canonical sysfs path.
pub fn read_active_console() -> Result<PathBuf> {
    read_active_console_from(Path::new("/sys/class/tty/console/active"))
}

/// Parse one `/sys/class/tty/console/active` payload into the primary
/// console's `/dev/<name>` path. Pure function — no I/O — so it can
/// be unit-tested off-target.
///
/// The first whitespace-delimited token wins; the rest are the
/// secondary outputs the kernel mirrors prints to. Whitespace-only or
/// empty input returns `None`.
fn parse_active_console(text: &str) -> Option<PathBuf> {
    let first = text.split_whitespace().next()?;
    if first.is_empty() {
        return None;
    }
    Some(PathBuf::from("/dev").join(first))
}

/// Whether the kernel-elected primary console is a serial / non-VT
/// line rather than an in-kernel virtual terminal.
///
/// The in-kernel VTs are named `ttyN` (`tty0`, `tty1`, …) and are the
/// only consoles whose keystrokes land on `/dev/tty<N>`; the splash
/// backend's `/dev/tty1` input path only works for those. Everything
/// else — `ttyS*` (16550 serial), `ttyUSB*`/`ttyACM*` (USB serial),
/// `ttyAMA*`/`hvc*` etc. — delivers operator keystrokes over
/// `/dev/console` instead, so the splash backend would render but never
/// see input. We classify by the leaf basename: a `tty<digits>` name is
/// a VT, anything else is serial.
///
/// Pure function — no I/O — so the backend-selection decision tree can
/// be unit-tested off-target.
pub fn console_path_is_serial(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    // A kernel VT is exactly `tty` followed by one-or-more ASCII digits;
    // anything else (ttyS*, ttyUSB*, hvc*, …) is a serial/non-VT line.
    !matches!(
        name.strip_prefix("tty"),
        Some(rest) if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
    )
}

/// Best-effort: is the kernel-elected primary interactive console a
/// serial line (no keyboard on `/dev/tty1`)?
///
/// Reads [`read_active_console`] and classifies it with
/// [`console_path_is_serial`]. On any read failure we return `false`
/// ("assume VT") so the splash path is preserved on the well-trodden
/// framebuffer machines; the serial fix only kicks in when we can
/// positively confirm a serial primary console.
pub fn active_console_is_serial() -> bool {
    match read_active_console() {
        Ok(path) => console_path_is_serial(&path),
        Err(_) => false,
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
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
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

    /// `parse_active_console` must take the FIRST whitespace-delimited
    /// token (the kernel-elected primary interactive console) and
    /// prepend `/dev/`. Both space- and tab-separated payloads occur
    /// in the wild depending on kernel version.
    #[test]
    fn parse_active_console_picks_first_word() {
        assert_eq!(
            parse_active_console("ttyS0\n"),
            Some(PathBuf::from("/dev/ttyS0"))
        );
        assert_eq!(
            parse_active_console("tty0 ttyS0\n"),
            Some(PathBuf::from("/dev/tty0"))
        );
        // Tabs and trailing newlines must both be tolerated.
        assert_eq!(
            parse_active_console("ttyS0\tttyAMA0\n"),
            Some(PathBuf::from("/dev/ttyS0"))
        );
    }

    #[test]
    fn parse_active_console_empty_input_is_none() {
        // The kernel never produces an empty file in practice, but
        // gracefully handling the edge case keeps the picker dialog's
        // fallback ("no active console detected") reachable.
        assert!(parse_active_console("").is_none());
        assert!(parse_active_console("   \n").is_none());
    }

    #[test]
    fn read_active_console_from_temp_file_round_trips() {
        // Production callers can't be tested without root, but the
        // path-based variant lets us pin the I/O wrapper against a
        // tempfile fixture so the parser is exercised end-to-end.
        let dir = match tempfile::tempdir() {
            Ok(d) => d,
            Err(_) => return,
        };
        let path = dir.path().join("active");
        if std::fs::write(&path, "ttyS0 tty0\n").is_err() {
            return;
        }
        let resolved = read_active_console_from(&path).expect("fixture must read");
        assert_eq!(resolved, PathBuf::from("/dev/ttyS0"));
    }

    #[test]
    fn console_path_is_serial_classifies_vt_vs_serial() {
        // Kernel VTs: keystrokes reach /dev/tty<N>, so NOT serial.
        assert!(!console_path_is_serial(Path::new("/dev/tty0")));
        assert!(!console_path_is_serial(Path::new("/dev/tty1")));
        assert!(!console_path_is_serial(Path::new("/dev/tty12")));
        // Serial / non-VT lines: input arrives on /dev/console.
        assert!(console_path_is_serial(Path::new("/dev/ttyS0")));
        assert!(console_path_is_serial(Path::new("/dev/ttyS1")));
        assert!(console_path_is_serial(Path::new("/dev/ttyUSB0")));
        assert!(console_path_is_serial(Path::new("/dev/ttyACM0")));
        assert!(console_path_is_serial(Path::new("/dev/ttyAMA0")));
        assert!(console_path_is_serial(Path::new("/dev/hvc0")));
        // `tty` with no trailing digit is not a VT name → serial-classed.
        assert!(console_path_is_serial(Path::new("/dev/tty")));
        assert!(console_path_is_serial(Path::new("/dev/ttyprintk")));
    }

    #[test]
    fn read_active_console_from_missing_file_is_io_error() {
        let path = PathBuf::from("/tmp/this/does/not/exist/nmbl-active-console-test");
        let err = read_active_console_from(&path).expect_err("missing file must error");
        match err {
            NmblError::Io { .. } => {}
            other => panic!("expected NmblError::Io, got {other:?}"),
        }
    }
}
