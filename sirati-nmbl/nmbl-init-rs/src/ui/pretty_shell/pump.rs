//! PTY read pump: drain child output into the VT parser each iteration.

use std::os::fd::AsFd;

use super::state::{PTY_READ_CHUNK, PtyShellState};

/// Internal pump-error type so the driver loop can distinguish "child
/// closed the PTY" (graceful) from "kernel returned an error" (log and
/// bail).
pub(super) enum PumpError {
    Eof,
    Io(std::io::Error),
}

/// Drain at most a few non-blocking reads from the master fd into the
/// VT parser. Returns `Ok(true)` if any bytes were fed (the grid may
/// have changed); `Ok(false)` if the fd was empty this slice.
pub(super) fn pump_pty(state: &mut PtyShellState) -> std::result::Result<bool, PumpError> {
    let mut buf = [0u8; PTY_READ_CHUNK];
    let mut any = false;
    // Bound the per-iteration drain so a runaway `yes` doesn't starve
    // the input poll. Multiple loop iterations will catch up over time.
    for _ in 0..8 {
        let fd = state.child.master.as_fd();
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return Err(PumpError::Eof),
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or(&[]);
                state.parser.advance(&mut state.term, bytes);
                any = true;
            }
            Err(rustix::io::Errno::AGAIN) => break,
            Err(rustix::io::Errno::IO) => {
                // EIO on a PTY master typically means the slave hung up
                // (shell exited). Treat as orderly EOF.
                return Err(PumpError::Eof);
            }
            Err(e) => return Err(PumpError::Io(std::io::Error::from(e))),
        }
    }
    Ok(any)
}
