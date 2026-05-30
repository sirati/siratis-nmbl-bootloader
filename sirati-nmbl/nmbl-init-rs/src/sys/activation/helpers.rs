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
/// async-signal-safe. The returned `env` carries only `DM_DISABLE_UDEV=1`
/// (see below) — activation tools are NixOS-built static binaries that
/// otherwise don't depend on PATH or locale env vars.
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

    // NMBL runs without udev. NixOS's cryptsetup / lvm / mdadm are built
    // with libdevmapper udev-cookie support and, by default, WAIT for
    // udev to create the `/dev/mapper/<name>` node after `cryptsetup
    // open` — which never happens here, so the device-mapper node never
    // appears and the post-activation device wait times out. Setting
    // `DM_DISABLE_UDEV=1` tells libdevmapper to fall back to creating the
    // nodes itself (direct mknod), exactly as it does in early-boot
    // initramfs environments. Harmless for activations that don't use
    // device-mapper.
    let env: Vec<CString> = vec![
        CString::new("DM_DISABLE_UDEV=1")
            .map_err(|e| io_activation("execve-env", nul_to_io(e, "DM_DISABLE_UDEV")))?,
    ];
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

/// Wait for `child` to terminate and translate the resulting
/// `WaitStatus` into a `ProcessOutcome`. EINTR is retried.
///
/// When `tick` is `Some`, this loop polls with `WNOHANG` and calls
/// `tick` every [`super::TICK_INTERVAL`] so the caller can advance a UI
/// spinner. When `tick` is `None`, the loop reverts to a blocking
/// `waitpid(None)` — identical behaviour to the original
/// non-tick-aware function.
pub(super) fn wait_for_child<F: FnMut() + ?Sized>(
    child: nix::unistd::Pid,
    binary: &Path,
    mut tick: Option<&mut F>,
) -> Result<ProcessOutcome> {
    use std::thread::sleep;

    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
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
                sleep(super::TICK_INTERVAL);
                continue;
            }
            // Stopped/Continued/Ptrace* are not possible without
            // WUNTRACED / WCONTINUED / ptrace — but waitpid returns
            // only Exited/Signaled/StillAlive in practice. We loop on
            // the unexpected variants rather than panic.
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
