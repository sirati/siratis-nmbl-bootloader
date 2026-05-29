#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use std::collections::VecDeque;
use std::sync::Mutex;

use super::byte_ring::{BYTE_LOG, BYTE_RING_CAPACITY, flush_to, snapshot_full};
use super::kmsg::emit_kmsg;
use super::ring::{LOG_RING, LOG_RING_CAPACITY};
use super::tui_flag::{clear_tui_active, set_tui_active, tui_active};

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

    // Header format is fixed; only the byte count varies.
    let dropped = {
        let g = BYTE_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.as_ref().expect("byte log").dropped_bytes
    };
    let expected = format!("=== nmbl-init: log truncated, earlier {dropped} bytes dropped ===");
    assert_eq!(first_line, expected);

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

    let dropped = {
        let g = BYTE_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.as_ref().expect("byte log").dropped_bytes
    };
    assert!(dropped > 0, "test precondition: expected dropped_bytes > 0");

    let snap = snapshot_full();
    assert_eq!(
        snap.first().map(String::as_str),
        Some(format!("… {dropped} earlier bytes truncated …").as_str())
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
