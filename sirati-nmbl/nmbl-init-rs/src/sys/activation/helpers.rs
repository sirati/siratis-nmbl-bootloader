//! Private helpers shared by the runner functions.

use std::ffi::{CString, NulError};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;

use nix::errno::Errno;
use nix::unistd::{read, write};

use crate::error::{NmblError, Result};

use super::ProcessOutcome;

/// Construct a NUL-terminated copy of `path` for `execve(2)`.
pub(super) fn path_to_cstring(path: &Path) -> std::result::Result<CString, std::io::Error> {
    CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| nul_to_io(e, "binary path"))
}

/// The basename of `binary`, falling back to the full path string if
/// the path is empty or has no final component. Used as `argv[0]` so
/// the child sees a sensible program name.
pub(super) fn derive_argv0(binary: &Path) -> Vec<u8> {
    match binary.file_name() {
        Some(name) => name.as_encoded_bytes().to_vec(),
        None => binary.as_os_str().as_encoded_bytes().to_vec(),
    }
}

/// Convert a `NulError` (interior-NUL in a string) into an io::Error
/// with enough context to identify which field failed.
pub(super) fn nul_to_io(err: NulError, field: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("interior NUL in {field}: {err}"),
    )
}

/// Wrap a `nix::Error` as an `NmblError::Activation { kind, source: Io }`.
/// `kind` is the activation-runner sub-step that failed (e.g. "fork",
/// "pipe", "waitpid") — not the activation *configuration* kind.
pub(super) fn nix_activation(kind: &str, source: nix::Error, context: &str) -> NmblError {
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
pub(super) fn io_activation(kind: &str, source: std::io::Error) -> NmblError {
    NmblError::Activation {
        kind: kind.to_string(),
        source: Box::new(NmblError::Io {
            source,
            context: format!("activation runner: {kind}"),
        }),
    }
}

/// Build the `(binary_c, full_argv, env)` triple needed by `execve(2)`.
///
/// All allocation happens here so the post-fork child path stays
/// async-signal-safe. The returned `env` is intentionally empty —
/// activation tools are NixOS-built static binaries that don't depend
/// on PATH or locale env vars.
pub(super) fn build_exec_args(
    binary: &Path,
    argv: &[String],
) -> Result<(CString, Vec<CString>, Vec<CString>)> {
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
    Ok((binary_c, full_argv, env))
}

/// Drain `fd` to EOF into a `Vec<u8>`, restarting on EINTR.
///
/// Errors are surfaced as `NmblError::Activation { kind = "stdout", … }`
/// so the caller can distinguish a capture failure from a fork/exec
/// failure or a non-zero exit.
pub(super) fn read_all(fd: &OwnedFd, binary: &Path) -> Result<Vec<u8>> {
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
            Err(Errno::EINTR) => continue,
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
pub(super) fn write_all(fd: &OwnedFd, mut buf: &[u8], binary: &Path) -> Result<()> {
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
            Err(Errno::EINTR) => continue,
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

/// Translate a terminal [`WaitStatus`] into a [`ProcessOutcome`].
///
/// Exited children report their raw exit code; signalled children
/// report `128 + signal` (the POSIX shell convention) so callers and
/// logs see a single integer regardless of how the child died.
fn outcome_from_status(status: nix::sys::wait::WaitStatus) -> Option<ProcessOutcome> {
    use nix::sys::wait::WaitStatus;
    match status {
        WaitStatus::Exited(_, code) => Some(ProcessOutcome {
            exit_code: code,
            normal_exit: true,
        }),
        WaitStatus::Signaled(_, sig, _) => {
            let sig_num: i32 = sig as i32;
            Some(ProcessOutcome {
                exit_code: 128i32.saturating_add(sig_num),
                normal_exit: false,
            })
        }
        _ => None,
    }
}

/// Reap `child` asynchronously via the poller's non-blocking
/// `waitpid(WNOHANG)` op, never blocking the single-threaded runtime.
///
/// Reuses [`reap_child`](crate::sys::poller::reap_child) rather than
/// re-rolling a WNOHANG loop: the op is paced by the poller while the
/// runtime keeps driving the concurrent remote-attach server and the
/// local spinner. A `None` status (e.g. the child was already reaped,
/// `ECHILD`) collapses to a synthetic success outcome — the child is
/// gone, which is what the caller needs to know.
pub(super) async fn reap_child_outcome(
    child: nix::unistd::Pid,
    binary: &Path,
    sender: &crate::sys::poller::LocalSender,
) -> Result<ProcessOutcome> {
    let _ = binary;
    match crate::sys::poller::reap_child(child, sender.clone()).await {
        Some(status) => Ok(outcome_from_status(status).unwrap_or(ProcessOutcome {
            // Defensive: the poller op only resolves on a terminal
            // status, so this branch is unreachable in practice.
            exit_code: 0,
            normal_exit: true,
        })),
        // ECHILD / already reaped: the child is gone. Report a clean
        // exit so the caller proceeds rather than hanging.
        None => Ok(ProcessOutcome {
            exit_code: 0,
            normal_exit: true,
        }),
    }
}

/// Blocking `waitpid(None)` reap — used only by the pre-runtime early
/// boot phase (bootstrap blkid) and the synchronous `--validate` CLI
/// path, where nothing runs concurrently so blocking is harmless. EINTR
/// is retried. The interactive/concurrent path uses
/// [`reap_child_outcome`] instead.
pub(super) fn wait_for_child_blocking(
    child: nix::unistd::Pid,
    binary: &Path,
) -> Result<ProcessOutcome> {
    use nix::sys::wait::{WaitStatus, waitpid};
    loop {
        match waitpid(child, None) {
            Ok(status @ (WaitStatus::Exited(..) | WaitStatus::Signaled(..))) => {
                if let Some(outcome) = outcome_from_status(status) {
                    return Ok(outcome);
                }
            }
            // Stopped/Continued/Ptrace*/StillAlive are not possible with
            // a blocking `waitpid(None)` and no WUNTRACED/ptrace — loop
            // defensively on the unexpected variants rather than panic.
            Ok(_) => continue,
            Err(Errno::EINTR) => continue,
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
