//! Decoder for the newc cpio fragment NMBL appends to the kexec'd initrd.
//!
//! [`crate::sys::cpio`] writes a minimal newc archive (directories, `0400`
//! regular files, a `TRAILER!!!` marker) carrying the LUKS keyfiles and the
//! NMBL log transcript. This module parses that byte stream back into a list
//! of [`CpioEntry`] so a status/harness view can show WHICH files NMBL
//! appended, their sizes, and a coarse type guess — without needing the
//! kernel to unpack it. It is the inverse of `sys::cpio::build_fragment`, and
//! the round-trip test pins that.
//!
//! The parser is defensive: a malformed header or a truncated body stops the
//! walk and returns whatever entries were decoded so far, so a partial or
//! unknown blob still yields a useful (if incomplete) view rather than an
//! error.

/// What kind of thing a cpio entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpioEntryKind {
    /// A directory (mode `S_IFDIR`).
    Directory,
    /// A regular file (mode `S_IFREG`).
    File,
    /// The end-of-archive `TRAILER!!!` marker.
    Trailer,
    /// Any other/unknown mode.
    Other,
}

/// One decoded cpio entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpioEntry {
    /// The in-cpio name (relative, no leading `/`), e.g. `etc/nmbl-luks/root`.
    pub name: String,
    /// The entry kind, from the mode's format bits.
    pub kind: CpioEntryKind,
    /// The raw mode value (permission + format bits).
    pub mode: u32,
    /// File-data length in bytes (0 for directories / the trailer).
    pub size: usize,
    /// A coarse content-type guess for regular files, from the name and the
    /// first bytes of the body. `None` for non-files.
    pub content_hint: Option<&'static str>,
}

const HEADER_LEN: usize = 110;
const MODE_FMT_MASK: u32 = 0o170_000;
const MODE_DIR: u32 = 0o040_000;
const MODE_FILE: u32 = 0o100_000;

fn parse_hex_field(bytes: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(bytes).ok()?;
    u32::from_str_radix(s, 16).ok()
}

fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

/// Decode a newc cpio fragment into its entries (including the trailer).
///
/// Walks concatenated newc records until the `TRAILER!!!` marker, a short
/// buffer, or a malformed header. Every field is bounds-checked; on any parse
/// failure the walk stops and the entries decoded so far are returned, so an
/// unknown or truncated blob degrades gracefully instead of erroring.
#[must_use]
pub fn decode_cpio_fragment(buf: &[u8]) -> Vec<CpioEntry> {
    let mut entries = Vec::new();
    let mut off = 0usize;
    while let Some(header) = buf.get(off..off + HEADER_LEN) {
        if header.get(..6) != Some(b"070701") {
            break;
        }
        // Field layout after the 6-byte magic: 13 × 8 hex chars. We need mode
        // (field 1), filesize (field 6), namesize (field 11).
        let field = |i: usize| -> Option<u32> {
            let start = 6 + i * 8;
            parse_hex_field(header.get(start..start + 8)?)
        };
        let (Some(mode), Some(filesize), Some(namesize)) = (field(1), field(6), field(11)) else {
            break;
        };
        let name_start = off + HEADER_LEN;
        let name_len = namesize as usize;
        let Some(name_bytes) = buf.get(name_start..name_start + name_len) else {
            break;
        };
        // Name includes a trailing NUL; strip it.
        let name =
            String::from_utf8_lossy(name_bytes.split_last().map_or(name_bytes, |(_, rest)| rest))
                .into_owned();
        // Header+name padded to 4; then file data, also padded to 4.
        let data_start = off + align4(HEADER_LEN + name_len);
        let data_len = filesize as usize;
        let body = buf.get(data_start..data_start + data_len);

        let kind = match mode & MODE_FMT_MASK {
            MODE_DIR => CpioEntryKind::Directory,
            MODE_FILE => CpioEntryKind::File,
            _ if name == "TRAILER!!!" => CpioEntryKind::Trailer,
            _ => CpioEntryKind::Other,
        };
        let content_hint = if kind == CpioEntryKind::File {
            Some(guess_content(&name, body.unwrap_or(&[])))
        } else {
            None
        };
        let is_trailer = name == "TRAILER!!!";
        entries.push(CpioEntry {
            name,
            kind: if is_trailer {
                CpioEntryKind::Trailer
            } else {
                kind
            },
            mode,
            size: data_len,
            content_hint,
        });
        if is_trailer {
            break;
        }
        off = data_start + align4(data_len);
    }
    entries
}

