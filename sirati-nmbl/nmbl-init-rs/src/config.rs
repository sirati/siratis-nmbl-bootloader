use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{NmblError, Result};
use crate::log::Verbosity;

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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    #[serde(default)]
    pub verbosity: Verbosity,

    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u32,

    #[serde(default = "default_panic_report_dir")]
    pub panic_report_dir: PathBuf,

    #[serde(default)]
    pub serial_console: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            verbosity: Verbosity::default(),
            timeout_secs: default_timeout_secs(),
            panic_report_dir: default_panic_report_dir(),
            serial_console: false,
        }
    }
}

fn default_timeout_secs() -> u32 {
    5
}

fn default_panic_report_dir() -> PathBuf {
    PathBuf::from("/run")
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelModules {
    #[serde(default)]
    pub explicit: Vec<String>,

    #[serde(default)]
    pub blacklist: Vec<String>,

    #[serde(default = "default_modules_dir")]
    pub modules_dir: PathBuf,
}

fn default_modules_dir() -> PathBuf {
    PathBuf::from("/lib/modules")
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemEntry {
    pub device: String,
    pub mountpoint: PathBuf,
    pub fstype: String,

    #[serde(default)]
    pub options: String,

    #[serde(default)]
    pub is_root: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub kind: ActivationKind,

    #[serde(default)]
    pub required_modules: Vec<String>,

    pub binary: PathBuf,

    #[serde(default)]
    pub argv: Vec<String>,

    #[serde(default)]
    pub produces_devices: Vec<PathBuf>,

    pub description: String,

    #[serde(default)]
    pub prompt_label: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivationKind {
    Lvm,
    Mdraid,
    LuksTpm,
    LuksKeyfile,
    LuksPassword,
    Zfs,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tui {
    #[serde(default = "default_true")]
    pub enable_editor: bool,

    #[serde(default = "default_true")]
    pub show_kernel_params: bool,
}

impl Default for Tui {
    fn default() -> Self {
        Self {
            enable_editor: true,
            show_kernel_params: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    #[serde(default = "default_nix_profiles_dir")]
    pub nix_profiles_dir: PathBuf,

    #[serde(default = "default_system_root")]
    pub system_root: PathBuf,

    #[serde(default = "default_shell")]
    pub shell: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            nix_profiles_dir: default_nix_profiles_dir(),
            system_root: default_system_root(),
            shell: default_shell(),
        }
    }
}

fn default_nix_profiles_dir() -> PathBuf {
    PathBuf::from("/mnt/system/nix/var/nix/profiles")
}

fn default_system_root() -> PathBuf {
    PathBuf::from("/mnt/system")
}

fn default_shell() -> PathBuf {
    PathBuf::from("/bin/sh")
}

impl Config {
    /// Reject device specifiers that the v1 mount layer cannot resolve. NMBL
    /// has no udev, so `/dev/disk/by-*` symlinks are never populated; the
    /// user must point at raw `/dev/*` device nodes.
    pub fn validate(&self) -> Result<()> {
        for fs in &self.filesystems {
            let dev = fs.device.as_str();
            if dev.starts_with("LABEL=") || dev.starts_with("UUID=") || dev.starts_with("PARTUUID=")
            {
                return Err(NmblError::ConfigInvalid {
                    reason: format!(
                        "device {dev:?} uses LABEL=/UUID=/PARTUUID= which NMBL does not \
                         resolve; use a raw /dev/* path"
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
        }
    }
}
