//! Mount namespace setup for the chrooted external rescue child.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::mount::{make_shared, mount_fs, umount};
use crate::{nmbl_info, nmbl_warn};

/// PID 1's own mountpoint that mirrors the child's `/rescue/mnt` via a
/// shared subtree, so PID 1 observes whatever the child mounts there.
const PID1_MNT: &str = "/mnt";
/// The chroot's `/mnt` (becomes `/mnt` after chroot); made a shared
/// subtree peer of [`PID1_MNT`].
const CHILD_MNT: &str = "/rescue/mnt";
/// NMBL's own root, bind-mounted into the chroot. Becomes `/nmbl-root`
/// after chroot, exposing the TUI socket at
/// `/nmbl-root/nmbl-run/tui.sock` (matches the rescue-sfs contract).
const CHILD_NMBL_ROOT: &str = "/rescue/nmbl-root";
/// Temporary bind that preserves the bootstrap boot mount before the shared
/// rescue `/mnt` subtree covers PID 1's original `/mnt`.
const CHILD_BOOT: &str = "/rescue/nmbl-boot";
/// Prepared regular file in the rescue image that receives a bind mount of
/// NMBL's running executable for remote TUI clients.
const CHILD_NMBL_BIN: &str = "/rescue/bin/nmbl";

/// One bind/shared-subtree step in the pre-fork mount plan. A pure
/// description so the sequence is unit-testable without privileges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MountStep {
    /// `mkdir -p path` (idempotent).
    MkDir(&'static str),
    /// `mount --bind src dst` (`MS_BIND`).
    Bind {
        src: &'static str,
        dst: &'static str,
    },
    /// `mount --rbind src dst` (`MS_BIND | MS_REC`).
    RBind {
        src: &'static str,
        dst: &'static str,
    },
    /// `mount --make-shared target` (`MS_SHARED`, no fstype/source).
    MakeShared(&'static str),
}

/// The pre-fork mount plan, in execution order. Pure so it can be
/// asserted on in tests:
///
/// ```text
/// mkdir -p /mnt /rescue/nmbl-root /rescue/mnt
/// bind        /rescue/mnt -> /rescue/mnt   (self-bind so it is a mount)
/// make-shared /rescue/mnt                  (MS_SHARED)
/// rbind       /rescue/mnt -> /mnt          (PID 1 sees child mounts)
/// rbind       /           -> /rescue/nmbl-root (expose NMBL root + socket)
/// ```
pub(crate) fn mount_plan() -> Vec<MountStep> {
    vec![
        MountStep::MkDir(PID1_MNT),
        MountStep::MkDir(CHILD_NMBL_ROOT),
        MountStep::MkDir(CHILD_MNT),
        MountStep::Bind {
            src: CHILD_MNT,
            dst: CHILD_MNT,
        },
        MountStep::MakeShared(CHILD_MNT),
        MountStep::RBind {
            src: CHILD_MNT,
            dst: PID1_MNT,
        },
        MountStep::RBind {
            src: "/",
            dst: CHILD_NMBL_ROOT,
        },
        MountStep::Bind {
            src: "/init",
            dst: CHILD_NMBL_BIN,
        },
    ]
}

/// The teardown plan applied after the child exits, in order. Lazy
/// `MNT_DETACH` is acceptable for the recursive binds (the task spec):
/// unmount PID 1's `/mnt` first (the propagation target), then the
/// child's `/rescue/mnt`, then NMBL's bind at `/rescue/nmbl-root`. Pure
/// for the same reason as [`mount_plan`].
pub(crate) fn umount_plan() -> Vec<&'static str> {
    vec![CHILD_NMBL_BIN, PID1_MNT, CHILD_MNT, CHILD_NMBL_ROOT]
}

/// Execute [`mount_plan`] with safe nix/std wrappers (parent side, PID 1
/// — runs before `fork`). Any failure aborts with a wrapped
/// [`NmblError::Rescue`] so the recovery flow can surface it.
pub(crate) fn child_boot_target(runtime_boot: &Path) -> PathBuf {
    CHILD_NMBL_ROOT
        .parse::<PathBuf>()
        .expect("static absolute path")
        .join(runtime_boot.strip_prefix("/").unwrap_or(runtime_boot))
}

pub(super) fn apply_mount_plan(config: &Config) -> Result<()> {
    let wrap = |source: NmblError| NmblError::Rescue {
        stage: "rescue-child-mount",
        source: Box::new(source),
    };
    if let Some(runtime_boot) = config.runtime_boot_mountpoint.as_deref() {
        ensure_dir(Path::new(CHILD_BOOT)).map_err(wrap)?;
        mount_fs(Some(runtime_boot), Path::new(CHILD_BOOT), "none", "bind").map_err(wrap)?;
    }
    for step in mount_plan() {
        match step {
            MountStep::MkDir(p) => ensure_dir(Path::new(p)).map_err(wrap)?,
            MountStep::Bind { src, dst } => {
                mount_fs(Some(Path::new(src)), Path::new(dst), "none", "bind").map_err(wrap)?;
            }
            MountStep::RBind { src, dst } => {
                mount_fs(Some(Path::new(src)), Path::new(dst), "none", "rbind").map_err(wrap)?;
            }
            MountStep::MakeShared(p) => make_shared(Path::new(p)).map_err(wrap)?,
        }
    }
    if let Some(runtime_boot) = config.runtime_boot_mountpoint.as_deref() {
        let target = child_boot_target(runtime_boot);
        ensure_dir(&target).map_err(wrap)?;
        mount_fs(Some(Path::new(CHILD_BOOT)), &target, "none", "bind").map_err(wrap)?;
    }
    Ok(())
}

/// Tear down the binds set up by [`apply_mount_plan`]. Best-effort: a
/// failed unmount is logged, never propagated — the recovery flow must
/// proceed regardless.
pub(super) fn teardown_mounts(config: &Config) {
    use nix::mount::MntFlags;
    if let Some(runtime_boot) = config.runtime_boot_mountpoint.as_deref() {
        let target = child_boot_target(runtime_boot);
        let _ = umount(&target, MntFlags::MNT_DETACH);
    }
    for target in umount_plan() {
        match umount(Path::new(target), MntFlags::MNT_DETACH) {
            Ok(()) => nmbl_info!("rescue child: detached {target}"),
            Err(e) => nmbl_warn!("rescue child: could not detach {target}: {e}"),
        }
    }
    let _ = umount(Path::new(CHILD_BOOT), MntFlags::MNT_DETACH);
}

/// Create `path` (and parents) idempotently. Mirrors `disk::ensure_dir`.
fn ensure_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(NmblError::Io {
            source: e,
            context: format!("creating {}", path.display()),
        }),
    }
}
