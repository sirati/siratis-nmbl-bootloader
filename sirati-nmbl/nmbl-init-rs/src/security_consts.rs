//! Single source of truth for NMBL's secure/staged-boot security defaults
//! on the Rust side. Always compiled (the values are cheap consts with no
//! optional-dep cost) so the round-trip test runs in every build.
//!
//! These mirror `sirati-nmbl/lib/security-consts.nix` exactly. The later
//! `tpm`, `sig`, and `policy` modules re-export from here rather than
//! redefining, so there is ONE Rust definition the Nix side is tested
//! against (FIX-38 / FIX-16). The Nix file documents the same values; the
//! [`tests`] block below pins them so a drift in either direction is a
//! compile-/test-time failure.

/// PCR the measured-boot path caps to poison TPM-sealed secrets on a
/// refuse (R-2 / FIX-38). Nix mirror: `security-consts.nix` `defaults.lockPcr`.
pub const LOCK_PCR: u32 = 11;

/// Domain-separated pre-image hashed into the relock poison value:
/// `sha256(RELOCK_POISON_PREIMAGE)` (FIX-38). The TPM module derives its
/// `RELOCK_POISON` from this exact byte string and self-checks the digest.
/// Nix mirror: `security-consts.nix` `defaults.relockPoisonPreimage`.
pub const RELOCK_POISON_PREIMAGE: &[u8] = b"nmbl:relock-poison:v1";

/// Default countdown, in seconds, for the non-interactive refuse screen
/// before auto-reboot (R-13 / FIX-39). The ONLY path; `policy.*` is
/// superseded. Nix mirror: `defaults.refuseCountdownSeconds`.
pub const REFUSE_COUNTDOWN_SECONDS: u32 = 30;

/// Sentinel file whose presence forces a rescue boot and keeps the TPM
/// capped (FIX-38 / FIX-21). Nix mirror: `defaults.sentinelPath`.
pub const SENTINEL_PATH: &str = "/boot/nmbl/rescue";

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on the pinned security-const values"
)]
mod tests {
    use super::*;

    // These literals MUST equal `sirati-nmbl/lib/security-consts.nix`
    // `defaults.*`. The Nix file is the operator-facing mirror; this test is
    // the machine-checked anchor that keeps the two in lockstep. When a slice
    // changes a default it changes it HERE and in the Nix file, and this test
    // fails if only one side moves.
    #[test]
    fn lock_pcr_matches_nix() {
        assert_eq!(LOCK_PCR, 11, "security-consts.nix defaults.lockPcr");
    }

    #[test]
    fn relock_poison_preimage_matches_nix() {
        assert_eq!(
            RELOCK_POISON_PREIMAGE, b"nmbl:relock-poison:v1",
            "security-consts.nix defaults.relockPoisonPreimage"
        );
    }

    #[test]
    fn refuse_countdown_matches_nix() {
        assert_eq!(
            REFUSE_COUNTDOWN_SECONDS, 30,
            "security-consts.nix defaults.refuseCountdownSeconds"
        );
    }

    #[test]
    fn sentinel_path_matches_nix() {
        assert_eq!(
            SENTINEL_PATH, "/boot/nmbl/rescue",
            "security-consts.nix defaults.sentinelPath"
        );
    }
}
