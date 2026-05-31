//! On-demand reader for the kernel ring buffer (`/dev/kmsg`).
//!
//! This is the read side of the kernel log; the write/tee side lives in
//! [`super::kmsg`]. It is consulted only when the operator presses Ctrl+K
//! in the in-TUI log viewer, so the cost (open + drain) is paid lazily and
//! the snapshot is always fresh.
//!
//! `/dev/kmsg` exposes one printk record per `read(2)`. Each record is
//! `prio,seq,ts_usec,flag[,...];message\n` optionally followed by indented
//! `key=value` continuation lines (`SUBSYSTEM=...`, `DEVICE=...`) which we
//! drop. We open the device `O_RDONLY | O_NONBLOCK` and read records until
//! `EAGAIN` (the buffer is drained), formatting each as
//! `[    12.345678] message` to match the familiar `dmesg` layout.
//!
//! No `unsafe`: the device is a plain file read via [`std::fs`], and the
//! record parser is pure. Read errors are surfaced as a one-line message so
//! the viewer can show `kernel log unavailable: <err>` rather than panic.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

/// Cap on records drained from `/dev/kmsg` in a single open. The kernel
/// buffer is bounded (typically a few thousand records); this guards
/// against a pathological never-ending read so the UI stays responsive.
const MAX_RECORDS: usize = 100_000;

/// Read and format the kernel ring buffer into display lines (oldest
/// first), or a single explanatory line on failure.
///
/// Called ONCE per viewer-open / Ctrl+K toggle — never per scroll or per
/// render frame: the result is cached in `Screen::Log { lines }` and
/// every subsequent scroll/redraw reads only that cached buffer. Draining
/// and parsing the whole `/dev/kmsg` ring (thousands of records, `\xNN`
/// unescaping) is O(buffer), so doing it per keystroke would make scroll
/// laggy; the snapshot-once rule keeps it cheap.
///
/// Always returns at least one line so the viewer never renders an empty
/// box with no explanation.
#[must_use]
pub fn snapshot_kernel() -> Vec<String> {
    match read_kmsg_raw() {
        Ok(raw) if raw.is_empty() => vec!["(kernel log is empty)".to_owned()],
        Ok(raw) => parse_kmsg(&raw),
        Err(e) => vec![format!("kernel log unavailable: {e}")],
    }
}

/// Boxed fake raw-reader installed by [`set_raw_reader_for_test`].
#[cfg(test)]
type RawReaderFn = Box<dyn FnMut() -> std::io::Result<String>>;

// Source of the raw `/dev/kmsg` byte stream. In production this always
// opens and drains the real device; tests install a fake via
// `set_raw_reader_for_test` so the snapshot-once contract can be asserted
// without a real kernel ring buffer.
#[cfg(test)]
thread_local! {
    static RAW_READER_OVERRIDE: std::cell::RefCell<Option<RawReaderFn>> =
        const { std::cell::RefCell::new(None) };
}

/// Fetch the raw record stream, honouring a test override when one is
/// installed. Production builds compile straight to [`read_dev_kmsg`].
fn read_kmsg_raw() -> std::io::Result<String> {
    #[cfg(test)]
    {
        let overridden =
            RAW_READER_OVERRIDE.with(|cell| cell.borrow_mut().as_mut().map(|reader| reader()));
        if let Some(result) = overridden {
            return result;
        }
    }
    read_dev_kmsg()
}

/// Install a fake raw-reader for the current thread, returning a guard
/// that uninstalls it on drop. Each call to [`snapshot_kernel`] invokes
/// the closure exactly once, so a counting closure proves the snapshot is
/// taken once per open/toggle and NOT re-read on scroll or render.
#[cfg(test)]
pub(crate) fn set_raw_reader_for_test(
    reader: impl FnMut() -> std::io::Result<String> + 'static,
) -> RawReaderGuard {
    RAW_READER_OVERRIDE.with(|cell| *cell.borrow_mut() = Some(Box::new(reader)));
    RawReaderGuard
}

/// Drops the thread-local raw-reader override installed by
/// [`set_raw_reader_for_test`].
#[cfg(test)]
pub(crate) struct RawReaderGuard;

#[cfg(test)]
impl Drop for RawReaderGuard {
    fn drop(&mut self) {
        RAW_READER_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
    }
}

