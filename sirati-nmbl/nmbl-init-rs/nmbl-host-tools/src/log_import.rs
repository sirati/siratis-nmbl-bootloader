//! `nmbl-log-import`: replay NMBL's pre-kexec transcript into the journal.
//!
//! The transcript (`/nmbl-log/nmbl.log`, spliced into the booted initramfs by
//! NMBL) carries kernel and device strings, so it is treated as untrusted
//! bytes. It is read with a hard size bound, split on `\n`, and every line is
//! rendered printable before it leaves this process: invalid UTF-8, NULs,
//! control characters, line/paragraph separators and bidi controls are
//! escaped, a literal backslash becomes `\\`, and each entry is capped at
//! [`MAX_LINE`] bytes. Entries go to journald over its native socket under
//! `SYSLOG_IDENTIFIER=nmbl-init`, falling back to `/dev/kmsg` per line. A
//! failed import is reported and still exits successfully: the transcript is
//! diagnostics and must never fail the boot.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};

/// Journal identifier every entry is tagged with.
pub const IDENTIFIER: &str = "nmbl-init";
/// journald's native protocol socket.
pub const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";
/// Most bytes read from the transcript. NMBL's ring holds 1 MiB plus a
/// one-line header, so a genuine transcript always fits; anything larger
/// keeps its newest bytes and gets a truncation header.
pub const MAX_TOTAL: u64 = 2 * 1024 * 1024;
/// Most bytes of one rendered entry, including the truncation marker.
pub const MAX_LINE: usize = 4096;
/// Most entries emitted from one transcript.
pub const MAX_LINES: usize = 50_000;

/// The bounded tail of a transcript.
#[derive(Debug, PartialEq, Eq)]
pub struct Bounded {
    /// Bytes skipped at the front (size bound plus the partial first line).
    pub dropped: u64,
    /// The bytes kept, starting at a line boundary when anything was dropped.
    pub body: Vec<u8>,
}

/// Read at most `max` bytes from the end of `src`. When the source is
/// larger, the partial line at the cut is dropped too, so the first kept
/// byte starts a line.
pub fn read_bounded<R: Read + Seek>(src: &mut R, max: u64) -> io::Result<Bounded> {
    let len = src.seek(SeekFrom::End(0))?;
    // One byte before the cut, so a cut on a line boundary loses no line.
    let start = len.saturating_sub(max).saturating_sub(1);
    src.seek(SeekFrom::Start(start))?;
    let mut body = Vec::new();
    src.take(len - start).read_to_end(&mut body)?;
    let mut dropped = start;
    if len > max {
        // Through the first newline; with none, just the extra lead byte.
        let cut = body
            .iter()
            .position(|&b| b == b'\n')
            .map_or(1, |nl| nl + 1)
            .min(body.len());
        body.drain(..cut);
        dropped += cut as u64;
    }
    Ok(Bounded { dropped, body })
}

/// Same wording as NMBL's own ring-overflow header, so readers handle both
/// the same way.
fn truncation_header(dropped: u64) -> String {
    format!("=== {IDENTIFIER}: log truncated, earlier {dropped} bytes dropped ===")
}

fn must_escape(c: char) -> bool {
    c != '\t'
        && (c.is_control()
            || matches!(c,
                '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'))
}

/// Render one raw line as a single printable entry of at most `max` bytes.
pub fn render_line(raw: &[u8], max: usize) -> String {
    let (out, consumed) = escape(raw, max);
    if consumed == raw.len() {
        return out;
    }
    // Sized for the whole line, so it covers whatever count remains.
    let budget = max.saturating_sub(line_marker(raw.len()).len());
    let (mut out, consumed) = escape(raw, budget);
    out.push_str(&line_marker(raw.len() - consumed));
    out
}

