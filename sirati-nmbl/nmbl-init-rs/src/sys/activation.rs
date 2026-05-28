//! Pure-mechanism execve runner for activation tools.
//!
//! This module is one of exactly three sites in the crate that are
//! allowed to replace the current process with another binary (the
//! other two being `src/shell.rs` and `src/panic.rs`). It exists so
//! the activation orchestrator can launch LVM/mdraid/cryptsetup/zpool
//! with a configured argv and — for the `luks-password` kind — a
//! passphrase piped to the child's stdin.
//!
//! It is deliberately stripped of policy: the caller decides which
//! binary to run, what arguments to pass, whether to feed bytes on
//! stdin, and whether a non-zero exit code is fatal. The runner only
//! reports *how* the child terminated.
//!
//! We use `nix`'s primitives directly rather than `std::process::Command`
//! because (a) we need fine-grained control of the stdin pipe and (b)
//! the CI grep enforces that `Command::` only ever appears in the
//! emergency-shell and panic-recovery paths.

use std::ffi::{CString, NulError};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use std::thread::sleep;
use std::time::Duration;

use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, close, dup2, execve, fork, pipe, read, write};

use crate::error::{NmblError, Result};

/// Poll cadence for the tick-aware wait helper.
///
/// 150 ms is brisk enough that the operator sees the spinner move
/// while a passphrase is being verified (cryptsetup --key-file=- runs
/// in well under a second on modern hardware, but Argon2id key
/// derivation can take ~1-3 s on a Raspberry Pi class CPU), and slow
/// enough that the WNOHANG polling overhead stays negligible.
const TICK_INTERVAL: Duration = Duration::from_millis(150);

/// How a child process terminated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessOutcome {
    /// `WEXITSTATUS` for normal exits, `128 + signal` for signalled
    /// exits (mirroring the shell convention so callers can log a
    /// single number).
    pub exit_code: i32,
    /// `true` if the child exited via `_exit`/`exit`, `false` if it
    /// was killed by a signal.
    pub normal_exit: bool,
}

/// Conventional shell exit code for "exec failed / command not found".
/// We surface this when the post-fork `execve(2)` returns an error so
/// the caller can distinguish a missing binary from a broken one.
const EXEC_FAILED_EXIT_CODE: i32 = 127;

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
    let binary_c =
        path_to_cstring(binary).map_err(|source| io_activation("execve-argv", source))?;

    let argv0 = derive_argv0(binary);
    let argv0_c =
        CString::new(argv0).map_err(|e| io_activation("execve-argv", nul_to_io(e, "argv0")))?;

    let mut full_argv: Vec<CString> = Vec::with_capacity(argv.len() + 1);
    full_argv.push(argv0_c);
    for (i, a) in argv.iter().enumerate() {
        let c = CString::new(a.as_bytes()).map_err(|e| {
            io_activation(
                "execve-argv",
                nul_to_io(e, &format!("argv[{}]", i.saturating_add(1))),
            )
        })?;
        full_argv.push(c);
    }

    // We intentionally pass an empty environment. Activation tools
    // are NixOS-built static binaries that don't depend on PATH or
    // locale env vars, and PID-1's own env is barely populated
    // anyway. If a future activation kind needs a specific env var,
    // it should be encoded into the `binary`/`argv` choice at Nix
    // build time, not inherited from PID 1.
    let env: Vec<CString> = Vec::new();

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
            // === CHILD ===
            // From this point until execve/_exit we MUST stay within
            // async-signal-safe territory: no allocation, no panics,
            // no Rust I/O (println!, eprintln!, etc.).
            if let Some((read_end, write_end)) = pipe_fds {
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

            // execve does not return on success.
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
    let binary_c =
        path_to_cstring(binary).map_err(|source| io_activation("execve-argv", source))?;

    let argv0 = derive_argv0(binary);
    let argv0_c =
        CString::new(argv0).map_err(|e| io_activation("execve-argv", nul_to_io(e, "argv0")))?;

    let mut full_argv: Vec<CString> = Vec::with_capacity(argv.len().saturating_add(1));
    full_argv.push(argv0_c);
    for (i, a) in argv.iter().enumerate() {
        let c = CString::new(a.as_bytes()).map_err(|e| {
            io_activation(
                "execve-argv",
                nul_to_io(e, &format!("argv[{}]", i.saturating_add(1))),
            )
        })?;
        full_argv.push(c);
    }

    let env: Vec<CString> = Vec::new();

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

            // execve does not return on success.
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

/// Drain `fd` to EOF into a `Vec<u8>`, restarting on EINTR.
///
/// Errors are surfaced as `NmblError::Activation { kind = "stdout", … }`
/// so the caller can distinguish a capture failure from a fork/exec
/// failure or a non-zero exit.
fn read_all(fd: &OwnedFd, binary: &Path) -> Result<Vec<u8>> {
    // 4 KiB matches the default pipe buffer on Linux and balances
    // syscall count vs. allocation for small payloads like blkid's.
    let mut buf = [0u8; 4096];
    let mut out: Vec<u8> = Vec::new();
    loop {
        match read(fd.as_raw_fd(), &mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                // Defensive slice — `n <= buf.len()` always holds on
                // a healthy kernel, but `get(..n)` is total.
                if let Some(chunk) = buf.get(..n) {
                    out.extend_from_slice(chunk);
                }
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                return Err(nix_activation(
                    "stdout",
                    e,
                    &format!("read stdout from {}", binary.display()),
                ));
            }
        }
    }
}

