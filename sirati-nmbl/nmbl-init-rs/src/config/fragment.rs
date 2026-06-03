#[cfg(feature = "staged-boot")]
use std::path::Path;

#[cfg(feature = "staged-boot")]
use serde::Deserialize;

#[cfg(feature = "staged-boot")]
use crate::error::{NmblError, Result};

#[cfg(all(feature = "staged-boot", feature = "image-splash"))]
use super::Splash;
#[cfg(feature = "staged-boot")]
use super::{
    Activation, DriverImagesConfig, EmergencyShellConfig, FilesystemEntry, General, KernelModules,
    Paths, RescueConfig, TpmConfig, Tui,
};

/// A staged-boot config OVERLAY: a partial [`super::Config`] the operator
/// ships on the verified priority volume, applied on top of the base
/// config once its detached signature verifies (R-3 / R-6). It is NOT a
/// full `Config` — every field is optional, so a fragment that sets only
/// `[general]` is as valid as one that sets nothing at all.
///
/// Each top-level table is an `Option<T>`: `None` means "the fragment did
/// not mention this table, leave the base config untouched", `Some(_)`
/// means "replace the base table with this one". That shape is what the
/// Wave-3 transactional merge (#33) consumes — it can tell which tables
/// the fragment actually carries without a separate presence flag, the
/// same FIX-56 reasoning that keeps `[staged]` flag-free. This module only
/// provides the load+parse; the validate-then-swap merge into a
/// `&mut Config` lives in `src/staged/`.
///
/// `deny_unknown_fields` is preserved verbatim from `Config`: a fragment
/// that names a table the schema does not know is a hard parse error, so a
/// typo'd or hostile-but-malformed (post-verify) overlay can never silently
/// no-op.
///
/// Deliberately omitted from the overlay surface:
///   * the security-policy tables (`[signing]`, `[secure_boot]`, `[staged]`)
///     — a staged fragment must never relax the enforcement posture or
///     re-point the staged source it was itself loaded through (FIX-53);
///   * the `#[serde(skip)]` runtime mountpoints — those are populated by the
///     boot pipeline, never parsed from any TOML.
///
/// Gated behind `staged-boot` (which implies `secure-boot`): there is no
/// staged overlay without signature verification, so the type only compiles
/// when the verifier is present.
#[cfg(feature = "staged-boot")]
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFragment {
    #[serde(default)]
    pub general: Option<General>,

    #[serde(default)]
    pub kernel_modules: Option<KernelModules>,

    #[serde(default)]
    pub filesystems: Option<Vec<FilesystemEntry>>,

    #[serde(default)]
    pub activations: Option<Vec<Activation>>,

    #[serde(default)]
    pub tui: Option<Tui>,

    #[serde(default)]
    pub paths: Option<Paths>,

    #[cfg(feature = "image-splash")]
    #[serde(default)]
    pub splash: Option<Splash>,

    #[serde(default)]
    pub rescue: Option<RescueConfig>,

    #[serde(default)]
    pub emergency_shell: Option<EmergencyShellConfig>,

    /// `[driver_images]` overlay: a staged volume legitimately delivers a
    /// fresh set of verified driver blobs, so the fragment may replace the
    /// base group.
    #[serde(default)]
    pub driver_images: Option<DriverImagesConfig>,

    /// `[tpm]` measured-boot overlay. Always-compiled in the base schema
    /// (FIX-09), so the fragment can carry it too.
    #[serde(default)]
    pub tpm: Option<TpmConfig>,
}

#[cfg(feature = "staged-boot")]
impl ConfigFragment {
    /// Parse a raw TOML overlay into a `ConfigFragment`, WITHOUT any I/O.
    /// Mirrors [`super::Config::parse_toml`] and reuses the same
    /// `NmblError::Config` diagnostic; `path` is carried only for that
    /// error. Unlike the full config it has no `validate()` pass — a
    /// fragment is validated as part of the Wave-3 merge (#33), against the
    /// `Config` it is being merged into, not in isolation.
    pub fn parse_toml(text: &str, path: &Path) -> Result<ConfigFragment> {
        toml::from_str(text).map_err(|source| NmblError::Config {
            source,
            path: path.to_path_buf(),
        })
    }

    /// Read and parse a signed config fragment from `path` (the caller is
    /// responsible for having verified its detached signature first — this
    /// is the load+parse step only; see the free [`load_fragment`]).
    pub fn load(path: &Path) -> Result<ConfigFragment> {
        let text = std::fs::read_to_string(path).map_err(|source| NmblError::Io {
            source,
            context: format!("reading config fragment {}", path.display()),
        })?;
        ConfigFragment::parse_toml(&text, path)
    }
}

/// Load a PARTIAL staged-boot config fragment from `path` for the Wave-3
/// transactional merge. Thin wrapper over [`ConfigFragment::load`] exposed
/// as `config::load_fragment` to sit alongside [`super::Config::load`]; it
/// performs the load+parse only, never the merge into a `&mut Config`.
#[cfg(feature = "staged-boot")]
pub fn load_fragment(path: &Path) -> Result<ConfigFragment> {
    ConfigFragment::load(path)
}
