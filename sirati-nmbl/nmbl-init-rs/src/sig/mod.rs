//! Signature-verification subsystem (secure/staged-boot) — FROZEN public API.
//!
//! This module is the facade that publishes the FROZEN signature-verification
//! contract (master-plan-v2 §B.3). Every #4/#5 consumer (generation guard,
//! rescue-image verify, driver-image load, staged merge, priority gate) binds
//! to the signatures here, so they are KEYSTONE-stable: the bodies are stubbed
//! for F2 #14/#15 (and return `Err(NmblError::Signature{stage:"stub-f2", …})`),
//! but the SHAPES below are final.
//!
//! ## Module layout
//!
//! - [`wire`] — the ALWAYS-COMPILED leaf (FIX-25): the fixed-width sidecar
//!   codec + [`fp`]/`domain_tag`, shared byte-for-byte with the host signer.
//!   Compiled in EVERY build, with or without `secure-boot`.
//! - [`alg`] / [`sidecar`] — the typed `AlgId`/`HashId` + the borrowed
//!   `SigSidecar` view; `secure-boot`-gated (only the verify path needs them).
//!
//! The verify pipeline (`keys.rs`/`verify.rs`/`gate.rs`/`tests/*` per §B.1) and
//! the real bodies land in F2 #14/#15; this commit FREEZES the contract.

// The wire leaf is ALWAYS compiled (FIX-25): it is the single sidecar-format
// definition shared with `nmbl-sign`, and it must link in the default build.
// `alg` is its always-compiled dependency — `wire` reads `AlgId::sig_len()`
// (the single length source — FIX-46), so the two travel together as the leaf.
pub mod alg;
pub mod wire;

#[cfg(feature = "secure-boot")]
pub mod sidecar;

#[cfg(feature = "secure-boot")]
mod facade;

// `AlgId`/`HashId` are part of the always-compiled leaf so the wire codec and
// the host signer share one definition.
pub use alg::{AlgId, HashId};

// Re-export the frozen verify surface (FIX-62 file-set). The verify entry
// points, policy/key types, and helpers all live behind the facade.
#[cfg(feature = "secure-boot")]
pub use sidecar::{SidecarError, SigSidecar};

#[cfg(feature = "secure-boot")]
pub use facade::{
    BakedKey, DOMAIN_DRIVER_IMAGE, DOMAIN_GEN_INITRD, DOMAIN_GEN_KERNEL, DOMAIN_PRIORITY_FILE,
    DOMAIN_RESCUE_SFS, DOMAIN_STAGED_FRAGMENT, FullFp, VerifyPolicy, ensure_generation_signed, fp,
    resolve_allowed_keys, verify_digest, verify_image_fd,
};

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
