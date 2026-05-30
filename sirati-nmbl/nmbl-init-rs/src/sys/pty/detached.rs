//! [`DetachedShell`] and [`spawn_shell_on_tty`]: fire-and-forget shell on a raw tty.

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use nix::unistd::{ForkResult, fork, setsid};

use super::{EXEC_FAILED_EXIT_CODE, preflight_shell, prepare_shell_cstrings};
use crate::error::{NmblError, Result};

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
    pub pid: nix::unistd::Pid,
}

/// Open the target tty in the parent so an `open(2)` failure surfaces
/// synchronously. The fd is inherited across `fork`; the child `dup2`s it
/// onto 0/1/2 before `execve`.
///
/// THE NO-HANG INVARIANT lives here. A plain `open(2)` on a tty WITHOUT
/// `O_NONBLOCK` blocks until carrier (DCD) is asserted on lines that
/// don't have `CLOCAL` set — a serial port (`/dev/ttyS0`) with nothing
/// plugged in NEVER raises carrier, so the open blocks forever. That is
/// the verified "launching the raw shell hangs" bug: the picker's first
/// option resolves to a carrier-less serial line and the fire-and-forget
/// spawn wedged on `open(2)`.
///
/// We therefore open with `O_NONBLOCK | O_NOCTTY | O_CLOEXEC` (bounded:
/// the kernel returns immediately regardless of carrier state), then
/// reject an already-occupied tty, then clear `O_NONBLOCK` so the shell
/// gets the usual blocking stdio semantics on its line.
fn open_tty_fd(tty_path: &Path) -> Result<OwnedFd> {
    use rustix::fs::{Mode as RustixMode, OFlags as RustixOFlagsAlias, fcntl_getfl, fcntl_setfl};

    // O_NONBLOCK guarantees the open returns in bounded time even on a
    // carrier-less / hung line; O_NOCTTY keeps this parent (PID 1) from
    // accidentally adopting the tty as its controlling terminal; O_CLOEXEC
    // means the bare fd doesn't leak past the child's execve (the dup2'd
    // 0/1/2 copies survive, which is what the shell needs).
    let fd = rustix::fs::open(
        tty_path,
        RustixOFlagsAlias::RDWR
            | RustixOFlagsAlias::NOCTTY
            | RustixOFlagsAlias::NONBLOCK
            | RustixOFlagsAlias::CLOEXEC,
        RustixMode::empty(),
    )
    .map_err(|e| NmblError::Tui {
        source: std::io::Error::other(format!(
            "opening shell tty {} failed: {e}",
            tty_path.display()
        )),
    })?;

    // Refuse a tty that already belongs to another live session (e.g. a
    // shell the operator spawned on this same line a moment ago). Without
    // this, the second spawn would race for the controlling terminal and
    // the operator would end up with two foreground shells fighting over
    // one line — surface a clear error and skip instead.
    if let Some(owner) = tty_controlling_session(&fd) {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "shell tty {} is already in use (controlling session pid {owner}); \
                 a shell is likely already running on this line — pick another tty",
                tty_path.display()
            )),
        });
    }

    // Hand the shell a blocking line: clear O_NONBLOCK now that the open
    // itself can no longer wedge us. Best-effort — a shell on a
    // non-blocking line still runs, it just may see spurious EAGAINs.
    if let Ok(flags) = fcntl_getfl(&fd) {
        let _ = fcntl_setfl(&fd, flags & !RustixOFlagsAlias::NONBLOCK);
    }

    Ok(fd)
}

/// Return the pid of the session that already owns `fd` as its
/// controlling terminal, if one exists AND it is a different, live
/// session than ours. Returns `None` for a free tty (no controlling
/// session), a non-tty, or our own session — none of which block a
/// fresh spawn.
///
/// `tcgetsid(3)` (rustix, no `unsafe`) returns the session id of the
/// tty's controlling session, or an error (ENOTTY / no session) when the
/// line is free. We additionally confirm the reported session leader is
/// still alive via a signal-0 `kill`, so a stale sid left by a crashed
/// shell doesn't permanently lock the operator out of the line.
fn tty_controlling_session(fd: &OwnedFd) -> Option<i32> {
    use std::os::fd::AsFd;

    let sid = rustix::termios::tcgetsid(fd.as_fd()).ok()?;
    let sid_raw = sid.as_raw_nonzero().get();

    // A tty whose controlling session is OUR session never blocks a spawn
    // — that's the normal "PID 1 inherited this console" case.
    if let Ok(our_sid) = rustix::process::getsid(None)
        && our_sid.as_raw_nonzero().get() == sid_raw
    {
        return None;
    }

    // Confirm the owning session leader is still alive; `test_kill_process`
    // is a signal-0 existence probe that delivers nothing. A dead leader
    // (ESRCH) means the line is effectively free, so don't report it as
    // in-use; EPERM means it exists but we can't signal it — still in-use.
    match rustix::process::test_kill_process(sid) {
        Ok(()) | Err(rustix::io::Errno::PERM) => Some(sid_raw),
        Err(_) => None,
    }
}

