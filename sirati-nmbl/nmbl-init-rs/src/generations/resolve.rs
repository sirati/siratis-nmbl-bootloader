//! Path-resolution helpers used during generation scanning.

use std::path::{Path, PathBuf};

use crate::error::{NmblError, Result};
use crate::{nmbl_verbose, nmbl_warn};

/// Single-level symlink resolution that rewrites absolute targets to be
/// reachable from NMBL's namespace.
///
/// The system disk's profile symlinks point at absolute store paths like
/// `/nix/store/<hash>/...`, but NMBL has the system root mounted under
/// `mount_prefix` (typically `/mnt/system`), so those targets don't exist
/// from NMBL's view. Mirroring the bash bootloader's `resolve_*_path`
/// helpers (commit e310b67), absolute targets are prefixed and relative
/// targets are joined against the link's parent directory. Non-symlinks
/// pass through unchanged.
pub(super) fn mount_aware_resolve(path: &Path, mount_prefix: &Path) -> std::io::Result<PathBuf> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }
    let target = std::fs::read_link(path)?;
    if target.is_absolute() {
        let rel = target.strip_prefix("/").unwrap_or(&target);
        Ok(mount_prefix.join(rel))
    } else {
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        Ok(parent.join(target))
    }
}

/// Read `<toplevel>/kernel-params` and split on whitespace. IO failures
/// degrade to an empty Vec with a warning — params are nice-to-have, not
/// fatal.
pub(super) fn read_kernel_params(toplevel: &Path) -> Vec<String> {
    let path = toplevel.join("kernel-params");
    match std::fs::read_to_string(&path) {
        Ok(text) => text.split_ascii_whitespace().map(String::from).collect(),
        Err(err) => {
            nmbl_warn!("kernel-params unreadable at {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// Best-effort: read `<toplevel>/nixos-version` for a human label. Missing
/// file → empty string (logged at verbose only).
pub(super) fn read_label(toplevel: &Path) -> String {
    let path = toplevel.join("nixos-version");
    match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().to_string(),
        Err(err) => {
            nmbl_verbose!("no nixos-version at {}: {err}", path.display());
            String::new()
        }
    }
}

/// Resolve `<toplevel>/kernel` and `<toplevel>/initrd` through
/// [`mount_aware_resolve`]. Either failing means the generation is broken
/// and the caller should skip it.
pub(super) fn resolve_kernel_initrd(
    toplevel: &Path,
    mount_prefix: &Path,
) -> Result<(PathBuf, PathBuf)> {
    let resolve = |name: &str| -> Result<PathBuf> {
        let p = toplevel.join(name);
        mount_aware_resolve(&p, mount_prefix).map_err(|source| NmblError::Io {
            source,
            context: format!("resolving {}", p.display()),
        })
    };
    Ok((resolve("kernel")?, resolve("initrd")?))
}

/// Probe `<profile_link>/init` WITHOUT following symlinks. We want the
/// un-resolved path because the chained kernel boots its own initrd which
/// will mount the store and execute exactly the string we hand it on the
/// cmdline; resolving here would replace `<profile_link>/init` with the
/// underlying store path, which is fine on disk but defeats the symlink
/// indirection that lets rollbacks point a fixed cmdline at a moving
/// target. We stat through the mount-aware toplevel (since accessing
/// anything under the raw profile link would walk an absolute store path)
/// but return the un-resolved profile-link path. Missing or unreadable →
/// `Err`, caller skips the generation.
pub(super) fn resolve_init_path(profile_link: &Path, toplevel: &Path) -> Result<PathBuf> {
    let probe = toplevel.join("init");
    std::fs::symlink_metadata(&probe).map_err(|source| NmblError::Io {
        source,
        context: format!("stat {}", probe.display()),
    })?;
    Ok(profile_link.join("init"))
}