/// Escape a prefix of `raw` into at most `budget` bytes; returns it and how
/// many input bytes it covers.
fn escape(raw: &[u8], budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut consumed = 0usize;
    let mut piece = String::new();
    for chunk in raw.utf8_chunks() {
        let valid = chunk.valid().chars().map(Ok);
        let invalid = chunk.invalid().iter().map(|&b| Err(b));
        for unit in valid.chain(invalid) {
            piece.clear();
            let width = match unit {
                Ok('\\') => {
                    piece.push_str("\\\\");
                    1
                }
                Ok(c) if must_escape(c) => {
                    let code = u32::from(c);
                    let _ = if code < 0x80 {
                        write!(piece, "\\x{code:02x}")
                    } else {
                        write!(piece, "\\u{{{code:x}}}")
                    };
                    c.len_utf8()
                }
                Ok(c) => {
                    piece.push(c);
                    c.len_utf8()
                }
                Err(b) => {
                    let _ = write!(piece, "\\x{b:02x}");
                    1
                }
            };
            if out.len() + piece.len() > budget {
                return (out, consumed);
            }
            out.push_str(&piece);
            consumed += width;
        }
    }
    (out, consumed)
}

fn line_marker(rest: usize) -> String {
    format!(" [+{rest} bytes truncated]")
}

/// Turn a bounded transcript into journal entries.
pub fn entries(src: &Bounded, max_line: usize, max_lines: usize) -> Vec<String> {
    let mut out = Vec::new();
    if src.dropped > 0 {
        out.push(truncation_header(src.dropped));
    }
    let mut lines = src.body.split(|&b| b == b'\n').filter(|l| !l.is_empty());
    for line in lines.by_ref().take(max_lines) {
        out.push(render_line(line, max_line));
    }
    let left = lines.count();
    if left > 0 {
        out.push(format!(
            "=== {IDENTIFIER}: log truncated, later {left} lines dropped ==="
        ));
    }
    out
}

/// One native-protocol datagram. `message` must be a [`render_line`] result,
/// which never contains a newline.
pub fn journal_datagram(message: &str) -> Vec<u8> {
    format!("SYSLOG_IDENTIFIER={IDENTIFIER}\nPRIORITY=6\nMESSAGE={message}\n").into_bytes()
}

/// Where entries go: journald first, `/dev/kmsg` when the journal refuses.
pub struct Sink {
    socket: Option<UnixDatagram>,
    path: PathBuf,
    kmsg: Option<File>,
}

impl Sink {
    pub fn new(path: PathBuf, kmsg: Option<File>) -> Self {
        Self {
            socket: UnixDatagram::unbound().ok(),
            path,
            kmsg,
        }
    }

    /// Deliver one entry. Errors only when neither channel took it.
    pub fn send(&mut self, message: &str) -> io::Result<()> {
        let journal = match &self.socket {
            Some(s) => s
                .send_to(&journal_datagram(message), &self.path)
                .map(|_| ()),
            None => Err(io::Error::other("no journal socket")),
        };
        match (journal, &mut self.kmsg) {
            (Ok(()), _) => Ok(()),
            (Err(_), Some(kmsg)) => {
                kmsg.write_all(format!("<6>{IDENTIFIER}: {}\n", fit(message, KMSG_LINE)).as_bytes())
            }
            (Err(e), None) => Err(e),
        }
    }

    /// Report the importer's own failure on stderr and, when available, as a
    /// kernel warning, which reaches the console even without a journal.
    pub fn report(&mut self, message: &str) {
        eprintln!("{message}");
        if let Some(kmsg) = &mut self.kmsg {
            let _ = kmsg.write_all(format!("<4>{}\n", fit(message, KMSG_LINE)).as_bytes());
        }
    }
}

/// Room for one `/dev/kmsg` record: the kernel refuses writes over 1024 bytes.
const KMSG_LINE: usize = 960;

/// Cut an already printable entry to at most `max` bytes on a character
/// boundary, marking what was dropped.
fn fit(message: &str, max: usize) -> String {
    if message.len() <= max {
        return message.to_owned();
    }
    let room = max.saturating_sub(line_marker(message.len()).len());
    let mut out = String::new();
    for c in message.chars() {
        if out.len() + c.len_utf8() > room {
            break;
        }
        out.push(c);
    }
    let rest = message.len() - out.len();
    out.push_str(&line_marker(rest));
    out
}

