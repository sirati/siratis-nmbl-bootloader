#[cfg(feature = "staged-boot")]
use std::path::PathBuf;

#[cfg(feature = "staged-boot")]
use serde::Deserialize;

/// `[staged]` table — the staged-boot pointer set. Names a signed
/// config fragment, its detached signature, and the priority-volume
/// image they live on, all as boot-partition-relative paths the Rust
/// loader joins against the runtime priority-volume mountpoint (the same
/// way [`crate::config::RescueConfig`] resolves `sfs_path`).
///
/// Gated behind `staged-boot` (which structurally implies `secure-boot`):
/// there is no staged path without signature verification (R-3 / R-6), so
/// the struct only compiles when the verifier is present. The Nix side
/// additionally rejects `boot.nmbl.staged.enable = true` without
/// `boot.nmbl.secureBoot.enable` (FIX-26) so the staged fragment can never
/// be self-mounted unverified.
///
/// Deliberately carries NO `has_config_fragment` boolean (FIX-56): whether
/// a fragment is actually present is decided by the Rust side checking file
/// existence at runtime, not by a build-time flag that could drift from the
/// on-disk reality.
#[cfg(feature = "staged-boot")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedConfig {
    /// Master switch. When `false` the staged pointer set is parsed but
    /// the boot-time dispatcher takes the legacy (non-staged) path. The
    /// Nix emit gate only writes `[staged]` when the operator enabled it,
    /// so an absent table decodes to `None` upstream.
    #[serde(default)]
    pub enable: bool,

    /// Priority-volume image (the verified container holding the signed
    /// fragment + drivers), relative to the priority-volume mountpoint.
    pub image: PathBuf,

    /// Signed config fragment inside the priority volume, applied on top
    /// of the base config once its signature verifies. Boot-partition-
    /// relative.
    pub fragment: PathBuf,

    /// Detached ML-DSA signature over [`Self::fragment`]. Boot-partition-
    /// relative.
    pub sig: PathBuf,
}
