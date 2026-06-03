//! Signature-verification subsystem (secure/staged-boot) — FROZEN public API.
//!
//! This module publishes the FROZEN signature-verification contract
//! (master-plan-v2 §B.3). Every #4/#5 consumer (generation guard, rescue-image
//! verify, driver-image load, staged merge, priority gate) binds to the
//! signatures re-exported here, so they are KEYSTONE-stable. The verify BODIES
//! are now REAL (F2 #14/#15): the fail-closed any-of ML-DSA pipeline in
//! [`verify`], backed by the parsed trust anchor in [`keys`]/[`baked_keys`].
//!
//! ## Module layout
//!
//! - [`wire`] — the ALWAYS-COMPILED leaf (FIX-25): the fixed-width sidecar
//!   codec + [`fp`]/`domain_tag`, shared byte-for-byte with the host signer.
//!   Compiled in EVERY build, with or without `secure-boot`.
//! - [`alg`] / [`sidecar`] — the typed `AlgId`/`HashId` + the borrowed
//!   `SigSidecar` view; `secure-boot`-gated (only the verify path needs them).
//!
//! The verify pipeline (`keys.rs`/`verify.rs`/`baked_keys.rs`/`tests/*` per
//! §B.1) holds the REAL ML-DSA fail-closed bodies (F2 #14/#15); the frozen
//! contract re-exported here is unchanged.

// The wire leaf is ALWAYS compiled (FIX-25): it is the single sidecar-format
// definition shared with `nmbl-sign`, and it must link in the default build.
// `alg` is its always-compiled dependency — `wire` reads `AlgId::sig_len()`
// (the single length source — FIX-46), so the two travel together as the leaf.
pub mod alg;
pub mod wire;

#[cfg(feature = "secure-boot")]
pub mod sidecar;

// The baked trust anchor + the parsed-key/verify pipeline are secure-boot
// gated. `baked_keys` is the committed-empty, Nix-regenerated trust material
// (R-5); `keys` parses it whole-set fail-closed (FIX-45); `verify` is the
// real any-of ML-DSA pipeline (FIX-01/46/50/51).
#[cfg(feature = "secure-boot")]
pub mod baked_keys;
#[cfg(feature = "secure-boot")]
pub mod gate;
#[cfg(feature = "secure-boot")]
pub mod keys;
// The per-generation sidecar locator (#18). Gated like the verify path it
// feeds: locating sidecars only matters when signatures are checked. Uses the
// shared `generations::gen_id` (FIX-07) so the scan path matches the signer's.
#[cfg(feature = "secure-boot")]
pub mod scan;
#[cfg(feature = "secure-boot")]
pub mod verify;

// Cross-cutting KATs (round-trip, domain-cross-reject, whole-set fail-closed).
#[cfg(all(test, feature = "secure-boot"))]
mod tests;

// `AlgId`/`HashId` are part of the always-compiled leaf so the wire codec and
// the host signer share one definition.
pub use alg::{AlgId, HashId};

// Re-export the frozen verify surface (FIX-62 file-set).
#[cfg(feature = "secure-boot")]
pub use sidecar::{SidecarError, SigSidecar};

#[cfg(feature = "secure-boot")]
pub use keys::{BakedKey, FullFp, VerifyingKeyEnum, fp, parse_baked_keys, resolve_allowed_keys};

// The per-generation sidecar locator (#18) and the boot-flow policy gate (#19).
#[cfg(feature = "secure-boot")]
pub use gate::{PolicyDecision, apply_policy, ensure_generation_signed_gated};
#[cfg(feature = "secure-boot")]
pub use scan::{GenBlob, SidecarResolution, generation_sig_dir, resolve_sig_sidecar};

#[cfg(feature = "secure-boot")]
pub use verify::{
    DOMAIN_DRIVER_IMAGE, DOMAIN_GEN_INITRD, DOMAIN_GEN_KERNEL, DOMAIN_PRIORITY_FILE,
    DOMAIN_RESCUE_SFS, DOMAIN_STAGED_FRAGMENT, VerifiedGeneration, VerifyPolicy,
    ensure_generation_signed, verify_digest, verify_generation_pinned, verify_image_fd,
    verify_image_fd_digest,
};

// The bytes-core verify entry: the ops-routed consumers (driver-image, staged,
// rescue) read their sidecar through `FsOps::read_file` (closure-aware) and call
// this with the bytes in hand, so a `--validate-initrm` dry-run verifies the
// sidecar from the extracted closure rather than the live host filesystem.
#[cfg(feature = "secure-boot")]
pub(crate) use verify::verify_image_fd_digest_bytes;

// ---- Always-compiled feature-presence probes (carried from the F1 stub) ----
// These prove at compile time that each optional dep the `secure-boot` feature
// pulls actually links (and that `fips204`'s pinned `ml-dsa-65`/`ml-dsa-87`
// features expose the `Ph::SHA512` pre-hash entry — FIX-50), so a bad feature
// pin fails fast here, not deep in #14/#15.
#[cfg(feature = "secure-boot")]
#[allow(unused_imports, reason = "dependency presence probe")]
use fips204::ml_dsa_65;
#[cfg(feature = "secure-boot")]
#[allow(unused_imports, reason = "dependency presence probe")]
use sha2::Sha512;
#[cfg(feature = "secure-boot")]
#[allow(unused_imports, reason = "dependency presence probe")]
use tpm2_protocol::TpmWriter;

/// Compile-time probe that `fips204`'s pre-hash `Ph::SHA512` variant is present
/// under the pinned features (FIX-50). A future feature/version change that
/// drops it is a hard build error HERE, not in the verify body.
#[cfg(feature = "secure-boot")]
const _: fips204::Ph = fips204::Ph::SHA512;
