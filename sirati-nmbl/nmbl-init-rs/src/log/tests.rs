#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use std::collections::VecDeque;
use std::sync::Mutex;

use super::byte_ring::{
    BYTE_RING_CAPACITY, ByteLog, assemble_flush_bytes, decode_snapshot_lines, new_byte_log,
    write_truncated,
};
use super::ring::{LOG_RING_CAPACITY, ring_push, ring_snapshot};
use super::tui_flag::{clear_tui_active, set_tui_active, tui_active};

// These tests exercise the ring / byte-ring LOGIC on LOCAL instances
// (`VecDeque<String>` / `ByteLog`) via the extracted pure helpers
// (`ring_push`, `ring_snapshot`, `ByteLog::append_line`,
// `flush_header`, `snapshot_lines`, `decode_snapshot_lines`), never
// touching the process-global `LOG_RING` / `BYTE_LOG` statics. The
// production wrappers delegate to exactly these helpers, so behaviour
// is verified faithfully while parallel `emit_kmsg` calls from other
// tests in the binary cannot contaminate the asserted state. No test
// lock or ring reset is needed — every fixture is freshly constructed.

fn fresh_ring() -> VecDeque<String> {
    VecDeque::with_capacity(LOG_RING_CAPACITY)
}

#[test]
fn snapshot_returns_recent_lines() {
    let mut ring = fresh_ring();
    for i in 0..10 {
        ring_push(&mut ring, &format!("line {i}"));
    }
    let snap = ring_snapshot(&ring, 5);
    assert_eq!(snap.len(), 5);
    assert_eq!(snap.first().map(String::as_str), Some("line 5"));
    assert_eq!(snap.get(4).map(String::as_str), Some("line 9"));
}

#[test]
fn snapshot_caps_at_ring_capacity() {
    let mut ring = fresh_ring();
    for i in 0..300 {
        ring_push(&mut ring, &format!("entry {i}"));
    }
    let snap = ring_snapshot(&ring, usize::MAX);
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
    let ring = VecDeque::new();
    assert!(ring_snapshot(&ring, 10).is_empty());
    assert!(ring_snapshot(&ring, 0).is_empty());
}

/// Push enough 1024-byte lines into a fresh `ByteLog` to overshoot
/// the 1 MiB cap by ~2 MiB, forcing front-eviction and a positive
/// drop count. Returns the populated log for the caller to assert on.
fn overflowed_byte_log(fill: char) -> ByteLog {
    let mut log = new_byte_log();
    let line: String = std::iter::repeat_n(fill, 1024).collect();
    for _ in 0..(3 * 1024) {
        log.append_line(&line);
    }
    log
}

/// Mirrors `snapshot_full`'s body on a local `ByteLog`: clone the
/// bytes out and decode them through the same pure helper the
/// production wrapper uses.
fn snapshot_lines(log: &ByteLog) -> Vec<String> {
    decode_snapshot_lines(log.dropped_bytes, log.buf.iter().copied().collect())
}

#[test]
fn byte_ring_overflow_drops_oldest_and_counts() {
    let log = overflowed_byte_log('x');
    assert!(
        log.buf.len() <= BYTE_RING_CAPACITY,
        "buf {} > cap",
        log.buf.len()
    );
    assert!(log.dropped_bytes > 0, "expected dropped_bytes > 0");
}

#[test]
fn flush_to_emits_truncation_header_when_dropped() {
    let log = overflowed_byte_log('y');

    // Drive the real on-disk write path with the local log's bytes
    // and header, exactly as `flush_to` does for the global ring.
    let body: Vec<u8> = log.buf.iter().copied().collect();
    let header = log.flush_header().expect("overflow yields a header");
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("nmbl.log");
    write_truncated(&path, Some(&header), &body).expect("write ok");

    let bytes = std::fs::read(&path).expect("read flushed file");
    let text = std::str::from_utf8(&bytes).expect("utf8");
    let first_line = text.split_once('\n').map(|(l, _)| l).unwrap_or(text);

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
    assert_eq!(
        dropped, log.dropped_bytes,
        "header byte count matches the local ring's drop accounting"
    );
}