/// Open the transcript without following symlinks or blocking on a FIFO, and
/// refuse anything that is not a regular file. `None` when it is absent.
fn open_source(path: &Path) -> io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) if f.metadata()?.is_file() => Ok(Some(f)),
        Ok(_) => Err(io::Error::other("not a regular file")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Import `src` through `sink`, then delete it (and its directory, if empty)
/// once every entry was delivered. A missing file is not an error.
pub fn import(src: &Path, sink: &mut Sink) -> Result<usize, String> {
    let Some(mut file) = open_source(src).map_err(|e| format!("open {}: {e}", src.display()))?
    else {
        return Ok(0);
    };
    let bounded =
        read_bounded(&mut file, MAX_TOTAL).map_err(|e| format!("read {}: {e}", src.display()))?;
    let entries = entries(&bounded, MAX_LINE, MAX_LINES);
    let failed = entries.iter().filter(|m| sink.send(m).is_err()).count();
    if failed > 0 {
        return Err(format!(
            "{failed} of {} entries could not be logged; keeping {}",
            entries.len(),
            src.display()
        ));
    }
    std::fs::remove_file(src).map_err(|e| format!("remove {}: {e}", src.display()))?;
    if let Some(dir) = src.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    Ok(entries.len())
}

/// `nmbl-log-import [--socket PATH] SRC`.
pub fn main_with(args: Vec<OsString>) -> Result<(), String> {
    const USAGE: &str = "usage: nmbl-log-import [--socket PATH] SRC";
    let (socket, src) = match <[OsString; 3]>::try_from(args) {
        Ok([flag, path, src]) if flag == "--socket" => (PathBuf::from(path), src),
        Ok(_) => return Err(USAGE.into()),
        Err(args) => match <[OsString; 1]>::try_from(args) {
            Ok([src]) => (PathBuf::from(JOURNAL_SOCKET), src),
            Err(_) => return Err(USAGE.into()),
        },
    };
    let kmsg = OpenOptions::new().write(true).open("/dev/kmsg").ok();
    let mut sink = Sink::new(socket, kmsg);
    if let Err(error) = import(Path::new(&src), &mut sink) {
        // The transcript is diagnostics only; losing it must never fail the
        // boot (a failed unit also withholds NMBL's boot blessing).
        sink.report(&format!("nmbl-log-import: {error}"));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, reason = "tests assert")]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn bounded(body: &[u8]) -> Bounded {
        Bounded {
            dropped: 0,
            body: body.to_vec(),
        }
    }

    fn printable(s: &str) -> bool {
        !s.chars().any(must_escape)
    }

    #[test]
    fn plain_lines_pass_through() {
        let e = entries(
            &bounded(b"phase 1: mount\nphase 2: probe\n"),
            MAX_LINE,
            MAX_LINES,
        );
        assert_eq!(e, ["phase 1: mount", "phase 2: probe"]);
    }

    #[test]
    fn last_line_without_newline_is_kept_and_blank_lines_dropped() {
        let e = entries(&bounded(b"\n\na\n\nb"), MAX_LINE, MAX_LINES);
        assert_eq!(e, ["a", "b"]);
    }

    #[test]
    fn nmbl_truncation_header_passes_through() {
        let text = b"=== nmbl-init: log truncated, earlier 4096 bytes dropped ===\nphase 1\n";
        let e = entries(&bounded(text), MAX_LINE, MAX_LINES);
        assert_eq!(
            e[0],
            "=== nmbl-init: log truncated, earlier 4096 bytes dropped ==="
        );
        assert_eq!(e[1], "phase 1");
    }

    #[test]
    fn hostile_bytes_are_escaped() {
        let raw = b"a\0b\x1b[2Jc\rd\x7fe\xff\xfe\\x41\tt";
        let got = render_line(raw, MAX_LINE);
        assert_eq!(got, "a\\x00b\\x1b[2Jc\\x0dd\\x7fe\\xff\\xfe\\\\x41\tt");
    }

    #[test]
    fn unicode_controls_are_escaped_and_text_kept() {
        let raw = "ü\u{85}x\u{2028}y\u{202e}z\u{feff}é".as_bytes();
        assert_eq!(
            render_line(raw, MAX_LINE),
            "ü\\u{85}x\\u{2028}y\\u{202e}z\\u{feff}é"
        );
    }

    #[test]
    fn truncated_utf8_sequence_is_escaped() {
        assert_eq!(render_line(b"ok\xe2\x82", MAX_LINE), "ok\\xe2\\x82");
    }

    #[test]
    fn journal_fields_cannot_be_injected() {
        let e = entries(
            &bounded(b"x\rPRIORITY=0\x00MESSAGE=y\n"),
            MAX_LINE,
            MAX_LINES,
        );
        assert_eq!(e.len(), 1);
        let dgram = String::from_utf8(journal_datagram(&e[0])).unwrap();
        assert_eq!(dgram.lines().count(), 3);
        assert!(dgram.lines().all(|l| !l.starts_with("PRIORITY=0")));
    }

    #[test]
    fn long_lines_are_capped_with_a_marker() {
        for max in [64, 100, 4096] {
            for raw in [
                vec![b'a'; 10_000],
                vec![0u8; 10_000],
                "é".repeat(5000).into_bytes(),
            ] {
                let got = render_line(&raw, max);
                assert!(got.len() <= max, "{} > {max}", got.len());
                assert!(got.ends_with("bytes truncated]"), "{got}");
            }
        }
    }

    #[test]
    fn marker_counts_the_dropped_input() {
        let got = render_line(&[b'a'; 1000], 64);
        let kept = got.find(' ').unwrap();
        assert_eq!(
            got,
            format!("{}{}", "a".repeat(kept), line_marker(1000 - kept))
        );
    }

    #[test]
    fn line_exactly_at_the_cap_is_not_marked() {
        assert_eq!(render_line(&[b'a'; 64], 64), "a".repeat(64));
    }

    #[test]
    fn every_byte_value_renders_printable() {
        let raw: Vec<u8> = (0..=255u8).collect();
        let got = render_line(&raw, MAX_LINE);
        assert!(printable(&got));
        assert!(!got.contains('\n'));
    }

    #[test]
    fn line_count_is_bounded() {
        let e = entries(&bounded(&b"x\n".repeat(10)), MAX_LINE, 3);
        assert_eq!(
            e,
            [
                "x",
                "x",
                "x",
                "=== nmbl-init: log truncated, later 7 lines dropped ==="
            ]
        );
    }

    #[test]
    fn small_source_is_read_whole() {
        let b = read_bounded(&mut Cursor::new(b"a\nb\n".to_vec()), 16).unwrap();
        assert_eq!(
            b,
            Bounded {
                dropped: 0,
                body: b"a\nb\n".to_vec()
            }
        );
    }

    #[test]
    fn oversized_source_keeps_the_tail_from_a_line_start() {
        let b = read_bounded(&mut Cursor::new(b"first\nsecond\nthird\n".to_vec()), 9).unwrap();
        assert_eq!(
            b,
            Bounded {
                dropped: 13,
                body: b"third\n".to_vec()
            }
        );
        let e = entries(&b, MAX_LINE, MAX_LINES);
        assert_eq!(
            e,
            [
                "=== nmbl-init: log truncated, earlier 13 bytes dropped ===",
                "third"
            ]
        );
    }

    #[test]
    fn cut_on_a_line_boundary_keeps_that_line() {
        let b = read_bounded(&mut Cursor::new(b"aaaa\nbbbb\n".to_vec()), 5).unwrap();
        assert_eq!(
            b,
            Bounded {
                dropped: 5,
                body: b"bbbb\n".to_vec()
            }
        );
    }

    #[test]
    fn oversized_source_without_newline_keeps_the_tail() {
        let b = read_bounded(&mut Cursor::new(vec![b'z'; 100]), 10).unwrap();
        assert_eq!(
            b,
            Bounded {
                dropped: 90,
                body: vec![b'z'; 10]
            }
        );
    }

    fn import_with_socket(
        content: &[u8],
    ) -> (Result<usize, String>, Vec<String>, tempfile::TempDir) {
        import_at(|dir| {
            let src = dir.join("nmbl-log/nmbl.log");
            std::fs::create_dir(dir.join("nmbl-log")).unwrap();
            std::fs::write(&src, content).unwrap();
            src
        })
    }

    /// Import the path `prepare` creates in a scratch dir into a live socket.
    fn import_at(
        prepare: impl FnOnce(&Path) -> PathBuf,
    ) -> (Result<usize, String>, Vec<String>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("journal");
        let server = UnixDatagram::bind(&sock).unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        // Drain concurrently: a full datagram queue blocks the sender.
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = server.recv(&mut buf) {
                got.push(String::from_utf8(buf[..n].to_vec()).unwrap());
            }
            got
        });
        let src = prepare(dir.path());
        let res = import(&src, &mut Sink::new(sock, None));
        (res, reader.join().unwrap(), dir)
    }

    #[test]
    fn import_sends_tagged_datagrams_and_removes_the_file() {
        let mut content = b"phase 1\n\0\x1b]0;pwn\x07\n".to_vec();
        content.extend(vec![b'q'; 100_000]);
        let (res, got, dir) = import_with_socket(&content);
        assert_eq!(res, Ok(3));
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0],
            "SYSLOG_IDENTIFIER=nmbl-init\nPRIORITY=6\nMESSAGE=phase 1\n"
        );
        assert_eq!(
            got[1],
            "SYSLOG_IDENTIFIER=nmbl-init\nPRIORITY=6\nMESSAGE=\\x00\\x1b]0;pwn\\x07\n"
        );
        assert!(got[2].len() < MAX_LINE + 64);
        assert!(!dir.path().join("nmbl-log").exists());
    }

    #[test]
    fn oversized_import_starts_with_the_truncation_header() {
        // 4096 lines of 512 bytes: 2 MiB plus the header line.
        let mut content = b"=== nmbl-init: log truncated, earlier 7 bytes dropped ===\n".to_vec();
        let header = content.len();
        let line = [b"x".repeat(511), b"\n".to_vec()].concat();
        content.extend(line.repeat(4096));
        let (res, got, _dir) = import_with_socket(&content);
        assert_eq!(res, Ok(4097));
        let first = format!("{}\n", truncation_header(header as u64));
        assert!(got[0].ends_with(&first), "{}", got[0]);
        assert!(got[1].ends_with(&format!("MESSAGE={}\n", "x".repeat(511))));
        assert_eq!(got.len(), 4097);
    }

    #[test]
    fn missing_source_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = Sink::new(dir.path().join("none"), None);
        assert_eq!(import(&dir.path().join("absent"), &mut sink), Ok(0));
    }

    #[test]
    fn undeliverable_import_keeps_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("nmbl.log");
        std::fs::write(&src, b"phase 1\n").unwrap();
        let mut sink = Sink::new(dir.path().join("no-journal"), None);
        assert!(import(&src, &mut sink).is_err());
        assert!(src.exists());
    }

    #[test]
    fn symlinks_are_refused() {
        let (res, got, dir) = import_at(|dir| {
            std::fs::write(dir.join("secret"), b"secret\n").unwrap();
            let link = dir.join("nmbl.log");
            std::os::unix::fs::symlink(dir.join("secret"), &link).unwrap();
            link
        });
        assert!(res.is_err());
        assert!(got.is_empty());
        assert!(dir.path().join("secret").exists());
    }

    #[test]
    fn kmsg_records_fit_the_kernel_limit() {
        assert_eq!(fit("short", KMSG_LINE), "short");
        let got = fit(&"é".repeat(3000), KMSG_LINE);
        assert!(got.len() <= KMSG_LINE, "{}", got.len());
        assert!(got.ends_with(" bytes truncated]"), "{got}");
    }

    #[test]
    fn failed_import_reports_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("nmbl.log");
        std::fs::write(&src, b"phase 1\n").unwrap();
        let args = vec![
            OsString::from("--socket"),
            dir.path().join("no-journal").into_os_string(),
            src.clone().into_os_string(),
        ];
        assert_eq!(main_with(args), Ok(()));
        assert!(src.exists());
    }

    #[test]
    fn fifos_are_refused_without_blocking() {
        let (res, got, _dir) = import_at(|dir| {
            let fifo = dir.join("fifo");
            let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: `c` is a valid NUL-terminated path.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            fifo
        });
        assert!(res.is_err());
        assert!(got.is_empty());
    }
}
