//! Locate the rescue squashfs on the boot partition.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};

use super::types::DEFAULT_SFS_BASENAME;

/// Resolve the on-disk path of the external rescue squashfs.
///
/// `rescue.sfs_path` is interpreted as a path RELATIVE TO THE BOOT
/// PARTITION ROOT; a leading `/` is tolerated and stripped so the
/// mountpoint join keeps the runtime mountpoint instead of replacing
/// it. When `sfs_path` is absent the basename
/// [`DEFAULT_SFS_BASENAME`] is used.
///
/// The runtime mountpoint comes from
/// [`Config::runtime_boot_mountpoint`], which Phase 0.5 populates after
/// `mount_boot` succeeds. In legacy embedded-config mode that field is
/// `None` — there is no NMBL-mounted boot partition, so external rescue
/// is not supported and this function surfaces a
/// `NmblError::Rescue { stage: "locate-sfs", … }` instead of fabricating
/// a path that would not resolve.
pub fn locate_sfs(config: &Config) -> Result<PathBuf> {
    let mountpoint =
        config
            .runtime_boot_mountpoint
            .as_deref()
            .ok_or_else(|| NmblError::Rescue {
                stage: "locate-sfs",
                source: Box::new(NmblError::ConfigInvalid {
                    reason:
                        "external rescue requires bootstrap mode: the runtime boot mountpoint is \
                         only known after Phase 0.5 mounts the boot partition, but this NMBL \
                         instance is running in legacy embedded-config mode"
                            .to_string(),
                    context: "resolving rescue.sfs_path against the runtime boot mountpoint"
                        .to_string(),
                }),
            })?;

    let relative: PathBuf = match config.rescue.sfs_path.as_deref() {
        Some(p) => strip_leading_slash(p).to_path_buf(),
        None => PathBuf::from(DEFAULT_SFS_BASENAME),
    };
    Ok(mountpoint.join(relative))
}

/// Strip a single leading `/` so [`Path::join`] keeps the mountpoint
/// instead of replacing it. Mirrors the helper in
/// [`crate::config::resolve_full_config_path`].
pub(super) fn strip_leading_slash(p: &Path) -> &Path {
    p.strip_prefix("/").unwrap_or(p)
}
