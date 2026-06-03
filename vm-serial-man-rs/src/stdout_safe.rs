//! EPIPE-safe stdout helpers shared by the daemon and the clients.
//!
//! The stdlib `println!`/`print!` macros call `Stdout::write_fmt` and **panic
//! the whole process** on any write error — most importantly a `BrokenPipe`
//! (EPIPE) once the downstream reader has gone away. In this tool stdout is
//! frequently a fragile pipe:
//!
//!  * the daemon's stdout is the detached `screen` session's pty, which can
//!    break under the high-volume full-screen TUI repaint stream NMBL emits
//!    while it holds the console (the LUKS modal), and
//!  * a client (`find`/`tail`/`send`/`trigger`) is routinely piped into
//!    `grep -q`, which closes the pipe the instant it matches — every
//!    subsequent `println!` in the client would then abort.
//!
//! A panic on either side is fatal to the test: a dead daemon leaves a stale
//! socket + orphaned QEMU, and a dead client aborts the assertion. These
//! helpers write directly and SWALLOW `BrokenPipe`, so a vanished reader simply
//! means "keep running headless". Other I/O errors are reported once and never
//! fatal.

use std::io::{ErrorKind, Write};

/// Write `s` followed by a newline to stdout, never panicking. EPIPE-safe
/// drop-in for `println!("{}", s)`.
pub fn write_stdout_line(s: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    swallow_broken_pipe(
        lock.write_all(s.as_bytes())
            .and_then(|()| lock.write_all(b"\n")),
    );
}

/// Write a bare newline to stdout, never panicking. EPIPE-safe drop-in for
/// `println!()`.
pub fn write_stdout_newline() {
    write_stdout_line("");
}

/// Collapse an stdout write result: `BrokenPipe` is fine (reader gone), other
/// errors are logged once but never fatal.
fn swallow_broken_pipe(result: std::io::Result<()>) {
    match result {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::BrokenPipe => {
            // Downstream reader closed; keep running headless.
        }
        Err(e) => {
            // Don't recurse through stdout — report on stderr, best-effort.
            let _ = writeln!(std::io::stderr(), "stdout write error: {e}");
        }
    }
}