/// Write the entire buffer to `fd`, restarting on short writes.
///
/// Errors are surfaced as `NmblError::Activation { kind = "stdin", … }`
/// so the caller can tell a passphrase-pipe failure apart from a
/// fork/exec failure.
fn write_all(fd: &OwnedFd, mut buf: &[u8], binary: &Path) -> Result<()> {
    while !buf.is_empty() {
        match write(fd, buf) {
            Ok(0) => {
                // A zero-length write is treated as EIO: we asked to
                // write N bytes and the kernel accepted none. This
                // shouldn't happen on a healthy pipe but we refuse
                // to spin.
                return Err(io_activation(
                    "stdin",
                    std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        format!("write returned 0 piping stdin to {}", binary.display()),
                    ),
                ));
            }
            Ok(n) => {
                // Defensive slice rather than indexing (clippy denies
                // `indexing_slicing`); on a normal pipe `n <= buf.len()`
                // always holds, but `get(n..)` is total.
                buf = match buf.get(n..) {
                    Some(rest) => rest,
                    None => &[],
                };
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                return Err(nix_activation(
                    "stdin",
                    e,
                    &format!("write stdin to {}", binary.display()),
                ));
            }
        }
    }
    Ok(())
}

/// Wait for `child` to terminate and translate the resulting
/// `WaitStatus` into a `ProcessOutcome`. EINTR is retried.
///
/// When `tick` is `Some`, this loop polls with `WNOHANG` and calls
/// `tick` every [`TICK_INTERVAL`] so the caller can advance a UI
/// spinner. When `tick` is `None`, the loop reverts to a blocking
/// `waitpid(None)` — identical behaviour to the original
/// non-tick-aware function.
fn wait_for_child<F: FnMut() + ?Sized>(
    child: Pid,
    binary: &Path,
    mut tick: Option<&mut F>,
) -> Result<ProcessOutcome> {
    loop {
        // Choose blocking vs. WNOHANG based on whether a tick callback
        // was supplied. The blocking branch matches the historical
        // behaviour to keep callers without UI plumbing cheap.
        let status = if tick.is_some() {
            waitpid(child, Some(WaitPidFlag::WNOHANG))
        } else {
            waitpid(child, None)
        };
        match status {
            Ok(WaitStatus::Exited(_, code)) => {
                return Ok(ProcessOutcome {
                    exit_code: code,
                    normal_exit: true,
                });
            }
            Ok(WaitStatus::Signaled(_, sig, _)) => {
                // Mirror the POSIX shell convention (128 + signal)
                // so callers and logs see a single integer regardless
                // of how the child died.
                let sig_num: i32 = sig as i32;
                return Ok(ProcessOutcome {
                    exit_code: 128i32.saturating_add(sig_num),
                    normal_exit: false,
                });
            }
            // `StillAlive` is the WNOHANG "child not yet exited"
            // signal — invoke the spinner tick and sleep one slice
            // before polling again.
            Ok(WaitStatus::StillAlive) => {
                if let Some(t) = tick.as_mut() {
                    (*t)();
                }
                sleep(TICK_INTERVAL);
                continue;
            }
            // Stopped/Continued/Ptrace* are not possible without
            // WUNTRACED / WCONTINUED / ptrace — but waitpid returns
            // only Exited/Signaled/StillAlive in practice. We loop on
            // the unexpected variants rather than panic.
            Ok(_) => continue,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                return Err(nix_activation(
                    "waitpid",
                    e,
                    &format!("wait for {}", binary.display()),
                ));
            }
        }
    }
}

/// Construct a NUL-terminated copy of `path` for `execve(2)`.
fn path_to_cstring(path: &Path) -> std::result::Result<CString, std::io::Error> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| nul_to_io(e, "binary path"))
}

/// The basename of `binary`, falling back to the full path string if
/// the path is empty or has no final component. Used as `argv[0]` so
/// the child sees a sensible program name.
fn derive_argv0(binary: &Path) -> Vec<u8> {
    match binary.file_name() {
        Some(name) => name.as_encoded_bytes().to_vec(),
        None => binary.as_os_str().as_encoded_bytes().to_vec(),
    }
}

