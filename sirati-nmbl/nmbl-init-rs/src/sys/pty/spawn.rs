//! [`spawn_shell`]: fork a child shell on a PTY pair.

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use nix::pty::{OpenptyResult, Winsize, openpty};
use nix::unistd::{ForkResult, fork, setsid};
use rustix::fs::OFlags as RustixOFlags;
use rustix::fs::fcntl_setfl;

use super::child::PtyChild;
use super::{
    EXEC_FAILED_EXIT_CODE, ensure_devpts_mounted, preflight_shell, prepare_shell_cstrings,
};
use crate::error::{NmblError, Result};

/// Open a PTY pair sized `cols`×`rows`, set the master non-blocking,
/// and return `(master, slave, master_raw_fd, slave_raw_fd)`.
/// All raw fds are pre-extracted so the post-fork child path can use
/// them without going through `OwnedFd` in async-signal-safe territory.
fn open_pty_pair(cols: u16, rows: u16) -> Result<(OwnedFd, OwnedFd, i32, i32)> {
    let winsize = Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };

    let OpenptyResult { master, slave } =
        openpty(Some(&winsize), None).map_err(|e| NmblError::Tui {
            source: std::io::Error::other(format!("openpty failed: {e}")),
        })?;

    // Make the master non-blocking so the driver loop can poll without
    // a worker thread. The slave keeps blocking semantics; the kernel
    // delivers SIGHUP / EIO to it when the master is closed, which is
    // the orderly shutdown signal the shell expects.
    fcntl_setfl(&master, RustixOFlags::NONBLOCK).map_err(|e| NmblError::Tui {
        source: std::io::Error::other(format!("fcntl(F_SETFL, O_NONBLOCK) on PTY master: {e}")),
    })?;

    let master_raw = master.as_raw_fd();
    let slave_raw = slave.as_raw_fd();
    Ok((master, slave, master_raw, slave_raw))
}

/// Post-fork child path for `spawn_shell`. Restricted to async-signal-safe
/// primitives. Wires the slave PTY onto stdio and `execve`s the shell.
///
/// # Safety
/// Must only be called from the child branch of `fork()`. No allocation,
/// no Rust I/O, no destructors. All `CString` args were allocated in the
/// parent before `fork`.
unsafe fn child_exec_on_pty(
    master_raw: i32,
    slave_raw: i32,
    path_c: &CString,
    argv0_c: &CString,
    env_term: &CString,
    env_path: &CString,
) -> ! {
    // Close the master in the child; only the parent uses it.
    // SAFETY: `libc::close` is async-signal-safe. The master fd
    // came from openpty(3) so the kernel owns the close.
    let _ = unsafe { libc::close(master_raw) };

    // Detach from the parent's session and create a new one so
    // the slave PTY can become our controlling terminal. setsid()
    // is async-signal-safe.
    let _ = setsid();

    // Claim the slave as our controlling tty.
    // SAFETY: `libc::ioctl` is async-signal-safe. TIOCSCTTY with
    // arg 0 attaches the slave fd as the session's controlling
    // terminal; the kernel validates that we are the session
    // leader (guaranteed by the setsid() above) and that the
    // tty has no other controlling session.
    let _ = unsafe { libc::ioctl(slave_raw, libc::TIOCSCTTY as _, 0) };

    // dup2 the slave onto fds 0/1/2 so the shell's stdio runs
    // through the PTY. dup2 is async-signal-safe.
    // SAFETY: same contract as above; libc::dup2 atomically
    // replaces the target fd.
    for target in [0, 1, 2] {
        if unsafe { libc::dup2(slave_raw, target) } < 0 {
            // SAFETY: post-fork child; _exit is the only correct
            // termination primitive (async-signal-safe, no
            // destructors). Project memory documents this
            // exception to the no-unsafe rule.
            unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
        }
    }
    // Close the original slave fd if it wasn't 0/1/2.
    if slave_raw > 2 {
        // SAFETY: close on a valid fd; async-signal-safe.
        let _ = unsafe { libc::close(slave_raw) };
    }

    // Build a tiny argv/env on the stack-allocated pointers. The
    // CStrings themselves were allocated in the parent and are
    // still mapped (fork() copies them via COW).
    let argv: [*const libc::c_char; 2] = [argv0_c.as_ptr(), std::ptr::null()];
    let envp: [*const libc::c_char; 3] = [env_term.as_ptr(), env_path.as_ptr(), std::ptr::null()];

    // SAFETY: libc::execve is async-signal-safe. On success it
    // does not return; on failure errno is set and we _exit
    // with the conventional 127 (command-not-found) code.
    // execve safety: we are a forked child process, not PID 1; our job is to replace ourselves with the requested shell.
    let _ = unsafe { libc::execve(path_c.as_ptr(), argv.as_ptr(), envp.as_ptr()) };

    // SAFETY: Unavoidable. Post-fork child must use _exit; no
    // crate wraps it (rustix #844). Same exception as
    // `src/sys/activation.rs` and `src/rescue/mod.rs`.
    unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) }
}

