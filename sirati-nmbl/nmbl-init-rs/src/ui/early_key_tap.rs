//! Early-boot keypress tap for instant boot.
//!
//! Instant boot ([`crate::ui::instant_boot`]) skips the selector countdown
//! only when the operator did NOT touch the keyboard/serial line while NMBL
//! was reaching the selector (device waits, mounts, verification — a few
//! seconds). To honour that, NMBL must be listening for input *from the
//! earliest possible moment*, long before the interactive [`crate::ui::console`]
//! backend is brought up.
//!
//! This module owns that early listener. [`arm`] opens `/dev/console`
//! read-only and non-blocking and records the fd in a process-global. [`poll`]
//! drains whatever bytes have arrived (without blocking) and latches a
//! sticky "a key was pressed" flag the instant any non-empty read succeeds.
//! [`key_pressed`] reports that flag.
//!
//! ## Why a raw fd and not the [`crate::ui::console`] backend
//!
//! The interactive console is only opened after phase 2a (early modules),
//! so it cannot observe a keypress during phase 0.5 (bootstrap), phase 1
//! (pseudo-fs mount) or the device-wait/mount work. The `SessionInteraction`
//! latch driven by [`crate::ui::console::LatchingConsole`] covers the window
//! from console-open to the selector; this tap covers the *earlier* window.
//! Together they answer "did the operator press a key any time before the
//! selector".
//!
//! ## Non-destructive, best-effort
//!
//! The tap opens its OWN fd on `/dev/console` (`O_RDONLY | O_NONBLOCK`), so it
//! never disturbs PID 1's stdio or the later raw-mode `TtyConsole` (which
//! opens its own `O_RDWR` fd and drives raw mode). We only ever DETECT that
//! bytes arrived; we never consume the operator's later real input through
//! this fd. Every failure (missing console, EAGAIN, closed fd) is swallowed —
//! the tap can only ever ADD a "key pressed" signal, and its absence
//! conservatively means "assume no early key", which the instant-boot policy
//! treats as permissive only when every OTHER condition also holds.
//!
//! ## Threading
//!
//! NMBL is single-threaded (PID 1). The global state is a `Mutex<Option<..>>`
//! guarded with `try_lock` on the hot path, matching the log ring's policy:
//! contention (impossible on one thread, but cheap to tolerate) degrades to
//! "no new bytes observed this poll", never a block.

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Process-global early-tap state. `None` until [`arm`] runs; `Some` holds the
/// non-blocking console fd and the sticky key-pressed flag.
static TAP: Mutex<Option<Tap>> = Mutex::new(None);

/// Sticky "a key was pressed during early boot" latch that OUTLIVES the tap fd.
/// Set the instant [`poll`] observes any byte; never cleared. This lets
/// [`disarm`] close the fd (so the interactive console does not contend for
/// input) while [`key_pressed`] keeps reporting the early keypress afterwards.
static LATCHED: AtomicBool = AtomicBool::new(false);

struct Tap {
    /// The `/dev/console` fd opened `O_RDONLY | O_NONBLOCK`. Owned so it is
    /// closed when the tap is replaced (re-arm) or the process exits.
    fd: OwnedFd,
    /// Sticky: set the instant any non-empty read succeeds. Never cleared.
    pressed: bool,
}

/// Open `/dev/console` non-blocking and arm the early tap. Best-effort: a
/// failure to open leaves the tap unarmed (so [`key_pressed`] stays `false`)
/// and is silently tolerated. Idempotent — a second call replaces the fd.
///
/// Called from earliest startup (right after stdio is wired) so a keypress
/// during any pre-selector phase is observable.
pub fn arm() {
    arm_path(Path::new("/dev/console"));
}

/// [`arm`] against a caller-chosen path, for tests.
pub fn arm_path(path: &Path) {
    use rustix::fs::{Mode, OFlags};
    let Ok(fd) = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    ) else {
        return;
    };
    // Best-effort: put a real tty into non-canonical, no-echo mode so a SINGLE
    // keypress (no Enter) is delivered to `read` immediately, instead of being
    // held by the line discipline until a newline. Failures are ignored — on a
    // pipe/FIFO (tests) or a non-tty console this ENOTTYs and the fd still
    // reads bytes fine; canonical mode merely delays detection to a full line.
    // We do NOT restore this: the interactive `TtyConsole` opens its own fd and
    // installs raw mode via TCSAFLUSH (from its own snapshot) once console
    // bring-up runs, and the emergency shell sets its own termios — so a
    // lingering non-canonical mode here never strands the operator.
    make_noncanonical_best_effort(fd.as_fd());
    LATCHED.store(false, Ordering::Relaxed);
    if let Ok(mut guard) = TAP.lock() {
        *guard = Some(Tap { fd, pressed: false });
    }
}