#[test]
fn flush_to_omits_header_without_truncation() {
    let mut log = new_byte_log();
    log.append_line("hello");
    log.append_line("world");

    assert!(
        log.flush_header().is_none(),
        "no overflow ⇒ no truncation header"
    );
    let body: Vec<u8> = log.buf.iter().copied().collect();
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("nmbl.log");
    write_truncated(&path, log.flush_header().as_deref(), &body).expect("write ok");

    let text = std::fs::read_to_string(&path).expect("read");
    assert_eq!(text, "hello\nworld\n");
}

#[test]
fn assemble_flush_bytes_equals_the_on_disk_flush_no_header() {
    // The ops-routed kexec-staging flush materialises the SAME bytes the direct
    // on-disk `write_truncated` would persist. Assert the in-memory assembler
    // matches `write_truncated`'s output byte-for-byte on a header-less ring.
    let mut log = new_byte_log();
    log.append_line("hello");
    log.append_line("world");
    let body: Vec<u8> = log.buf.iter().copied().collect();

    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("nmbl.log");
    write_truncated(&path, log.flush_header().as_deref(), &body).expect("write ok");
    let on_disk = std::fs::read(&path).expect("read flushed file");

    let staged = assemble_flush_bytes(log.flush_header().as_deref(), &body);
    assert_eq!(staged, on_disk, "staged bytes must equal the on-disk flush");
    assert_eq!(staged, b"hello\nworld\n");
}

#[test]
fn assemble_flush_bytes_includes_truncation_header() {
    // When the ring overflowed, the staged bytes must carry the SAME truncation
    // header `write_truncated` prepends, ahead of the body.
    let log = overflowed_byte_log('z');
    let body: Vec<u8> = log.buf.iter().copied().collect();
    let header = log.flush_header().expect("overflow yields a header");

    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("nmbl.log");
    write_truncated(&path, Some(&header), &body).expect("write ok");
    let on_disk = std::fs::read(&path).expect("read flushed file");

    let staged = assemble_flush_bytes(Some(&header), &body);
    assert_eq!(staged, on_disk, "staged bytes must equal the on-disk flush");
    assert!(
        staged.starts_with(b"=== nmbl-init: log truncated, earlier "),
        "staged bytes must lead with the truncation header",
    );
}

#[test]
fn snapshot_full_returns_all_emitted_lines() {
    let mut log = new_byte_log();
    let lines = ["alpha", "beta", "gamma"];
    for l in &lines {
        log.append_line(l);
    }
    let snap = snapshot_lines(&log);
    assert_eq!(
        snap,
        lines.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
    );
}

#[test]
fn snapshot_full_prepends_truncation_note_when_dropped() {
    let log = overflowed_byte_log('z');
    let snap = snapshot_lines(&log);
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
    assert_eq!(
        dropped, log.dropped_bytes,
        "note byte count matches the local ring's drop accounting"
    );
}

#[test]
fn snapshot_full_empty_when_uninitialised() {
    // An empty byte ring decodes to no lines and adds no note.
    let log = new_byte_log();
    assert!(snapshot_lines(&log).is_empty());
    assert!(decode_snapshot_lines(0, Vec::new()).is_empty());
}

#[test]
fn flush_to_round_trips_emitted_lines() {
    let mut log = new_byte_log();
    let lines = ["alpha", "beta", "gamma"];
    for l in &lines {
        log.append_line(l);
    }

    let body: Vec<u8> = log.buf.iter().copied().collect();
    let dir = tempfile::tempdir().expect("tmpdir");
    let path = dir.path().join("nmbl.log");
    write_truncated(&path, log.flush_header().as_deref(), &body).expect("write ok");

    let text = std::fs::read_to_string(&path).expect("read");
    let read_back: Vec<&str> = text.lines().collect();
    assert_eq!(read_back, lines);
}

/// The TUI refcount IS a process global, but no other unit test in
/// the crate runs the production paths that toggle it, so a single
/// dedicated lock keeps this test's inc/dec sequence observable
/// against any future sibling that touches the refcount.
static TUI_REFCOUNT_TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn tui_refcount_tracks_nested_owners() {
    let _guard = TUI_REFCOUNT_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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
