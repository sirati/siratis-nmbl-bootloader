//! Fork/execve runners: `run`, `run_with_tick`, and `run_capture`.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use nix::unistd::{ForkResult, close, dup2, execve, fork, pipe};

use crate::error::Result;

use super::helpers::{build_exec_args, nix_activation, read_all, wait_for_child, write_all};
use super::{EXEC_FAILED_EXIT_CODE, ProcessOutcome};

/// Run `binary` with `argv` (which is the *tail* of the new process's
/// argv — `argv[0]` is automatically set to the basename of `binary`).
///
/// If `stdin_data` is `Some`, a pipe is created and the bytes are
/// written to it from the parent after the fork. The child sees those
/// bytes on file descriptor 0. This is used only by the `luks-password`
/// activation kind to feed the passphrase to `cryptsetup`.
///
/// Stdout and stderr are inherited from the parent so tool output
/// appears in the boot console (and, during tests, in the test
/// runner's captured streams).
///
/// A non-zero `exit_code` is **not** an `Err`. The caller decides
/// whether that's fatal: `cryptsetup` returning 2 (wrong passphrase)
/// is operationally different from the binary failing to start.
pub fn run(binary: &Path, argv: &[String], stdin_data: Option<&[u8]>) -> Result<ProcessOutcome> {
    run_with_tick(binary, argv, stdin_data, None::<&mut dyn FnMut()>)
}

/// Tick-aware variant of [`run`]: while waiting for the child to exit,
/// invoke `tick` every ~150 ms. The caller uses the callback to advance
/// a UI spinner so a slow activation (e.g. Argon2id LUKS unlock on a
/// low-power CPU) doesn't look like the boot hung.
///
/// Semantics otherwise match [`run`]: stdin is piped, stdout/stderr are
/// inherited, the [`ProcessOutcome`] reports the child's exit code, and
/// a non-zero exit is **not** an `Err`.
///
/// `tick` is called from the parent process after the fork; on the
/// child side nothing is changed — see the SAFETY comment in [`run`]
/// for the post-fork constraints. Pass `None` to get the blocking
/// behaviour of [`run`].
pub fn run_with_tick<F: FnMut() + ?Sized>(
    binary: &Path,
    argv: &[String],
    stdin_data: Option<&[u8]>,
    tick: Option<&mut F>,
) -> Result<ProcessOutcome> {
    // All CString construction and Vec allocation MUST happen here,
    // in the parent, before fork(2). After the fork, the child is
    // restricted to async-signal-safe operations until execve(2)
    // succeeds or _exit(2) is called.
    let (binary_c, full_argv, env) = build_exec_args(binary, argv)?;

    // Optional stdin pipe. We hold the OwnedFds in the parent until
    // after we have decided what to do with them — Rust's Drop will
    // close any that escape the explicit paths.
    let pipe_fds: Option<(OwnedFd, OwnedFd)> = if stdin_data.is_some() {
        Some(pipe().map_err(|e| nix_activation("pipe", e, "create stdin pipe"))?)
    } else {
        None
    };

    // SAFETY: `nix::unistd::fork` is `unsafe` by design — there is no
    // safe wrapper in any Rust crate (the closest safe alternative is
    // `std::process::Command`, which we cannot use: the CI grep
    // restricts `Command::` to the emergency-shell and panic paths,
    // and `Command` cannot run an in-process pre-exec child that pipes
    // bytes into stdin without crossing the same fork boundary). The
    // post-fork child path below is restricted to async-signal-safe
    // operations (dup2, close, execve, _exit); all CString allocation
    // happened in the parent before we got here.
    let fork_result = unsafe { fork() }
        .map_err(|e| nix_activation("fork", e, &format!("fork for {}", binary.display())))?;

    match fork_result {
        ForkResult::Child => {
            child_stdin_setup(pipe_fds);
            // execve safety: we are a forked child, not PID 1; our job is to run the activation helper. execve does not return on success.
            let _ = execve(&binary_c, &full_argv, &env);
            // execve failed (binary missing, permission denied,
            // ENOEXEC, ELOOP, …). The conventional "command not
            // found" exit code lets the parent — and ultimately the
            // activation orchestrator — surface the right diagnostic.
            //
            // SAFETY: Unavoidable, same reasoning as the dup2-failure
            // branch above — post-fork child, must use the
            // async-signal-safe `_exit(2)` rather than Rust's
            // destructor-running `process::exit`. No safe wrapper
            // exists in `nix` or `rustix` today.
            unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
        }
        ForkResult::Parent { child } => {
            // === PARENT ===
            if let Some((read_end, write_end)) = pipe_fds {
                // Close the read end first so a misbehaving child
                // that exits without reading triggers SIGPIPE on our
                // write (which is what we want — we'd rather see
                // the failure than block forever).
                drop(read_end);
                // SAFETY of unwrap-avoidance: `stdin_data` is
                // guaranteed to be Some here because pipe_fds is
                // Some iff stdin_data is Some — but we still match
                // defensively to avoid expect/unwrap.
                if let Some(bytes) = stdin_data {
                    write_all(&write_end, bytes, binary)?;
                }
                drop(write_end);
            }

            wait_for_child(child, binary, tick)
        }
    }
}

