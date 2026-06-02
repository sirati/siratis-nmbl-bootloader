use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{NmblError, Result};

pub(crate) mod bootstrap;
mod driver_image;
mod entries;
mod general;
mod paths;
mod rescue_cfg;
mod splash;
mod stateful_cfg;
mod tpm;
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
pub use driver_image::{DriverImageSpec, DriverImagesConfig};
pub use entries::{Activation, ActivationKind, FilesystemEntry};
pub use general::{General, KernelModules};
pub use paths::Paths;
pub use rescue_cfg::{EmergencyShellConfig, RescueConfig};
pub use tpm::{SealedSecret, TpmConfig};
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

    // ───────────────────── secure/staged-boot config anchor ─────────────────
    // INSERTION ANCHOR (FIX-60): the F1 security slices add their config
    // tables here, each a `#[serde(default)] pub <name>: <Cfg>,` line gated
    // as applicable. Inserting at this marker keeps the parallel slices from
    // colliding on the struct-field list. Add the matching default in
    // `recovery_default()` at its twin anchor.
    //   #6  signing : #[cfg(feature="secure-boot")] pub signing: SigningConfig,
    //   #7  tpm     : pub tpm: TpmConfig,            (always-compiled — FIX-09)
    //   #8  driver  : pub driver_images: DriverImageConfig,
    //   #9  staged  : #[cfg(feature="staged-boot")] pub staged: Option<StagedConfig>,
    //   #10 secureB : #[cfg(feature="secure-boot")] pub secure_boot: SecureBootConfig,
    // ─────────────────────────────────────────────────────────────────────────
    /// `[driver_images]` group (#8): verified out-of-tree driver squashfs
    /// blobs NMBL loop-mounts and `finit_module`s before kexec. Always
    /// compiled; the Nix side rejects `enable = true` without an active
    /// secure-boot table (FIX-05) so an unverified image is never honoured.
    #[serde(default)]
    pub driver_images: DriverImagesConfig,

    /// `[tpm]` measured-boot config (#7). ALWAYS compiled (FIX-09): the
    /// knobs live in the base schema regardless of the `secure-boot`
    /// feature, so a `[tpm]` table parses on every build.
    #[serde(default)]
    pub tpm: TpmConfig,

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
        self.validate_activation_covers_filesystems()?;
        Ok(())
    }

    /// Defense-in-depth satisfiability check (mirrors the Nix-side
    /// LVM-on-LUKS assertion). NMBL replaces NixOS stage-1, so it can
    /// only mount a filesystem whose device it can actually bring up
    /// with its own activation plan. We can't see `boot.initrd.luks`
    /// here (that's post-kexec config), but we CAN reject configs that
    /// are internally unsatisfiable: a `/dev/mapper/<X>` filesystem
    /// device that no activation could ever produce.
    ///
    /// A `/dev/mapper/*` device is satisfiable if AT LEAST ONE of:
    ///   - a `luks*` activation lists exactly that path in
    ///     `produces_devices`, OR
    ///   - an `lvm` activation is present (`vgchange -ay` can yield any
    ///     `/dev/mapper/<vg>-<lv>`), OR
    ///   - an `mdraid` activation is present.
    ///
    /// Bare `/dev/sd*`, `/dev/nvme*`, `/dev/vd*`, `/dev/md*` devices are
    /// not device-mapper nodes and are never subject to this check. We
    /// reject ONLY the definitely-unsatisfiable case — a false reject
    /// would brick a valid machine at boot.
    fn validate_activation_covers_filesystems(&self) -> Result<()> {
        let has_lvm = self
            .activations
            .iter()
            .any(|a| a.kind == ActivationKind::Lvm);
        let has_mdraid = self
            .activations
            .iter()
            .any(|a| a.kind == ActivationKind::Mdraid);
        // An LVM or mdraid activation can produce arbitrary mapper nodes,
        // so any mapper filesystem is plausibly satisfiable.
        if has_lvm || has_mdraid {
            return Ok(());
        }

        let is_luks = |k: ActivationKind| {
            matches!(
                k,
                ActivationKind::LuksTpm
                    | ActivationKind::LuksKeyfile
                    | ActivationKind::LuksPassword
            )
        };

        for fs in &self.filesystems {
            let dev = fs.device.as_str();
            if !dev.starts_with("/dev/mapper/") {
                continue;
            }
            let produced_by_luks = self.activations.iter().any(|a| {
                is_luks(a.kind)
                    && a.produces_devices
                        .iter()
                        .any(|p| p.as_os_str() == fs.device.as_str())
            });
            if !produced_by_luks {
                return Err(NmblError::ConfigInvalid {
                    reason: format!(
                        "device-mapper filesystem {dev:?} cannot be produced by any \
                         configured activation. NMBL replaces NixOS stage-1 and only \
                         brings up the devices its own [[activation]] plan describes; \
                         no luks activation produces this exact /dev/mapper node and \
                         there is no lvm or mdraid activation that could. Add a matching \
                         boot.nmbl.activation.luks entry (whose produces_devices is {dev:?}), \
                         or an lvm/mdraid activation if this volume is an LVM LV / RAID \
                         member — otherwise NMBL would wait for a device that never appears \
                         and the boot would hang"
                    ),
                    context: format!(
                        "validating activation coverage for filesystem {}",
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
            // ───────────── secure/staged-boot recovery_default anchor ─────────
            // INSERTION ANCHOR (FIX-60): twin of the struct-field anchor. Each
            // F1 security slice adds its field default here (e.g.
            // `signing: SigningConfig::default(),`), gated to match its
            // struct field. recovery_default must stay strict-shape (FIX-53):
            // reaching recovery never relaxes the security posture.
            // ──────────────────────────────────────────────────────────────────
            driver_images: DriverImagesConfig::default(),
            tpm: TpmConfig::default(),
            runtime_boot_mountpoint: None,
            #[cfg(feature = "stateful")]
            runtime_state_mountpoint: None,
        }
    }
}
