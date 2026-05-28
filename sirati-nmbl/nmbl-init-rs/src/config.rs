use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{NmblError, Result};
use crate::log::Verbosity;
use crate::rescue::RescueMode;

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

    /// Populated by Phase 0.5 with the runtime mountpoint of the boot
    /// partition. `None` in legacy embedded-config mode. Never parsed
    /// from TOML — `#[serde(skip)]` keeps it out of the wire schema and
    /// makes [`Default for Config`] supply `None` automatically.
    #[serde(skip)]
    pub runtime_boot_mountpoint: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    #[serde(default)]
    pub verbosity: Verbosity,

    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u32,

    /// Per-device readiness budget (seconds) used while waiting for a
    /// `fileSystems[].device` to appear during mount, and while waiting
    /// for cryptsetup / LVM / mdraid activations to materialise their
    /// produced block devices. Honoured by `devices::wait_for` at every
    /// call site.
    #[serde(default = "default_device_timeout_secs")]
    pub device_timeout_secs: u64,

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
            device_timeout_secs: default_device_timeout_secs(),
            panic_report_dir: default_panic_report_dir(),
            serial_console: false,
        }
    }
}

fn default_timeout_secs() -> u32 {
    5
}

fn default_device_timeout_secs() -> u64 {
    30
}

fn default_panic_report_dir() -> PathBuf {
    PathBuf::from("/run")
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelModules {
    /// Modules loaded BEFORE the boot console is brought up. Reserved
    /// for graphics drivers (`virtio_gpu`, `simpledrm`, `i915`, …) so
    /// `/dev/dri/card*` exists when `open_console` tries to attach the
    /// splash backend. Loaded by `modules::load_modules(_,_,
    /// ModuleSet::Early)` during phase 2a, immediately before
    /// `open_console`.
    #[serde(default)]
    pub early: Vec<String>,

    /// Storage / filesystem / activation modules loaded in phase 2b,
    /// AFTER the boot console is up so the operator sees per-module
    /// progress.
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

    /// When set on a `luks-password` activation, NMBL captures the
    /// typed passphrase and injects it into the kexec'd initrd as a
    /// keyfile at this in-cpio path (e.g. `/etc/nmbl-luks/cryptroot`).
    /// The next stage's NixOS config points
    /// `boot.initrd.luks.devices.<name>.keyFile` at the same path so
    /// the operator only types once. The path stays in memory only —
    /// it lives in the initrd tmpfs, dropped at switch-root.
    #[serde(default)]
    pub pass_to_stage1: Option<PathBuf>,
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

#[cfg(feature = "image-splash")]
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

#[cfg(feature = "image-splash")]
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

#[cfg(feature = "image-splash")]
fn default_splash_background() -> PathBuf {
    PathBuf::from("/etc/splash/image.png")
}

#[cfg(feature = "image-splash")]
fn default_splash_font() -> PathBuf {
    PathBuf::from("/etc/splash/font.ttf")
}

#[cfg(feature = "image-splash")]
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

/// `[rescue]` section of the operator's runtime config. Selects the
/// rescue mode (see [`RescueMode`]) and optionally pins the on-disk
/// path of `nmbl-rescue.sfs`. The network-rescue fields (Phase E.1)
/// supply the disk-rescue fallback that fetches `nmbl-rescue.sfs`
/// from an operator-pinned HTTP URL.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueConfig {
    /// Which rescue path [`crate::rescue::dispatch`] takes. Defaults to
    /// [`RescueMode::Embedded`] to preserve the legacy behaviour for
    /// installs that have not opted in to the external squashfs.
    #[serde(default)]
    pub mode: RescueMode,

    /// Path to `nmbl-rescue.sfs` RELATIVE TO THE BOOT PARTITION ROOT.
    /// A leading `/` is tolerated and stripped at resolution time. When
    /// `None` the rescue dispatcher uses the default
    /// `"nmbl-rescue.sfs"`. The runtime mountpoint is supplied
    /// out-of-band via [`Config::runtime_boot_mountpoint`] (populated by
    /// Phase 0.5), so this value is always boot-partition-relative
    /// regardless of where the operator's boot is mounted.
    #[serde(default)]
    pub sfs_path: Option<PathBuf>,

    /// Master switch for the network-rescue fallback. When `false`
    /// (the default) the External arm of [`crate::rescue::dispatch`]
    /// halts after the disk-rescue attempt fails, even if the
    /// `network-rescue` Cargo feature is compiled in. Matches the
    /// Nix-side `boot.nmbl.rescue.network` option emitted by E.3.
    #[serde(default)]
    pub network: bool,

    /// Pre-filled URL shown on the rescue source-picker's URL prompt.
    /// Empty string means "no prefill" — the operator types the URL
    /// from scratch. Matches `boot.nmbl.rescue.defaultUrl`.
    #[serde(default)]
    pub default_url: String,

    /// Pre-filled expected SHA-256 (lowercase hex) for the rescue
    /// squashfs. Empty string means "no prefill" — the operator
    /// confirms the computed hash without a pinned reference. Matches
    /// `boot.nmbl.rescue.defaultSha256`.
    #[serde(default)]
    pub default_sha256: String,
}

/// `[emergency_shell]` section of the runtime config. Controls which
/// `/dev/<tty>` devices the operator may multiplex the emergency shell
/// onto. The list is operator-curated because exposing a root shell on
/// a serial console (IPMI SOL, server-room concentrator, etc.) is a
/// privilege exposure — the default of `[]` keeps the shell pinned to
/// `/dev/console` (the kernel-elected primary interactive console)
/// unless the operator opts in.
///
/// At picker time the dialog joins `extra_consoles` with the resolved
/// `/dev/console` target so the operator sees the full candidate list;
/// nothing is auto-added behind their back.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmergencyShellConfig {
    /// Additional `/dev/<tty>` paths offered as multiplex targets in
    /// the picker dialog. Operator-owned: each entry MUST be a tty the
    /// operator considers safe to expose a root shell on. Defaults to
    /// empty so only `/dev/console` is offered out of the box.
    #[serde(default)]
    pub extra_consoles: Vec<String>,
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
            runtime_boot_mountpoint: None,
        }
    }
}

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

