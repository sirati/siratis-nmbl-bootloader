use std::path::PathBuf;

use serde::Deserialize;

use crate::rescue::RescueMode;

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
}

fn default_rescue_entrypoint() -> PathBuf {
    PathBuf::from("/bin/sh")
}

impl Default for RescueConfig {
    fn default() -> Self {
        Self {
            mode: RescueMode::default(),
            sfs_path: None,
            network: false,
            default_url: String::new(),
            default_sha256: String::new(),
            entrypoint: default_rescue_entrypoint(),
            force_on_boot: false,
        }
    }
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