/// Coarse content type for a regular file, from its name and leading bytes.
/// Deliberately small: it exists to label the handover view, not to sniff
/// arbitrary formats.
fn guess_content(name: &str, body: &[u8]) -> &'static str {
    if name == "nmbl-log/nmbl.log" || name.ends_with("/nmbl.log") {
        return "nmbl log transcript";
    }
    if name.starts_with("etc/nmbl-luks/") || name.contains("nmbl-luks") {
        return "LUKS keyfile (secret)";
    }
    if body.is_empty() {
        return "empty";
    }
    if body
        .iter()
        .all(|&b| b == b'\n' || b == b'\t' || (0x20..=0x7e).contains(&b))
    {
        "text"
    } else {
        "binary"
    }
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
    use crate::sys::cpio::{InjectionEntry, build_fragment};
    use std::path::PathBuf;

    #[test]
    fn round_trips_a_keyfile_plus_log_fragment() {
        // Build the exact fragment shape the kexec path builds: a LUKS keyfile
        // and the log transcript, then decode it back and assert names, sizes,
        // and content hints.
        let key_path = PathBuf::from("/etc/nmbl-luks/cryptroot");
        let log_path = PathBuf::from("/nmbl-log/nmbl.log");
        let key = b"super-secret-volume-key";
        let log = b"[nmbl] phase 1\n[nmbl] kexec\n";
        let entries = vec![
            InjectionEntry {
                path: key_path.as_path(),
                content: key,
            },
            InjectionEntry {
                path: log_path.as_path(),
                content: log,
            },
        ];
        let fragment = build_fragment(&entries);

        let decoded = decode_cpio_fragment(&fragment);
        // Directories, the two files, and the trailer are all present.
        let files: Vec<_> = decoded
            .iter()
            .filter(|e| e.kind == CpioEntryKind::File)
            .collect();
        assert_eq!(files.len(), 2, "two files must decode");

        let keyf = files
            .iter()
            .find(|e| e.name == "etc/nmbl-luks/cryptroot")
            .expect("keyfile entry");
        assert_eq!(keyf.size, key.len());
        assert_eq!(keyf.content_hint, Some("LUKS keyfile (secret)"));

        let logf = files
            .iter()
            .find(|e| e.name == "nmbl-log/nmbl.log")
            .expect("log entry");
        assert_eq!(logf.size, log.len());
        assert_eq!(logf.content_hint, Some("nmbl log transcript"));

        // The archive ends with the trailer.
        assert_eq!(decoded.last().map(|e| e.kind), Some(CpioEntryKind::Trailer));
    }

    #[test]
    fn parent_directories_are_reported() {
        let key_path = PathBuf::from("/etc/nmbl-luks/root");
        let entries = vec![InjectionEntry {
            path: key_path.as_path(),
            content: b"k",
        }];
        let fragment = build_fragment(&entries);
        let decoded = decode_cpio_fragment(&fragment);
        let dirs: Vec<_> = decoded
            .iter()
            .filter(|e| e.kind == CpioEntryKind::Directory)
            .map(|e| e.name.as_str())
            .collect();
        assert!(dirs.contains(&"etc"), "etc dir present: {dirs:?}");
        assert!(
            dirs.contains(&"etc/nmbl-luks"),
            "nested dir present: {dirs:?}"
        );
    }

    #[test]
    fn garbage_input_returns_empty_not_panic() {
        assert!(decode_cpio_fragment(b"not a cpio archive").is_empty());
        assert!(decode_cpio_fragment(&[]).is_empty());
    }

    #[test]
    fn truncated_after_header_stops_gracefully() {
        let key_path = PathBuf::from("/k");
        let entries = vec![InjectionEntry {
            path: key_path.as_path(),
            content: b"value",
        }];
        let fragment = build_fragment(&entries);
        // Cut the buffer mid-body: the walk must stop without panicking.
        let decoded = decode_cpio_fragment(&fragment[..fragment.len() - 3]);
        // It may decode fewer entries, but never panics and never over-reads.
        assert!(decoded.len() <= 2);
    }
}
