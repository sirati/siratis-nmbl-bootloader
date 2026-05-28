//! Capture the current QEMU framebuffer via the monitor socket.
//!
//! Connects to QEMU's HMP (human monitor protocol) Unix socket, waits for the
//! `(qemu) ` prompt, drives a `screendump <path>` command, then waits for the
//! prompt again before returning. The dump is PPM. The HMP `screendump`
//! command only accepts positional `<filename> [device [head]]` arguments
//! (the `path=`/`format=` keyword form belongs to QMP, the JSON protocol). The
//! positional form has been stable since QEMU 2.x, so no version dance is
//! needed.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use thiserror::Error;

/// How long to wait for a single read from the monitor socket before giving up.
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The HMP prompt QEMU emits after every successful command.
const PROMPT: &[u8] = b"(qemu) ";

/// Marker tokens HMP uses to flag command failures. The HMP `screendump`
/// command emits `Error: ...` on a line of its own when something goes wrong
/// (bad path, unknown device, etc.); we treat any line starting with one of
/// these as a failure.
const ERROR_MARKERS: &[&str] = &["Error:", "error:"];

/// Errors that can happen while talking to the QEMU monitor.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// Failed to connect to the monitor socket.
    #[error("failed to connect to QEMU monitor socket: {0}")]
    Connect(#[source] io::Error),

    /// Generic I/O failure while reading from or writing to the socket.
    #[error("I/O error talking to QEMU monitor: {0}")]
    Io(#[from] io::Error),

    /// Read timed out before the expected prompt arrived.
    #[error("timed out waiting for QEMU monitor prompt")]
    MonitorTimeout,

    /// The monitor closed the connection without replying.
    #[error("QEMU monitor returned an empty response")]
    EmptyResponse,

    /// The monitor replied with an error line to our `screendump` command.
    #[error("QEMU monitor rejected screendump command: {0}")]
    CommandRejected(String),
}

/// Capture the current framebuffer to `dst` (PPM format).
///
/// This is a blocking call that connects to the monitor Unix socket, runs the
/// `screendump` command, and returns once QEMU has written the file and
/// emitted a fresh prompt.
pub fn capture(monitor_socket: &Path, dst: &Path) -> Result<(), CaptureError> {
    let stream = UnixStream::connect(monitor_socket).map_err(CaptureError::Connect)?;
    stream.set_read_timeout(Some(DEFAULT_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(DEFAULT_READ_TIMEOUT))?;
    run_screendump(stream, dst)
}

/// Drive `screendump` on an already-opened HMP stream. Factored out so tests
/// can pump a synthetic stream through the same protocol code.
pub(crate) fn run_screendump<S>(mut stream: S, dst: &Path) -> Result<(), CaptureError>
where
    S: Read + Write,
{
    // Drain the banner + first prompt.
    read_until_prompt(&mut stream)?;

    let dst_str = dst.to_string_lossy();
    let cmd = format!("screendump {dst_str}\n");
    stream.write_all(cmd.as_bytes())?;
    stream.flush()?;

    // QEMU echoes the command back (often with readline cursor escapes) and
    // then emits any error/output lines before the next `(qemu) ` prompt.
    // Strip the echoed command before scanning for errors so we don't
    // misinterpret echoed input as a server-side error message.
    let response = read_until_prompt(&mut stream)?;
    let trimmed = strip_command_echo(&response, cmd.trim_end());
    if response_indicates_error(&trimmed) {
        return Err(CaptureError::CommandRejected(trimmed));
    }
    Ok(())
}

/// Read bytes from `stream` until we see a `(qemu) ` prompt. Returns the
/// accumulated text (UTF-8 lossy) excluding the trailing prompt so callers
/// can inspect it for errors.
fn read_until_prompt<S: Read>(stream: &mut S) -> Result<String, CaptureError> {
    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => {
                if buf.is_empty() {
                    return Err(CaptureError::EmptyResponse);
                }
                // Treat early EOF after some data as success only if we saw
                // a prompt; otherwise this is a truncated response.
                if buf.windows(PROMPT.len()).any(|w| w == PROMPT) {
                    return Ok(trim_prompt(buf));
                }
                return Err(CaptureError::EmptyResponse);
            }
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(PROMPT.len()).any(|w| w == PROMPT) {
                    return Ok(trim_prompt(buf));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(CaptureError::MonitorTimeout);
            }
            Err(e) => return Err(CaptureError::Io(e)),
        }
    }
}

