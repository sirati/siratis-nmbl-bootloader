//! Signature-algorithm and digest identifiers — the SINGLE source of the
//! ML-DSA public-key and signature lengths (FIX-46).
//!
//! Both the sidecar wire format (`sig/wire.rs`) and the verifier
//! (`sig/verify.rs`, F2 #15) read `pk_len()`/`sig_len()` from here, so the
//! parser and the verifier can never disagree on how many bytes a signature
//! or key occupies. The numeric discriminants are the on-wire `alg_id`/
//! `hash_id` byte values; they are part of the frozen sidecar v1 contract
//! and MUST NOT change.

/// Signature algorithm baked into the sidecar header `alg_id` byte.
///
/// Discriminants are the on-wire values and are frozen as part of sidecar v1.
/// FIPS-204 ML-DSA at NIST security categories 3 (ML-DSA-65) and 5
/// (ML-DSA-87); both verified through the `fips204` crate's pre-hash
/// (`Ph::SHA512`) entry (FIX-50).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AlgId {
    /// FIPS-204 ML-DSA-65 (NIST category 3).
    MlDsa65 = 1,
    /// FIPS-204 ML-DSA-87 (NIST category 5).
    MlDsa87 = 2,
}

impl AlgId {
    /// Decode the on-wire `alg_id` byte. Panic-free: an unknown value yields
    /// `None` rather than indexing or unwrapping (the sidecar parser turns
    /// this into a typed error).
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::MlDsa65),
            2 => Some(Self::MlDsa87),
            _ => None,
        }
    }

    /// The on-wire `alg_id` byte for this algorithm.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Encoded public-key length in bytes. The SINGLE source of this value
    /// for both the sidecar/key parser and the verifier (FIX-46).
    ///
    /// ML-DSA public-key sizes are fixed by FIPS-204: 1952 bytes for
    /// ML-DSA-65, 2592 bytes for ML-DSA-87.
    #[must_use]
    pub const fn pk_len(self) -> usize {
        match self {
            Self::MlDsa65 => 1952,
            Self::MlDsa87 => 2592,
        }
    }

    /// Encoded signature length in bytes. The SINGLE source of this value for
    /// both the sidecar parser and the verifier (FIX-46): the parser carves
    /// the trailing signature field at exactly this width and the verifier
    /// hard-errors (never `continue`s) on any other length.
    ///
    /// ML-DSA signature sizes are fixed by FIPS-204: 3309 bytes for
    /// ML-DSA-65, 4627 bytes for ML-DSA-87.
    #[must_use]
    pub const fn sig_len(self) -> usize {
        match self {
            Self::MlDsa65 => 3309,
            Self::MlDsa87 => 4627,
        }
    }
}

/// Pre-hash digest identifier baked into the sidecar header `hash_id` byte.
///
/// Discriminants are the on-wire values and are frozen as part of sidecar v1.
/// Only SHA-512 is defined: NMBL signs the SHA-512 digest of the image and
/// verifies through ML-DSA's `Ph::SHA512` pre-hash entry (FIX-50).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum HashId {
    /// SHA-512 (64-byte digest); the only digest the verify path consumes.
    Sha512 = 1,
}

impl HashId {
    /// Decode the on-wire `hash_id` byte. Panic-free: unknown ⇒ `None`.
    #[must_use]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Sha512),
            _ => None,
        }
    }

    /// The on-wire `hash_id` byte for this digest.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        self as u8
    }

    /// Digest length in bytes (SHA-512 ⇒ 64).
    #[must_use]
    pub const fn digest_len(self) -> usize {
        match self {
            Self::Sha512 => 64,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on the frozen alg/hash discriminants and lengths"
)]
mod tests {
    use super::*;

    #[test]
    fn alg_roundtrips_through_u8() {
        for alg in [AlgId::MlDsa65, AlgId::MlDsa87] {
            assert_eq!(AlgId::from_u8(alg.to_u8()), Some(alg));
        }
    }

    #[test]
    fn alg_unknown_is_none_not_panic() {
        assert_eq!(AlgId::from_u8(0), None);
        assert_eq!(AlgId::from_u8(3), None);
        assert_eq!(AlgId::from_u8(255), None);
    }

    #[test]
    fn hash_roundtrips_and_rejects_unknown() {
        assert_eq!(
            HashId::from_u8(HashId::Sha512.to_u8()),
            Some(HashId::Sha512)
        );
        assert_eq!(HashId::from_u8(0), None);
        assert_eq!(HashId::from_u8(2), None);
    }

    #[test]
    fn pk_and_sig_lengths_match_fips204() {
        // Frozen FIPS-204 sizes; the verifier and parser both read these.
        assert_eq!(AlgId::MlDsa65.pk_len(), 1952);
        assert_eq!(AlgId::MlDsa65.sig_len(), 3309);
        assert_eq!(AlgId::MlDsa87.pk_len(), 2592);
        assert_eq!(AlgId::MlDsa87.sig_len(), 4627);
    }

    #[test]
    fn sha512_digest_len_is_64() {
        assert_eq!(HashId::Sha512.digest_len(), 64);
    }
}
