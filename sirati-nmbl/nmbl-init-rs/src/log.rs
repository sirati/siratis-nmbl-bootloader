use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use serde::Deserialize;

/// Tmpfs path the byte-ring is flushed to before every terminal action
/// and before kexec. The same path is recreated in the next kernel's
/// initramfs by the cpio fragment spliced into `kexec_file_load(2)`, so
/// the booted system's `nmbl-log-import` stage-1 helper can drain it.
/// Single source of truth for both the dispatcher (`main.rs`) and the
/// kexec staging path (`boot.rs`).
pub const NMBL_LOG_PATH: &str = "/nmbl-log/nmbl.log";

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
/// On `suspend` / handover to kexec/execve the count is decremented so the
/// post-handover path sees normal eprintln output again.
///
/// A refcount rather than a bool so nested/overlapping console owners
/// (e.g. a screen suspending to spawn a sub-console, or two paired
/// open/drop scopes) compose correctly: stderr stays suppressed as long
/// as *any* owner holds the console, and resumes only when the last one
/// releases it.
static TUI_CONSOLE_REFCOUNT: AtomicUsize = AtomicUsize::new(0);

/// Mark the console as TUI-owned: the `nmbl_*!` macros stop writing to
/// stderr until the matching [`clear_tui_active`] runs. Each call
/// increments a refcount, so every `set` must be paired with exactly one
/// `clear`. Cheap; safe to call from any code path that brings up a
/// [`crate::ui::console::Console`].
pub fn set_tui_active() {
    TUI_CONSOLE_REFCOUNT.fetch_add(1, Ordering::SeqCst);
}

/// Inverse of [`set_tui_active`]. Called when the TUI hands the screen
/// back to the kernel/foreign userspace (suspend, kexec handoff,
/// emergency-shell relay, drop on scope exit). Decrements the refcount;
/// stderr output resumes once it reaches zero.
pub fn clear_tui_active() {
    // saturating_sub semantics via fetch_update so an unpaired clear can
    // never wrap the count around to a huge value (which would wedge
    // stderr off forever).
    let _ = TUI_CONSOLE_REFCOUNT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        Some(n.saturating_sub(1))
    });
}

