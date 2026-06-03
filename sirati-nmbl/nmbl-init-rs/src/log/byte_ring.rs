use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// Byte-ring capacity (1 MiB). Chosen to be big enough to hold a full
/// rescue-path transcript including hot-loop retries, while still
/// fitting easily in tmpfs at boot. On overflow the front (oldest) bytes
/// are dropped and the dropped count is remembered so the eventual
/// `flush_to` consumer can flag truncation in a header line.
pub(super) const BYTE_RING_CAPACITY: usize = 1024 * 1024;

/// Mirror of every `emit_kmsg` body (with its trailing `\n` appended)
/// stored as raw bytes. Persisted to disk by `flush_to` right before
/// kexec drops the pagecache, giving the next stage a complete NMBL
/// transcript even when the kernel ring buffer has rotated past it.
pub(super) struct ByteLog {
    pub(super) buf: VecDeque<u8>,
    pub(super) dropped_bytes: u64,
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

    /// Append `line\n` to the ring as one unit. Building the on-disk
    /// representation up-front means a single append either lands whole
    /// or (under overflow) gets truncated as one unit — no risk of a
    /// half-line surviving at the front of the ring. The pure half of
    /// [`push_byte_ring`]; runs on a borrowed log so overflow/drop
    /// accounting is testable on a local instance.
    pub(super) fn append_line(&mut self, line: &str) {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        self.append(&bytes);
    }

    /// Compute the optional truncation header for a flush. The pure half
    /// of [`flush_to`]'s under-lock block: when bytes were dropped off
    /// the front, the file's first line names the lost count so
    /// downstream tooling does not treat the remainder as the whole
    /// transcript.
    pub(super) fn flush_header(&self) -> Option<String> {
        if self.dropped_bytes > 0 {
            Some(format!(
                "=== nmbl-init: log truncated, earlier {} bytes dropped ===\n",
                self.dropped_bytes
            ))
        } else {
            None
        }
    }
}

/// Test-only constructor for a fresh, empty `ByteLog` so the ring tests
/// can exercise append/overflow/flush logic on a local instance instead
/// of the process-global [`BYTE_LOG`] static.
#[cfg(test)]
pub(super) fn new_byte_log() -> ByteLog {
    ByteLog::new()
}

/// Decode buffered transcript bytes into lines (oldest first),
/// prepending a truncation note when `dropped > 0`. Free function so
/// [`snapshot_full`] can clone the bytes out under the `BYTE_LOG` lock
/// and run the (potentially large) UTF-8 decode + split after the guard
/// drops — matching `flush_to`'s lock-release-before-work pattern.
pub(super) fn decode_snapshot_lines(dropped: u64, body: Vec<u8>) -> Vec<String> {
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

pub(super) static BYTE_LOG: Mutex<Option<ByteLog>> = Mutex::new(None);

/// Append `line\n` to the byte ring, dropping on lock contention so the
/// hot logging path never blocks the boot. Mirrors `push_ring`'s
/// try_lock policy for the same reason.
pub(super) fn push_byte_ring(line: &str) {
    let Ok(mut guard) = BYTE_LOG.try_lock() else {
        return;
    };
    let log = guard.get_or_insert_with(ByteLog::new);
    log.append_line(line);
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
    let (header, body) = snapshot_flush_parts();
    write_truncated(path, header.as_deref(), &body)
}

/// Snapshot the optional truncation header + body bytes the next [`flush_to`]
/// would persist, holding the `BYTE_LOG` lock only long enough to clone the
/// bytes out (same access pattern as [`flush_to`] / [`snapshot_full`]). On a
/// poisoned mutex this degrades to an empty transcript rather than erroring —
/// the byte ring is best-effort.
fn snapshot_flush_parts() -> (Option<String>, Vec<u8>) {
    let Ok(guard) = BYTE_LOG.lock() else {
        return (None, Vec::new());
    };
    match guard.as_ref() {
        Some(log) => {
            let header = log.flush_header();
            // Clone bytes out under the lock so we release it before doing any
            // further work (the caller may block on disk / fsync, etc.).
            let body: Vec<u8> = log.buf.iter().copied().collect();
            (header, body)
        }
        None => (None, Vec::new()),
    }
}

/// Materialise the EXACT bytes [`flush_to`] would write to `path` (optional
/// truncation header followed by the body), WITHOUT performing any file I/O.
///
/// Used by the pre-kexec log staging so the flush can be routed through the
/// [`FsOps`](crate::sys::ops::FsOps) seam: `--validate-initrm` no-ops the write
/// instead of a direct `create + truncate + fsync`, while a real boot writes
/// the byte-identical content via `FsOps::write_file`. The header/body are
/// assembled the same way [`write_truncated`] lays them on disk, so the staged
/// bytes equal what `flush_to` produces.
#[must_use]
pub fn snapshot_flush_bytes() -> Vec<u8> {
    let (header, body) = snapshot_flush_parts();
    assemble_flush_bytes(header.as_deref(), &body)
}

/// Concatenate the optional truncation `header` and `body` into the single
/// buffer [`snapshot_flush_bytes`] returns — laid out EXACTLY as
/// [`write_truncated`] writes them to disk (header first, then body). The pure
/// half of [`snapshot_flush_bytes`], so the in-memory staging bytes can be
/// asserted byte-for-byte on a local fixture without the `BYTE_LOG` static.
pub(super) fn assemble_flush_bytes(header: Option<&str>, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(header.map_or(0, str::len) + body.len());
    if let Some(h) = header {
        out.extend_from_slice(h.as_bytes());
    }
    out.extend_from_slice(body);
    out
}

/// Snapshot the FULL buffered boot transcript as lines (oldest first).
///
/// Unlike [`super::snapshot`] — which only returns the 256-line tail of the
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
    decode_snapshot_lines(dropped, body)
}

/// Open `path` truncated and write the optional header + body, then
/// fsync. Split out so `flush_to` can short-circuit when the ring is
/// uninitialised without duplicating the I/O path.
pub(super) fn write_truncated(
    path: &Path,
    header: Option<&str>,
    body: &[u8],
) -> std::io::Result<()> {
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
