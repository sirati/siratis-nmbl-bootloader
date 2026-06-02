//! Signature-verification subsystem (secure/staged-boot).
//!
//! F1 SKELETON ONLY. This is an empty, no-op scaffold that exists so the
//! `secure-boot` Cargo feature resolves its optional dependencies
//! (`fips204`, `tpm2-protocol`, `sha2`) and compiles clean with the feature
//! both ON and OFF. The real ML-DSA verify pipeline (`alg`, `sidecar`,
//! `wire`, `keys`, `baked_keys`, `verify`, `gate`, `tests/*`) lands in F2
//! per master-plan-v2 §B.1; nothing here is on a trust path yet.
//!
//! The three `use` lines below are deliberate: they prove at compile time
//! that each optional dep the `secure-boot` feature pulls actually links
//! (and that `fips204`'s pinned `ml-dsa-65`/`ml-dsa-87` features expose the
//! `Ph::SHA512` pre-hash entry — FIX-50), so the F1 gate catches a bad
//! feature pin before any real code depends on it.

// Touch each optional dependency so the skeleton fails fast if a feature
// pin or version is wrong. `#[allow(unused_imports)]`: these are presence
// probes, not yet used by real code.
#[allow(unused_imports, reason = "F1 skeleton: dependency presence probe")]
use fips204::ml_dsa_65;
#[allow(unused_imports, reason = "F1 skeleton: dependency presence probe")]
use sha2::Sha512;
#[allow(unused_imports, reason = "F1 skeleton: dependency presence probe")]
use tpm2_protocol::TpmWriter;

/// Compile-time probe that `fips204`'s pre-hash `Ph::SHA512` variant is
/// present under the pinned features (FIX-50). Referencing the variant in a
/// `const` forces a hard build error here — not deep in F2 — if a future
/// feature/version change drops it.
const _: fips204::Ph = fips204::Ph::SHA512;
