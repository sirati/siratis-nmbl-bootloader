#[cfg(feature = "stateful")]
use serde::Deserialize;

/// `[stateful]` section. Required fields have no Rust defaults — the
/// Nix side enforces the operator-facing defaults so a typo'd TOML
/// fails parsing instead of silently picking a value.
#[cfg(feature = "stateful")]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatefulConfig {
    /// Maximum number of consecutive failed boots before the rollback
    /// loop gives up and surfaces a rescue condition.
    pub max_recovery_attempts: u32,

    /// systemd target whose `Reached` signal flips the
    /// `last_boot_succeeded` flag. Not consumed by NMBL at boot time —
    /// the systemd unit (Phase 5) runs `nmbl-init --boot-succeeded`
    /// after this target is reached — but parsed and stored so future
    /// boot-time consumers can read the same source of truth.
    pub success_target: String,
}