/// Drain every currently-available record from `/dev/kmsg`.
///
/// Opens `O_RDONLY | O_NONBLOCK | O_CLOEXEC` and reads one record per
/// `read(2)` until `EAGAIN`/`EWOULDBLOCK` signals the buffer is drained.
/// `EPIPE` means records were overwritten between reads (we raced the
/// kernel wrapping the buffer); we simply continue. Returns the
/// concatenated raw records (each still newline-terminated).
fn read_dev_kmsg() -> std::io::Result<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open("/dev/kmsg")?;
    drain_records(&mut file)
}

/// Pull records out of an already-opened non-blocking reader until it would
/// block. Split out from [`read_dev_kmsg`] so the drain loop is testable
/// against any [`Read`] without a real `/dev/kmsg`.
fn drain_records(reader: &mut impl Read) -> std::io::Result<String> {
    // Kernel records are emitted whole per read; 8 KiB comfortably holds the
    // longest printk line plus its continuation metadata.
    let mut buf = [0u8; 8192];
    let mut out = String::new();
    for _ in 0..MAX_RECORDS {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(chunk) = buf.get(..n) {
                    out.push_str(&String::from_utf8_lossy(chunk));
                }
            }
            Err(e) => match e.raw_os_error() {
                // EAGAIN (== EWOULDBLOCK on Linux): no more records now.
                Some(libc::EAGAIN) => break,
                // EPIPE: the kernel overwrote records mid-drain; the next
                // read resumes at a valid record. Keep going.
                Some(libc::EPIPE) => continue,
                // EINTR: interrupted syscall; retry.
                Some(libc::EINTR) => continue,
                _ => return Err(e),
            },
        }
    }
    Ok(out)
}

/// Parse a raw `/dev/kmsg` byte stream into formatted display lines.
///
/// Each record's header is `prio,seq,ts_usec,flag[,extra];message`. We keep
/// only `ts_usec` and `message`, render the timestamp as seconds with six
/// fractional digits, and embed any embedded newlines the kernel escaped as
/// `\x0a` back as real line breaks (the kernel C-escapes control bytes in
/// the message body). Indented continuation lines (`SUBSYSTEM=...`) are
/// dropped. Malformed records are skipped rather than aborting the parse.
#[must_use]
pub fn parse_kmsg(raw: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for record in raw.split('\n') {
        // Continuation lines for the previous record are indented with a
        // space/tab; they carry SUBSYSTEM=/DEVICE= metadata we don't show.
        if record.is_empty() || record.starts_with(' ') || record.starts_with('\t') {
            continue;
        }
        if let Some(parsed) = parse_record(record) {
            lines.push(parsed);
        }
    }
    lines
}

/// Parse one record's header line into `[   ts] message`, or `None` if the
/// line doesn't look like a kmsg record (no `;` separating header/message).
fn parse_record(record: &str) -> Option<String> {
    let (header, message) = record.split_once(';')?;
    // header = prio,seq,ts_usec,flag[,...]
    let mut fields = header.split(',');
    let _prio = fields.next()?;
    let _seq = fields.next()?;
    let ts_usec: u64 = fields.next()?.trim().parse().ok()?;
    // Remaining fields (flag, optional extras) are ignored.

    let message = unescape_kmsg(message);
    Some(format!("{} {message}", format_ts(ts_usec)))
}

/// Format a microseconds-since-boot timestamp as `[    12.345678]`,
/// right-aligning the seconds in a 5-wide field like `dmesg` does.
#[must_use]
fn format_ts(ts_usec: u64) -> String {
    let secs = ts_usec / 1_000_000;
    let frac = ts_usec % 1_000_000;
    format!("[{secs:5}.{frac:06}]")
}

