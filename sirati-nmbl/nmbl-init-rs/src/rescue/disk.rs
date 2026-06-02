//! External rescue squashfs path (PLAN.md §Option 2 "Rust-side flow",
//! Phase C.1).
//!
//! When `rescue.mode = "external"` the rescue toolkit lives on the
//! boot partition as `nmbl-rescue.sfs`. This module performs the full
//! lazy-mount dance:
//!
//! 1. Locate the squashfs via [`super::locate_sfs`].
//! 2. Allocate a free loop minor via
//!    [`crate::sys::loopdev::allocate_loop_device`].
//! 3. Open `/dev/loopN` read-write (LOOP_CONFIGURE refuses an RO fd
//!    even when the backing is RO; the RO-ness is set independently
//!    via `LO_FLAGS_READ_ONLY`).
//! 4. Open the squashfs `O_RDONLY | CLOEXEC` and feed both fds to
//!    [`crate::sys::loopdev::configure_loop_device`].
//! 5. Mount `/dev/loopN` read-only at `/run/nmbl-rescue/lower`, layer a
//!    tmpfs upper over it, and mount an `overlay` at `/rescue` so the
//!    rescue root is writable (live-CD style — the squashfs itself
//!    stays read-only, all writes land in the tmpfs upper).
//! 6. Hand the writable `/rescue` overlay to
//!    [`crate::rescue::child::run_external_rescue_child`], which forks a
//!    chrooted child rooted at `/rescue` while NMBL stays PID 1 on the
//!    initramfs rootfs.
//!
//! Every failure point is wrapped in [`NmblError::Rescue`] with a
//! `stage` string the emergency-shell banner surfaces verbatim.

use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno as RustixErrno;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::loopdev::{allocate_loop_device, configure_loop_device, open_loop_device};
use crate::sys::mount::mount_fs;

/// Mountpoint where the writable rescue overlay is staged before the
/// root switch. Lives at the initramfs root because `/rescue` is
/// unlikely to collide with anything the initramfs created.
pub(crate) const RESCUE_MOUNT: &str = "/rescue";

/// Read-only squashfs lower layer of the rescue overlay.
const RESCUE_LOWER: &str = "/run/nmbl-rescue/lower";

/// tmpfs holding the overlay's writable upper + work dirs.
const RESCUE_RW: &str = "/run/nmbl-rescue/rw";

/// Overlay upper dir (where all writes to `/rescue` land).
const RESCUE_UPPER: &str = "/run/nmbl-rescue/rw/upper";

/// Overlay work dir (overlayfs scratch space; must share a filesystem
/// with the upper dir, hence both live in the same tmpfs).
const RESCUE_WORK: &str = "/run/nmbl-rescue/rw/work";

/// Loop-mount the rescue squashfs and layer a writable overlay at
/// `/rescue`, returning the mount path. Caller is responsible for
/// dropping the live boot console
/// (so the backend's Drop impl restores VT text mode + termios) and
/// then calling [`crate::rescue::child::run_external_rescue_child`] with
/// the returned path — splitting the steps this way keeps the fork +
/// chroot out of band from the (potentially-failing) mount work, so the
/// dispatcher can fall through to network-rescue without losing the
/// console.
///
/// `cause` is the error that triggered the rescue. It is logged
/// before the loop-mount dance so the operator can see what failed
/// even if the squashfs mount itself misbehaves.
pub fn prepare_disk_rescue(config: &Config, cause: &NmblError) -> Result<&'static Path> {
    let sfs_path = super::locate_sfs(config)?;
    eprintln!(
        "[nmbl] external rescue: mounting {} (triggered by: {})",
        sfs_path.display(),
        cause
    );

    if !sfs_path.exists() {
        return Err(NmblError::Rescue {
            stage: "locate-sfs",
            source: Box::new(NmblError::Io {
                source: io::Error::from(io::ErrorKind::NotFound),
                context: format!(
                    "rescue squashfs {} not found on boot partition",
                    sfs_path.display(),
                ),
            }),
        });
    }

    // loop + squashfs are NO LONGER eagerly loaded on every boot (they
    // were dropped from NMBL's runtime explicit-load list in
    // lib/options.nix). NMBL needs them only to loop-mount this blob, so
    // we load them ON DEMAND right here — the single choke point every
    // rescue entry path (interactive emergency + force_on_boot) funnels
    // through before touching /dev/loop-control. Their .ko still ship in
    // the initramfs (lib/config.nix `rescueDiskModules` keeps them in the
    // staged closure), so this load resolves. Best-effort: if the normal
    // boot path already loaded them (e.g. root on squashfs) this is a
    // cheap no-op; on failure we log and proceed so `allocate_loop_device`
    // surfaces the real error with its own `loop-alloc` stage.
    ensure_loop_squashfs_modules(config);

    let index = allocate_loop_device().map_err(|source| NmblError::Rescue {
        stage: "loop-alloc",
        source: Box::new(source),
    })?;

    let loop_fd = open_loop_device(index, true).map_err(|source| NmblError::Rescue {
        stage: "loop-open",
        source: Box::new(source),
    })?;

    let sfs_fd = rustix::fs::open(&sfs_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| NmblError::Rescue {
            stage: "sfs-open",
            source: Box::new(NmblError::Io {
                source: io_error_from_rustix(e),
                context: format!("opening {}", sfs_path.display()),
            }),
        })?;

    configure_loop_device(&loop_fd, &sfs_fd, true).map_err(|source| NmblError::Rescue {
        stage: "loop-configure",
        source: Box::new(source),
    })?;

    let loop_dev = PathBuf::from(format!("/dev/loop{index}"));
    mount_overlay_root(&loop_dev)?;

    Ok(Path::new(RESCUE_MOUNT))
}

