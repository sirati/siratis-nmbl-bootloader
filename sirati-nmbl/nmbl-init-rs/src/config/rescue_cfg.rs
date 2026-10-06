use std::path::PathBuf;

use serde::Deserialize;

use crate::rescue::{RescueImageFormat, RescueMode};

/// `[rescue]` section of the operator's runtime config. Selects the
/// rescue mode (see [`RescueMode`]) and optionally pins the on-disk
/// path of `nmbl-rescue.sfs`. The network-rescue fields (Phase E.1)
/// supply the disk-rescue fallback that fetches `nmbl-rescue.sfs`
/// from an operator-pinned HTTP URL.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueConfig {
    /// Which rescue path [`crate::rescue::dispatch`] takes. Defaults to
    /// [`RescueMode::Embedded`] to preserve the legacy behaviour for
    /// installs that have not opted in to the external squashfs.
    #[serde(default)]
    pub mode: RescueMode,
    #[serde(default)]
    pub identity_volume: Option<RescueIdentityVolume>,

    /// Path to `nmbl-rescue.sfs` RELATIVE TO THE BOOT PARTITION ROOT.
    /// A leading `/` is tolerated and stripped at resolution time. When
    /// `None` the rescue dispatcher uses the default
    /// `"nmbl-rescue.sfs"`. The runtime mountpoint is supplied
    /// out-of-band via [`Config::runtime_boot_mountpoint`] (populated by
    /// Phase 0.5), so this value is always boot-partition-relative
    /// regardless of where the operator's boot is mounted.
    #[serde(default)]
    pub sfs_path: Option<PathBuf>,

    /// `[rescue.image]`: format and pinned digest of the image at
    /// `sfs_path`. Rendered by the Nix build next to the image it
    /// describes, so a config always names exactly one rescue image.
    #[serde(default)]
    pub image: RescueImage,

    /// `[rescue.network_stage]`: the signed rescue networking EROFS the
    /// full-system rescue mounts at `/nmbl-network`. Absent when the
    /// rescue carries its own drivers and network profile.
    #[serde(default)]
    pub network_stage: Option<RescueNetworkStage>,

    /// `[rescue.tools]`: the separately pinned EROFS carrying `nmblctl` and
    /// its closure, mounted at `/nmbl-tools`. Kept out of the stage-2 image
    /// because `nmblctl` is built with this host's signing public keys.
    #[serde(default)]
    pub tools: Option<RescueTools>,

    /// `[rescue.system]`: host data for the full-system rescue. The stage-2
    /// image is host-independent; NMBL hands these values to its `/init`
    /// at runtime (see `crate::rescue::host`).
    #[serde(default)]
    pub system: Option<RescueSystem>,

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

    /// Absolute path INSIDE the rescue squashfs that the loader
    /// `execve`s after switch_root. Defaults to `/bin/sh` (the flat
    /// busybox image). The full recovery system (`fullSystem.enable`)
    /// sets this to `/init`, a bash PID-1 script that brings up
    /// pseudo-filesystems, an overlay'd writable store, networking, the
    /// nix-daemon and sshd before dropping to a console shell. Matches
    /// `boot.nmbl.rescue.fullSystem` wiring emitted by config-toml.nix.
    #[serde(default = "default_rescue_entrypoint")]
    pub entrypoint: PathBuf,

    /// Test/recovery escape hatch: when `true`, NMBL skips the normal
    /// generation-boot flow and goes straight to [`crate::rescue::dispatch`]
    /// on every boot (only meaningful with `mode = "external"`). Defaults
    /// to `false` so production boots are unaffected. The check runs right
    /// after Phase 0.5 mounts the boot partition (so the runtime boot
    /// mountpoint the disk-rescue path needs is already known) and before
    /// any interactive console comes up — making it a fully deterministic,
    /// no-input trigger for automated rescue verification. Matches
    /// `boot.nmbl.rescue.forceOnBoot`.
    #[serde(default)]
    pub force_on_boot: bool,

    /// The ONE setting that decides whether a failed boot with nothing left
    /// to fall back to enters rescue (`true`) or the emergency menu
    /// (`false`). See [`crate::rescue::automatic`]. `mode` only selects which
    /// rescue is entered. Matches `boot.nmbl.rescue.automatic`.
    #[serde(default)]
    pub automatic: bool,
}

fn default_rescue_entrypoint() -> PathBuf {
    PathBuf::from("/bin/sh")
}

impl Default for RescueConfig {
    fn default() -> Self {
        Self {
            mode: RescueMode::default(),
            identity_volume: None,
            sfs_path: None,
            image: RescueImage::default(),
            network_stage: None,
            tools: None,
            system: None,
            network: false,
            default_url: String::new(),
            default_sha256: String::new(),
            entrypoint: default_rescue_entrypoint(),
            force_on_boot: false,
            automatic: false,
        }
    }
}

/// `[rescue.image]`: what NMBL mounts as the rescue root.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueImage {
    /// Filesystem of the image; NMBL loads only this module to mount it.
    #[serde(default)]
    pub format: RescueImageFormat,
    /// Lowercase hex SHA-512 of the exact image this config was built
    /// with. When present the image is refused unless its bytes (read
    /// over the same pinned fd that is then loop-bound) match, so even an
    /// older image signed with the same key cannot be substituted.
    #[serde(default)]
    pub sha512: Option<String>,
}

/// `[rescue.system]`: what makes the shared stage-2 rescue image this
/// host's rescue.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueSystem {
    /// TCP port the rescue sshd listens on.
    pub sshd_port: u16,
    /// `authorized_keys` lines for root.
    #[serde(default)]
    pub authorized_keys: Vec<String>,
    /// Persistent SSH host key in NMBL's namespace (seen by the rescue
    /// under `/nmbl-root`). Without it the rescue generates an ephemeral
    /// key.
    #[serde(default)]
    pub host_key_path: Option<PathBuf>,
    /// Kernel modules the rescue loads from its own module tree.
    #[serde(default)]
    pub modules: Vec<String>,
    /// The data-only network profile, when no networking stage carries it.
    #[serde(default)]
    pub network_profile: Option<String>,
}

/// `[rescue.network_stage]`: the signed networking EROFS for the rescue.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueNetworkStage {
    /// Boot-partition-relative path of the EROFS image.
    pub path: PathBuf,
    /// Lowercase hex SHA-512 of the exact stage this config was built with.
    #[serde(default)]
    pub sha512: Option<String>,
}

/// `[rescue.tools]`: the rescue tools EROFS.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueTools {
    /// Boot-partition-relative path of the EROFS image.
    pub path: PathBuf,
    /// Lowercase hex SHA-512 of the exact image this config was built with.
    /// Required: an unpinned tools image is never mounted.
    pub sha512: String,
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

/// Plaintext installed identity volume; never unlocked by rescue.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RescueIdentityVolume {
    pub device: PathBuf,
    pub fstype: String,
    #[serde(default)]
    pub options: Vec<String>,
    /// Crypto providers not discoverable through filesystem modules.dep.
    #[serde(default)]
    pub required_modules: Vec<String>,
}
