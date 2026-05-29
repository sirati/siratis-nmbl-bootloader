use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{NmblError, Result};

use super::general::default_modules_dir;

/// Top-level wrapper for `/etc/nmbl/bootstrap.toml`, the tiny pre-stage
/// config that is embedded directly into the initramfs. It points at the
/// boot filesystem holding the real (per-generation) `Config` and lists
/// the kernel modules required to mount that filesystem.
///
/// Kept deliberately separate from [`Config`]: the bootstrap step runs
/// before any user-controlled config is reachable, so its schema is
/// frozen at initramfs build time and must stay minimal.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub bootstrap: BootstrapSection,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapSection {
    /// Path to the real config relative to [`BootstrapBootFs::mountpoint`].
    #[serde(default = "default_bootstrap_config_path")]
    pub config_path: PathBuf,

    pub boot_fs: BootstrapBootFs,

    #[serde(default)]
    pub kernel_modules: BootstrapKernelModules,

    #[serde(default)]
    pub rescue: BootstrapRescue,

    /// Optional read-write twin of [`BootstrapBootFs`] used by the
    /// stateful runtime for `state.bin` I/O. Absent when the host has no
    /// stateful storage configured; the bootstrap stage then skips the
    /// extra mount entirely.
    #[serde(default)]
    pub state: Option<BootstrapStateMount>,
}

/// Boot-filesystem descriptor used by the bootstrap stage. Shape mirrors
/// [`FilesystemEntry`] but is intentionally a distinct type: the bootstrap
/// schema is frozen at initramfs build time and must not grow fields that
/// only make sense for the user-controlled [`Config`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapBootFs {
    pub device: String,
    pub fstype: String,

    #[serde(default)]
    pub options: String,

    pub mountpoint: PathBuf,
}

/// Mountpoint of the read-write state filesystem used by the runtime to
/// persist `state.bin`. Device, fstype and mount options come from the
/// already-mounted [`BootstrapBootFs`] twin, so this descriptor only
/// needs to know where to bind the writable view.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapStateMount {
    pub mountpoint: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapKernelModules {
    #[serde(default)]
    pub explicit: Vec<String>,

    /// Mirrors [`KernelModules::modules_dir`]; defaults to
    /// `/lib/modules` so the bootstrap stage and the full-config stage
    /// agree on where to find `modules.dep` unless the operator overrides
    /// both.
    #[serde(default = "default_modules_dir")]
    pub modules_dir: PathBuf,
}

impl Default for BootstrapKernelModules {
    fn default() -> Self {
        Self {
            explicit: Vec::new(),
            modules_dir: default_modules_dir(),
        }
    }
}

/// Optional network-rescue defaults. Tolerated even when network rescue
/// is off so the same `bootstrap.toml` can be shared across builds.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapRescue {
    #[serde(default)]
    pub default_url: String,

    #[serde(default)]
    pub default_sha256: String,
}

pub(crate) fn default_bootstrap_config_path() -> PathBuf {
    PathBuf::from("/nmbl/config.toml")
}

/// Resolve the absolute on-disk path of the full `Config` by joining the
/// boot filesystem mountpoint with `config_path`. Any leading `/` on
/// `config_path` is stripped so [`Path::join`] keeps the mountpoint
/// instead of replacing it.
pub fn resolve_full_config_path(mountpoint: &Path, config_path: &Path) -> PathBuf {
    let stripped = config_path.strip_prefix("/").unwrap_or(config_path);
    mountpoint.join(stripped)
}

impl BootstrapConfig {
    /// Reject contradictory rescue defaults: `default_url` and
    /// `default_sha256` are both stringly-typed sentinels (empty =
    /// absent). The hash without a URL is unusable and the URL without
    /// a hash is unsafe to fetch, so the only sensible states are
    /// "both set" or "both empty".
    pub fn validate(&self) -> Result<()> {
        let url_set = !self.bootstrap.rescue.default_url.is_empty();
        let sha_set = !self.bootstrap.rescue.default_sha256.is_empty();
        if url_set ^ sha_set {
            return Err(NmblError::ConfigInvalid {
                reason: "bootstrap.rescue.default_url and bootstrap.rescue.default_sha256 must be \
                     set together or both left empty"
                    .to_string(),
                context: "validating bootstrap rescue defaults".to_string(),
            });
        }
        Ok(())
    }

    /// Read and parse `/etc/nmbl/bootstrap.toml` (or whichever embedded
    /// path the caller passes). Both the I/O and parse steps are wrapped
    /// in [`NmblError::Bootstrap`] so callers can distinguish bootstrap
    /// failure from user-config failure by variant rather than string
    /// matching.
    pub fn load(path: &Path) -> Result<BootstrapConfig> {
        let text = std::fs::read_to_string(path).map_err(|source| NmblError::Bootstrap {
            stage: "load-toml",
            source: Box::new(NmblError::Io {
                source,
                context: format!("reading bootstrap config {}", path.display()),
            }),
        })?;

        let config: BootstrapConfig =
            toml::from_str(&text).map_err(|source| NmblError::Bootstrap {
                stage: "parse-toml",
                source: Box::new(NmblError::Config {
                    source,
                    path: path.to_path_buf(),
                }),
            })?;

        config.validate().map_err(|source| NmblError::Bootstrap {
            stage: "validate",
            source: Box::new(source),
        })?;
        Ok(config)
    }
}
