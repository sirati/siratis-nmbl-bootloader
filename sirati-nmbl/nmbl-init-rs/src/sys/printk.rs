//! Best-effort control over the kernel `printk` console loglevel.
//!
//! While the TUI owns the screen we want the kernel to stop routing
//! informational `printk` messages to whichever console it picked up
//! from the `console=` cmdline. On a framebuffer console the splash
//! backend already handles this via `KDSETMODE(KD_GRAPHICS)` (see
//! [`crate::ui::console::tty`]). On a serial console there is no
//! graphics-mode equivalent — the kernel keeps writing every
//! informational printk to the UART, where it interleaves with the
//! ratatui repaints and produces visible smear (e.g. duplicated
//! `[nmbl] phase 3` lines, one from our `eprintln!` path and one from
//! the kernel's printk echo of our own kmsg write).
//!
//! The fix is `dmesg -n 1` equivalent: write a single byte to
//! `/proc/sys/kernel/printk` to lower the *console* loglevel to 1
//! (KERN_ALERT). The kernel ring buffer keeps every message — `dmesg`
//! still shows the full transcript — only the console echo is silenced.
//! Restoration happens via [`PrintkQuiet`]'s Drop / explicit `restore`.
//!
//! Everything is best-effort: on a read-only `/proc` mount or a missing
//! file we simply do not change the loglevel and the operator may see
//! kernel chatter through the TUI. We never panic and we never propagate
//! the error to a caller — silencing kernel printk is a polish, not a
//! correctness requirement.

use std::fs;
use std::io::Write;

/// Loglevel we lower the kernel console to while the TUI is foreground.
/// `1` = KERN_ALERT; only true-emergency messages reach the console at
/// this level. KERN_EMERG (`0`) would silence even oopses, which we
/// definitely want to see if they happen mid-boot.
const QUIET_CONSOLE_LOGLEVEL: u8 = 1;

/// Path the kernel exposes the four-number printk policy on. Stable ABI
/// since Linux 2.x.
const PRINTK_SYSCTL: &str = "/proc/sys/kernel/printk";

/// Parsed snapshot of `/proc/sys/kernel/printk`. The kernel emits four
/// whitespace-separated integers: `current default minimum default`.
/// We only need the *current* console loglevel for restoration; the
/// remaining three are preserved verbatim so a write-back round-trips.
struct PrintkSnapshot {
    current: u8,
    default: u8,
    minimum: u8,
    boot_default: u8,
}

impl PrintkSnapshot {
    fn parse(raw: &str) -> Option<Self> {
        let mut it = raw.split_ascii_whitespace();
        let current = it.next()?.parse::<u8>().ok()?;
        let default = it.next()?.parse::<u8>().ok()?;
        let minimum = it.next()?.parse::<u8>().ok()?;
        let boot_default = it.next()?.parse::<u8>().ok()?;
        Some(PrintkSnapshot {
            current,
            default,
            minimum,
            boot_default,
        })
    }

    fn serialise_with_current(&self, current: u8) -> String {
        format!(
            "{} {} {} {}\n",
            current, self.default, self.minimum, self.boot_default
        )
    }
}

/// RAII handle: lowers the kernel console loglevel on construction,
/// restores it on drop. Use one of these alongside the [`Console`]
/// lifetime so console suspend/resume (emergency-shell relay) and final
/// drop (kexec handoff) both restore the operator's pre-NMBL loglevel.
///
/// [`Console`]: crate::ui::console::Console
pub struct PrintkQuiet {
    /// `Some(snapshot)` iff we successfully read and changed the loglevel
    /// at construction; `None` means we observed an unwritable
    /// `/proc/sys/kernel/printk` and have nothing to restore.
    saved: Option<PrintkSnapshot>,
}

impl PrintkQuiet {
    /// Try to lower the console loglevel. Always returns an instance —
    /// the inner `saved` field tracks whether we actually changed
    /// anything, so Drop knows whether to write back. Never errors;
    /// the worst-case is `saved = None` and the operator sees kernel
    /// printk chatter through the TUI just like before this change.
    #[must_use]
    pub fn engage() -> PrintkQuiet {
        let Ok(raw) = fs::read_to_string(PRINTK_SYSCTL) else {
            return PrintkQuiet { saved: None };
        };
        let Some(snap) = PrintkSnapshot::parse(&raw) else {
            return PrintkQuiet { saved: None };
        };
        if snap.current <= QUIET_CONSOLE_LOGLEVEL {
            // Someone (initramfs hook, kernel cmdline `quiet`) already
            // lowered it. Don't claim the previous mode so Drop won't
            // attempt to raise it.
            return PrintkQuiet { saved: None };
        }
        let body = snap.serialise_with_current(QUIET_CONSOLE_LOGLEVEL);
        if write_atomically(PRINTK_SYSCTL, body.as_bytes()).is_err() {
            return PrintkQuiet { saved: None };
        }
        PrintkQuiet { saved: Some(snap) }
    }

    /// Explicit restoration: writes the saved snapshot back. Idempotent
    /// — calling `restore` then dropping is fine; the Drop impl sees
    /// `saved = None` after a successful explicit restore and is a
    /// no-op. Returns immediately if there is nothing to restore.
    pub fn restore(&mut self) {
        let Some(snap) = self.saved.take() else {
            return;
        };
        let body = snap.serialise_with_current(snap.current);
        // Best-effort: if /proc is read-only now (extremely unusual) we
        // accept the loglevel staying at QUIET_CONSOLE_LOGLEVEL. An
        // operator can `dmesg -n <n>` to recover.
        let _ = write_atomically(PRINTK_SYSCTL, body.as_bytes());
    }
}

impl Drop for PrintkQuiet {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Open `path` for writing, write `body` in one syscall. The sysctl
/// expects a single write that overwrites the whole policy line; we
/// must not append a second write because the kernel parses each one
/// independently.
fn write_atomically(path: &str, body: &[u8]) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new().write(true).truncate(true).open(path)?;
    f.write_all(body)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on parse contract"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_round_trips_known_snapshot() {
        let snap = PrintkSnapshot::parse("4 4 1 7\n").expect("parse 4-tuple");
        assert_eq!(snap.current, 4);
        assert_eq!(snap.default, 4);
        assert_eq!(snap.minimum, 1);
        assert_eq!(snap.boot_default, 7);
        assert_eq!(snap.serialise_with_current(1), "1 4 1 7\n");
    }

    #[test]
    fn parse_rejects_missing_fields() {
        assert!(PrintkSnapshot::parse("4 4 1").is_none());
        assert!(PrintkSnapshot::parse("").is_none());
    }

    #[test]
    fn parse_rejects_non_numeric() {
        assert!(PrintkSnapshot::parse("nope 4 1 7").is_none());
    }

    /// Calling `engage()` in an environment where `/proc/sys/kernel/printk`
    /// is not writable (most test runners) must produce an instance with
    /// `saved = None`, never panic, and Drop must be a no-op.
    #[test]
    fn engage_on_unavailable_sysctl_does_not_panic() {
        // If we happen to be running as root on a system with /proc
        // mounted r/w this test may actually lower the console
        // loglevel — but it must then restore it on drop.
        let q = PrintkQuiet::engage();
        drop(q);
    }
}
