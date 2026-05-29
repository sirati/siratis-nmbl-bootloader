//! Switch-root dance and rescue-shell execve construction.

use std::ffi::CString;
use std::io;
use std::path::Path;

use nix::mount::MsFlags;
use nix::unistd::{chdir, chroot};

use crate::error::{NmblError, Result};
use crate::terminal::TerminalAction;

/// Switch from the initramfs root into `new_root` and produce a
/// [`TerminalAction::Execve`] for `/bin/sh`.
///
/// Mirrors the busybox `switch_root(8)` dance: `chdir(new_root)` →
/// `mount --move . /` (MS_MOVE) → `chroot(.)` → `chdir(/)`. The
/// actual `execve` is deferred to the dispatcher in `main` so any
/// console handles still on the stack have been dropped first.
///
/// Replaces `pivot_root(2)`, which always returns `EINVAL` when the
/// outgoing root is the initramfs rootfs pseudo-filesystem. After
/// MS_MOVE the initramfs is detached and no longer reachable via any
/// path.
pub(crate) fn switch_root_and_exec(new_root: &Path, entrypoint: &Path) -> Result<TerminalAction> {
    // Step 1: cd into the new root (the mounted squashfs).
    chdir(new_root).map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: format!("chdir({})", new_root.display()),
        }),
    })?;

    // Step 2: Move the new-root mount to /, replacing the initramfs
    // rootfs. MS_MOVE reassigns the mount point atomically.
    nix::mount::mount(
        Some("."),
        "/",
        Option::<&str>::None,
        MsFlags::MS_MOVE,
        Option::<&str>::None,
    )
    .map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: "mount --move . /".to_string(),
        }),
    })?;

    // Step 3: chroot into the new `/` (the squashfs).
    chroot(".").map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: "chroot(.)".to_string(),
        }),
    })?;

    // Step 4: Update the cwd to the new root.
    chdir("/").map_err(|source| NmblError::Rescue {
        stage: "switch-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: "chdir(/) after chroot".to_string(),
        }),
    })?;

    // Step 5: Populate /dev in the new root. The MS_MOVE above detached
    // the initramfs devtmpfs, so the rescue root's /dev is an empty
    // mountpoint with no /dev/console. The dispatcher in `main` re-opens
    // /dev/console to redirect the child's stdio before execve, and the
    // full-system entrypoint (`/init`) also does its own `exec bash <
    // /dev/console` — both need a populated /dev. Mount devtmpfs here so
    // the device nodes exist before either consumer runs. Non-fatal: the
    // entrypoint's own `mount -t devtmpfs ... || true` tolerates a stale
    // mount, and the dispatcher's stdio redirect is soft on this path.
    mount_dev_in_new_root();

    build_rescue_shell_action(entrypoint)
}

/// Mount `devtmpfs` at `/dev` in the freshly switched-root rescue root
/// so `/dev/console` (and friends) exist before the dispatcher's stdio
/// redirect and before the rescue entrypoint runs.
///
/// Best-effort by design: any failure is logged at warn level and the
/// caller proceeds. The full-system `/init` re-mounts devtmpfs itself
/// (`mount -t devtmpfs ... || true`), and the busybox image's stdio
/// only needs the node to exist, so a partial setup here never strands
/// the operator. `EBUSY` (already mounted) is treated as success.
fn mount_dev_in_new_root() {
    use crate::{nmbl_info, nmbl_warn};

    let dev = Path::new("/dev");
    match std::fs::create_dir_all(dev) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => nmbl_warn!("rescue: could not create /dev in new root: {e}"),
    }
    match crate::sys::mount::mount_fs(None, dev, "devtmpfs", "mode=755,nosuid") {
        Ok(()) => nmbl_info!("rescue: mounted /dev in new root"),
        Err(NmblError::Mount {
            source: nix::errno::Errno::EBUSY,
            ..
        }) => nmbl_info!("rescue: /dev already mounted in new root (EBUSY)"),
        Err(e) => nmbl_warn!("rescue: could not mount /dev in new root: {e}"),
    }
}

/// Construct the [`TerminalAction::Execve`] for the rescue entrypoint
/// inside the freshly switched-root rescue root with a minimal
/// `TERM=linux` + `PATH` environment. Shared by the disk and network
/// rescue paths. The entrypoint is `config.rescue.entrypoint`: the flat
/// busybox image leaves it at the default `/bin/sh`; the full recovery
/// system pins it to `/init` (a bash PID-1 script). No banner: the
/// rescue UI has already taken the operator through its own screens, so
/// a second emergency banner would be redundant.
pub(super) fn build_rescue_shell_action(entrypoint: &Path) -> Result<TerminalAction> {
    let entry_bytes = entrypoint.as_os_str().as_encoded_bytes();
    let path_c = CString::new(entry_bytes).map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "rescue entrypoint path contains interior NUL".to_string(),
            context: format!("preparing execve of {}", entrypoint.display()),
        }),
    })?;
    // argv0 = basename of the entrypoint (e.g. "sh" or "init"), falling
    // back to the full path if it has no file name component.
    let argv0_bytes: Vec<u8> = entrypoint
        .file_name()
        .map(|n| n.as_encoded_bytes().to_vec())
        .unwrap_or_else(|| entry_bytes.to_vec());
    let argv0_c = CString::new(argv0_bytes).map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "rescue argv0 contains interior NUL".to_string(),
            context: format!("preparing execve of {}", entrypoint.display()),
        }),
    })?;
    let term_c = CString::new("TERM=linux").map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "TERM environment string contains interior NUL".to_string(),
            context: format!("preparing execve of {}", entrypoint.display()),
        }),
    })?;
    let path_env_c =
        CString::new("PATH=/bin:/sbin:/usr/bin:/usr/sbin").map_err(|_| NmblError::Rescue {
            stage: "exec-shell",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "PATH environment string contains interior NUL".to_string(),
                context: format!("preparing execve of {}", entrypoint.display()),
            }),
        })?;

    Ok(TerminalAction::Execve {
        path: path_c,
        argv: vec![argv0_c],
        env: vec![term_c, path_env_c],
        banner: None,
        rescue_handoff: true,
    })
}
