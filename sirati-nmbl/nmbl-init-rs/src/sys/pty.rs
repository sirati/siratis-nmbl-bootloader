//! Pseudo-terminal allocation + child-process spawn for the
//! [`crate::ui::pretty_shell`] terminal emulator.
//!
//! Differs from `src/sys/activation.rs` in that the child process is NOT
//! observed for an exit status synchronously: NMBL stays in the parent,
//! pumps bytes between the master PTY fd and an in-process terminal
//! emulator, and renders the resulting grid into the bordered TUI box.
//! When the operator's shell exits, the master fd reads return EOF and
//! the driver loop tears the child down.
//!
//! The single `unsafe` block is the post-`fork(2)` child path; it is
//! restricted to async-signal-safe calls (`setsid`, `ioctl`, `dup2`,
//! `close`, `execve`, `_exit`) per the project's "minimize unsafe"
//! discipline. All `CString`s, `OwnedFd`s, and `Vec` allocations happen
//! in the parent before the fork; nothing in the child path allocates.

use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;

use nix::errno::Errno;
use nix::mount::{MsFlags, mount};
use nix::pty::{OpenptyResult, Winsize, openpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork, setsid};
use rustix::fs::fcntl_setfl;
use rustix::fs::OFlags as RustixOFlags;

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

/// Conventional shell exit code surfaced when the post-fork `execve(2)`
/// fails. Matches the value used in `src/sys/activation.rs`.
const EXEC_FAILED_EXIT_CODE: i32 = 127;

/// Verify the shell binary actually exists and is executable *before*
/// we fork. A post-fork `execve(2)` failure can only `_exit(127)` from
/// the async-signal-safe child path — it cannot report which errno it
/// hit. That silent death is exactly the "Raw Shell does nothing" bug:
/// in external-rescue mode the initramfs ships no `/bin/sh`, so the
/// child execve'd nothing and exited instantly while the parent saw a
/// healthy fork and reported success. Checking up here turns that into
/// a descriptive `Err` the emergency UI can surface.
fn preflight_shell(shell_path: &Path) -> Result<()> {
    use rustix::fs::{Access, access};
    match access(shell_path, Access::EXEC_OK) {
        Ok(()) => Ok(()),
        Err(e) => Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "emergency shell {} is not executable: {e}; \
                 in external-rescue mode the initramfs ships no /bin/sh — \
                 set boot.nmbl.paths.shell to a binary present in the initrd",
                shell_path.display()
            )),
        }),
    }
}