/// Strip the trailing `(qemu) ` prompt (and any leading copy of it left over
/// from the banner) so we can scan only the command output text.
fn trim_prompt(mut buf: Vec<u8>) -> String {
    // Drop the final prompt occurrence.
    if let Some(idx) = buf
        .windows(PROMPT.len())
        .rposition(|w| w == PROMPT)
    {
        buf.truncate(idx);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn response_indicates_error(response: &str) -> bool {
    ERROR_MARKERS
        .iter()
        .any(|marker| response.contains(marker))
}

/// QEMU's HMP echoes input characters back to the socket, often interleaved
/// with terminal escape codes (`\x1b[K`, `\b`, etc.). We don't want to mistake
/// the echoed command for the server's actual reply when scanning for errors,
/// so we strip ANSI/control noise and remove a single occurrence of the
/// command string we sent.
fn strip_command_echo(response: &str, cmd: &str) -> String {
    // Drop ANSI CSI sequences and bare control bytes (\x08 backspace, \x1b).
    let mut cleaned = String::with_capacity(response.len());
    let mut chars = response.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip "[...<final>" up to and including a final byte in @-~.
            if matches!(chars.peek(), Some('[')) {
                chars.next();
                for inner in chars.by_ref() {
                    if matches!(inner, '@'..='~') {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\x08' || c == '\r' {
            continue;
        }
        cleaned.push(c);
    }
    if let Some(idx) = cleaned.find(cmd) {
        let mut out = String::with_capacity(cleaned.len());
        out.push_str(&cleaned[..idx]);
        out.push_str(&cleaned[idx + cmd.len()..]);
        out
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read, Write};
    use std::path::PathBuf;

    /// Synthetic full-duplex stream: reads from a canned byte queue (one chunk
    /// per `read` call) and captures writes verbatim.
    struct FakeStream {
        reads: Vec<Vec<u8>>,
        writes: Vec<u8>,
    }

    impl FakeStream {
        fn new(reads: Vec<Vec<u8>>) -> Self {
            Self {
                reads,
                writes: Vec::new(),
            }
        }
    }

    impl Read for FakeStream {
        fn read(&mut self, dst: &mut [u8]) -> io::Result<usize> {
            if self.reads.is_empty() {
                return Ok(0);
            }
            let next = self.reads.remove(0);
            let n = next.len().min(dst.len());
            dst[..n].copy_from_slice(&next[..n]);
            // If the canned chunk was larger than dst, put the remainder back.
            if n < next.len() {
                self.reads.insert(0, next[n..].to_vec());
            }
            Ok(n)
        }
    }

    impl Write for FakeStream {
        fn write(&mut self, src: &[u8]) -> io::Result<usize> {
            self.writes.extend_from_slice(src);
            Ok(src.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sends_positional_screendump_and_succeeds() {
        // Banner + prompt, then post-command prompt with no error text.
        let chunks = vec![
            b"QEMU 9.0.0 monitor - type 'help' for more information\n(qemu) ".to_vec(),
            b"\n(qemu) ".to_vec(),
        ];
        let mut stream = FakeStream::new(chunks);
        let dst = PathBuf::from("/tmp/shot.ppm");
        run_screendump(&mut stream, &dst).expect("screendump should succeed");

        let written = String::from_utf8(stream.writes.clone()).unwrap();
        assert_eq!(
            written, "screendump /tmp/shot.ppm\n",
            "only the positional form should ever be sent"
        );
    }

    #[test]
    fn tolerates_readline_echo_with_escape_codes() {
        // Real QEMU echoes back the command interleaved with CSI sequences;
        // make sure our parser doesn't trip over them.
        let mut echoed = String::from("(qemu) ");
        for c in "screendump /tmp/shot.ppm".chars() {
            echoed.push(c);
            echoed.push_str("\x1b[K\x08");
        }
        echoed.push_str("screendump /tmp/shot.ppm\n(qemu) ");

        let chunks = vec![
            b"QEMU 10.2.2 monitor - type 'help' for more information\n(qemu) ".to_vec(),
            echoed.into_bytes(),
        ];
        let mut stream = FakeStream::new(chunks);
        let dst = PathBuf::from("/tmp/shot.ppm");
        run_screendump(&mut stream, &dst)
            .expect("noisy echo with no error line should still parse as success");
    }

    #[test]
    fn errors_when_monitor_reports_error_line() {
        let chunks = vec![
            b"QEMU 9.0.0 monitor - type 'help' for more information\n(qemu) ".to_vec(),
            b"Error: failed to open file '/no/such/dir/shot.ppm'\n(qemu) ".to_vec(),
        ];
        let mut stream = FakeStream::new(chunks);
        let dst = PathBuf::from("/no/such/dir/shot.ppm");
        let err = run_screendump(&mut stream, &dst)
            .expect_err("error line should surface as CommandRejected");
        match err {
            CaptureError::CommandRejected(msg) => {
                assert!(msg.contains("Error:"), "got {msg:?}");
            }
            other => panic!("expected CommandRejected, got {other:?}"),
        }
    }

    #[test]
    fn empty_response_is_reported() {
        let chunks: Vec<Vec<u8>> = vec![];
        let mut stream = FakeStream::new(chunks);
        let dst = PathBuf::from("/tmp/shot.ppm");
        let err = run_screendump(&mut stream, &dst)
            .expect_err("empty banner should error");
        assert!(matches!(err, CaptureError::EmptyResponse));
    }
}