/// Internal helper for the `nmbl_*!` macros so the macro body stays
/// short and the gating logic has a single home.
#[doc(hidden)]
pub fn tui_active() -> bool {
    TUI_CONSOLE_REFCOUNT.load(Ordering::SeqCst) > 0
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
    push_byte_ring(line);
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

/// Byte-ring capacity (1 MiB). Chosen to be big enough to hold a full
/// rescue-path transcript including hot-loop retries, while still
/// fitting easily in tmpfs at boot. On overflow the front (oldest) bytes
/// are dropped and the dropped count is remembered so the eventual
/// `flush_to` consumer can flag truncation in a header line.
const BYTE_RING_CAPACITY: usize = 1024 * 1024;

/// Mirror of every `emit_kmsg` body (with its trailing `\n` appended)
/// stored as raw bytes. Persisted to disk by `flush_to` right before
/// kexec drops the pagecache, giving the next stage a complete NMBL
/// transcript even when the kernel ring buffer has rotated past it.
struct ByteLog {
    buf: VecDeque<u8>,
    dropped_bytes: u64,
}

impl ByteLog {
    const fn new() -> Self {
        Self {
            buf: VecDeque::new(),
            dropped_bytes: 0,
        }
    }

    fn append(&mut self, bytes: &[u8]) {
        self.buf.extend(bytes);
        if self.buf.len() > BYTE_RING_CAPACITY {
            let drop = self.buf.len() - BYTE_RING_CAPACITY;
            // VecDeque::drain on the prefix range pops `drop` bytes off
            // the front in O(drop); we count them as truncated so the
            // flushed header can name the exact number.
            self.buf.drain(..drop);
            self.dropped_bytes = self.dropped_bytes.saturating_add(drop as u64);
        }
    }
}

static BYTE_LOG: Mutex<Option<ByteLog>> = Mutex::new(None);

/// Append `line\n` to the byte ring, dropping on lock contention so the
/// hot logging path never blocks the boot. Mirrors `push_ring`'s
/// try_lock policy for the same reason.
fn push_byte_ring(line: &str) {
    let Ok(mut guard) = BYTE_LOG.try_lock() else {
        return;
    };
    let log = guard.get_or_insert_with(ByteLog::new);
    // Build the on-disk representation up-front so a single append
    // either lands whole or (under overflow) gets truncated as one
    // unit — no risk of a half-line surviving at the front of the ring.
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    log.append(&bytes);
}

/// Persist the byte ring to `path`, replacing any prior contents.
///
/// Used right before kexec hands off to the next kernel: kexec drops the
/// pagecache, so the existing `write_all + flush` in `panic::write_report`
/// would lose the tail of NMBL's transcript. The extra `fsync(2)` here
/// forces the writeback so the on-disk log survives the handoff.
///
/// When the in-memory ring overflowed (`dropped_bytes > 0`), the file's
/// first line is a fixed marker naming the byte count that was lost off
/// the front, so downstream tooling does not silently treat the
/// remainder as the entire transcript.
pub fn flush_to(path: &Path) -> std::io::Result<()> {
    let (header, body) = {
        let guard = BYTE_LOG
            .lock()
            .map_err(|_| std::io::Error::other("byte log mutex poisoned"))?;
        match guard.as_ref() {
            Some(log) => {
                let header = if log.dropped_bytes > 0 {
                    Some(format!(
                        "=== nmbl-init: log truncated, earlier {} bytes dropped ===\n",
                        log.dropped_bytes
                    ))
                } else {
                    None
                };
                // Clone bytes out under the lock so we release it before
                // doing file I/O (which can block on disk, fsync, etc.).
                let body: Vec<u8> = log.buf.iter().copied().collect();
                (header, body)
            }
            None => (None, Vec::new()),
        }
    };
    write_truncated(path, header.as_deref(), &body)
}

/// Snapshot the FULL buffered boot transcript as lines (oldest first).
///
/// Unlike [`snapshot`] — which only returns the 256-line tail of the
/// string ring — this drains the ~1 MiB `BYTE_LOG` byte ring, so it
/// covers the complete NMBL transcript (modulo any bytes already dropped
/// off the front under overflow). Intended for the in-process log viewer
/// that wants the whole boot, not just the visible tail.
///
/// When the byte ring overflowed (`dropped_bytes > 0`), a single note
/// line is prepended naming how many bytes were lost off the front, so a
/// reader does not mistake the remainder for the entire boot.
///
/// The `BYTE_LOG` mutex is held only long enough to clone the bytes out
/// (mirroring `flush_to`); the UTF-8 split happens after the guard drops.
#[must_use]
pub fn snapshot_full() -> Vec<String> {
    let (dropped, body) = {
        let Ok(guard) = BYTE_LOG.lock() else {
            return Vec::new();
        };
        match guard.as_ref() {
            // Clone bytes out under the lock so we release it before the
            // (potentially large) UTF-8 decode + split — same access
            // pattern as `flush_to`.
            Some(log) => (
                log.dropped_bytes,
                log.buf.iter().copied().collect::<Vec<u8>>(),
            ),
            None => return Vec::new(),
        }
    };

    let text = String::from_utf8_lossy(&body);
    // `lines()` would swallow a meaningful trailing empty line and also
    // strip the final `\n`; splitting on '\n' keeps the round-trip
    // predictable. Each emitted body was stored with a trailing '\n', so
    // the split yields a trailing empty element we drop.
    let mut lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }

    if dropped > 0 {
        lines.insert(0, format!("… {dropped} earlier bytes truncated …"));
    }
    lines
}

