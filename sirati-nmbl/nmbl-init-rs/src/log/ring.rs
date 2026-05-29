use std::collections::VecDeque;
use std::sync::Mutex;

/// Capacity of the in-memory log ring. Sized to comfortably cover a
/// full NMBL boot transcript (device probing + module loads + menu
/// entry chatter) without spending more than a few KiB; older lines
/// are evicted FIFO. The TUI never asks for more than the visible
/// screen height anyway, so this is effectively "as much scrollback
/// as we keep alive".
pub(super) const LOG_RING_CAPACITY: usize = 256;

/// In-memory ring of recently emitted log bodies (no `[nmbl]` prefix,
/// no `<6>` priority — the render layer adds those if it wants). Held
/// behind a `Mutex` so `nmbl_*!` calls from any thread tee here safely.
/// Lazily initialised so first-touch is the boot's first log line, not
/// program startup.
pub(super) static LOG_RING: Mutex<Option<VecDeque<String>>> = Mutex::new(None);

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
    ring_push(ring, line);
}

/// FIFO push with capacity-bounded eviction. The pure half of
/// [`push_ring`]: takes a ring the caller already owns/locked so the
/// eviction logic can be exercised on a local `VecDeque` without
/// touching the process-global [`LOG_RING`].
pub(super) fn ring_push(ring: &mut VecDeque<String>, line: &str) {
    if ring.len() == LOG_RING_CAPACITY {
        ring.pop_front();
    }
    ring.push_back(line.to_owned());
}

/// Tail snapshot of a ring (most recent last). The pure half of
/// [`snapshot`]; operates on a borrowed ring so the ordering/capping
/// behaviour is testable on a local instance.
pub(super) fn ring_snapshot(ring: &VecDeque<String>, n: usize) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let take = n.min(ring.len());
    let start = ring.len().saturating_sub(take);
    ring.iter().skip(start).cloned().collect()
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
    ring_snapshot(ring, n)
}