fn default_bootstrap_config_path() -> PathBuf {
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
    fn device_timeout_secs_defaults_to_thirty_when_absent() {
        // External TOMLs predating the knob must keep parsing cleanly
        // and observe the historic 30 s budget so the boot UX doesn't
        // silently regress on upgrade.
        let toml_text = "[general]\ntimeout_secs = 3\n";
        let config: Config = toml::from_str(toml_text).expect("config must parse");
        assert_eq!(config.general.device_timeout_secs, 30);
    }

    #[test]
    fn device_timeout_secs_is_honoured_when_present() {
        let toml_text = "[general]\ndevice_timeout_secs = 90\n";
        let config: Config = toml::from_str(toml_text).expect("config must parse");
        assert_eq!(config.general.device_timeout_secs, 90);
    }

    #[cfg(feature = "image-splash")]
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

    #[cfg(feature = "image-splash")]
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

    #[cfg(feature = "image-splash")]
    #[test]
    fn config_rejects_unknown_splash_field() {
        let toml_text = "[splash]\nfoo = 1\n";
        let err =
            toml::from_str::<Config>(toml_text).expect_err("unknown splash field must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("foo") || msg.contains("unknown"),
            "rejection should mention the unknown field, got: {msg}",
        );
    }
    #[test]
    fn bootstrap_parses_full_schema() {
        let toml = r#"
[bootstrap]
config_path = "/nmbl/config.toml"

[bootstrap.boot_fs]
device     = "/dev/disk/by-partlabel/disk-main-ESP"
fstype     = "vfat"
options    = "ro"
mountpoint = "/mnt/boot"

[bootstrap.kernel_modules]
explicit = ["vfat", "nls_cp437", "nls_iso8859_1", "ahci", "nvme"]

[bootstrap.rescue]
default_url    = "https://example.invalid/rescue.cpio"
default_sha256 = "deadbeef"
"#;
        let cfg: BootstrapConfig = toml::from_str(toml).expect("full schema must parse");
        assert_eq!(
            cfg.bootstrap.config_path,
            PathBuf::from("/nmbl/config.toml")
        );
        assert_eq!(
            cfg.bootstrap.boot_fs.device,
            "/dev/disk/by-partlabel/disk-main-ESP",
        );
        assert_eq!(cfg.bootstrap.boot_fs.fstype, "vfat");
        assert_eq!(cfg.bootstrap.boot_fs.options, "ro");
        assert_eq!(cfg.bootstrap.boot_fs.mountpoint, PathBuf::from("/mnt/boot"));
        assert_eq!(
            cfg.bootstrap.kernel_modules.explicit,
            vec!["vfat", "nls_cp437", "nls_iso8859_1", "ahci", "nvme"],
        );
        assert_eq!(
            cfg.bootstrap.rescue.default_url,
            "https://example.invalid/rescue.cpio",
        );
        assert_eq!(cfg.bootstrap.rescue.default_sha256, "deadbeef");
    }

    #[test]
    fn bootstrap_parses_minimal_with_defaults() {
        let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
        let cfg: BootstrapConfig = toml::from_str(toml).expect("minimal schema must parse");
        assert_eq!(
            cfg.bootstrap.config_path,
            PathBuf::from("/nmbl/config.toml")
        );
        assert_eq!(cfg.bootstrap.boot_fs.options, "");
        assert!(cfg.bootstrap.kernel_modules.explicit.is_empty());
        // Default mirrors `KernelModules::modules_dir` so the bootstrap
        // and full-config stages agree on where `modules.dep` lives.
        assert_eq!(
            cfg.bootstrap.kernel_modules.modules_dir,
            PathBuf::from("/lib/modules"),
        );
        assert_eq!(cfg.bootstrap.rescue.default_url, "");
        assert_eq!(cfg.bootstrap.rescue.default_sha256, "");
    }

    #[test]
    fn bootstrap_kernel_modules_dir_override_parses() {
        let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.kernel_modules]
explicit    = ["vfat"]
modules_dir = "/run/custom/modules"
"#;
        let cfg: BootstrapConfig = toml::from_str(toml).expect("override schema must parse");
        assert_eq!(
            cfg.bootstrap.kernel_modules.modules_dir,
            PathBuf::from("/run/custom/modules"),
        );
        assert_eq!(cfg.bootstrap.kernel_modules.explicit, vec!["vfat"]);
    }

    #[test]
    fn bootstrap_rejects_unknown_top_level_field() {
        let toml = r#"
[bootstrap]
config_path = "/nmbl/config.toml"
mystery     = "nope"

[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
        let err = toml::from_str::<BootstrapConfig>(toml)
            .expect_err("unknown field in [bootstrap] must be rejected");
        assert!(
            err.to_string().contains("mystery"),
            "error should mention the unknown field, got: {err}",
        );
    }

    #[test]
    fn bootstrap_rejects_unknown_boot_fs_field() {
        let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
secret     = "boom"
"#;
        toml::from_str::<BootstrapConfig>(toml)
            .expect_err("unknown field in boot_fs must be rejected");
    }

    #[test]
    fn bootstrap_rejects_missing_device() {
        let toml = r#"
[bootstrap.boot_fs]
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
        let err = toml::from_str::<BootstrapConfig>(toml)
            .expect_err("missing boot_fs.device must be rejected");
        assert!(
            err.to_string().contains("device"),
            "error should mention the missing field, got: {err}",
        );
    }

    #[test]
    fn bootstrap_rejects_missing_boot_fs_section() {
        let toml = r#"
[bootstrap]
config_path = "/nmbl/config.toml"
"#;
        toml::from_str::<BootstrapConfig>(toml)
            .expect_err("missing boot_fs section must be rejected");
    }

    #[test]
    fn bootstrap_rescue_section_optional() {
        let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
        let cfg: BootstrapConfig = toml::from_str(toml).expect("rescue must be optional");
        assert_eq!(cfg.bootstrap.rescue.default_url, "");
        assert_eq!(cfg.bootstrap.rescue.default_sha256, "");
    }

    #[test]
    fn bootstrap_load_reads_from_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bootstrap.toml");
        std::fs::write(
            &path,
            r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#,
        )
        .expect("write bootstrap toml");

        let cfg = BootstrapConfig::load(&path).expect("load must succeed");
        assert_eq!(cfg.bootstrap.boot_fs.device, "/dev/sda1");
    }

    #[test]
    fn bootstrap_load_missing_file_is_bootstrap_load_toml_error() {
        use std::error::Error;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nope.toml");
        let err = BootstrapConfig::load(&path).expect_err("missing file must error");
        match &err {
            NmblError::Bootstrap { stage, source } => {
                assert_eq!(*stage, "load-toml", "stage should mark the failed step");
                assert!(
                    matches!(source.as_ref(), NmblError::Io { .. }),
                    "Bootstrap should wrap an Io error, got: {source:?}",
                );
            }
            other => panic!("expected Bootstrap variant, got: {other:?}"),
        }
        // The chained source must reach the inner Io variant so the
        // emergency-shell banner's chain walker keeps working.
        let inner = Error::source(&err).expect("Bootstrap must expose a source");
        assert!(
            inner.to_string().contains("reading bootstrap config"),
            "inner source should describe the read step, got: {inner}",
        );
    }

    #[test]
    fn bootstrap_load_parse_failure_is_bootstrap_parse_toml_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "this is = not valid = toml").expect("write");
        let err = BootstrapConfig::load(&path).expect_err("bad toml must error");
        match &err {
            NmblError::Bootstrap { stage, source } => {
                assert_eq!(*stage, "parse-toml", "stage should mark the failed step");
                assert!(
                    matches!(source.as_ref(), NmblError::Config { .. }),
                    "Bootstrap should wrap a Config error, got: {source:?}",
                );
            }
            other => panic!("expected Bootstrap variant, got: {other:?}"),
        }
    }

    #[test]
    fn bootstrap_validate_rejects_url_without_sha() {
        let cfg = BootstrapConfig {
            bootstrap: BootstrapSection {
                config_path: default_bootstrap_config_path(),
                boot_fs: BootstrapBootFs {
                    device: "/dev/sda1".to_string(),
                    fstype: "vfat".to_string(),
                    options: String::new(),
                    mountpoint: PathBuf::from("/mnt/boot"),
                },
                kernel_modules: BootstrapKernelModules::default(),
                rescue: BootstrapRescue {
                    default_url: "https://example.invalid/rescue.cpio".to_string(),
                    default_sha256: String::new(),
                },
            },
        };
        let err = cfg.validate().expect_err("url without sha must reject");
        match err {
            NmblError::ConfigInvalid { reason, .. } => {
                assert!(reason.contains("default_url"), "{reason}");
                assert!(reason.contains("default_sha256"), "{reason}");
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn bootstrap_validate_rejects_sha_without_url() {
        let cfg = BootstrapConfig {
            bootstrap: BootstrapSection {
                config_path: default_bootstrap_config_path(),
                boot_fs: BootstrapBootFs {
                    device: "/dev/sda1".to_string(),
                    fstype: "vfat".to_string(),
                    options: String::new(),
                    mountpoint: PathBuf::from("/mnt/boot"),
                },
                kernel_modules: BootstrapKernelModules::default(),
                rescue: BootstrapRescue {
                    default_url: String::new(),
                    default_sha256: "deadbeef".to_string(),
                },
            },
        };
        cfg.validate().expect_err("sha without url must reject");
    }

    #[test]
    fn resolve_full_config_path_strips_leading_slash() {
        let mp = Path::new("/mnt/boot");
        let cp = Path::new("/nmbl/config.toml");
        assert_eq!(
            resolve_full_config_path(mp, cp),
            PathBuf::from("/mnt/boot/nmbl/config.toml"),
        );
    }

    #[test]
    fn resolve_full_config_path_joins_relative_path() {
        let mp = Path::new("/mnt/boot");
        let cp = Path::new("nmbl/config.toml");
        assert_eq!(
            resolve_full_config_path(mp, cp),
            PathBuf::from("/mnt/boot/nmbl/config.toml"),
        );
    }

    #[test]
    fn resolve_full_config_path_handles_nested_mountpoint() {
        let mp = Path::new("/run/nmbl/boot");
        let cp = Path::new("/nmbl/config.toml");
        assert_eq!(
            resolve_full_config_path(mp, cp),
            PathBuf::from("/run/nmbl/boot/nmbl/config.toml"),
        );
    }

    #[test]
    fn rescue_section_defaults_when_absent() {
        // Empty config — every section must default. The rescue section
        // is `#[serde(default)]` so absence is the operator's signal
        // that they want the legacy embedded shell behaviour.
        let cfg: Config = toml::from_str("").expect("missing rescue section must default");
        assert_eq!(cfg.rescue.mode, RescueMode::default());
        assert!(cfg.rescue.sfs_path.is_none());
    }

    #[test]
    fn rescue_section_parses_all_three_modes() {
        for (raw, expected) in [
            ("embedded", RescueMode::Embedded),
            ("external", RescueMode::External),
            ("none", RescueMode::None),
        ] {
            let toml = format!(
                r#"
[rescue]
mode = "{raw}"
"#
            );
            let cfg: Config = toml::from_str(&toml).expect("mode value must parse");
            assert_eq!(cfg.rescue.mode, expected, "mode={raw}");
        }
    }

    #[test]
    fn rescue_section_parses_sfs_path_override() {
        let toml = r#"
[rescue]
mode     = "external"
sfs_path = "/mnt/boot/nmbl-rescue.sfs"
"#;
        let cfg: Config = toml::from_str(toml).expect("override must parse");
        assert_eq!(cfg.rescue.mode, RescueMode::External);
        assert_eq!(
            cfg.rescue.sfs_path,
            Some(PathBuf::from("/mnt/boot/nmbl-rescue.sfs")),
        );
    }

    #[test]
    fn rescue_section_rejects_unknown_field() {
        let toml = r#"
[rescue]
mode    = "external"
mystery = "boom"
"#;
        toml::from_str::<Config>(toml).expect_err("unknown field must reject");
    }

    #[test]
    fn emergency_shell_defaults_to_empty_extra_consoles() {
        // The default — no opt-in — must pin the picker to /dev/console
        // only. Adding extra_consoles is an explicit operator action,
        // not a side effect of upgrading the config schema.
        let cfg: Config = toml::from_str("").expect("missing emergency_shell must default");
        assert!(cfg.emergency_shell.extra_consoles.is_empty());
    }

    #[test]
    fn emergency_shell_parses_extra_consoles_list() {
        let toml = r#"
[emergency_shell]
extra_consoles = ["/dev/ttyS0", "/dev/tty1"]
"#;
        let cfg: Config = toml::from_str(toml).expect("extra_consoles list must parse");
        assert_eq!(
            cfg.emergency_shell.extra_consoles,
            vec!["/dev/ttyS0".to_string(), "/dev/tty1".to_string()],
        );
    }

    #[test]
    fn emergency_shell_rejects_unknown_field() {
        let toml = r#"
[emergency_shell]
extra_consoles = []
mystery        = "boom"
"#;
        toml::from_str::<Config>(toml)
            .expect_err("unknown emergency_shell field must be rejected");
    }

    #[test]
    fn rescue_default_mode_is_embedded() {
        let cfg = RescueConfig::default();
        assert_eq!(cfg.mode, RescueMode::Embedded);
        assert!(cfg.sfs_path.is_none());
    }

    #[test]
    fn recovery_default_has_no_runtime_boot_mountpoint() {
        // Legacy embedded-config mode never mounts a boot partition, so
        // the recovery-default Config must report None for the runtime
        // mountpoint. `rescue::locate_sfs` keys off this to surface a
        // clear diagnostic instead of fabricating a path.
        let cfg = Config::recovery_default();
        assert!(cfg.runtime_boot_mountpoint.is_none());
    }

    #[test]
    fn runtime_boot_mountpoint_is_not_parsed_from_toml() {
        // The field is `#[serde(skip)]` so even if the operator's TOML
        // contains a stray top-level `runtime_boot_mountpoint = "…"` it
        // must be rejected as an unknown field by `deny_unknown_fields`.
        let toml = r#"runtime_boot_mountpoint = "/mnt/boot""#;
        toml::from_str::<Config>(toml).expect_err("runtime_boot_mountpoint is runtime-only");
    }

    #[test]
    fn bootstrap_validate_accepts_both_empty_and_both_set() {
        let mk = |url: &str, sha: &str| BootstrapConfig {
            bootstrap: BootstrapSection {
                config_path: default_bootstrap_config_path(),
                boot_fs: BootstrapBootFs {
                    device: "/dev/sda1".to_string(),
                    fstype: "vfat".to_string(),
                    options: String::new(),
                    mountpoint: PathBuf::from("/mnt/boot"),
                },
                kernel_modules: BootstrapKernelModules::default(),
                rescue: BootstrapRescue {
                    default_url: url.to_string(),
                    default_sha256: sha.to_string(),
                },
            },
        };
        mk("", "").validate().expect("both empty must pass");
        mk("https://example.invalid/r.cpio", "deadbeef")
            .validate()
            .expect("both set must pass");
    }
}