/// Mount `devpts` on `/dev/pts` so `openpty(3)` can hand out slave
/// terminals via `/dev/ptmx`. Idempotent: `EBUSY` (already mounted) and
/// `ENOENT` on `/dev/pts` (directory missing → create it once) are
/// transparently handled.
///
/// `nmbl-init`'s phase 1 deliberately keeps the pseudo-fs set minimal
/// (`/proc`, `/sys`, `/dev`, `/run`, `/tmp`); devpts only matters here,
/// inside the pretty-shell session, so we mount on demand rather than
/// pay the cost on every boot.
fn ensure_devpts_mounted() -> Result<()> {
    let target = Path::new("/dev/pts");
    if let Err(e) = std::fs::create_dir_all(target) {
        // `AlreadyExists` is the happy path on a second invocation.
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(NmblError::Io {
                source: e,
                context: "creating /dev/pts mountpoint".to_string(),
            });
        }
    }
    // gid=5 mirrors the standard `tty` group on most distros. mode=620
    // matches what util-linux mounts at boot.
    let opts = "newinstance,ptmxmode=0666,mode=0620,gid=5";
    match mount(
        Some("devpts"),
        target,
        Some("devpts"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC,
        Some(opts),
    ) {
        Ok(()) => {}
        Err(Errno::EBUSY) => {
            // Already mounted; benign.
        }
        Err(e) => {
            return Err(NmblError::Mount {
                src: Some(std::path::PathBuf::from("devpts")),
                dst: target.to_path_buf(),
                fstype: "devpts".to_string(),
                source: e,
            });
        }
    }
    // A `newinstance` mount populates `/dev/pts/ptmx` but leaves
    // `/dev/ptmx` (which libc's openpty uses) untouched. Symlink it.
    // Best-effort: pre-existing `/dev/ptmx` is left alone.
    let ptmx = Path::new("/dev/ptmx");
    if let Err(e) = std::fs::symlink_metadata(ptmx)
        && e.kind() == std::io::ErrorKind::NotFound
    {
        // Create the symlink. Failure here is non-fatal — many
        // distros ship the char-device version of /dev/ptmx via
        // devtmpfs, in which case our symlink isn't needed.
        if let Err(se) = std::os::unix::fs::symlink("pts/ptmx", ptmx) {
            nmbl_warn!(
                "could not create /dev/ptmx -> pts/ptmx symlink: {se}; \
                 openpty may still work via the devtmpfs ptmx node",
            );
        }
    }
    Ok(())
}

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

    /// Send `SIGTERM` to the child and reap it. Best-effort; logs but
    /// does not propagate errors so the caller's cleanup path can always
    /// proceed.
    pub fn terminate(&self) {
        let _ = kill(self.pid, Signal::SIGTERM);
        // Drain the zombie. A second-shot SIGKILL fallback is overkill
        // for an interactive shell that just got SIGTERM; if it ignores
        // the signal the reaper at PID 1 will collect it on the next
        // boot cycle.
        let _ = waitpid(self.pid, None);
    }
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

    let path_c = CString::new(shell_path.as_os_str().as_encoded_bytes()).map_err(|_| {
        NmblError::Tui {
            source: std::io::Error::other("shell path contains interior NUL"),
        }
    })?;

    let argv0_bytes: Vec<u8> = shell_path
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| shell_path.as_os_str().as_encoded_bytes().to_vec());
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Tui {
        source: std::io::Error::other("shell argv0 contains interior NUL"),
    })?;

    // Minimal environment so curses-y programs find a sensible TERM and
    // basic PATH lookups still work when busybox sh is invoked without a
    // login profile.
    let env_term = CString::new("TERM=xterm-256color").map_err(|_| NmblError::Tui {
        source: std::io::Error::other("static TERM env contains interior NUL"),
    })?;
    let env_path = CString::new("PATH=/usr/sbin:/usr/bin:/sbin:/bin").map_err(|_| {
        NmblError::Tui {
            source: std::io::Error::other("static PATH env contains interior NUL"),
        }
    })?;

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

    // Pre-extract raw fds so the child path (which must not allocate)
    // can dup2 them without going through OwnedFd::as_raw_fd in unsafe
    // territory.
    let master_raw = master.as_raw_fd();
    let slave_raw = slave.as_raw_fd();

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
            let envp: [*const libc::c_char; 3] =
                [env_term.as_ptr(), env_path.as_ptr(), std::ptr::null()];

            // SAFETY: libc::execve is async-signal-safe. On success it
            // does not return; on failure errno is set and we _exit
            // with the conventional 127 (command-not-found) code.
            let _ = unsafe { libc::execve(path_c.as_ptr(), argv.as_ptr(), envp.as_ptr()) };

            // SAFETY: Unavoidable. Post-fork child must use _exit; no
            // crate wraps it (rustix #844). Same exception as
            // `src/sys/activation.rs` and `src/rescue/mod.rs`.
            unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
        }
    }
}

/// Handle to a fire-and-forget shell whose stdio is wired directly to
/// an operator-supplied tty (no PTY in the middle). Returned by
/// [`spawn_shell_on_tty`] so the orchestrator can wait on / reap the
/// child without holding a relay loop open.
///
/// The orchestrator drops back to the previous TUI screen immediately
/// after this returns; the shell runs to its natural conclusion on the
/// operator's chosen tty, completely independent of NMBL's event loop.
pub struct DetachedShell {
    /// Pid of the spawned child. Used for the boot-time reaper at PID 1
    /// to eventually collect; we intentionally do NOT `waitpid` in the
    /// fire-and-forget caller because that would block on a shell that
    /// could outlive the entire NMBL session.
    pub pid: Pid,
}

