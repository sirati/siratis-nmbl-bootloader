#[cfg(feature = "secure-boot")]
use std::path::PathBuf;

#[cfg(feature = "secure-boot")]
use serde::Deserialize;

#[cfg(feature = "secure-boot")]
use crate::security_consts::{REFUSE_COUNTDOWN_SECONDS, SENTINEL_PATH};

/// `[secure_boot]` table — the top-level secure-boot policy (slice #10).
///
/// Owns the ONE [`PriorityVolume`] concept (R-3): the first volume NMBL
/// mounts and verifies a signed file on before it will proceed to a
/// measured boot or consume a staged fragment. Carries the refuse-screen
/// countdown, the rescue sentinel path, and the enforcement/TPM posture.
///
/// Gated behind `secure-boot` (the verifier is required for any of this to
/// mean anything). The Nix side (`lib/modules/security/secure-boot.nix`)
/// asserts `enable ⇒ enforce` unless `allow_audit_mode_insecure` (FIX-31)
/// and `enable ⇒ (signing.enable || keys)` so a #5 gate has a trust anchor.
///
/// `deny_unknown_fields` keeps the Nix emitter and this struct from
/// silently drifting; every field is `#[serde(default)]` so an absent
/// `[secure_boot]` table decodes to the disabled, audit-neutral default.
#[cfg(feature = "secure-boot")]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecureBootConfig {
    /// Master switch. When `false` (the default) the secure-boot priority
    /// gate is skipped entirely — the runtime takes the legacy boot path.
    /// Enabling it pulls the `secure-boot` Cargo feature in via the Nix
    /// `secureBootActive` implication (FIX-16).
    #[serde(default)]
    pub enable: bool,

    /// The priority volume NMBL mounts (read-only) and verifies a signed
    /// file on before proceeding (R-3). `None` when the operator has not
    /// configured a priority volume; the Nix side only emits this when a
    /// device is set.
    #[serde(default)]
    pub priority_volume: Option<PriorityVolume>,

    /// Boot-partition-relative path of the signed file NMBL reads off the
    /// priority volume and verifies against the baked trust anchor (domain
    /// `b"nmbl:priority-file:v1"`). Joined against the priority-volume
    /// mountpoint at runtime.
    #[serde(default)]
    pub signed_file_path: PathBuf,

    /// Trust-anchor key fingerprints the priority gate narrows to before
    /// verifying (full `fp()` fingerprints, R-3/FIX-08). Empty narrows to
    /// the whole baked set. The Nix side build-warns when more than one key
    /// is baked and this is empty (FIX-54).
    #[serde(default)]
    pub allowed_key_ids: Vec<String>,

    /// Sentinel file whose presence forces a rescue boot and keeps the TPM
    /// capped (FIX-21/FIX-38). Defaults to the single-sourced
    /// [`SENTINEL_PATH`] (`/boot/nmbl/rescue`), kept in lockstep with the
    /// Nix `security-consts.nix` `sentinelPath`.
    #[serde(default = "default_sentinel_path")]
    pub sentinel_path: PathBuf,

    /// Fail-closed enforcement. When `true`, a priority-gate / signature
    /// failure refuses the boot (reboots into rescue). When `false` with
    /// `enable = true` the checks run but only warn — audit mode, which the
    /// Nix assertion `enable ⇒ enforce` gates behind the separate
    /// `allow_audit_mode_insecure` flag (FIX-31).
    #[serde(default)]
    pub enforce: bool,

    /// Hard-require a working TPM for the secure-boot path. Mirrors
    /// `tpm.require_tpm`; surfaced here so the secure-boot table can demand
    /// TPM presence even when `[tpm].measure` is left at its default. The
    /// Nix layer defaults this on for measuring/secure builds (FIX-28).
    #[serde(default)]
    pub require_tpm: bool,

    /// Countdown, in seconds, for the non-interactive refuse screen before
    /// auto-reboot (R-13/FIX-39). The ONLY path; `policy.*` is superseded.
    /// Defaults to the single-sourced [`REFUSE_COUNTDOWN_SECONDS`] (= 30).
    #[serde(default = "default_refuse_countdown_seconds")]
    pub refuse_countdown_seconds: u32,

    /// Deliberate opt-in to insecure audit mode (`enable && !enforce`).
    /// Required by the Nix `enable ⇒ enforce` assertion so audit mode needs
    /// two distinct flags, never one (FIX-31). Defaults to `false`.
    #[serde(default)]
    pub allow_audit_mode_insecure: bool,
}

/// The ONE priority/first-volume concept (R-3): the volume NMBL mounts
/// read-only and verifies a signed file on before proceeding to a measured
/// boot or consuming a staged fragment.
///
/// `inside_luks` marks a priority volume that is itself a LUKS-backed
/// mapper node, so the gate knows the volume only exists after a LUKS
/// activation (and the refuse path must close that mapper). The default
/// `options` are the hardened read-only set; the gate always mounts `ro`.
#[cfg(feature = "secure-boot")]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorityVolume {
    /// Block device (or `/dev/mapper/<x>` node) backing the priority
    /// volume. Resolved the same way a `[[filesystems]]` device is.
    pub device: PathBuf,

    /// Initramfs mountpoint the gate binds the priority volume at before
    /// reading the signed file.
    pub mountpoint: PathBuf,

    /// Filesystem type NMBL mounts the priority volume as.
    pub fstype: String,

    /// Mount options. Defaults to the hardened read-only set
    /// (`ro,nosuid,nodev,noexec`); the gate always enforces `ro`
    /// regardless.
    #[serde(default = "default_priority_options")]
    pub options: String,

    /// Whether the priority volume is a LUKS-backed mapper node. When
    /// `true` the volume only appears after a LUKS activation, and the
    /// refuse path closes the mapper as part of sealing (FIX-03/FIX-21).
    #[serde(default)]
    pub inside_luks: bool,
}

#[cfg(feature = "secure-boot")]
fn default_sentinel_path() -> PathBuf {
    PathBuf::from(SENTINEL_PATH)
}

#[cfg(feature = "secure-boot")]
fn default_refuse_countdown_seconds() -> u32 {
    REFUSE_COUNTDOWN_SECONDS
}

#[cfg(feature = "secure-boot")]
fn default_priority_options() -> String {
    "ro,nosuid,nodev,noexec".to_owned()
}

#[cfg(feature = "secure-boot")]
impl Default for SecureBootConfig {
    fn default() -> Self {
        Self {
            enable: false,
            priority_volume: None,
            signed_file_path: PathBuf::new(),
            allowed_key_ids: Vec::new(),
            sentinel_path: default_sentinel_path(),
            enforce: false,
            require_tpm: false,
            refuse_countdown_seconds: default_refuse_countdown_seconds(),
            allow_audit_mode_insecure: false,
        }
    }
}
