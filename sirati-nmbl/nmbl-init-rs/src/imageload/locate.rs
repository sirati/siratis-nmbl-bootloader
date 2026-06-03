//! Resolve a declared driver image's on-disk paths and open its fd (#23).
//!
//! [`crate::config::DriverImageSpec`] carries `path` / `sig_path` RELATIVE TO
//! THE BOOT PARTITION ROOT (mirroring `rescue.sfs_path`). This module joins
//! them against [`crate::config::Config::runtime_boot_mountpoint`] — the
//! runtime mountpoint Phase 0.5 records after mounting the boot partition — and
//! opens the image `O_RDONLY | CLOEXEC` ONCE so the rest of the pipeline shares
//! a single pinned fd (FIX-02).

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::config::{Config, DriverImageSpec};
use crate::error::{NmblError, Result};
use crate::sys::ops::FsOps;

/// A driver image's resolved absolute paths.
#[derive(Debug)]
pub(super) struct ResolvedImage {
    /// Absolute path of the squashfs on the (mounted) boot partition.
    pub image_path: PathBuf,
    /// Absolute path of the detached signature sidecar.
    pub sig_path: PathBuf,
}

/// Resolve `spec`'s boot-relative `path` + `sig_path` against the runtime boot
/// mountpoint.
///
/// # Errors
/// [`NmblError::DriverImage`] (`stage = "locate"`) when no boot partition is
/// mounted (legacy embedded-config mode), or when `path`/`sig_path` is empty.
pub(super) fn resolve_image(config: &Config, spec: &DriverImageSpec) -> Result<ResolvedImage> {
    let mountpoint = config.runtime_boot_mountpoint.as_deref().ok_or_else(|| {
        locate_err(
            "driver images require bootstrap mode: the runtime boot mountpoint is only known after \
             Phase 0.5 mounts the boot partition, but this NMBL instance is running in legacy \
             embedded-config mode",
            "resolving driver_images paths against the runtime boot mountpoint",
        )
    })?;

    if spec.path.as_os_str().is_empty() {
        return Err(locate_err(
            "driver image declared with an empty path",
            "resolving driver_images.path",
        ));
    }
    if spec.sig_path.as_os_str().is_empty() {
        return Err(locate_err(
            "driver image declared with an empty sig_path",
            "resolving driver_images.sig_path",
        ));
    }

    Ok(ResolvedImage {
        image_path: mountpoint.join(strip_leading_slash(&spec.path)),
        sig_path: mountpoint.join(strip_leading_slash(&spec.sig_path)),
    })
}

/// Open the resolved image read-only through the [`FsOps`] seam — the single
/// pinned fd the whole verify→mount→load pipeline shares (FIX-02). Never
/// reopened. Routing through `open_ro` lets `--validate-initrm` open the
/// closure-mapped bytes (side-effect-free) while the real boot opens the
/// on-disk squashfs; either way the fd verified is the fd mounted.
///
/// # Errors
/// [`NmblError::DriverImage`] (`stage = "open"`) wrapping the underlying
/// `NmblError::Io` when the image cannot be opened.
pub(super) fn open_image_ro(ops: &mut impl FsOps, resolved: &ResolvedImage) -> Result<File> {
    ops.open_ro(&resolved.image_path)
        .map_err(|e| NmblError::DriverImage {
            stage: "open",
            source: Box::new(NmblError::Io {
                source: e,
                context: format!("opening driver image {}", resolved.image_path.display()),
            }),
        })
}

/// Build a `locate`-stage [`NmblError::DriverImage`] wrapping a
/// `ConfigInvalid`. Shared so the empty-path / no-mountpoint cases read the
/// same.
fn locate_err(reason: &str, context: &str) -> NmblError {
    NmblError::DriverImage {
        stage: "locate",
        source: Box::new(NmblError::ConfigInvalid {
            reason: reason.to_string(),
            context: context.to_string(),
        }),
    }
}

