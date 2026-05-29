//! [`PtyChild`] handle and associated methods.

use std::os::fd::{BorrowedFd, OwnedFd};

use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::error::{NmblError, Result};

/// Handle to a child shell process running on a PTY pair. The parent
/// (NMBL) reads/writes the master fd; the slave fd is owned by the
/// child after `fork`.
pub struct PtyChild {
    /// Master end of the PTY pair. Non-blocking; reads return
    /// `Ok(None)` when nothing is buffered.
    pub master: OwnedFd,
    /// Pid of the child shell. Used for `waitpid(WNOHANG)` and the
    /// kill-on-drop cleanup path.
    pub pid: Pid,
}

impl PtyChild {
    /// Master fd as a `BorrowedFd` for callers that need to read/write
    /// without taking ownership.
    pub fn master_fd(&self) -> BorrowedFd<'_> {
        // Borrowing through AsFd would also work; the project pattern
        // is to expose `BorrowedFd` directly so callers can pass it to
        // rustix without an extra conversion.
        use std::os::fd::AsFd;
        self.master.as_fd()
    }

    /// Poll for child termination without blocking. Returns
    /// `Ok(Some(status))` if the child has exited, `Ok(None)` otherwise.
    pub fn try_wait(&self) -> Result<Option<WaitStatus>> {
        match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => Ok(None),
            Ok(status) => Ok(Some(status)),
            Err(e) => Err(NmblError::Tui {
                source: std::io::Error::other(format!(
                    "waitpid({}, WNOHANG) failed: {e}",
                    self.pid
                )),
            }),
        }
    }

    /// Push a new window size onto the PTY master so the slave (and thus
    /// the child shell + any full-screen program running on it) sees the
    /// new geometry and receives `SIGWINCH`.
    ///
    /// `TIOCSWINSZ` on the master propagates to the slave's terminal and
    /// raises `SIGWINCH` in the foreground process group, exactly as a
    /// real terminal emulator does when its window is resized.
    /// Best-effort: a failure here only means the child keeps the stale
    /// `$LINES`/`$COLUMNS`; the in-process grid still reflows.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let winsize = rustix::termios::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // rustix exposes the `TIOCSWINSZ` ioctl without raw `unsafe`.
        rustix::termios::tcsetwinsize(&self.master, winsize).map_err(|e| NmblError::Tui {
            source: std::io::Error::from(e),
        })
    }

    /// Tear down the child shell and reap it. Best-effort; never
    /// propagates errors so the caller's cleanup path can always proceed.
    ///
    /// An *interactive* bash IGNORES `SIGTERM`, so the previous
    /// `SIGTERM` + blocking `waitpid(self.pid, None)` deadlocked the
    /// single-threaded GUI on the `~.` quit path (shell still alive): the
    /// signal did nothing and the reap never returned. We send `SIGHUP`
    /// (the terminal-hangup signal an interactive shell honours), give it
    /// a brief grace window, then escalate to `SIGKILL` — which cannot be
    /// caught or ignored — so the final reap is guaranteed to return.
    pub fn terminate(&self) {
        let _ = kill(self.pid, Signal::SIGHUP);

        // Grace window (~200 ms) for the shell to exit on its own.
        for _ in 0..20 {
            match waitpid(self.pid, Some(WaitPidFlag::WNOHANG)) {
                // Still running: wait a beat and poll again.
                Ok(WaitStatus::StillAlive) => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                // Reaped, or an error we can't recover from — done.
                _ => return,
            }
        }

        // Still alive after the grace window: SIGKILL is uncatchable, so
        // this blocking reap is guaranteed to return promptly.
        let _ = kill(self.pid, Signal::SIGKILL);
        let _ = waitpid(self.pid, None);
    }
}
