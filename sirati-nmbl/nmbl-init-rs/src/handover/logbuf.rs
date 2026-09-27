//! Decoder for NMBL's log handover buffer.
//!
//! NMBL flushes its byte-ring transcript to `/nmbl-log/nmbl.log` and splices it
//! into the kexec'd initrd (see [`crate::log`] / [`crate::boot`]). Each line is
//! a message body; the file may open with a truncation header when the ring
//! overflowed. This decoder turns that text back into structured [`LogLine`]s —
//! a level and the message — so a status/harness view can render coloured,
//! levelled, pageable output. It is the read side the running system and the
//! container harness share.
//!
//! The stored transcript carries the message body without an embedded printk
//! level (the `<6>[nmbl] ` wrapper is added only on the `/dev/kmsg` tee, not in
//! the byte ring). We therefore classify the level heuristically from the
//! message text, which is deterministic and good enough for a coloured view;
//! the raw message is always preserved verbatim.

/// A log line's severity, for colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// A truncation / structural note the decoder synthesised.
    Meta,
    /// Warnings (`warn`, `failed`, `error`, `refuse`).
    Warn,
    /// Ordinary info.
    Info,
}

/// One decoded log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// The classified level.
    pub level: LogLevel,
    /// The message text, verbatim (any `[nmbl] ` prefix stripped).
    pub message: String,
}

/// The kmsg-style prefix NMBL adds only on the /dev/kmsg tee. If a stored line
/// happens to carry it (e.g. a transcript captured from kmsg), strip it.
fn strip_prefix(line: &str) -> &str {
    // Optional `<N>` printk level, then optional `[nmbl] `.
    let line = if let Some(rest) = line.strip_prefix('<') {
        rest.split_once('>').map_or(line, |(_, r)| r)
    } else {
        line
    };
    line.strip_prefix("[nmbl] ").unwrap_or(line)
}

/// Classify a message's level from its text. Deterministic keyword match:
/// warning/error vocabulary → [`LogLevel::Warn`], everything else info.
fn classify(message: &str) -> LogLevel {
    let lower = message.to_ascii_lowercase();
    const WARN_MARKERS: [&str; 6] = ["warn", "error", "failed", "fail:", "refuse", "panic"];
    if WARN_MARKERS.iter().any(|m| lower.contains(m)) {
        LogLevel::Warn
    } else {
        LogLevel::Info
    }
}

/// Decode a raw log transcript into structured lines.
///
/// A leading `=== nmbl-init: log truncated …` header (written by
/// [`crate::log::flush_to`] on ring overflow) or a `… earlier bytes truncated …`
/// note becomes a [`LogLevel::Meta`] line; every other non-empty line is
/// classified and its `[nmbl] ` prefix stripped. Blank trailing lines are
/// dropped so the round trip with the encoder's `\n`-terminated bodies is
/// exact.
#[must_use]
pub fn decode_log_buffer(text: &str) -> Vec<LogLine> {
    text.split('\n')
        .filter(|l| !l.is_empty())
        .map(|line| {
            let is_meta = line.starts_with("=== nmbl-init:")
                || line.contains("earlier bytes truncated")
                || line.contains("bytes dropped");
            if is_meta {
                LogLine {
                    level: LogLevel::Meta,
                    message: line.to_string(),
                }
            } else {
                let msg = strip_prefix(line).to_string();
                LogLine {
                    level: classify(&msg),
                    message: msg,
                }
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used,
    reason = "tests assert"
)]
mod tests {
    use super::*;

    #[test]
    fn classifies_info_and_warn_lines() {
        let text = "phase 1: mount pseudo-filesystems\nboot failed; entering rescue\n";
        let lines = decode_log_buffer(text);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].level, LogLevel::Info);
        assert_eq!(lines[1].level, LogLevel::Warn);
    }

    #[test]
    fn strips_the_kmsg_prefix() {
        let lines = decode_log_buffer("<6>[nmbl] kexec: handing off\n");
        assert_eq!(lines[0].message, "kexec: handing off");
        assert_eq!(lines[0].level, LogLevel::Info);
    }

    #[test]
    fn truncation_header_is_meta() {
        let text = "=== nmbl-init: log truncated, earlier 4096 bytes dropped ===\nphase 1\n";
        let lines = decode_log_buffer(text);
        assert_eq!(lines[0].level, LogLevel::Meta);
        assert_eq!(lines[1].level, LogLevel::Info);
    }

    #[test]
    fn round_trips_the_byte_ring_snapshot_shape() {
        // The byte ring stores each body with a trailing '\n'; decode must
        // recover exactly the emitted messages with no phantom blank line.
        let emitted = ["phase 2a: early modules", "signature verified: gen 3 OK"];
        let joined = format!("{}\n{}\n", emitted[0], emitted[1]);
        let lines = decode_log_buffer(&joined);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].message, emitted[0]);
        assert_eq!(lines[1].message, emitted[1]);
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(decode_log_buffer("").is_empty());
        assert!(decode_log_buffer("\n\n").is_empty());
    }
}
