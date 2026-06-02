use std::path::PathBuf;

use serde::Deserialize;

use crate::security_consts::LOCK_PCR;

/// `[tpm]` table. ALWAYS compiled (FIX-09): the TPM knobs live in the
/// base schema regardless of the `secure-boot` Cargo feature, so a
/// config that sets `[tpm]` parses identically on a feature-free build
/// and the runtime can reason about the measured-boot posture without a
/// cfg-gated struct. The actual measuring/sealing code in
/// `src/tpm/measure` IS `secure-boot`-gated; this is only the config.
///
/// Wire-mirrored by `lib/modules/security/tpm.nix` and emitted by
/// `lib/config-toml.nix`. `deny_unknown_fields` keeps the Nix emitter
/// and this struct from silently drifting.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TpmConfig {
    /// Extend the lock PCR with NMBL's boot events (R-7). When `false`
    /// (the default) NMBL performs no measurement and the TPM modules
    /// are not force-loaded early.
    #[serde(default)]
    pub measure: bool,

    /// PCR index NMBL measures into / caps to poison sealed secrets on a
    /// refuse. Defaults to the single-sourced lock PCR (`LOCK_PCR`, = 11),
    /// kept in lockstep with the Nix `security-consts.nix` `lockPcr`.
    #[serde(default = "default_pcr_index")]
    pub pcr_index: u32,

    /// Hard-require a working TPM: abort the boot if the device is
    /// absent or unusable instead of degrading to an unmeasured boot
    /// (FIX-28). The Nix layer defaults this to `true` whenever
    /// `measure` or secure boot is on, else `false`; the runtime serde
    /// default mirrors the "off" tail so a hand-written TOML without the
    /// key keeps the permissive posture.
    #[serde(default)]
    pub require_tpm: bool,

    /// Resource-manager TPM device NMBL talks to. `/dev/tpmrm0` (the
    /// in-kernel resource manager) by default — never the raw
    /// `/dev/tpm0`, which would contend with other TPM users.
    #[serde(default = "default_device")]
    pub device: PathBuf,

    /// Secrets sealed to the TPM that NMBL unseals at boot (gated on the
    /// PCR policy). Empty by default. Each entry names a sealed blob on
    /// the boot partition and where its plaintext is materialised in the
    /// initramfs after a successful unseal.
    #[serde(default)]
    pub sealed_secrets: Vec<SealedSecret>,
}

/// One TPM-sealed secret: a sealed blob staged on the boot partition and
/// the in-initramfs path its unsealed plaintext is written to.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealedSecret {
    /// Stable identifier used in logs and the TUI.
    pub name: String,

    /// Boot-partition-relative path to the sealed blob, resolved against
    /// the runtime boot mountpoint (Phase 0.5) the same way the rescue
    /// `sfs_path` is.
    pub sealed_path: PathBuf,

    /// Absolute initramfs path the unsealed plaintext is written to.
    pub unseal_to: PathBuf,
}

impl Default for TpmConfig {
    fn default() -> Self {
        Self {
            measure: false,
            pcr_index: default_pcr_index(),
            require_tpm: false,
            device: default_device(),
            sealed_secrets: Vec::new(),
        }
    }
}

fn default_pcr_index() -> u32 {
    LOCK_PCR
}

fn default_device() -> PathBuf {
    PathBuf::from("/dev/tpmrm0")
}
