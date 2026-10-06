//! Shared type definitions and constants for the rescue subsystem.

use serde::Deserialize;

/// Default basename of the rescue squashfs on the boot partition. Used
/// when `[rescue].sfs_path` is absent from the operator's runtime
/// config.
pub(super) const DEFAULT_SFS_BASENAME: &str = "nmbl-rescue.sfs";

/// On-disk format of the external rescue image named by
/// `[rescue].sfs_path`. Declared by the (embedded or signed) runtime config,
/// never sniffed from the image: NMBL mounts exactly the declared
/// filesystem and only ever needs that one module. The flat busybox rescue
/// stays squashfs; the full-system rescue (stage 2) is EROFS.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RescueImageFormat {
    /// Legacy and flat busybox rescue images.
    #[default]
    Squashfs,
    /// The full-system rescue image (stage 2).
    Erofs,
}

impl RescueImageFormat {
    /// Kernel filesystem type, which is also the module providing it.
    #[must_use]
    pub fn fstype(self) -> &'static str {
        match self {
            Self::Squashfs => "squashfs",
            Self::Erofs => "erofs",
        }
    }
}

/// How [`crate::shell::drop_to_emergency`] reaches the operator. Comes
/// from the runtime [`Config`]'s `[rescue]` section; persists to TOML
/// as kebab-case strings (`"embedded"`, `"external"`, `"none"`).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RescueMode {
    /// Legacy: busybox baked into the initramfs.
    /// [`crate::shell::drop_to_emergency`] execs `cfg.paths.shell`
    /// directly via [`exec_embedded`].
    #[default]
    Embedded,
    /// `nmbl-rescue.sfs` on the boot partition; loop-mounted on demand
    /// by [`disk::try_disk_rescue`].
    External,
    /// No rescue tools shipped; halt with a structured banner via
    /// [`halt_with_banner`].
    None,
}
