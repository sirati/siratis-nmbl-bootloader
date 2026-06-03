//! TPM 2.0 cap/transport/presence core (ALWAYS-COMPILED — FIX-09).
//!
//! This subtree is compiled in EVERY build, independent of the
//! `secure-boot` feature, because the lock-on-rescue guard
//! (`policy::guard`) must be able to cap the lock PCR — and to know
//! whether a TPM is even present — even when secure-boot is off. The
//! only cost is the non-optional, pure-Rust [`tpm2_protocol`] marshaler
//! (zero-dep / `no_std`); no C-FFI / `*-sys` / `openssl` crate enters the
//! closure (FIX-43).
//!
//! Layout:
//! - [`transport`] — `/dev/tpmrm0` open + `transact` (rustix only, NO
//!   `ioctl`, zero new `unsafe`).
//! - [`commands`] — `pcr_read` / `pcr_extend` / [`cap_pcr_outcome`] built on
//!   `tpm2-protocol` marshaling, with EXPLICIT response-code checking
//!   (a non-success RC ⇒ [`CapOutcome::Failed`] — FIX-27).
//! - [`presence`] — deterministic sysfs presence latch (FIX-28).
//!
//! The `secure-boot`-only PCR-11 measure path (`tpm/measure.rs`) is added
//! later and is feature-gated; nothing here is.

pub mod commands;
pub mod presence;
pub mod transport;

#[cfg(test)]
mod tests;

pub use commands::{cap_lock_pcr, cap_pcr, cap_pcr_outcome, pcr_extend, pcr_read};
pub use presence::tpm_present;
pub use transport::TpmDevice;

/// The PCR the measured-boot path caps to poison TPM-sealed secrets on a
/// refuse (R-2 / FIX-38). Single-sourced from [`crate::security_consts`]
/// so the Nix mirror (`lib/security-consts.nix`) and this module never
/// drift. Value: `11`.
pub const LOCK_PCR: u32 = crate::security_consts::LOCK_PCR;

/// The 32-byte poison value extended into [`LOCK_PCR`] to irreversibly
/// invalidate any TPM-sealed secret on a refuse (one-way: a PCR can only be
/// extended, never rolled back without a reboot/reset).
///
/// Derived as `SHA-256(`[`crate::security_consts::RELOCK_POISON_PREIMAGE`]`)`
/// = `SHA-256(b"nmbl:relock-poison:v1")`. The digest is committed here as a
/// literal so the value is available in EVERY build (`sha2` is an optional
/// dep — FIX-09), and the `#[cfg]`-gated `poison_self_check` test in
/// [`tests`] recomputes it from the preimage and asserts equality, so a
/// drift on either side is a test-time failure (FIX-38).
pub const RELOCK_POISON: [u8; 32] = [
    0x38, 0x97, 0x99, 0x4c, 0x99, 0xb8, 0x5d, 0x89, 0xd0, 0x98, 0xf4, 0xe5, 0x48, 0x05, 0x9f, 0x43,
    0xe2, 0x34, 0xa1, 0xd1, 0x6d, 0xf2, 0xa5, 0xcf, 0x72, 0x2f, 0x3b, 0x4b, 0xea, 0x35, 0xa0, 0x1b,
];

/// The rich outcome of an attempt to cap (poison) the lock PCR (R-7 /
/// FIX-27). Config-free by construction: the policy layer decides what
/// each variant means (e.g. `NoTpm` ⇒ degrade-open unless `requireTpm`;
/// `Failed` ⇒ fail-closed ALWAYS; a present-but-uncappable TPM is
/// `Failed`, never `NoTpm`).
#[derive(Debug)]
pub enum CapOutcome {
    /// The lock PCR was successfully extended with [`RELOCK_POISON`] and the
    /// TPM acknowledged with `TPM_RC_SUCCESS`. The only outcome on which the
    /// guard's latch may flip to "sealed".
    Capped,
    /// No TPM is present (`/dev/tpmrm0` is absent / could not be opened).
    /// There is nothing to cap; the policy layer decides degrade-open vs
    /// fail-closed based on `requireTpm`.
    NoTpm,
    /// A TPM IS present but the cap did NOT complete successfully — a
    /// transport error, a marshal/unmarshal failure, or (critically) a
    /// non-success response code (FIX-27). ALWAYS fail-closed: a present
    /// TPM whose cap we cannot confirm must divert to refuse, never be
    /// treated as a benign `NoTpm`.
    Failed(crate::error::NmblError),
}