/// Allocate a PTY pair and `fork(2)` a child running `shell_path` with
/// the slave as its controlling terminal. The parent receives the
/// master fd (non-blocking) and the child Pid.
///
/// `cols`/`rows` size the slave PTY's window so `$LINES`/`$COLUMNS`
/// (and TIOCGWINSZ) report the dimensions of the bordered box we
/// render the terminal into. The child inherits the parent's environment
/// minus a minimal `TERM=xterm-256color` injection so curses-style
/// applications work.
pub fn spawn_shell(shell_path: &Path, cols: u16, rows: u16) -> Result<PtyChild> {
    // Fail loudly up-front if the shell binary is missing/non-exec so
    // the operator gets a real error instead of a shell that silently
    // dies in the post-fork child (the "Raw Shell does nothing" bug).
    preflight_shell(shell_path)?;

    // `nmbl-init`'s phase 1 doesn't mount `/dev/pts`; openpty(3) reads
    // `/dev/ptmx` and writes the PTY name back to `/dev/pts/N`, so we
    // mount devpts on demand before the first PTY allocation.
    ensure_devpts_mounted()?;

    // === Parent-side allocation: ALL CString / Vec construction MUST
    // happen here, before fork(2). The post-fork child path is restricted
    // to async-signal-safe operations. ===
    let (path_c, argv0_c, env_term, env_path) = prepare_shell_cstrings(shell_path)?;
    let (master, slave, master_raw, slave_raw) = open_pty_pair(cols, rows)?;

    // SAFETY: `nix::unistd::fork` is `unsafe` by design — there is no
    // safe wrapper in any Rust crate. The child branch below is
    // restricted to async-signal-safe primitives (`setsid`, `ioctl`,
    // `dup2`, `close`, `execve`, `_exit`); no allocation, no Rust I/O,
    // no destructors that touch shared state. All CString and Vec
    // allocations happened in the parent above. This pattern mirrors
    // `src/sys/activation.rs` and is one of the documented exceptions
    // to the project's "minimize unsafe" rule.
    let fork_result = unsafe { fork() }.map_err(|e| NmblError::Tui {
        source: std::io::Error::other(format!("fork() for pretty-shell: {e}")),
    })?;

    match fork_result {
        ForkResult::Parent { child } => {
            // Close the slave fd in the parent; only the child uses it.
            // The master OwnedFd stays alive in the returned PtyChild.
            drop(slave);
            Ok(PtyChild { master, pid: child })
        }
        ForkResult::Child => {
            // === CHILD ===
            // From this point until execve/_exit we MUST stay within
            // async-signal-safe territory: no allocation, no Rust I/O.
            // SAFETY: we are in the child branch of fork(); child_exec_on_pty
            // is restricted to async-signal-safe calls and does not return.
            unsafe {
                child_exec_on_pty(
                    master_raw, slave_raw, &path_c, &argv0_c, &env_term, &env_path,
                )
            }
        }
    }
}
