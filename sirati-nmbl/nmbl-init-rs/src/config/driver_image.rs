use std::path::PathBuf;

use serde::Deserialize;

/// `[driver_images]` section of the runtime config (group #8). Holds the
/// operator-declared list of detached, *verified* driver-image squashfs
/// blobs NMBL loop-mounts and `finit_module`s before kexec.
///
/// The driver-image feature is always compiled, but the VERIFY step it
/// depends on lives behind the `secure-boot` Cargo feature. The Nix side
/// (`lib/modules/security/driver-image.nix`) rejects `enable = true` without
/// an active secure-boot table at build time (FIX-05), so an unverified image
/// can never reach this config in a way the loader would honour.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverImagesConfig {
    /// Master switch. When `false` (the default) NMBL skips the driver-image
    /// phase entirely regardless of any `images` entries, so a build that
    /// never opted in keeps the legacy boot flow.
    #[serde(default)]
    pub enable: bool,

    /// The declared images, in load order. Emitted by config-toml.nix as an
    /// array-of-tables (`[[driver_images.images]]`), mirroring the
    /// `filesystems` / `activations` precedent.
    #[serde(default)]
    pub images: Vec<DriverImageSpec>,
}

/// A single driver image: a signed squashfs of out-of-tree kernel modules
/// plus the metadata NMBL needs to verify it, load its modules, and avoid
/// in-tree driver conflicts. Paths are boot-partition-relative; the loader
/// joins them against the runtime boot mountpoint.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriverImageSpec {
    /// Location of the driver squashfs RELATIVE TO THE BOOT PARTITION ROOT.
    #[serde(default)]
    pub path: PathBuf,

    /// Location of the detached signature for `path`, RELATIVE TO THE BOOT
    /// PARTITION ROOT. Verified against the operator's public keys before the
    /// image is loop-mounted.
    #[serde(default)]
    pub sig_path: PathBuf,

    /// Out-of-tree module names this image provides, in the order NMBL
    /// `finit_module`s them after verifying and loop-mounting the squashfs.
    #[serde(default)]
    pub modules: Vec<String>,

    /// In-tree module names to blacklist before loading this image's drivers,
    /// so a conflicting built-in does not claim the device first.
    #[serde(default)]
    pub blacklist: Vec<String>,
}
