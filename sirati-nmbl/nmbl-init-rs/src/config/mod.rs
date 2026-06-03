use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{NmblError, Result};

pub(crate) mod bootstrap;
mod driver_image;
mod entries;
mod fragment;
mod general;
mod paths;
mod rescue_cfg;
#[cfg(feature = "secure-boot")]
mod secure_boot;
#[cfg(feature = "secure-boot")]
mod signing;
mod splash;
#[cfg(feature = "staged-boot")]
mod staged;
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

#[cfg(feature = "staged-boot")]
pub use bootstrap::BootstrapStaged;
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

#[cfg(feature = "secure-boot")]
pub use secure_boot::{PriorityVolume, SecureBootConfig};

#[cfg(feature = "secure-boot")]
pub use signing::{SigningConfig, UkiSigningConfig};

#[cfg(feature = "image-splash")]
pub use splash::{Splash, SplashBackgroundLocation};

#[cfg(feature = "staged-boot")]
pub use fragment::{ConfigFragment, load_fragment};

#[cfg(feature = "staged-boot")]
pub use staged::StagedConfig;

#[cfg(feature = "stateful")]
pub use stateful_cfg::StatefulConfig;

#[derive(Debug, Clone, Deserialize)]
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

    /// `[signing]` table — signature-enforcement POLICY (secure-boot
    /// builds only). Carries enforcement posture only; the trust-anchor
    /// public keys are baked into the binary, never parsed from TOML
    /// (R-5/FIX-04). See [`SigningConfig`].
    #[cfg(feature = "secure-boot")]
    #[serde(default)]
    pub signing: SigningConfig,

    /// `[secure_boot]` table (#10) — the top-level secure-boot policy:
    /// the ONE [`PriorityVolume`] concept (R-3), the refuse-screen
    /// countdown, the rescue sentinel, and the enforcement/TPM posture.
    /// Secure-boot builds only; the Nix emit gate is `secureBootActive`
    /// (FIX-16), so a feature-free binary never receives a `[secure_boot]`
    /// table its `deny_unknown_fields` parser would reject.
    #[cfg(feature = "secure-boot")]
    #[serde(default)]
    pub secure_boot: SecureBootConfig,

    /// Top-level `[staged]` table naming the priority-volume image plus
    /// the signed config fragment + signature paths within it. Absent in
    /// non-staged builds and in staged builds whose Nix config did not
    /// enable `boot.nmbl.staged.enable`; the Nix emit gate is the same
    /// `staged-boot` boolean as this `#[cfg]` (FIX-40), so a build without
    /// the feature never receives a `[staged]` table it cannot parse.
    #[cfg(feature = "staged-boot")]
    #[serde(default)]
    pub staged: Option<StagedConfig>,

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

    /// Resolve the runtime mountpoint of the boot partition — the writable
    /// FAT/ESP that carries the per-generation signature sidecars
    /// (`<boot>/nmbl/sigs/<gen-id>/…`), the rescue sentinel, and the rescue
    /// squashfs.
    ///
    /// Phase 0.5 (bootstrap mode) mounts the boot fs out-of-band and records
    /// its mountpoint in [`Self::runtime_boot_mountpoint`], which is
    /// authoritative when present. In legacy embedded-config mode there is no
    /// Phase 0.5: the boot partition is just one of [`Self::filesystems`],
    /// mounted under `system_root` at its `/boot` mountpoint by
    /// [`crate::devices::mount_system_filesystems`]. We then derive the same
    /// `<system_root>/boot` path from that entry so the signature verify can
    /// locate the sidecars the install signer wrote to the boot partition.
    ///
    /// The boot entry is the non-root filesystem whose configured mountpoint
    /// is `/boot` — the SAME location the install-time generation signer
    /// hard-codes (`/boot/nmbl/sigs`). Returns `None` only when neither a
    /// runtime mountpoint nor a `/boot` filesystem entry exists.
    #[must_use]
    pub fn resolve_boot_mountpoint(&self) -> Option<PathBuf> {
        if let Some(mp) = self.runtime_boot_mountpoint.as_deref() {
            return Some(mp.to_path_buf());
        }
        let system_root = self.paths.system_root.as_path();
        self.filesystems
            .iter()
            .find(|fs| !fs.is_root && fs.mountpoint == Path::new("/boot"))
            .map(|fs| crate::devices::resolve_mountpoint(system_root, fs))
    }

    /// Parse a raw TOML string into a `Config`, WITHOUT the `validate()`
    /// pass or any I/O. Factored out of [`Config::load`] so the same parse
    /// step can be reused (e.g. by the staged-boot fragment loader, which
    /// shares this `deny_unknown_fields` decode but layers its own merge).
    /// `path` is carried only for the [`NmblError::Config`] diagnostic.
    pub fn parse_toml(text: &str, path: &Path) -> Result<Config> {
        toml::from_str(text).map_err(|source| NmblError::Config {
            source,
            path: path.to_path_buf(),
        })
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|source| NmblError::Io {
            source,
            context: format!("reading config file {}", path.display()),
        })?;

        let config = Config::parse_toml(&text, path)?;

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
            // signing: enforce stays false in recovery (audit-neutral); the
            // baked keys are unaffected, and reaching recovery never relaxes
            // the cap/seal posture (FIX-53/FIX-04).
            #[cfg(feature = "secure-boot")]
            signing: SigningConfig::default(),
            // secure_boot: default is disabled/audit-neutral — reaching
            // recovery never relaxes the cap/seal posture (FIX-53). The
            // priority gate is skipped (`enable = false`) but the sentinel
            // path and countdown stay at their single-sourced defaults.
            #[cfg(feature = "secure-boot")]
            secure_boot: SecureBootConfig::default(),
            // Recovery never self-mounts a staged fragment: `None` keeps
            // the loader on the verified base config only (FIX-53).
            #[cfg(feature = "staged-boot")]
            staged: None,
            runtime_boot_mountpoint: None,
            #[cfg(feature = "stateful")]
            runtime_state_mountpoint: None,
        }
    }
}
