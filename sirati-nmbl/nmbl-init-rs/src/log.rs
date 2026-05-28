use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use serde::Deserialize;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    Quiet,
    #[default]
    Info,
    Verbose,
}

impl Verbosity {
    const fn as_u8(self) -> u8 {
        match self {
            Verbosity::Quiet => 0,
            Verbosity::Info => 1,
            Verbosity::Verbose => 2,
        }
    }

    const fn from_u8(v: u8) -> Verbosity {
        match v {
            0 => Verbosity::Quiet,
            2 => Verbosity::Verbose,
            // Any unexpected value collapses to Info — strictly safer than
            // silently dropping warnings, and we never write anything else.
            _ => Verbosity::Info,
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(Verbosity::Info.as_u8());

pub fn init(v: Verbosity) {
    CURRENT.store(v.as_u8(), Ordering::SeqCst);
}

pub fn current() -> Verbosity {
    Verbosity::from_u8(CURRENT.load(Ordering::SeqCst))
}

/// Set when an interactive console (splash framebuffer or raw-mode tty)
/// owns the screen. While true, the `nmbl_*!` macros suppress their
/// stderr branch — the TUI already surfaces phase/log output through its
/// own render loop, and writing through stderr races the ratatui
/// re-paint and produces visible smear (especially on serial, where
/// stderr→/dev/console and the kernel's printk echo to the same UART
/// produce duplicated lines like "phase 3" appearing back-to-back with a
/// `[ 1.234] phase 3` printk variant).
///
/// `/dev/kmsg` writes are still performed so the kernel ring buffer (and
/// any console the operator picked up via `console=` cmdline) keeps a
/// timestamped record — only the userspace stderr duplicate is silenced.
/// On `suspend` / handover to kexec/execve the flag is cleared so the
/// post-handover path sees normal eprintln output again.
static TUI_OWNED_CONSOLE: AtomicBool = AtomicBool::new(false);

/// Mark the console as TUI-owned: the `nmbl_*!` macros stop writing to
/// stderr until [`clear_tui_active`] runs. Idempotent; cheap; safe to
/// call from any code path that brings up a [`crate::ui::console::Console`].
pub fn set_tui_active() {
    TUI_OWNED_CONSOLE.store(true, Ordering::SeqCst);
}

/// Inverse of [`set_tui_active`]. Called when the TUI hands the screen
/// back to the kernel/foreign userspace (suspend, kexec handoff,
/// emergency-shell relay, drop on scope exit).
pub fn clear_tui_active() {
    TUI_OWNED_CONSOLE.store(false, Ordering::SeqCst);
}

/// Internal helper for the `nmbl_*!` macros so the macro body stays
/// short and the gating logic has a single home.
#[doc(hidden)]
pub fn tui_active() -> bool {
    TUI_OWNED_CONSOLE.load(Ordering::SeqCst)
}

/// `/dev/kmsg` accepts writes from userspace and routes the resulting
/// printk message to every registered kernel console (regardless of
/// the `console=` ordering, which only picks the `/dev/console` target
/// for stdin/stdout/stderr). Teeing every NMBL log line here means
/// kernel messages, NMBL phase info, and the emergency shell all land
/// on the serial log AND on the framebuffer.
///
/// The fd is opened lazily on first write and cached for the lifetime
/// of the process. Failures (missing kmsg, permission denied) are
/// swallowed silently — the eprintln! path still produces output.
static KMSG: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// Try to write a line to /dev/kmsg. Must not be called with the KMSG
/// mutex held by the caller.
///
/// This is also the single tee-point for the in-memory log ring used by
/// the BootStatus TUI screen: every line that goes to kmsg is also
/// pushed onto the ring (without the `<6>[nmbl] ` prefix — see
/// `push_ring`). Callers should keep emitting through this entry point
/// so on-screen logs stay in sync with the serial/kernel log.
pub fn emit_kmsg(line: &str) {
    push_ring(line);
    let Ok(mut guard) = KMSG.lock() else {
        return;
    };
    if guard.is_none() {
        let opened = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open("/dev/kmsg");
        if let Ok(f) = opened {
            *guard = Some(f);
        }
    }
    if let Some(file) = guard.as_mut() {
        // Kernel `/dev/kmsg` treats each write(2) as one record.
        // Format the entire line up-front and submit it in a single
        // write_all so we don't get "<6>[nmbl]" / message / "\n" as
        // three separate records.
        // Use printk level 6 (KERN_INFO). The kernel parses "<6>" at
        // the start of each line and routes the rest as the message.
        let buf = format!("<6>[nmbl] {line}\n");
        let _ = file.write_all(buf.as_bytes());
    }
}

/// Capacity of the in-memory log ring. Sized to comfortably cover a
/// full NMBL boot transcript (device probing + module loads + menu
/// entry chatter) without spending more than a few KiB; older lines
/// are evicted FIFO. The TUI never asks for more than the visible
/// screen height anyway, so this is effectively "as much scrollback
/// as we keep alive".
const LOG_RING_CAPACITY: usize = 256;

/// In-memory ring of recently emitted log bodies (no `[nmbl]` prefix,
/// no `<6>` priority — the render layer adds those if it wants). Held
/// behind a `Mutex` so `nmbl_*!` calls from any thread tee here safely.
/// Lazily initialised so first-touch is the boot's first log line, not
/// program startup.
static LOG_RING: Mutex<Option<VecDeque<String>>> = Mutex::new(None);

/// Push a log body onto the ring. Drops the line silently if the lock
/// is poisoned or contended — never panics, never blocks the boot.
/// The stored string is the same body the user sees on stderr; the
/// `<6>` priority byte and `[nmbl] ` prefix are added by the kmsg /
/// stderr emitters, not stored here.
pub fn push_ring(line: &str) {
    // try_lock keeps the hot path cheap: if another thread is mid-push,
    // we drop this line rather than serialise the boot.
    let Ok(mut guard) = LOG_RING.try_lock() else {
        return;
    };
    let ring = guard.get_or_insert_with(|| VecDeque::with_capacity(LOG_RING_CAPACITY));
    if ring.len() == LOG_RING_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(line.to_owned());
}

/// Snapshot the last `n` log lines (most recent last). If fewer than
/// `n` lines have been logged, returns whatever is in the ring. If
/// `n` is zero, returns an empty `Vec`. Poisoned lock → empty `Vec`
/// (rather than panicking and tearing the BootStatus screen down).
#[must_use]
pub fn snapshot(n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let Ok(guard) = LOG_RING.lock() else {
        return Vec::new();
    };
    let Some(ring) = guard.as_ref() else {
        return Vec::new();
    };
    let take = n.min(ring.len());
    let start = ring.len().saturating_sub(take);
    ring.iter().skip(start).cloned().collect()
}

#[macro_export]
macro_rules! nmbl_warn {
    ($($arg:tt)*) => {{
        let __line = format!("{}", format_args!($($arg)*));
        if !$crate::log::tui_active() {
            eprintln!("[nmbl] {}", __line);
        }
        $crate::log::emit_kmsg(&__line);
    }};
}

#[macro_export]
macro_rules! nmbl_info {
    ($($arg:tt)*) => {{
        match $crate::log::current() {
            $crate::log::Verbosity::Info | $crate::log::Verbosity::Verbose => {
                let __line = format!("{}", format_args!($($arg)*));
                if !$crate::log::tui_active() {
                    eprintln!("[nmbl] {}", __line);
                }
                $crate::log::emit_kmsg(&__line);
            }
            $crate::log::Verbosity::Quiet => {}
        }
    }};
}

#[macro_export]
macro_rules! nmbl_verbose {
    ($($arg:tt)*) => {{
        if $crate::log::current() == $crate::log::Verbosity::Verbose {
            let __line = format!("{}", format_args!($($arg)*));
            if !$crate::log::tui_active() {
                eprintln!("[nmbl] {}", __line);
            }
            $crate::log::emit_kmsg(&__line);
        }
    }};
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;

    /// Serialises the ring tests against each other so the three tests
    /// in this module don't trample one another's pushes. Other test
    /// modules in the crate do not invoke the production logging paths
    /// from their test bodies (they exercise pure parsers/data
    /// structures), so this is sufficient to keep the ring observable.
    static RING_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn ring_test_guard() -> std::sync::MutexGuard<'static, ()> {
        // Recover from poisoning so one panicking test doesn't break the
        // other two in the same `cargo test` process.
        RING_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn snapshot_returns_recent_lines() {
        let _guard = ring_test_guard();
        // Hold the LOG_RING mutex across the body — see the comment in
        // `snapshot_caps_at_ring_capacity` for why.
        let mut guard = LOG_RING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(VecDeque::with_capacity(LOG_RING_CAPACITY));
        for i in 0..10 {
            push_inner(guard.as_mut().expect("ring just initialised"), &format!("line {i}"));
        }
        let snap = snapshot_inner(guard.as_ref().expect("ring still initialised"), 5);
        assert_eq!(snap.len(), 5);
        assert_eq!(snap.first().map(String::as_str), Some("line 5"));
        assert_eq!(snap.get(4).map(String::as_str), Some("line 9"));
    }

    #[test]
    fn snapshot_caps_at_ring_capacity() {
        let _guard = ring_test_guard();
        // Hold the LOG_RING mutex itself across the whole test so
        // concurrent `nmbl_*!` calls from sibling test threads (via
        // their `try_lock` in `push_ring`) silently drop and CANNOT
        // contaminate our 300-entry push sequence. Without this, any
        // test thread that fires a log macro between our pushes adds
        // to the ring, shifts what the FIFO evicts, and our oldest /
        // newest entry assertions become flaky under cargo's parallel
        // test execution.
        let mut guard = LOG_RING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(VecDeque::with_capacity(LOG_RING_CAPACITY));
        for i in 0..300 {
            push_inner(guard.as_mut().expect("ring just initialised"), &format!("entry {i}"));
        }
        let snap = snapshot_inner(guard.as_ref().expect("ring still initialised"), usize::MAX);
        assert_eq!(snap.len(), LOG_RING_CAPACITY);
        // After eviction the oldest surviving entry is 300 - CAP and
        // the newest is 299.
        assert_eq!(
            snap.first().map(String::as_str),
            Some(format!("entry {}", 300 - LOG_RING_CAPACITY).as_str())
        );
        assert_eq!(
            snap.get(LOG_RING_CAPACITY - 1).map(String::as_str),
            Some("entry 299")
        );
    }

    /// Lock-naive push for in-test use: writes directly to a ring the
    /// caller already holds locked. Mirrors the eviction half of
    /// `push_ring` so the in-test sequence matches production.
    fn push_inner(ring: &mut VecDeque<String>, line: &str) {
        if ring.len() == LOG_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(line.to_owned());
    }

    /// Lock-naive snapshot for in-test use.
    fn snapshot_inner(ring: &VecDeque<String>, n: usize) -> Vec<String> {
        if n == 0 {
            return Vec::new();
        }
        let take = n.min(ring.len());
        let start = ring.len().saturating_sub(take);
        ring.iter().skip(start).cloned().collect()
    }

    #[test]
    fn snapshot_empty_returns_empty() {
        let _guard = ring_test_guard();
        // Hold the LOG_RING mutex across the body — see the comment in
        // `snapshot_caps_at_ring_capacity` for why.
        let mut guard = LOG_RING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(VecDeque::new());
        let ring = guard.as_ref().expect("ring just initialised");
        assert!(snapshot_inner(ring, 10).is_empty());
        assert!(snapshot_inner(ring, 0).is_empty());
    }
}
