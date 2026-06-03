//! Cross-cutting KATs for the real ML-DSA verify pipeline (#15).
//!
//! Shared signing helpers + the submodules holding the round-trip,
//! domain-cross-reject, whole-set-fail-closed, and signature-length KATs.
//! These use `fips204`'s seed-based keygen + deterministic sign so they need
//! no external RNG crate (avoiding the `rand_core` version skew).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "KATs assert on known vectors and may panic on failure"
)]

mod verify;

use fips204::traits::{KeyGen, SerDes, Signer};
use fips204::{Ph, ml_dsa_65, ml_dsa_87};

use crate::sig::alg::{AlgId, HashId};
use crate::sig::keys::BakedKey;
use crate::sig::wire::{self, Header};

/// A signer over one deterministic ML-DSA key pair, used to mint real sidecars
/// for the verify KATs. Mirrors what the host `nmbl-sign` tool does: pre-hash
/// the SAME digest under `Ph::SHA512` with the per-role domain as the ctx.
pub(super) struct TestSigner {
    alg: AlgId,
    sk65: Option<ml_dsa_65::PrivateKey>,
    sk87: Option<ml_dsa_87::PrivateKey>,
    pk_bytes: Vec<u8>,
}

impl TestSigner {
    /// Deterministic key pair from `seed` for `alg`.
    pub(super) fn new(alg: AlgId, seed: u8) -> Self {
        match alg {
            AlgId::MlDsa65 => {
                let (pk, sk) = ml_dsa_65::KG::keygen_from_seed(&[seed; 32]);
                Self {
                    alg,
                    sk65: Some(sk),
                    sk87: None,
                    pk_bytes: pk.into_bytes().to_vec(),
                }
            }
            AlgId::MlDsa87 => {
                let (pk, sk) = ml_dsa_87::KG::keygen_from_seed(&[seed; 32]);
                Self {
                    alg,
                    sk65: None,
                    sk87: Some(sk),
                    pk_bytes: pk.into_bytes().to_vec(),
                }
            }
        }
    }

    /// The baked (verifying) key this signer corresponds to.
    pub(super) fn baked_key(&self) -> BakedKey {
        BakedKey::parse(&self.pk_bytes, self.alg).unwrap()
    }

    /// Sign `digest` under `domain` with the pre-hash (`Ph::SHA512`) entry —
    /// the exact triple the verifier checks. Deterministic (seeded) so the KAT
    /// is reproducible.
    pub(super) fn sign(&self, digest: &[u8; 64], domain: &[u8]) -> Vec<u8> {
        let sig_seed = [0x42u8; 32];
        match self.alg {
            AlgId::MlDsa65 => self
                .sk65
                .as_ref()
                .unwrap()
                .try_hash_sign_with_seed(&sig_seed, digest, domain, &Ph::SHA512)
                .unwrap()
                .to_vec(),
            AlgId::MlDsa87 => self
                .sk87
                .as_ref()
                .unwrap()
                .try_hash_sign_with_seed(&sig_seed, digest, domain, &Ph::SHA512)
                .unwrap()
                .to_vec(),
        }
    }

    /// Build a complete, parseable sidecar buffer (header + signature) binding
    /// `digest` to `domain`. `key_id` is the order hint (any value is fine).
    pub(super) fn sidecar_bytes(&self, digest: &[u8; 64], domain: &[u8], key_id: u32) -> Vec<u8> {
        let signature = self.sign(digest, domain);
        let header = Header {
            alg: self.alg,
            hash: HashId::Sha512,
            key_id,
            domain: wire::domain_tag(domain),
        };
        let mut buf = wire::encode(&header).to_vec();
        buf.extend_from_slice(&signature);
        buf
    }
}