/// Convert a `NulError` (interior-NUL in a string) into an io::Error
/// with enough context to identify which field failed.
fn nul_to_io(err: NulError, field: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("interior NUL in {field}: {err}"),
    )
}

/// Wrap a `nix::Error` as an `NmblError::Activation { kind, source: Io }`.
/// `kind` is the activation-runner sub-step that failed (e.g. "fork",
/// "pipe", "waitpid") — not the activation *configuration* kind.
fn nix_activation(kind: &str, source: nix::Error, context: &str) -> NmblError {
    NmblError::Activation {
        kind: kind.to_string(),
        source: Box::new(NmblError::Io {
            source: source.into(),
            context: context.to_string(),
        }),
    }
}

/// Wrap a `std::io::Error` (e.g. from a CString failure) into the same
/// shape so all activation-runner errors share a structure.
fn io_activation(kind: &str, source: std::io::Error) -> NmblError {
    NmblError::Activation {
        kind: kind.to_string(),
        source: Box::new(NmblError::Io {
            source,
            context: format!("activation runner: {kind}"),
        }),
    }
}

#[cfg(all(test, unix))]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Locate a binary on disk. We can't rely on `/bin/<x>` existing
    /// inside a `nix develop` shell (which often has an almost-empty
    /// `/bin`), so we resolve via `PATH` ourselves — keeping the test
    /// dependency surface to just the standard library.
    fn which(name: &str) -> Option<PathBuf> {
        let path_env = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    #[test]
    fn true_exits_zero() {
        let Some(bin) = which("true") else {
            eprintln!("skipping: `true` not found on PATH");
            return;
        };
        let out = run(&bin, &[], None).expect("run /usr/bin/true");
        assert!(out.normal_exit, "true should exit normally");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn false_exits_one() {
        let Some(bin) = which("false") else {
            eprintln!("skipping: `false` not found on PATH");
            return;
        };
        let out = run(&bin, &[], None).expect("run /usr/bin/false");
        assert!(out.normal_exit, "false should exit normally");
        assert_eq!(out.exit_code, 1);
    }

    #[test]
    fn cat_consumes_piped_stdin() {
        let Some(bin) = which("cat") else {
            eprintln!("skipping: `cat` not found on PATH");
            return;
        };
        let out = run(&bin, &[], Some(b"hello\n")).expect("run cat with stdin");
        assert!(out.normal_exit, "cat should exit normally");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn missing_binary_yields_127() {
        let bogus = PathBuf::from("/nonexistent/path/xyz-nmbl-activation-test");
        let out = run(&bogus, &[], None).expect("run should report, not error");
        assert!(out.normal_exit, "missing-binary path uses _exit(127)");
        assert_eq!(
            out.exit_code, EXEC_FAILED_EXIT_CODE,
            "execve failure must surface as 127"
        );
    }

    #[test]
    fn capture_echo_returns_stdout_bytes() {
        let Some(bin) = which("echo") else {
            eprintln!("skipping: `echo` not found on PATH");
            return;
        };
        let (outcome, captured) = run_capture(&bin, &["hello".to_string()]).expect("run echo");
        assert!(outcome.normal_exit);
        assert_eq!(outcome.exit_code, 0);
        // `echo` appends a newline; we should see it.
        assert_eq!(captured, b"hello\n");
    }

    #[test]
    fn capture_missing_binary_yields_127_and_empty_buffer() {
        let bogus = PathBuf::from("/nonexistent/path/xyz-nmbl-capture-test");
        let (outcome, captured) = run_capture(&bogus, &[]).expect("run_capture should report");
        assert!(outcome.normal_exit, "missing-binary path uses _exit(127)");
        assert_eq!(outcome.exit_code, EXEC_FAILED_EXIT_CODE);
        assert!(
            captured.is_empty(),
            "no stdout should have been produced by a missing binary",
        );
    }

    #[test]
    fn capture_handles_payload_larger_than_pipe_buffer() {
        // A payload bigger than the 4 KiB pipe read buffer exercises
        // the multi-iteration path in `read_all`. We use `printf` to
        // emit a deterministic string of known size.
        let Some(bin) = which("printf") else {
            eprintln!("skipping: `printf` not found on PATH");
            return;
        };
        // 10 000 'x' characters, no trailing newline.
        let pattern = "x".repeat(10_000);
        let (outcome, captured) =
            run_capture(&bin, &["%s".to_string(), pattern.clone()]).expect("run printf");
        assert!(outcome.normal_exit);
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(captured.len(), pattern.len());
        assert!(captured.iter().all(|b| *b == b'x'));
    }
}