/// Wire the read end of `pipe_fds` onto stdin (fd 0) in the child.
///
/// This is a mechanical extraction of the child-side stdin setup from
/// `run_with_tick` to keep that function under 100 lines. Called only
/// from the `ForkResult::Child` arm — must stay async-signal-safe.
fn child_stdin_setup(pipe_fds: Option<(OwnedFd, OwnedFd)>) {
    let Some((read_end, write_end)) = pipe_fds else {
        return;
    };
    // === CHILD ===
    // From this point until execve/_exit we MUST stay within
    // async-signal-safe territory: no allocation, no panics,
    // no Rust I/O (println!, eprintln!, etc.).
    let read_fd = read_end.as_raw_fd();
    // Close the write end first — only the parent writes.
    // We rely on `close()` from nix (a thin wrapper over
    // the libc close(2), which is async-signal-safe).
    let _ = close(write_end.as_raw_fd());
    // dup2 the read end onto stdin (fd 0). If read_fd
    // already is 0 (very unlikely — would mean stdin was
    // closed in the parent), dup2 is a no-op AND does
    // not close the target, which is what we want.
    if read_fd != 0 {
        if dup2(read_fd, 0).is_err() {
            // SAFETY: Unavoidable. We are in the post-fork
            // child where only async-signal-safe calls are
            // permitted; `std::process::exit` is *not*
            // async-signal-safe (it runs Rust destructors
            // and atexit handlers) so the only correct
            // primitive here is `_exit(2)`. No crate wraps
            // it safely — `rustix` 0.38 exposes neither
            // `_exit` nor `exit_group` (issues #844 / #845
            // open at rustix). The argument is a plain int.
            unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
        }
        let _ = close(read_fd);
    }
    // The OwnedFds above will not Drop because we _exit
    // or execve before the function returns; but even if
    // they did, close() is signal-safe.
    // Suppress drop side-effects by forgetting them.
    std::mem::forget(read_end);
    std::mem::forget(write_end);
}

/// Run `binary` with `argv` and capture its stdout into a `Vec<u8>`.
///
/// Same fork/execve mechanism as [`run`], but a pipe is wired between
/// the child's fd 1 and the parent's read end. Stderr is still
/// inherited from the parent (we only care about diagnostic capture
/// for tools like `blkid` that emit their machine-readable payload on
/// stdout). Stdin is closed in the child by being left untouched —
/// it stays whatever the parent passed in, which is fine for read-
/// only tools that don't read from stdin.
///
/// Used by `sys::blkid` to capture `blkid -o export` payloads. A
/// non-zero exit code is still reported via `ProcessOutcome`, not
/// `Err` — `blkid` legitimately exits 2 for "no superblock", which
/// the caller treats as "empty attributes" rather than a fault.
pub fn run_capture(binary: &Path, argv: &[String]) -> Result<(ProcessOutcome, Vec<u8>)> {
    // Same parent-side allocation discipline as `run`: anything that
    // can allocate must happen here so the post-fork child path stays
    // async-signal-safe.
    let (binary_c, full_argv, env) = build_exec_args(binary, argv)?;

    let (read_end, write_end): (OwnedFd, OwnedFd) =
        pipe().map_err(|e| nix_activation("pipe", e, "create stdout pipe"))?;

    // SAFETY: see the `run` function above for the full rationale on
    // why `nix::unistd::fork` must remain unsafe and why we cannot
    // delegate to `std::process::Command` (CI grep + Command's
    // inability to pre-exec under our policy). Identical reasoning
    // applies here.
    let fork_result = unsafe { fork() }
        .map_err(|e| nix_activation("fork", e, &format!("fork for {}", binary.display())))?;

    match fork_result {
        ForkResult::Child => {
            // === CHILD ===
            // Wire the write end of the pipe onto fd 1 (stdout).
            // Close the read end — only the parent reads.
            let write_fd = write_end.as_raw_fd();
            let _ = close(read_end.as_raw_fd());

            if write_fd != 1 {
                if dup2(write_fd, 1).is_err() {
                    // SAFETY: post-fork child — only async-signal-safe
                    // calls permitted; `_exit(2)` rather than Rust's
                    // destructor-running `process::exit`. See `run`.
                    unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
                }
                let _ = close(write_fd);
            }
            // Prevent OwnedFd Drop from running — execve or _exit
            // follows immediately, but be explicit.
            std::mem::forget(read_end);
            std::mem::forget(write_end);

            // execve safety: we are a forked child, not PID 1; our job is to run the activation helper. execve does not return on success.
            let _ = execve(&binary_c, &full_argv, &env);

            // SAFETY: see the analogous comment in `run`.
            unsafe { libc::_exit(EXEC_FAILED_EXIT_CODE) };
        }
        ForkResult::Parent { child } => {
            // === PARENT ===
            // Drop the write end so the child's stdout is the only
            // writer; once it exits or closes fd 1 our read loop
            // sees EOF instead of blocking forever.
            drop(write_end);

            let captured = read_all(&read_end, binary)?;
            drop(read_end);

            let outcome = wait_for_child(child, binary, None::<&mut dyn FnMut()>)?;
            Ok((outcome, captured))
        }
    }
}
