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
use std::path::Path;

use crate::error::{NmblError, Result};
use crate::sys::ops::FsOps;

mod child;
mod detached;
mod spawn;
#[cfg(test)]
mod tests;

pub use child::PtyChild;
pub use detached::{DetachedShell, spawn_shell_on_tty};
pub use spawn::spawn_shell;

/// Conventional shell exit code surfaced when the post-fork `execve(2)`
/// fails. Matches the value used in `src/sys/activation.rs`.
pub(super) const EXEC_FAILED_EXIT_CODE: i32 = 127;

/// Verify the shell binary actually exists and is executable *before*
/// we fork. A post-fork `execve(2)` failure can only `_exit(127)` from
/// the async-signal-safe child path — it cannot report which errno it
/// hit. That silent death is exactly the "Raw Shell does nothing" bug:
/// in external-rescue mode the initramfs ships no `/bin/sh`, so the
/// child execve'd nothing and exited instantly while the parent saw a
/// healthy fork and reported success. Checking up here turns that into
/// a descriptive `Err` the emergency UI can surface.
pub(super) fn preflight_shell(fs: &dyn FsOps, shell_path: &Path) -> Result<()> {
    use rustix::fs::{Access, access};
    // Presence goes through the FsOps seam so a dry-run can satisfy it
    // from a closure; the exec-bit check stays a real `access(2)` —
    // FsOps has no executability predicate and a dry-run that satisfied
    // `exists` will short-circuit before reaching the live fork anyway.
    if !fs.exists(shell_path) {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "emergency shell {} does not exist; \
                 in external-rescue mode the initramfs ships no /bin/sh — \
                 set boot.nmbl.paths.shell to a binary present in the initrd",
                shell_path.display()
            )),
        });
    }
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

/// Build the four `CString`s that every shell-spawn variant needs before
/// `fork(2)`: the executable path, `argv[0]`, `TERM`, and `PATH`. All
/// allocation must happen in the parent before `fork`; this helper
/// centralises that work so neither `spawn_shell` nor `spawn_shell_on_tty`
/// has to repeat it.
pub(super) fn prepare_shell_cstrings(
    shell_path: &Path,
) -> Result<(CString, CString, CString, CString)> {
    let path_c =
        CString::new(shell_path.as_os_str().as_encoded_bytes()).map_err(|_| NmblError::Tui {
            source: std::io::Error::other("shell path contains interior NUL"),
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
    let env_path =
        CString::new("PATH=/usr/sbin:/usr/bin:/sbin:/bin").map_err(|_| NmblError::Tui {
            source: std::io::Error::other("static PATH env contains interior NUL"),
        })?;

    Ok((path_c, argv0_c, env_term, env_path))
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
pub(super) fn ensure_devpts_mounted(fs: &mut dyn FsOps) -> Result<()> {
    use crate::nmbl_warn;
    use nix::errno::Errno;

    let target = Path::new("/dev/pts");
    // Route the mountpoint mkdir through the FsOps seam. `ensure_dir`
    // (mkdir -p) is idempotent: a pre-existing directory is the happy
    // path on a second invocation.
    if let Err(e) = fs.ensure_dir(target) {
        // `AlreadyExists` is the happy path on a second invocation.
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(NmblError::Io {
                source: e,
                context: "creating /dev/pts mountpoint".to_string(),
            });
        }
    }
    // gid=5 mirrors the standard `tty` group on most distros. mode=620
    // matches what util-linux mounts at boot. `nosuid,noexec` lead so
    // `mount_fs`'s option-folding reproduces the exact same MS_NOSUID|
    // MS_NOEXEC flags the direct `mount(2)` call set; the remaining
    // tokens forward verbatim as the devpts `data` string.
    let opts = "nosuid,noexec,newinstance,ptmxmode=0666,mode=0620,gid=5";
    match fs.mount(Some(Path::new("devpts")), target, "devpts", opts) {
        Ok(()) => {}
        // Already mounted; benign. `mount_fs` wraps the raw errno in
        // `NmblError::Mount`, so match the EBUSY through that shape.
        Err(NmblError::Mount {
            source: Errno::EBUSY,
            ..
        }) => {}
        Err(e) => return Err(e),
    }
    // A `newinstance` mount populates `/dev/pts/ptmx` but leaves
    // `/dev/ptmx` (which libc's openpty uses) untouched. Symlink it.
    // Best-effort: pre-existing `/dev/ptmx` is left alone. FsOps has no
    // symlink op, so this stays a genuine best-effort `symlink(2)`; a
    // dry-run reaches this only after a real devpts mount it cannot
    // perform, so it implicitly no-ops symlink creation along that path.
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