/// Post-fork child path for `spawn_shell_on_tty`. Restricted to
/// async-signal-safe primitives. Wires `tty_raw` onto stdio and
/// `execve`s the shell.
///
/// # Safety
/// Must only be called from the child branch of `fork()`. No allocation,
/// no Rust I/O, no destructors. All `CString` args were allocated in the
/// parent before `fork`.
unsafe fn child_exec_on_tty(
    tty_raw: i32,
    path_c: &CString,
    argv0_c: &CString,
    env_term: &CString,
    env_path: &CString,
) -> ! {
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
    let envp: [*const libc::c_char; 3] = [env_term.as_ptr(), env_path.as_ptr(), std::ptr::null()];

    // SAFETY: libc::execve is async-signal-safe. On success it
    // does not return.
    // execve safety: we are a forked child process, not PID 1; our job is to replace ourselves with the requested shell.
    let _ = unsafe { libc::execve(path_c.as_ptr(), argv.as_ptr(), envp.as_ptr()) };

    // SAFETY: Unavoidable. Post-fork child must use _exit (the
    // existing documented exception to the "no new unsafe" rule).
    unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) }
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
    // Same preflight as spawn_shell: a missing /bin/sh would otherwise
    // produce a "Shell spawned" toast for a child that instantly
    // _exit(127)s, masking the real problem.
    preflight_shell(shell_path)?;

    // === Parent-side allocation: ALL CString / Vec construction MUST
    // happen here, before fork(2). The post-fork child path is restricted
    // to async-signal-safe operations (same discipline as spawn_shell). ===
    let (path_c, argv0_c, env_term, env_path) = prepare_shell_cstrings(shell_path)?;

    // Open the target tty in the PARENT so an open(2) failure surfaces
    // synchronously (not as an opaque _exit code from a forked child).
    // The fd is then inherited across fork; the child dup2's it onto
    // 0/1/2 before execve.
    let tty_fd = open_tty_fd(tty_path)?;
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
            // SAFETY: we are in the child branch of fork(); child_exec_on_tty
            // is restricted to async-signal-safe calls and does not return.
            unsafe { child_exec_on_tty(tty_raw, &path_c, &argv0_c, &env_term, &env_path) }
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
    use std::time::{Duration, Instant};

    /// Wall-clock budget every "must not hang" assertion holds the spawn
    /// path to. Opening a tty is a handful of syscalls; anything past this
    /// means we blocked, which is exactly the bug under test.
    const BOUND: Duration = Duration::from_secs(3);

    /// A missing device node must produce an `Err` (ENOENT), and it must
    /// do so essentially instantly — never block.
    #[test]
    fn open_tty_fd_missing_path_errors_bounded() {
        let start = Instant::now();
        let res = open_tty_fd(Path::new("/dev/this-tty-does-not-exist-nmbl-test"));
        assert!(res.is_err(), "missing tty must Err, got Ok");
        assert!(
            start.elapsed() < BOUND,
            "open_tty_fd blocked on missing path"
        );
    }

    /// `/dev/null` is a chardev with no controlling session, so the open
    /// succeeds and `tty_controlling_session` reports "free". Skipped on
    /// sandboxes without `/dev/null`.
    #[test]
    fn open_tty_fd_on_free_chardev_succeeds_bounded() {
        if std::fs::metadata("/dev/null").is_err() {
            return;
        }
        let start = Instant::now();
        let fd = open_tty_fd(Path::new("/dev/null")).expect("/dev/null must open");
        assert!(start.elapsed() < BOUND, "open_tty_fd blocked on /dev/null");
        // /dev/null is not a tty → tcgetsid ENOTTY → reported as free.
        assert_eq!(tty_controlling_session(&fd), None);
    }

    /// THE NO-HANG INVARIANT, end to end on a real tty that is ALREADY in
    /// use. We allocate a PTY pair, hand the slave to a forked child that
    /// `setsid`+`TIOCSCTTY`s it (becoming the controlling session of that
    /// line), then assert `open_tty_fd` on the same slave path returns the
    /// "already in use" `Err` WITHIN the wall-clock bound — never blocks.
    ///
    /// This reproduces the operator scenario (a shell already running on
    /// the chosen tty) without a framebuffer VT, using the pty subsystem
    /// available in CI.
    #[test]
    fn open_tty_fd_on_occupied_tty_errors_without_hanging() {
        use nix::pty::{OpenptyResult, openpty};
        use nix::unistd::{ForkResult, fork};
        use std::os::fd::AsRawFd;

        // Need devpts for openpty; skip on sandboxes that forbid it.
        if super::super::ensure_devpts_mounted().is_err() {
            return;
        }
        let OpenptyResult { master, slave } = match openpty(None, None) {
            Ok(p) => p,
            Err(_) => return, // sandbox without ptys
        };
        // Resolve the slave's /dev/pts/N path so we can re-open it by name.
        let slave_path = match std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())) {
            Ok(p) => p,
            Err(_) => return,
        };
        if !slave_path.starts_with("/dev/pts/") {
            return;
        }

        // Fork a child that owns the slave as its controlling terminal and
        // then sleeps, so the line is genuinely occupied during the test.
        let slave_raw = slave.as_raw_fd();
        // SAFETY: post-fork child below only calls async-signal-safe libc
        // primitives (setsid/ioctl/pause/_exit); no allocation.
        match unsafe { fork() } {
            Ok(ForkResult::Child) => unsafe {
                libc::setsid();
                libc::ioctl(slave_raw, libc::TIOCSCTTY as _, 0);
                libc::pause();
                libc::_exit(0);
            },
            Ok(ForkResult::Parent { child }) => {
                // Give the child a moment to claim the controlling tty.
                std::thread::sleep(Duration::from_millis(50));

                let start = Instant::now();
                let res = open_tty_fd(&slave_path);
                let elapsed = start.elapsed();

                // Tear the child down before asserting so a failure doesn't
                // leak a paused process.
                let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
                let _ = nix::sys::wait::waitpid(child, None);
                drop(slave);
                drop(master);

                assert!(
                    elapsed < BOUND,
                    "open_tty_fd on an occupied tty blocked for {elapsed:?}"
                );
                assert!(
                    res.is_err(),
                    "open_tty_fd on an occupied tty must Err (in-use), got Ok"
                );
            }
            Err(_) => {} // fork blocked by sandbox; nothing to assert
        }
    }
}