/// Strip a single leading `/` so [`Path::join`] keeps the mountpoint instead of
/// replacing it (mirrors `rescue::locate::strip_leading_slash`).
fn strip_leading_slash(p: &Path) -> &Path {
    p.strip_prefix("/").unwrap_or(p)
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

    fn spec(path: &str, sig: &str) -> DriverImageSpec {
        DriverImageSpec {
            path: PathBuf::from(path),
            sig_path: PathBuf::from(sig),
            modules: Vec::new(),
            blacklist: Vec::new(),
        }
    }

    #[test]
    fn resolve_joins_against_mountpoint_and_strips_leading_slash() {
        let mut cfg = Config::recovery_default();
        cfg.runtime_boot_mountpoint = Some(PathBuf::from("/run/nmbl-boot"));
        // A leading slash on the relative path must NOT replace the mountpoint.
        let resolved =
            resolve_image(&cfg, &spec("/nmbl/d.sfs", "nmbl/d.sfs.sig")).expect("resolve");
        assert_eq!(
            resolved.image_path,
            PathBuf::from("/run/nmbl-boot/nmbl/d.sfs")
        );
        assert_eq!(
            resolved.sig_path,
            PathBuf::from("/run/nmbl-boot/nmbl/d.sfs.sig")
        );
    }

    #[test]
    fn resolve_without_mountpoint_is_locate_error() {
        let cfg = Config::recovery_default(); // runtime_boot_mountpoint = None
        let err = resolve_image(&cfg, &spec("nmbl/d.sfs", "nmbl/d.sfs.sig"))
            .expect_err("no mountpoint must error");
        match err {
            NmblError::DriverImage { stage, source } => {
                assert_eq!(stage, "locate");
                assert!(matches!(*source, NmblError::ConfigInvalid { .. }));
            }
            other => panic!("expected DriverImage, got {other:?}"),
        }
    }

    #[test]
    fn resolve_empty_path_is_locate_error() {
        let mut cfg = Config::recovery_default();
        cfg.runtime_boot_mountpoint = Some(PathBuf::from("/run/nmbl-boot"));
        let err =
            resolve_image(&cfg, &spec("", "nmbl/d.sfs.sig")).expect_err("empty path must error");
        assert!(matches!(
            err,
            NmblError::DriverImage {
                stage: "locate",
                ..
            }
        ));
    }

    #[test]
    fn resolve_empty_sig_path_is_locate_error() {
        let mut cfg = Config::recovery_default();
        cfg.runtime_boot_mountpoint = Some(PathBuf::from("/run/nmbl-boot"));
        let err =
            resolve_image(&cfg, &spec("nmbl/d.sfs", "")).expect_err("empty sig_path must error");
        assert!(matches!(
            err,
            NmblError::DriverImage {
                stage: "locate",
                ..
            }
        ));
    }

    #[test]
    fn open_missing_image_is_open_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = ResolvedImage {
            image_path: dir.path().join("does-not-exist.sfs"),
            sig_path: dir.path().join("does-not-exist.sfs.sig"),
        };
        let mut ops = crate::sys::ops::RealSys::sync_only();
        let err = open_image_ro(&mut ops, &resolved).expect_err("missing image must error");
        assert!(matches!(err, NmblError::DriverImage { stage: "open", .. }));
    }

    #[test]
    fn open_existing_image_returns_fd() {
        let dir = tempfile::tempdir().expect("tempdir");
        let img = dir.path().join("d.sfs");
        std::fs::write(&img, b"squashfs-placeholder").expect("write");
        let resolved = ResolvedImage {
            image_path: img,
            sig_path: dir.path().join("d.sfs.sig"),
        };
        // Opening succeeds even though the bytes are not a real squashfs — the
        // fd is just pinned here; verify/mount handle the content later.
        let mut ops = crate::sys::ops::RealSys::sync_only();
        open_image_ro(&mut ops, &resolved).expect("open existing image");
    }
}