/// Decode the kernel's `\xNN` C-style escapes in a message body. The kernel
/// escapes control bytes (including embedded newlines as `\x0a`) so a record
/// is always a single physical line on the wire; we turn those back into the
/// bytes they represent. Unrecognised escapes are passed through verbatim.
#[must_use]
fn unescape_kmsg(message: &str) -> String {
    if !message.contains('\\') {
        return message.to_owned();
    }
    let mut out = String::with_capacity(message.len());
    let mut bytes = message.bytes().peekable();
    while let Some(b) = bytes.next() {
        // Try to decode a `\xNN` escape; on any miss, emit bytes verbatim.
        if b == b'\\' && bytes.peek() == Some(&b'x') {
            let mut lookahead = bytes.clone();
            lookahead.next(); // consume the peeked 'x'
            let hi = lookahead.next().and_then(|c| (c as char).to_digit(16));
            let lo = lookahead.next().and_then(|c| (c as char).to_digit(16));
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi * 16 + lo) as u8) as char);
                bytes = lookahead;
                continue;
            }
        }
        out.push(b as char);
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_record() {
        let parsed = parse_kmsg("6,123,456789,-;hello world\n");
        assert_eq!(parsed, vec!["[    0.456789] hello world".to_owned()]);
    }

    #[test]
    fn formats_timestamp_into_seconds() {
        // 12_345_678 µs = 12.345678 s
        let parsed = parse_kmsg("4,9,12345678,-;late message\n");
        assert_eq!(parsed, vec!["[   12.345678] late message".to_owned()]);
    }

    #[test]
    fn drops_continuation_metadata_lines() {
        let raw = "6,1,100,-;usb 1-1: new device\n SUBSYSTEM=usb\n DEVICE=+usb\n";
        let parsed = parse_kmsg(raw);
        assert_eq!(
            parsed,
            vec!["[    0.000100] usb 1-1: new device".to_owned()]
        );
    }

    #[test]
    fn parses_multiple_records_in_order() {
        let raw = "6,1,1000000,-;first\n6,2,2000000,-;second\n";
        let parsed = parse_kmsg(raw);
        assert_eq!(
            parsed,
            vec![
                "[    1.000000] first".to_owned(),
                "[    2.000000] second".to_owned(),
            ]
        );
    }

    #[test]
    fn unescapes_embedded_control_bytes() {
        // Kernel escapes a literal newline in the body as \x0a.
        let parsed = parse_kmsg("6,1,0,-;line one\\x0aline two\n");
        assert_eq!(parsed, vec!["[    0.000000] line one\nline two".to_owned()]);
    }

    #[test]
    fn skips_malformed_records_without_semicolon() {
        let raw = "garbage with no separator\n6,2,500,-;good line\n";
        let parsed = parse_kmsg(raw);
        assert_eq!(parsed, vec!["[    0.000500] good line".to_owned()]);
    }

    #[test]
    fn skips_record_with_nonnumeric_timestamp() {
        let raw = "6,2,notanumber,-;bad ts\n6,3,700,-;ok\n";
        let parsed = parse_kmsg(raw);
        assert_eq!(parsed, vec!["[    0.000700] ok".to_owned()]);
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(parse_kmsg("").is_empty());
    }

    #[test]
    fn drain_records_stops_on_eagain() {
        // A reader that yields one record then signals EAGAIN, mimicking a
        // drained non-blocking /dev/kmsg.
        struct OneThenBlock(bool);
        impl Read for OneThenBlock {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    self.0 = false;
                    let rec = b"6,1,0,-;only\n";
                    buf[..rec.len()].copy_from_slice(rec);
                    Ok(rec.len())
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::EAGAIN))
                }
            }
        }
        let mut r = OneThenBlock(true);
        let raw = drain_records(&mut r).expect("drain should swallow EAGAIN");
        assert_eq!(parse_kmsg(&raw), vec!["[    0.000000] only".to_owned()]);
    }

    #[test]
    fn drain_records_continues_past_epipe() {
        // EPIPE (records overwritten mid-drain) must not abort the drain.
        struct PipeThenRecord(u8);
        impl Read for PipeThenRecord {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += 1;
                match self.0 {
                    1 => Err(std::io::Error::from_raw_os_error(libc::EPIPE)),
                    2 => {
                        let rec = b"6,5,9,-;after wrap\n";
                        buf[..rec.len()].copy_from_slice(rec);
                        Ok(rec.len())
                    }
                    _ => Err(std::io::Error::from_raw_os_error(libc::EAGAIN)),
                }
            }
        }
        let mut r = PipeThenRecord(0);
        let raw = drain_records(&mut r).expect("EPIPE should be skipped");
        assert_eq!(
            parse_kmsg(&raw),
            vec!["[    0.000009] after wrap".to_owned()]
        );
    }
}