/// Modules NMBL ITSELF needs to stage the writable rescue root before
/// switch_root: `loop` for `LOOP_CTL_GET_FREE` on `/dev/loop-control`,
/// `squashfs` for the read-only lower mount, and `overlay` for the
/// live-CD `/rescue` overlay ([`mount_overlay_root`]) layered over that
/// lower with a tmpfs upper. All three are loaded into NMBL's running
/// kernel here — the rescue `/init` runs only AFTER switch_root, far too
/// late to provide the `overlay` this dispatch already depends on.
const RESCUE_DISK_MODULES: [&str; 3] = ["loop", "squashfs", "overlay"];

/// Load `loop` + `squashfs` + `overlay` on demand, immediately before the
/// loop-mount + overlay dance.
///
/// These are deliberately absent from NMBL's eager boot-time module list
/// (lib/options.nix) — NMBL needs them only on a rescue dispatch, at most
/// once per boot, so loading them on every boot just to support a rescue
/// that usually never fires would be wasteful. Their `.ko` still ship in
/// the initramfs (lib/config.nix `rescueDiskModules`), so this resolves.
/// Idempotent: if the normal boot path already loaded them (e.g. a
/// squashfs root), the inner loader reports `AlreadyLoaded` and this is a
/// no-op; likewise the rescue `/init`'s own later `modprobe overlay`
/// becomes a no-op once this has run.
///
/// Best-effort by design — any error is logged, not propagated: if the
/// modules are genuinely missing, the subsequent `allocate_loop_device`,
/// squashfs `mount`, or `overlay` `mount` fails with a far more
/// actionable, stage-tagged [`NmblError::Rescue`] than a premature bail
/// here would give.
fn ensure_loop_squashfs_modules(config: &Config) {
    let modules: Vec<String> = RESCUE_DISK_MODULES.iter().map(|m| m.to_string()).collect();
    if let Err(err) = crate::modules::load_modules(
        &config.kernel_modules.modules_dir,
        &modules,
        &config.kernel_modules.blacklist,
    ) {
        eprintln!(
            "[nmbl] external rescue: on-demand load of loop+squashfs failed \
             ({err}); continuing — the loop-mount will surface the real error"
        );
    }
}

/// Build the writable rescue root as a live-CD-style overlay: a
/// read-only squashfs lower (the `loop_dev` allocated by the caller), a
/// tmpfs upper, and an `overlay` mounted at `/rescue`. The chrooted
/// rescue path (its `/init`) needs to write into the root — populate
/// `/dev`, create `/nmbl-root`, `/mnt`, … — which a bare read-only
/// squashfs can't support; the tmpfs upper absorbs every write while
/// the image stays untouched.
///
/// Shared by the disk-rescue path and the network-rescue path (which
/// loop-mounts a memfd-backed squashfs) so both land on an identical
/// writable `/rescue` the chrooted child runner can use.
///
/// All mount failures are wrapped in [`NmblError::Rescue`] with the
/// `mount-rescue` stage so the emergency banner reads the same as the
/// previous read-only path.
pub(crate) fn mount_overlay_root(loop_dev: &Path) -> Result<()> {
    let wrap = |source: NmblError| NmblError::Rescue {
        stage: "mount-rescue",
        source: Box::new(source),
    };

    // mkdir -p every intermediate dir before mounting onto it.
    for dir in [RESCUE_LOWER, RESCUE_RW, RESCUE_MOUNT] {
        ensure_dir(Path::new(dir)).map_err(wrap)?;
    }

    // 1. squashfs (read-only) at the overlay lower layer.
    mount_fs(Some(loop_dev), Path::new(RESCUE_LOWER), "squashfs", "ro").map_err(wrap)?;

    // 2. tmpfs for the writable upper + work dirs, then carve both out.
    mount_fs(None, Path::new(RESCUE_RW), "tmpfs", "nosuid,nodev,mode=755").map_err(wrap)?;
    ensure_dir(Path::new(RESCUE_UPPER)).map_err(wrap)?;
    ensure_dir(Path::new(RESCUE_WORK)).map_err(wrap)?;

    // 3. overlay at /rescue — writes land in the tmpfs upper.
    let data = format!("lowerdir={RESCUE_LOWER},upperdir={RESCUE_UPPER},workdir={RESCUE_WORK}");
    mount_fs(
        Some(Path::new("overlay")),
        Path::new(RESCUE_MOUNT),
        "overlay",
        &data,
    )
    .map_err(wrap)?;

    Ok(())
}