/// Fork a shell whose stdin/stdout/stderr are duplicated to `tty_path`
/// and return immediately. NO PTY allocation; the shell runs directly
/// on the operator-supplied char device. Used by the console-picker's
/// fire-and-forget path: when the operator selects targets that do
/// NOT include the splash's current display tty, NMBL must spawn the
/// shell on those targets and return to the previous screen, not enter
/// a relay loop on the wrong fd.
///
/// The shell becomes the session leader on `tty_path` so signals
/// generated by the keyboard (^C, ^Z) are routed correctly. The parent
/// (NMBL) keeps no fd into the line; reaping is left to the PID-1
/// init-style reaper that NMBL's main loop already runs.
pub fn spawn_shell_on_tty(shell_path: &Path, tty_path: &Path) -> Result<DetachedShell> {
    use rustix::fs::{Mode as RustixMode, OFlags as RustixOFlagsAlias};

    // Same preflight as spawn_shell: a missing /bin/sh would otherwise
    // produce a "Shell spawned" toast for a child that instantly
    // _exit(127)s, masking the real problem.
    preflight_shell(shell_path)?;

    // === Parent-side allocation: ALL CString / Vec construction MUST
    // happen here, before fork(2). The post-fork child path is restricted
    // to async-signal-safe operations (same discipline as spawn_shell). ===

    let path_c = CString::new(shell_path.as_os_str().as_encoded_bytes()).map_err(|_| {
        NmblError::Tui {
            source: std::io::Error::other("shell path contains interior NUL"),
        }
    })?;
    let argv0_bytes: Vec<u8> = shell_path
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| shell_path.as_os_str().as_encoded_bytes().to_vec());
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Tui {
        source: std::io::Error::other("shell argv0 contains interior NUL"),
    })?;
    let env_term = CString::new("TERM=xterm-256color").map_err(|_| NmblError::Tui {
        source: std::io::Error::other("static TERM env contains interior NUL"),
    })?;
    let env_path = CString::new("PATH=/usr/sbin:/usr/bin:/sbin:/bin").map_err(|_| {
        NmblError::Tui {
            source: std::io::Error::other("static PATH env contains interior NUL"),
        }
    })?;

    // Open the target tty in the PARENT so an open(2) failure surfaces
    // synchronously (not as an opaque _exit code from a forked child).
    // The fd is then inherited across fork; the child dup2's it onto
    // 0/1/2 before execve.
    let tty_fd = rustix::fs::open(
        tty_path,
        RustixOFlagsAlias::RDWR | RustixOFlagsAlias::NOCTTY,
        RustixMode::empty(),
    )
    .map_err(|e| NmblError::Tui {
        source: std::io::Error::other(format!(
            "opening shell tty {} failed: {e}",
            tty_path.display()
        )),
    })?;
    let tty_raw = tty_fd.as_raw_fd();

    // SAFETY: `nix::unistd::fork` is `unsafe` by design (no safe wrapper
    // exists). The child branch is restricted to async-signal-safe
    // primitives (`setsid`, `ioctl`, `dup2`, `close`, `execve`, `_exit`)
    // — no allocation, no Rust I/O, no destructors. All CString and
    // OwnedFd allocations are completed above in the parent.
    let fork_result = unsafe { fork() }.map_err(|e| NmblError::Tui {
        source: std::io::Error::other(format!("fork() for detached shell: {e}")),
    })?;

    match fork_result {
        ForkResult::Parent { child } => {
            // The child owns the tty fd through the dup2 chain; the
            // parent's copy is no longer needed. Dropping it here is
            // safe — the kernel reference-counts the open file.
            drop(tty_fd);
            Ok(DetachedShell { pid: child })
        }
        ForkResult::Child => {
            // === CHILD ===
            // Same async-signal-safe constraints as `spawn_shell`. We
            // must NOT panic, allocate, or run Rust destructors here.

            // Detach + own the tty as our controlling terminal. setsid
            // is async-signal-safe; failure means we'll just lack a
            // session — the shell still runs.
            let _ = setsid();
            // SAFETY: libc::ioctl is async-signal-safe; TIOCSCTTY=0
            // claims the tty as the controlling terminal for the new
            // session created by setsid above. Failure is non-fatal
            // (e.g. on a non-VT line) and the shell still runs.
            let _ = unsafe { libc::ioctl(tty_raw, libc::TIOCSCTTY as _, 0) };

            // Dup the tty onto 0/1/2 so the shell's stdio runs on the
            // operator's line.
            for target in [0, 1, 2] {
                if unsafe { libc::dup2(tty_raw, target) } < 0 {
                    // SAFETY: post-fork child; _exit is the only correct
                    // termination primitive. Same exception documented
                    // in spawn_shell above.
                    unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
                }
            }
            // Close the original tty fd if it wasn't 0/1/2.
            if tty_raw > 2 {
                // SAFETY: close on a valid fd; async-signal-safe.
                let _ = unsafe { libc::close(tty_raw) };
            }

            let argv: [*const libc::c_char; 2] = [argv0_c.as_ptr(), std::ptr::null()];
            let envp: [*const libc::c_char; 3] =
                [env_term.as_ptr(), env_path.as_ptr(), std::ptr::null()];

            // SAFETY: libc::execve is async-signal-safe. On success it
            // does not return.
            let _ = unsafe { libc::execve(path_c.as_ptr(), argv.as_ptr(), envp.as_ptr()) };

            // SAFETY: Unavoidable. Post-fork child must use _exit (the
            // existing documented exception to the "no new unsafe" rule).
            unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::path::PathBuf;

    #[test]
    fn preflight_shell_rejects_missing_binary() {
        // Regression for the "Raw Shell does nothing" bug: in
        // external-rescue mode the initramfs ships no /bin/sh, so the
        // forked child execve'd nothing and silently _exit(127)'d. The
        // preflight must turn a missing/non-exec shell into an Err the
        // emergency UI can surface, not a healthy-looking fork.
        let missing = PathBuf::from("/definitely/not/here/bin/sh");
        let err = preflight_shell(&missing).expect_err("missing shell must error");
        assert!(matches!(err, NmblError::Tui { .. }), "got {err:?}");
    }

    #[test]
    fn preflight_shell_accepts_executable() {
        // A real executable on the host passes. Skip if /bin/sh is
        // absent (extremely sandboxed CI), trying /bin/echo as a fallback.
        for cand in ["/bin/sh", "/bin/echo", "/usr/bin/env"] {
            let p = PathBuf::from(cand);
            if std::fs::metadata(&p).is_ok() {
                preflight_shell(&p).expect("executable must pass preflight");
                return;
            }
        }
    }

    /// Spawning `/bin/echo` (which is not a shell) is enough to verify
    /// fork/execve + master-fd readback work end-to-end without
    /// depending on a `/bin/sh` that varies across CI images. The child
    /// writes a short line and exits; the parent reads the bytes back
    /// from the master and reaps the child via `try_wait`.
    #[test]
    fn spawn_shell_basic_roundtrip() {
        // Skip on extremely sandboxed test envs where /bin/echo doesn't
        // exist — the test depends on a real executable to fork into.
        let echo = PathBuf::from("/bin/echo");
        if std::fs::metadata(&echo).is_err() {
            return;
        }
        let child = match spawn_shell(&echo, 80, 24) {
            Ok(c) => c,
            // Sandboxes that block fork or openpty return EPERM/ENOTTY.
            Err(_) => return,
        };

        // The PTY master is non-blocking. Drain until the child exits.
        // Cap iterations so the test cannot hang on a hostile sandbox.
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let raw = child.master.as_fd();
        let mut reaped = false;
        for _ in 0..1000 {
            match rustix::io::read(raw, &mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(tmp.get(..n).unwrap_or(&[])),
                Err(rustix::io::Errno::AGAIN) => {
                    std::thread::yield_now();
                }
                Err(_) => break,
            }
            if !reaped {
                if let Ok(Some(_)) = child.try_wait() {
                    reaped = true;
                }
            } else if buf.contains(&b'i') {
                // Got the 'i' from "hi" — enough evidence the pipe
                // works. Stop reading to keep the test bounded.
                break;
            }
        }
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("hi"), "expected 'hi' in PTY output, got {s:?}");
        // Best-effort reap if try_wait above missed the exit window.
        child.terminate();
    }
}