/// Open `path` truncated and write the optional header + body, then
/// fsync. Split out so `flush_to` can short-circuit when the ring is
/// uninitialised without duplicating the I/O path.
fn write_truncated(path: &Path, header: Option<&str>, body: &[u8]) -> std::io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    if let Some(h) = header {
        f.write_all(h.as_bytes())?;
    }
    f.write_all(body)?;
    f.flush()?;
    // rustix's safe fsync wrapper takes any AsFd; the std File implements
    // it via the OS-specific extension trait, so no unsafe is required.
    rustix::fs::fsync(&f).map_err(std::io::Error::from)?;
    Ok(())
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
        let mut guard = LOG_RING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(VecDeque::with_capacity(LOG_RING_CAPACITY));
        for i in 0..10 {
            push_inner(
                guard.as_mut().expect("ring just initialised"),
                &format!("line {i}"),
            );
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
        let mut guard = LOG_RING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(VecDeque::with_capacity(LOG_RING_CAPACITY));
        for i in 0..300 {
            push_inner(
                guard.as_mut().expect("ring just initialised"),
                &format!("entry {i}"),
            );
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
        let mut guard = LOG_RING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(VecDeque::new());
        let ring = guard.as_ref().expect("ring just initialised");
        assert!(snapshot_inner(ring, 10).is_empty());
        assert!(snapshot_inner(ring, 0).is_empty());
    }

    /// Serialises the byte-ring tests against each other and against
    /// the string-ring tests. The byte ring tests must NOT hold the
    /// `BYTE_LOG` / `LOG_RING` mutexes across `emit_kmsg` calls (those
    /// `try_lock` and drop on contention), so this outer lock is the
    /// only thing keeping concurrent test threads from contaminating
    /// the shared byte ring static.
    static BYTE_LOG_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn byte_log_test_guard() -> std::sync::MutexGuard<'static, ()> {
        BYTE_LOG_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Wipe both rings so the next test starts from a clean slate. Used
    /// both at the start of a byte-ring test (in case a prior test left
    /// the static populated) and at the end (so the next test gets the
    /// same fresh state regardless of execution order).
    fn reset_rings() {
        let mut s = LOG_RING
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *s = None;
        drop(s);
        let mut b = BYTE_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *b = None;
    }

    #[test]
    fn byte_ring_overflow_drops_oldest_and_counts() {
        let _outer = byte_log_test_guard();
        // Combine the string-ring lock too; emit_kmsg touches both rings
        // and we hold neither static lock across the calls.
        let _inner = ring_test_guard();
        reset_rings();

        // Each line is 1024 bytes + '\n'. Push enough to overshoot the
        // 1 MiB cap by ~2 MiB worth, so the drop count is well above
        // zero and the buffer is forced back below cap.
        let line = "x".repeat(1024);
        let pushes = 3 * 1024;
        for _ in 0..pushes {
            emit_kmsg(&line);
        }

        let guard = BYTE_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let log = guard.as_ref().expect("byte log initialised by emit_kmsg");
        assert!(
            log.buf.len() <= BYTE_RING_CAPACITY,
            "buf {} > cap",
            log.buf.len()
        );
        assert!(log.dropped_bytes > 0, "expected dropped_bytes > 0");
        drop(guard);

        reset_rings();
    }

    #[test]
    fn flush_to_emits_truncation_header_when_dropped() {
        let _outer = byte_log_test_guard();
        let _inner = ring_test_guard();
        reset_rings();

        // Force overflow.
        let line = "y".repeat(1024);
        for _ in 0..(3 * 1024) {
            emit_kmsg(&line);
        }

        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("nmbl.log");
        flush_to(&path).expect("flush_to ok");

        let bytes = std::fs::read(&path).expect("read flushed file");
        let text = std::str::from_utf8(&bytes).expect("utf8");
        let first_line = text.split_once('\n').map(|(l, _)| l).unwrap_or(text);

        // Header format is fixed; only the byte count varies. Parse the
        // count out of the header line itself rather than re-reading the
        // global byte ring afterwards: under parallel `--all-features`
        // runs other tests in the same binary emit through `emit_kmsg`
        // into the shared `BYTE_LOG` (they don't take the log-test lock),
        // which could bump `dropped_bytes` between flush and re-read and
        // made the exact-count compare flaky. The header `flush_to` wrote
        // is internally consistent, so assert on its format + a positive
        // count instead.
        let prefix = "=== nmbl-init: log truncated, earlier ";
        let suffix = " bytes dropped ===";
        assert!(
            first_line.starts_with(prefix) && first_line.ends_with(suffix),
            "unexpected truncation header: {first_line:?}"
        );
        let dropped: u64 = first_line
            .trim_start_matches(prefix)
            .trim_end_matches(suffix)
            .parse()
            .expect("truncation header carries a numeric dropped-byte count");
        assert!(
            dropped > 0,
            "expected a positive dropped-byte count, got {dropped}"
        );

        reset_rings();
    }

    #[test]
    fn flush_to_omits_header_without_truncation() {
        let _outer = byte_log_test_guard();
        let _inner = ring_test_guard();
        reset_rings();

        emit_kmsg("hello");
        emit_kmsg("world");

        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("nmbl.log");
        flush_to(&path).expect("flush_to ok");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            !text.starts_with("=== nmbl-init: log truncated"),
            "unexpected truncation header in {text:?}"
        );
        assert_eq!(text, "hello\nworld\n");

        reset_rings();
    }

    #[test]
    fn snapshot_full_returns_all_emitted_lines() {
        let _outer = byte_log_test_guard();
        let _inner = ring_test_guard();
        reset_rings();

        let lines = ["alpha", "beta", "gamma"];
        for l in &lines {
            emit_kmsg(l);
        }

        let snap = snapshot_full();
        assert_eq!(
            snap,
            lines.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
        );

        reset_rings();
    }

    #[test]
    fn snapshot_full_prepends_truncation_note_when_dropped() {
        let _outer = byte_log_test_guard();
        let _inner = ring_test_guard();
        reset_rings();

        // Force overflow so dropped_bytes > 0.
        let line = "z".repeat(1024);
        for _ in 0..(3 * 1024) {
            emit_kmsg(&line);
        }

        // Parse the count from the prepended note itself rather than
        // re-reading the shared global byte ring: concurrent emits from
        // other tests under parallel `--all-features` can bump
        // `dropped_bytes` between the read and the snapshot, making an
        // exact compare flaky (see `flush_to_emits_truncation_header_*`).
        let snap = snapshot_full();
        let note = snap
            .first()
            .map(String::as_str)
            .expect("snapshot non-empty after overflow");
        let prefix = "… ";
        let suffix = " earlier bytes truncated …";
        assert!(
            note.starts_with(prefix) && note.ends_with(suffix),
            "unexpected truncation note: {note:?}"
        );
        let dropped: u64 = note
            .trim_start_matches(prefix)
            .trim_end_matches(suffix)
            .parse()
            .expect("truncation note carries a numeric dropped-byte count");
        assert!(
            dropped > 0,
            "expected a positive dropped-byte count, got {dropped}"
        );

        reset_rings();
    }

    #[test]
    fn snapshot_full_empty_when_uninitialised() {
        let _outer = byte_log_test_guard();
        let _inner = ring_test_guard();
        reset_rings();

        assert!(snapshot_full().is_empty());

        reset_rings();
    }

    #[test]
    fn tui_refcount_tracks_nested_owners() {
        // No shared ring statics involved, but the refcount is a process
        // global; serialise against other tests that may toggle it via
        // the string-ring lock to keep the inc/dec sequence observable.
        let _guard = ring_test_guard();
        // Start from a known-zero baseline regardless of prior tests.
        while tui_active() {
            clear_tui_active();
        }
        assert!(!tui_active());

        set_tui_active();
        set_tui_active();
        assert!(tui_active(), "two owners: still active");

        clear_tui_active();
        assert!(tui_active(), "one owner left: still active");

        clear_tui_active();
        assert!(!tui_active(), "last owner released: inactive");

        // An unpaired extra clear must not wrap the count or wedge stderr
        // off; it saturates at zero.
        clear_tui_active();
        assert!(!tui_active());
        set_tui_active();
        assert!(tui_active(), "active again after a single set");
        clear_tui_active();
        assert!(!tui_active());
    }

    #[test]
    fn flush_to_round_trips_emitted_lines() {
        let _outer = byte_log_test_guard();
        let _inner = ring_test_guard();
        reset_rings();

        let lines = ["alpha", "beta", "gamma"];
        for l in &lines {
            emit_kmsg(l);
        }

        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("nmbl.log");
        flush_to(&path).expect("flush_to ok");

        let text = std::fs::read_to_string(&path).expect("read");
        let read_back: Vec<&str> = text.lines().collect();
        assert_eq!(read_back, lines);

        reset_rings();
    }
}