/// Clear `ICANON`/`ECHO` and set `VMIN=0, VTIME=0` on `fd` so a single early
/// keypress is readable non-blockingly. Best-effort: any error (not a tty,
/// unsupported) is swallowed.
fn make_noncanonical_best_effort<F: AsFd>(fd: F) {
    use rustix::termios::{OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
    let Ok(mut tio) = tcgetattr(&fd) else {
        return;
    };
    tio.local_modes -= rustix::termios::LocalModes::ICANON | rustix::termios::LocalModes::ECHO;
    tio.special_codes[SpecialCodeIndex::VMIN] = 0;
    tio.special_codes[SpecialCodeIndex::VTIME] = 0;
    let _ = tcsetattr(&fd, OptionalActions::Now, &tio);
}

/// Drain any bytes that have arrived on the tapped fd without blocking, and
/// latch the key-pressed flag if any were read. Safe to call repeatedly
/// throughout early boot; a no-op when the tap is unarmed. Returns `true` if
/// the flag is (now) set.
pub fn poll() -> bool {
    let Ok(mut guard) = TAP.try_lock() else {
        return false;
    };
    let Some(tap) = guard.as_mut() else {
        return false;
    };
    // Drain the fd fully so a burst does not leave the kernel buffer full and
    // wedge later opens. We only care THAT bytes arrived, never their value.
    let mut buf = [0u8; 256];
    loop {
        match rustix::io::read(tap.fd.as_fd(), &mut buf) {
            Ok(0) => break, // EOF on this fd; nothing more to read.
            Ok(_) => {
                tap.pressed = true; // Any byte is operator presence.
                LATCHED.store(true, Ordering::Relaxed);
            }
            Err(rustix::io::Errno::AGAIN) => break, // No data ready right now.
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => break, // Any other error: stop, keep whatever we latched.
        }
    }
    tap.pressed
}

/// Whether a key was pressed during early boot. Runs a final [`poll`] first so
/// a keypress that landed since the last drain is counted, then consults the
/// persistent [`LATCHED`] flag — which survives [`disarm`], so this keeps
/// reporting a true early keypress even after the tap fd has been closed for
/// console bring-up. `false` when no early key was ever seen.
#[must_use]
pub fn key_pressed() -> bool {
    poll();
    LATCHED.load(Ordering::Relaxed)
}

/// Close the tap fd (so the interactive `/dev/console` consumer does not
/// contend with it for input) while PRESERVING the latched flag. Called right
/// before console bring-up. After this the fd is gone but [`key_pressed`] still
/// reports any early keypress via [`LATCHED`].
pub fn disarm() {
    // Final drain to catch anything buffered right up to teardown.
    poll();
    if let Ok(mut guard) = TAP.lock() {
        *guard = None;
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    // These tests share the process-global TAP, so they must not run
    // concurrently against it. `cargo test` runs tests in one binary on
    // multiple threads by default; we serialise with a dedicated lock.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn reset() {
        if let Ok(mut guard) = TAP.lock() {
            *guard = None;
        }
        LATCHED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn unarmed_tap_reports_no_key() {
        let _serial = TEST_LOCK.lock().unwrap();
        reset();
        assert!(!key_pressed(), "an unarmed tap must never report a key");
    }

    #[test]
    fn detects_bytes_available_on_the_tapped_fd() {
        let _serial = TEST_LOCK.lock().unwrap();
        reset();
        // A regular file with pre-written bytes stands in for a console that
        // already has a keypress buffered: opened O_RDONLY|O_NONBLOCK, the
        // first read returns those bytes and latches the flag. (A real console
        // is a tty, but the detection contract — "a non-empty read means a key
        // arrived" — is identical and file-backed here for hermeticity.)
        let dir = tempfile::tempdir().expect("tempdir");
        let node = dir.path().join("console");

        // Empty file first: a read returns EOF (0 bytes) → no key.
        std::fs::write(&node, b"").expect("create empty");
        arm_path(&node);
        assert!(!key_pressed(), "empty input must not latch a key");
        reset();

        // Now a file with a byte in it: the tap reads it and latches.
        std::fs::write(&node, b"x").expect("write byte");
        arm_path(&node);
        assert!(key_pressed(), "a readable byte must latch the key flag");
        assert!(key_pressed(), "the key flag must stay latched");
        reset();
    }

    #[test]
    fn disarm_preserves_the_latched_flag() {
        let _serial = TEST_LOCK.lock().unwrap();
        reset();
        let dir = tempfile::tempdir().expect("tempdir");
        let node = dir.path().join("console");
        std::fs::write(&node, b"k").expect("write byte");
        arm_path(&node);

        assert!(key_pressed(), "key must be observed before disarm");
        disarm();
        // The fd is gone, but the persistent latch still reports the keypress.
        assert!(
            key_pressed(),
            "the latched early keypress must survive disarm"
        );
        reset();
        assert!(!key_pressed(), "reset clears the latch");
    }
}
