//! Read-only loop-mount of a verified driver image (#23, step 2).
//!
//! After [`super::verify`] has accepted the pinned fd, bind THAT SAME fd to a
//! free loop device read-only (reusing [`crate::sys::loopdev::loop_bind_ro`],
//! #22) and mount the squashfs `ro` at a per-image mountpoint. No reopen: the
//! verified fd is the fd the kernel reads through (FIX-02). The squashfs is the
//! mount source and `ro` the only option — a driver image is immutable.
//!
//! [`teardown_image`] is the per-image half of the normal-path teardown
//! (`detach_all_driver_images`): lazy-unmount then `LOOP_CLR_FD`, best-effort.

use std::os::fd::BorrowedFd;
use std::path::{Path, PathBuf};

use nix::mount::MntFlags;

use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::loopdev::{loop_bind_ro, open_loop_device};
use crate::sys::ops::FsOps;

/// Root under which each driver image gets its own numbered mountpoint.
const DRIVER_MOUNT_ROOT: &str = "/run/nmbl-driver-images";

/// What a successful [`mount_squashfs_ro`] produced.
#[derive(Debug)]
pub(super) struct Mounted {
    /// The bound `/dev/loopN` minor.
    pub loop_index: u32,
    /// Where the squashfs is mounted read-only.
    pub mountpoint: PathBuf,
}

/// Loop-bind `image_fd` read-only and mount its squashfs `ro` at
/// `<DRIVER_MOUNT_ROOT>/<index>`.
///
/// `index` is the image's position in the declared list — it only namespaces
/// the mountpoint so concurrent images do not collide. Returns the loop minor +
/// mountpoint for the handle/teardown.
///
/// # Errors
/// [`NmblError::DriverImage`] tagged with the failing stage: `loop-alloc` /
/// `loop-open` / `loop-configure` (re-wrapped verbatim from
/// [`loop_bind_ro`]'s [`crate::sys::loopdev::LoopBindError`]), `mkdir`, or
/// `mount`.
pub(super) fn mount_squashfs_ro(
    ops: &mut impl FsOps,
    image_fd: BorrowedFd<'_>,
    index: usize,
) -> Result<Mounted> {
    // (a) Shared allocate→open→configure dance, read-only (#22). Re-wrap its
    // stage tag verbatim into a DriverImage so the banner names the exact step.
    let loop_index = loop_bind_ro(&image_fd).map_err(|e| NmblError::DriverImage {
        stage: e.stage,
        source: e.source,
    })?;

    let mountpoint = PathBuf::from(format!("{DRIVER_MOUNT_ROOT}/{index}"));
    let loop_dev = PathBuf::from(format!("/dev/loop{loop_index}"));

    // (b) mkdir -p the per-image mountpoint before mounting onto it.
    ensure_dir(ops, &mountpoint)?;

    // (c) Mount the squashfs read-only. The driver image is immutable — there
    // is no writable overlay (unlike rescue): NMBL only reads `.ko` + firmware.
    ops.mount(Some(&loop_dev), &mountpoint, "squashfs", "ro")
        .map_err(|source| NmblError::DriverImage {
            stage: "mount",
            source: Box::new(source),
        })?;

    Ok(Mounted {
        loop_index,
        mountpoint,
    })
}

/// Best-effort per-image teardown for the normal (non-shell) path: lazily
/// unmount the squashfs, then release the loop binding with `LOOP_CLR_FD`.
///
/// Failures are logged, never propagated — teardown runs on the success path
/// right before kexec, where a stuck unmount must not strand the boot. The lazy
/// `MNT_DETACH` unmount detaches the subtree even if something still references
/// it; the kernel also auto-releases the loop binding once the mount is gone,
/// so the explicit `LOOP_CLR_FD` is belt-and-braces.
#[cfg(feature = "secure-boot")]
pub(super) fn teardown_image(ops: &mut impl FsOps, loop_index: u32, mountpoint: &Path) {
    if let Err(e) = ops.umount(mountpoint, MntFlags::MNT_DETACH) {
        nmbl_warn!(
            "driver-image teardown: lazy unmount of {} failed: {}",
            mountpoint.display(),
            e
        );
    }
    // Open the loop node RW to issue LOOP_CLR_FD; on failure the kernel will
    // still have auto-released the binding when the lazy unmount completed.
    match open_loop_device(loop_index, true) {
        Ok(loop_fd) => {
            if let Err(e) = crate::sys::loopdev::detach_loop_device(&loop_fd) {
                nmbl_warn!(
                    "driver-image teardown: LOOP_CLR_FD on loop{} failed: {} \
                     (kernel auto-releases on unmount)",
                    loop_index,
                    e
                );
            }
        }
        Err(e) => nmbl_warn!(
            "driver-image teardown: reopening loop{} for detach failed: {}",
            loop_index,
            e
        ),
    }
}

/// `mkdir -p` the mountpoint through the [`FsOps`] seam, tolerating an
/// already-existing dir. Wrapped as a `mkdir`-stage [`NmblError::DriverImage`].
fn ensure_dir(ops: &mut impl FsOps, path: &Path) -> Result<()> {
    match ops.ensure_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(NmblError::DriverImage {
            stage: "mkdir",
            source: Box::new(NmblError::Io {
                source: e,
                context: format!("creating driver-image mountpoint {}", path.display()),
            }),
        }),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::sys::loopdev::LOOP_CONTROL_PATH;
    use std::os::fd::AsFd;

    #[test]
    fn ensure_dir_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("a/b/c");
        let mut ops = crate::sys::ops::RealSys::sync_only();
        ensure_dir(&mut ops, &target).expect("first mkdir");
        // Second call on the now-existing dir must still succeed.
        ensure_dir(&mut ops, &target).expect("idempotent mkdir");
        assert!(target.is_dir());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn mount_without_loop_control_is_loop_alloc_error() {
        // On a host lacking /dev/loop-control the very first step (loop-alloc)
        // fails, and its stage tag must surface verbatim through the DriverImage
        // re-wrap — proving the bind failure routes to the banner correctly and
        // that NOTHING is mounted on a bind failure.
        if Path::new(LOOP_CONTROL_PATH).exists() {
            eprintln!("skipping: {LOOP_CONTROL_PATH} present");
            return;
        }
        let backing = tempfile::tempfile().expect("tempfile");
        let mut ops = crate::sys::ops::RealSys::sync_only();
        let err = mount_squashfs_ro(&mut ops, backing.as_fd(), 0)
            .expect_err("no loop-control must error");
        match err {
            NmblError::DriverImage { stage, .. } => assert_eq!(stage, "loop-alloc"),
            other => panic!("expected DriverImage(loop-alloc), got {other:?}"),
        }
    }
}
