use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{NmblError, Result};

pub(crate) mod bootstrap;
mod entries;
mod general;
mod paths;
mod rescue_cfg;
mod splash;
mod stateful_cfg;
mod tui;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests;

pub use bootstrap::{
    BootstrapBootFs, BootstrapConfig, BootstrapKernelModules, BootstrapRescue, BootstrapSection,
    BootstrapStateMount, resolve_full_config_path,
};
pub use entries::{Activation, ActivationKind, FilesystemEntry};
pub use general::{General, KernelModules};
pub use paths::Paths;
pub use rescue_cfg::{EmergencyShellConfig, RescueConfig};
pub use tui::Tui;

#[cfg(feature = "image-splash")]
pub use splash::{Splash, SplashBackgroundLocation};

#[cfg(feature = "stateful")]
pub use stateful_cfg::StatefulConfig;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub general: General,

    #[serde(default)]
    pub kernel_modules: KernelModules,

    #[serde(default)]
    pub filesystems: Vec<FilesystemEntry>,

    #[serde(default)]
    pub activations: Vec<Activation>,

    #[serde(default)]
    pub tui: Tui,

    #[serde(default)]
    pub paths: Paths,

    #[cfg(feature = "image-splash")]
    #[serde(default)]
    pub splash: Splash,

    #[serde(default)]
    pub rescue: RescueConfig,

    #[serde(default)]
    pub emergency_shell: EmergencyShellConfig,

    /// Top-level `[stateful]` table that gates the rollback flow. Absent
    /// in non-stateful builds and in stateful builds whose Nix config did
    /// not enable `boot.nmbl.stateful.enable`. When `Some`, the boot-time
    /// dispatcher reads `state.bin` and consults
    /// [`crate::state::decide`].
    #[cfg(feature = "stateful")]
    #[serde(default)]
    pub stateful: Option<StatefulConfig>,

    /// Populated by Phase 0.5 with the runtime mountpoint of the boot
    /// partition. `None` in legacy embedded-config mode. Never parsed
    /// from TOML — `#[serde(skip)]` keeps it out of the wire schema and
    /// makes [`Default for Config`] supply `None` automatically.
    #[serde(skip)]
    pub runtime_boot_mountpoint: Option<PathBuf>,

    /// Populated by Phase 0.5 when the bootstrap TOML carries a
    /// `[bootstrap.state]` section. Holds the RW twin mountpoint of the
    /// boot filesystem so `select_and_act` can resolve `state.bin` and
    /// the kexec teardown can detach the mount before handoff. `None`
    /// when the operator has not opted into stateful storage.
    #[cfg(feature = "stateful")]
    #[serde(skip)]
    pub runtime_state_mountpoint: Option<PathBuf>,
}

impl Config {
    /// Reject device specifiers the mount layer cannot resolve.
    ///
    /// `LABEL=`/`UUID=`/`PARTUUID=` short forms remain rejected — NMBL
    /// has no user-space resolver for them and the kernel does not
    /// accept those strings as device arguments to `mount(2)`.
    /// Absolute paths under `/dev/disk/by-*` are now ALLOWED: the
    /// `sys::blkid::populate_disk_by_symlinks` pass that runs at the
    /// start of [`crate::devices::mount_system_filesystems`] creates
    /// those symlinks udev-less from `blkid -o export` output.
    pub fn validate(&self) -> Result<()> {
        for fs in &self.filesystems {
            let dev = fs.device.as_str();
            if dev.starts_with("LABEL=") || dev.starts_with("UUID=") || dev.starts_with("PARTUUID=")
            {
                return Err(NmblError::ConfigInvalid {
                    reason: format!(
                        "device {dev:?} uses LABEL=/UUID=/PARTUUID= short form which NMBL \
                         does not resolve; use the /dev/disk/by-label/<name> (or \
                         by-uuid/by-partlabel/by-partuuid) symlink form instead — NMBL \
                         populates those at boot via blkid"
                    ),
                    context: format!(
                        "validating filesystem entry for {}",
                        fs.mountpoint.display()
                    ),
                });
            }
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|source| NmblError::Io {
            source,
            context: format!("reading config file {}", path.display()),
        })?;

        let config: Config = toml::from_str(&text).map_err(|source| NmblError::Config {
            source,
            path: path.to_path_buf(),
        })?;

        config.validate()?;
        Ok(config)
    }

    /// Last-ditch fallback used when `/etc/nmbl/config.toml` can't be
    /// loaded. Hard-coded defaults give the emergency shell enough to
    /// function: a usable `shell` path for `execve`, default verbosity,
    /// and empty filesystems / activations / explicit modules so no
    /// phase tries to act on user data we don't actually have.
    pub fn recovery_default() -> Self {
        Self {
            general: General::default(),
            kernel_modules: KernelModules::default(),
            filesystems: Vec::new(),
            activations: Vec::new(),
            tui: Tui::default(),
            paths: Paths::default(),
            #[cfg(feature = "image-splash")]
            splash: Splash::default(),
            rescue: RescueConfig::default(),
            emergency_shell: EmergencyShellConfig::default(),
            #[cfg(feature = "stateful")]
            stateful: None,
            runtime_boot_mountpoint: None,
            #[cfg(feature = "stateful")]
            runtime_state_mountpoint: None,
        }
    }
}
