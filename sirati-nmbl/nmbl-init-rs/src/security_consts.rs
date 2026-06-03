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

/// The committed 32-byte poison digest, pinned independently of `sha2` so the
/// always-compiled `security_consts` test catches a silent change to the
/// derived value even in a feature-free build (FIX-38). It MUST equal
/// `SHA-256(`[`RELOCK_POISON_PREIMAGE`]`)` — the `sha2`-gated
/// `tpm::tests::poison_self_check` recomputes and asserts that derivation; here
/// we pin the resulting bytes so a hand-edit of the `tpm::RELOCK_POISON` literal
/// (or this one) is also caught. Mirror: `security-consts.nix`
/// `defaults.relockPoisonDigest`.
#[cfg(test)]
pub(crate) const RELOCK_POISON_DIGEST: [u8; 32] = [
    0x38, 0x97, 0x99, 0x4c, 0x99, 0xb8, 0x5d, 0x89, 0xd0, 0x98, 0xf4, 0xe5, 0x48, 0x05, 0x9f, 0x43,
    0xe2, 0x34, 0xa1, 0xd1, 0x6d, 0xf2, 0xa5, 0xcf, 0x72, 0x2f, 0x3b, 0x4b, 0xea, 0x35, 0xa0, 0x1b,
];

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on the pinned security-const values"
)]
mod tests {
    use super::*;

    // ── The Rust-side pins (FIX-38) ──────────────────────────────────────
    //
    // The literals below are the machine-checked anchor. `nmbl_init_security_
    // consts_match_nix` (further down) then reads the ACTUAL
    // `lib/security-consts.nix` and asserts the Nix `defaults.*` equal these,
    // so a silent change to EITHER side is caught: the Rust pin fails if a
    // slice edits the const here without the Nix file, and the Nix-agreement
    // test fails if the Nix file moves without the const.

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

    /// The poison DIGEST the lock PCR is capped with is pinned here byte-for-
    /// byte (FIX-38). This runs in EVERY build (no `sha2` needed), so even a
    /// feature-free test run catches a hand-edit of the committed digest; the
    /// `sha2`-gated `tpm::tests::poison_self_check` separately proves the bytes
    /// really are `SHA-256(RELOCK_POISON_PREIMAGE)`.
    #[test]
    fn relock_poison_digest_is_pinned() {
        assert_eq!(
            crate::tpm::RELOCK_POISON,
            RELOCK_POISON_DIGEST,
            "tpm::RELOCK_POISON must equal the pinned digest (FIX-38)"
        );
    }

    // ── The Nix-side agreement (FIX-38) ──────────────────────────────────
    //
    // The pins above are only HALF the contract: they catch a drift in the
    // Rust file, but a silent change to `lib/security-consts.nix` would slip
    // through if nothing read that file. This test does: it parses the actual
    // Nix mirror and asserts each `defaults.*` equals the Rust const, so a
    // security default cannot move on the Nix side without breaking the build.
    //
    // The Nix file lives one level up from the crate root
    // (`$CARGO_MANIFEST_DIR/../lib/security-consts.nix`), OUTSIDE the crate's
    // own source tree, so it is unreachable inside the sandboxed `nix build`
    // (whose source is just the crate). Like the `NMBL_TEST_FONT` precedent,
    // the test then skips cleanly: the `nmbl-init-security-consts` flake check
    // (which greps the Rust literals) covers the sandbox case, and `cargo test`
    // in the worktree exercises the live cross-file agreement.

    /// Pull the value following `<key> = ` up to the line's `;` from the Nix
    /// `defaults` block. Returns the raw token (still quoted for strings).
    fn nix_default(nix: &str, key: &str) -> Option<String> {
        let needle = format!("{key} = ");
        let line = nix.lines().find(|l| l.trim_start().starts_with(&needle))?;
        let after = line.split_once(&needle)?.1;
        Some(after.split(';').next()?.trim().to_string())
    }

    #[test]
    fn nmbl_init_security_consts_match_nix() {
        let nix_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../lib/security-consts.nix");
        let Ok(nix) = std::fs::read_to_string(nix_path) else {
            // Outside the worktree (e.g. the sandboxed nix build): the Nix
            // mirror is not part of the crate source. Skip — the flake check
            // covers this case. (Mirrors the NMBL_TEST_FONT skip.)
            eprintln!(
                "security-consts.nix not reachable at {nix_path}; skipping Nix-agreement test"
            );
            return;
        };

        assert_eq!(
            nix_default(&nix, "lockPcr").as_deref(),
            Some(LOCK_PCR.to_string().as_str()),
            "security-consts.nix defaults.lockPcr must equal Rust LOCK_PCR"
        );
        assert_eq!(
            nix_default(&nix, "refuseCountdownSeconds").as_deref(),
            Some(REFUSE_COUNTDOWN_SECONDS.to_string().as_str()),
            "security-consts.nix defaults.refuseCountdownSeconds must equal Rust REFUSE_COUNTDOWN_SECONDS"
        );
        let preimage_str = std::str::from_utf8(RELOCK_POISON_PREIMAGE).unwrap();
        assert_eq!(
            nix_default(&nix, "relockPoisonPreimage").as_deref(),
            Some(format!("\"{preimage_str}\"").as_str()),
            "security-consts.nix defaults.relockPoisonPreimage must equal Rust RELOCK_POISON_PREIMAGE"
        );
        assert_eq!(
            nix_default(&nix, "sentinelPath").as_deref(),
            Some(format!("\"{SENTINEL_PATH}\"").as_str()),
            "security-consts.nix defaults.sentinelPath must equal Rust SENTINEL_PATH"
        );
    }
}
