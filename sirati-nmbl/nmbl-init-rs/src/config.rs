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

    #[serde(default)]
    pub splash: Splash,
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
pub struct Splash {
    #[serde(default)]
    pub enable: bool,

    #[serde(default = "default_splash_background")]
    pub background_image: PathBuf,

    #[serde(default = "default_splash_font")]
    pub font_path: PathBuf,

    #[serde(default = "default_dri_path")]
    pub dri_path: PathBuf,
}

impl Default for Splash {
    fn default() -> Self {
        Self {
            enable: false,
            background_image: default_splash_background(),
            font_path: default_splash_font(),
            dri_path: default_dri_path(),
        }
    }
}

fn default_splash_background() -> PathBuf {
    PathBuf::from("/etc/splash/image.png")
}

fn default_splash_font() -> PathBuf {
    PathBuf::from("/etc/splash/font.ttf")
}

fn default_dri_path() -> PathBuf {
    PathBuf::from("/dev/dri/card0")
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
            splash: Splash::default(),
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;

    fn fs_entry(device: &str, mountpoint: &str) -> FilesystemEntry {
        FilesystemEntry {
            device: device.to_string(),
            mountpoint: PathBuf::from(mountpoint),
            fstype: "ext4".to_string(),
            options: String::new(),
            is_root: false,
        }
    }

    fn config_with(entries: Vec<FilesystemEntry>) -> Config {
        let mut c = Config::recovery_default();
        c.filesystems = entries;
        c
    }

    #[test]
    fn validate_accepts_dev_disk_by_label_paths() {
        // After the udev-less symlink populator landed, by-* paths
        // are valid `fileSystems[].device` strings.
        let c = config_with(vec![
            fs_entry("/dev/disk/by-label/boot", "/boot"),
            fs_entry("/dev/disk/by-partlabel/disk-main-ESP", "/boot"),
            fs_entry("/dev/disk/by-uuid/1234-ABCD", "/"),
            fs_entry("/dev/disk/by-partuuid/abcdef01-1234", "/data"),
        ]);
        c.validate().expect("by-* paths must validate");
    }

    #[test]
    fn validate_accepts_raw_dev_paths() {
        let c = config_with(vec![fs_entry("/dev/vda1", "/boot")]);
        c.validate().expect("raw /dev/* paths must validate");
    }

    #[test]
    fn validate_still_rejects_label_short_form() {
        let c = config_with(vec![fs_entry("LABEL=boot", "/boot")]);
        let err = c
            .validate()
            .expect_err("LABEL= short form must be rejected");
        match err {
            NmblError::ConfigInvalid { reason, .. } => {
                assert!(
                    reason.contains("LABEL=") || reason.contains("short form"),
                    "rejection message should mention LABEL=/short form, got: {reason}",
                );
                assert!(
                    reason.contains("/dev/disk/by-"),
                    "rejection should point at the by-* symlink form, got: {reason}",
                );
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn validate_still_rejects_uuid_short_form() {
        let c = config_with(vec![fs_entry("UUID=1234-ABCD", "/")]);
        c.validate().expect_err("UUID= short form must be rejected");
    }

    #[test]
    fn validate_still_rejects_partuuid_short_form() {
        let c = config_with(vec![fs_entry("PARTUUID=abc-123", "/data")]);
        c.validate()
            .expect_err("PARTUUID= short form must be rejected");
    }

    #[test]
    fn config_parses_without_splash_table() {
        // A config that doesn't mention [splash] at all must still parse,
        // because the feature defaults to off and existing on-disk configs
        // predate the new table.
        let toml_text = "[general]\ntimeout_secs = 3\n";
        let config: Config = toml::from_str(toml_text).expect("config must parse");
        assert!(!config.splash.enable, "splash must default to disabled");
        assert_eq!(
            config.splash.background_image,
            PathBuf::from("/etc/splash/image.png"),
        );
        assert_eq!(
            config.splash.font_path,
            PathBuf::from("/etc/splash/font.ttf"),
        );
        assert_eq!(config.splash.dri_path, PathBuf::from("/dev/dri/card0"));
    }

    #[test]
    fn config_parses_with_splash_table() {
        let toml_text = "[splash]\nenable = true\nbackground_image = \"/foo.png\"\n";
        let config: Config = toml::from_str(toml_text).expect("config must parse");
        assert!(config.splash.enable, "enable = true must round-trip");
        assert_eq!(config.splash.background_image, PathBuf::from("/foo.png"));
        // Unset fields still pick up their defaults.
        assert_eq!(
            config.splash.font_path,
            PathBuf::from("/etc/splash/font.ttf"),
        );
        assert_eq!(config.splash.dri_path, PathBuf::from("/dev/dri/card0"));
    }

    #[test]
    fn config_rejects_unknown_splash_field() {
        let toml_text = "[splash]\nfoo = 1\n";
        let err = toml::from_str::<Config>(toml_text)
            .expect_err("unknown splash field must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("foo") || msg.contains("unknown"),
            "rejection should mention the unknown field, got: {msg}",
        );
    }
}
