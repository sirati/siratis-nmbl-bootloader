//! Small shared primitives reused across boot paths (master-plan §B.1).
//!
//! `hex` is UNGATED — a pure lowercase-hex encoder with no optional deps.
//! `hash` is gated `any(network-rescue, secure-boot)` because it `use`s the
//! optional `sha2` dependency (FIX-23): an ungated import would break the
//! default, feature-free build.

pub mod hex;

#[cfg(any(feature = "network-rescue", feature = "secure-boot"))]
pub mod hash;
