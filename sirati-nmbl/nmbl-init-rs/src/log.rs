use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

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

/// Reset the ring to its uninitialised state. Test-only — used to
/// isolate ring tests from log noise produced by other tests in the
/// same `cargo test` process.
#[cfg(test)]
pub(crate) fn clear_ring() {
    if let Ok(mut guard) = LOG_RING.lock() {
        *guard = None;
    }
}

#[macro_export]
macro_rules! nmbl_warn {
    ($($arg:tt)*) => {{
        let __line = format!("{}", format_args!($($arg)*));
        eprintln!("[nmbl] {}", __line);
        $crate::log::emit_kmsg(&__line);
    }};
}

#[macro_export]
macro_rules! nmbl_info {
    ($($arg:tt)*) => {{
        match $crate::log::current() {
            $crate::log::Verbosity::Info | $crate::log::Verbosity::Verbose => {
                let __line = format!("{}", format_args!($($arg)*));
                eprintln!("[nmbl] {}", __line);
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
            eprintln!("[nmbl] {}", __line);
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
        clear_ring();
        for i in 0..10 {
            push_ring(&format!("line {i}"));
        }
        let snap = snapshot(5);
        assert_eq!(snap.len(), 5);
        assert_eq!(snap.first().map(String::as_str), Some("line 5"));
        assert_eq!(snap.get(4).map(String::as_str), Some("line 9"));
    }

    #[test]
    fn snapshot_caps_at_ring_capacity() {
        let _guard = ring_test_guard();
        clear_ring();
        for i in 0..300 {
            push_ring(&format!("entry {i}"));
        }
        let snap = snapshot(usize::MAX);
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

    #[test]
    fn snapshot_empty_returns_empty() {
        let _guard = ring_test_guard();
        clear_ring();
        assert!(snapshot(10).is_empty());
        assert!(snapshot(0).is_empty());
    }
}