/// Create `path` (and parents) on the rescue mountpoint side. Mirrors
/// the `ensure_dir` helper from `src/mount.rs`; we keep a copy here
/// because that one is module-private and the rescue path wants a
/// path-aware context string.
fn ensure_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(NmblError::Io {
            source: e,
            context: format!("creating {}", path.display()),
        }),
    }
}

/// Map a `rustix::io::Errno` to `std::io::Error` so it can ride inside
/// `NmblError::Io`. Same shape as the helper in `sys::loopdev`.
fn io_error_from_rustix(e: RustixErrno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use crate::rescue::RescueMode;
    use crate::sys::loopdev::LOOP_CONTROL_PATH;

    fn cfg_with_sfs(sfs: Option<PathBuf>, mountpoint: Option<PathBuf>) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = RescueConfig {
            mode: RescueMode::External,
            sfs_path: sfs,
            ..RescueConfig::default()
        };
        c.runtime_boot_mountpoint = mountpoint;
        c
    }

    #[test]
    fn prepare_disk_rescue_missing_sfs_is_locate_sfs_error() {
        // Point at a path we know cannot exist so the locate-sfs guard
        // fires before we touch /dev/loop-control. This lets the test
        // assert error shape on every host, not just ones with a loop
        // control node available.
        let dir = tempfile::tempdir().expect("tempdir");
        let bogus_name = format!(
            "nmbl-rescue-missing-{}-{}.sfs",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let bogus = dir.path().join(&bogus_name);
        assert!(
            !bogus.exists(),
            "test precondition: bogus path must be absent"
        );

        let cause = NmblError::ConfigInvalid {
            reason: "synthetic".to_string(),
            context: "test".to_string(),
        };
        let cfg = cfg_with_sfs(
            Some(PathBuf::from(&bogus_name)),
            Some(dir.path().to_path_buf()),
        );
        let err = prepare_disk_rescue(&cfg, &cause).expect_err("missing sfs must error");
        match err {
            NmblError::Rescue { stage, source } => {
                assert_eq!(stage, "locate-sfs");
                match *source {
                    NmblError::Io { context, .. } => {
                        assert!(context.contains(&bogus.display().to_string()), "{context}");
                    }
                    other => panic!("expected Io inside Rescue, got {other:?}"),
                }
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
    }

    #[test]
    fn prepare_disk_rescue_without_mountpoint_is_locate_sfs_error() {
        // Legacy embedded-config mode: no runtime boot mountpoint is set,
        // so the locate-sfs guard must short-circuit before any disk I/O.
        let cause = NmblError::ConfigInvalid {
            reason: "synthetic".to_string(),
            context: "test".to_string(),
        };
        let cfg = cfg_with_sfs(None, None);
        let err = prepare_disk_rescue(&cfg, &cause)
            .expect_err("missing runtime boot mountpoint must error");
        match err {
            NmblError::Rescue { stage, source } => {
                assert_eq!(stage, "locate-sfs");
                assert!(
                    matches!(*source, NmblError::ConfigInvalid { .. }),
                    "expected ConfigInvalid inside Rescue, got {source:?}",
                );
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn prepare_disk_rescue_no_loop_control_is_loop_alloc_error() {
        // Mirrors the skip pattern from sys::loopdev::tests: only
        // exercise the loop-alloc arm on hosts that lack
        // /dev/loop-control. Where the node exists the unprivileged
        // sandbox would still take us through allocate_loop_device,
        // which we don't want to depend on.
        if Path::new(LOOP_CONTROL_PATH).exists() {
            eprintln!("skipping: {LOOP_CONTROL_PATH} present");
            return;
        }

        // Stage a real squashfs-shaped tempfile so the locate-sfs guard
        // passes and we reach the loop-alloc step.
        let dir = tempfile::tempdir().expect("tempdir");
        let sfs = dir.path().join("nmbl-rescue.sfs");
        std::fs::write(&sfs, b"placeholder").expect("write sfs");

        let cfg = cfg_with_sfs(
            Some(PathBuf::from("nmbl-rescue.sfs")),
            Some(dir.path().to_path_buf()),
        );
        let cause = NmblError::ConfigInvalid {
            reason: "synthetic".to_string(),
            context: "test".to_string(),
        };
        let err = prepare_disk_rescue(&cfg, &cause).expect_err("no loop-control must error");
        match err {
            NmblError::Rescue { stage, .. } => {
                assert_eq!(stage, "loop-alloc");
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
    }

    #[test]
    fn ensure_dir_handles_existing_path() {
        ensure_dir(Path::new("/")).expect("root always exists");
    }
}
