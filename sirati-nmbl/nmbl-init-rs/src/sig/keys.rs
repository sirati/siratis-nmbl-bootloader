//! Parsed baked trust-anchor keys (#14 — FIX-45/FIX-08).
//!
//! Wraps the raw [`baked_keys::BAKED_KEYS`](super::baked_keys::BAKED_KEYS)
//! byte/alg pairs in a parsed [`BakedKey`] (the `fips204` public key + its
//! algorithm + the raw bytes the fingerprint hashes), and exposes the
//! whole-set-fail-closed parser plus the trust-narrowing helpers the verify
//! pipeline and c4's gate consume.
//!
//! Key invariants:
//! - [`parse_baked_keys`] is WHOLE-SET fail-closed (FIX-45): if ANY baked key
//!   fails `try_from_bytes` (wrong length, malformed encoding) the WHOLE call
//!   returns `Err`. It never returns a shortened `Vec` that silently drops a
//!   bad key — a build that bakes a corrupt key must refuse to verify anything,
//!   not quietly trust the survivors.
//! - [`resolve_allowed_keys`] narrows on the FULL 32-byte fingerprint only
//!   (FIX-08), never on the sidecar's `key_id` hint.

use fips204::traits::{SerDes, Verifier};
use fips204::{Ph, ml_dsa_65, ml_dsa_87};

use crate::error::{NmblError, Result};

use super::alg::AlgId;
use super::baked_keys::{BAKED_KEYS, REQUIRE_KEYS};
use super::wire;

// ---- Compile-time fail-closed asserts (R-5/FIX-24) --------------------------
//
// These are the Rust backstop to the Nix `assertMsg`: even if a future build
// path bypasses the eval-time check, an enforcement build (`REQUIRE_KEYS`)
// with NO baked keys is a hard COMPILE error here — never a runtime allow-all.

/// Zero-keys assert: a build that REQUIRES keys must bake at least one. When
/// `REQUIRE_KEYS` is `false` (the committed stub / measure-only) the condition
/// is trivially satisfied, so the default build still compiles with `&[]`.
const _: () = assert!(
    !REQUIRE_KEYS || !BAKED_KEYS.is_empty(),
    "secure-boot signature enforcement requires at least one baked public key \
     (set boot.nmbl.signing.publicKeys); an empty BAKED_KEYS would be a runtime \
     allow-all"
);

/// Per-key length assert: every baked blob must be EXACTLY `alg.pk_len()` long
/// (FIX-24). A wrong-length blob is a packaging bug that would otherwise only
/// surface as a runtime parse failure; catch it at build time. Evaluated as a
/// const fn over the static slice so it runs with zero runtime cost.
const _: () = assert_baked_key_lengths();

/// Const helper for the per-key length assert above. Walks `BAKED_KEYS` and
/// panics (a compile error in const context) if any blob's length disagrees
/// with its declared algorithm's `pk_len()`.
const fn assert_baked_key_lengths() {
    let mut i = 0;
    while i < BAKED_KEYS.len() {
        // Indexing a static slice in a const fn is bounds-checked by the
        // compiler; the `i < len` guard keeps it in range.
        #[allow(
            clippy::indexing_slicing,
            reason = "const-context bounds-checked slice walk for the length assert"
        )]
        let (bytes, alg) = BAKED_KEYS[i];
        assert!(
            bytes.len() == alg.pk_len(),
            "a baked public key has the wrong length for its declared algorithm"
        );
        i += 1;
    }
}

/// A parsed `fips204` verifying key, dispatched by algorithm.
///
/// Boxed per arm so the enum does not balloon to the largest key's footprint on
/// every `BakedKey` (the two ML-DSA public keys differ in size). Holds only
/// public material; no secret key ever enters `nmbl-init`.
#[derive(Clone)]
pub enum VerifyingKeyEnum {
    /// An ML-DSA-65 (NIST category 3) verifying key.
    MlDsa65(Box<ml_dsa_65::PublicKey>),
    /// An ML-DSA-87 (NIST category 5) verifying key.
    MlDsa87(Box<ml_dsa_87::PublicKey>),
}

impl core::fmt::Debug for VerifyingKeyEnum {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The fips204 key types do not derive Debug; print only the variant.
        match self {
            Self::MlDsa65(_) => f.write_str("VerifyingKeyEnum::MlDsa65(..)"),
            Self::MlDsa87(_) => f.write_str("VerifyingKeyEnum::MlDsa87(..)"),
        }
    }
}

/// Result of a single-key pre-hash verify attempt. Distinguishes a clean
/// accept/reject from a STRUCTURAL signature-length mismatch so the verify loop
/// can hard-error on the latter instead of silently `continue`-ing (FIX-46).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyVerify {
    /// The signature verified under this key.
    Accept,
    /// The signature did not verify under this key (try the next key).
    Reject,
    /// The signature byte length did not match `alg.sig_len()`. This is an
    /// internal inconsistency the caller must turn into a hard error — never a
    /// "try the next key" — because the sidecar parser already carved the
    /// signature at exactly `sig_len()` (FIX-46).
    BadSigLen,
}

impl VerifyingKeyEnum {
    /// Verify a pre-hashed (SHA-512) ML-DSA signature over `digest` under this
    /// key, with the per-role `ctx` domain (FIX-01). `sig` is the raw signature
    /// bytes carved from the sidecar; this converts it to the fixed-width
    /// `[u8; sig_len]` array `fips204` requires, returning [`KeyVerify::BadSigLen`]
    /// if the length is wrong rather than panicking.
    ///
    /// The pre-hash entry (`Ph::SHA512`) hashes `digest` internally before the
    /// lattice verify, mirroring how the host signer pre-hashes the SAME digest
    /// (the cross-crate `(domain, Ph::SHA512, AlgId)` triple is pinned by the
    /// round-trip KAT).
    #[must_use]
    pub fn hash_verify_digest(&self, digest: &[u8], sig: &[u8], ctx: &[u8]) -> KeyVerify {
        match self {
            Self::MlDsa65(pk) => {
                let Ok(sig_arr): core::result::Result<[u8; ml_dsa_65::SIG_LEN], _> = sig.try_into()
                else {
                    return KeyVerify::BadSigLen;
                };
                if pk.hash_verify(digest, &sig_arr, ctx, &Ph::SHA512) {
                    KeyVerify::Accept
                } else {
                    KeyVerify::Reject
                }
            }
            Self::MlDsa87(pk) => {
                let Ok(sig_arr): core::result::Result<[u8; ml_dsa_87::SIG_LEN], _> = sig.try_into()
                else {
                    return KeyVerify::BadSigLen;
                };
                if pk.hash_verify(digest, &sig_arr, ctx, &Ph::SHA512) {
                    KeyVerify::Accept
                } else {
                    KeyVerify::Reject
                }
            }
        }
    }
}

/// A trusted public key baked into the measured initramfs.
///
/// Carries the parsed `fips204` verifying key, its algorithm, and the raw
/// encoded bytes — the EXACT pre-image [`wire::fp`] hashes (FIX-65). The verify
/// pipeline tries each key whose `alg` matches the sidecar; c4's gate narrows
/// the set on the full fingerprint via [`resolve_allowed_keys`].
#[derive(Debug, Clone)]
pub struct BakedKey {
    /// The parsed verifying key, dispatched by algorithm.
    pub key: VerifyingKeyEnum,
    /// Algorithm this key verifies under (drives `sig_len`/`pk_len`).
    pub alg: AlgId,
    /// Raw encoded public-key bytes — the pre-image of [`Self::fingerprint`].
    pub pubkey: Vec<u8>,
}

impl BakedKey {
    /// Full 32-byte fingerprint of this key (`fp(self.pubkey)`).
    #[must_use]
    pub fn fingerprint(&self) -> FullFp {
        fp(&self.pubkey)
    }

    /// Parse one `(raw-bytes, alg)` pair into a `BakedKey`, fail-closed.
    /// Crate-internal so the cross-cutting KATs can craft a key set.
    pub(crate) fn parse(bytes: &[u8], alg: AlgId) -> Result<Self> {
        let key = match alg {
            AlgId::MlDsa65 => {
                let arr: [u8; ml_dsa_65::PK_LEN] = bytes
                    .try_into()
                    .map_err(|_| length_error(alg, bytes.len()))?;
                let pk =
                    ml_dsa_65::PublicKey::try_from_bytes(arr).map_err(|e| parse_error(alg, e))?;
                VerifyingKeyEnum::MlDsa65(Box::new(pk))
            }
            AlgId::MlDsa87 => {
                let arr: [u8; ml_dsa_87::PK_LEN] = bytes
                    .try_into()
                    .map_err(|_| length_error(alg, bytes.len()))?;
                let pk =
                    ml_dsa_87::PublicKey::try_from_bytes(arr).map_err(|e| parse_error(alg, e))?;
                VerifyingKeyEnum::MlDsa87(Box::new(pk))
            }
        };
        Ok(Self {
            key,
            alg,
            pubkey: bytes.to_vec(),
        })
    }
}

fn length_error(alg: AlgId, got: usize) -> NmblError {
    NmblError::Signature {
        stage: "baked-key-length",
        detail: format!(
            "baked {alg:?} key is {got} bytes, expected {}",
            alg.pk_len()
        ),
    }
}

fn parse_error(alg: AlgId, reason: &'static str) -> NmblError {
    NmblError::Signature {
        stage: "baked-key-parse",
        detail: format!("baked {alg:?} key failed to decode: {reason}"),
    }
}

/// A full 32-byte public-key fingerprint (`fp` output). c4's gate narrows
/// `allowed_key_ids` on the WHOLE fingerprint, never a truncation or the
/// sidecar's `key_id` hint (FIX-08).
pub type FullFp = [u8; 32];

/// Full public-key fingerprint: `SHA-256(b"nmbl:keyfp:v1" || pubkey)`.
///
/// Thin re-export of the always-compiled [`wire::fp`] so consumers depend on
/// ONE definition (FIX-08/FIX-65). The pre-image is the exact raw baked-key
/// bytes.
#[must_use]
pub fn fp(pubkey: &[u8]) -> FullFp {
    wire::fp(pubkey)
}

/// Parse the WHOLE baked-key set, fail-closed (FIX-45).
///
/// Returns `Ok(keys)` only when EVERY baked entry parses; the first failure
/// aborts with `Err` and NO partial set is returned. A build with a corrupt
/// baked key therefore verifies NOTHING rather than silently trusting the
/// remaining keys.
///
/// Note: a well-formed build cannot reach a parse failure here — the per-key
/// `const _` length assert and the Nix-side fileset already reject a malformed
/// blob at build time — but the runtime fail-closed path is the defence in
/// depth the audit requires.
pub fn parse_baked_keys() -> Result<Vec<BakedKey>> {
    parse_key_set(BAKED_KEYS)
}

/// Parse an arbitrary `(raw-bytes, alg)` slice, whole-set fail-closed.
///
/// The body of [`parse_baked_keys`], factored so the KATs can exercise the
/// whole-set semantics on a crafted set (the real `BAKED_KEYS` static is empty
/// in this build). Crate-internal: callers parse the BAKED set via
/// [`parse_baked_keys`].
pub(crate) fn parse_key_set(pairs: &[(&[u8], AlgId)]) -> Result<Vec<BakedKey>> {
    let mut out = Vec::with_capacity(pairs.len());
    for &(bytes, alg) in pairs {
        out.push(BakedKey::parse(bytes, alg)?);
    }
    Ok(out)
}

/// Order a baked-key slice so the key whose fingerprint matches the sidecar's
/// `key_id` HINT (its first 4 little-endian fingerprint bytes) is tried first
/// (FIX-08). This is a pure PERFORMANCE hint: it never narrows trust, never
/// removes a key, and a wrong/zero `key_id` simply leaves the order unchanged —
/// every key is still tried by the any-of verify loop. Returns borrows in the
/// hinted order.
#[must_use]
pub fn order_by_hint<'a>(keys: &'a [BakedKey], key_id: u32) -> Vec<&'a BakedKey> {
    let want = key_id.to_le_bytes();
    let mut ordered: Vec<&'a BakedKey> = keys.iter().collect();
    // Stable partition: hinted matches first, everyone else after, original
    // relative order preserved within each group. No key is dropped.
    ordered.sort_by_key(|k| {
        let fpr = k.fingerprint();
        let head = fpr.get(..4).unwrap_or(&[]);
        u8::from(head != want)
    });
    ordered
}

/// Narrow a baked-key set to those whose FULL fingerprint is in `allowed`
/// (FIX-08). Returns borrows in baked order. An empty `allowed` means "no
/// restriction" — every baked key is allowed — so a single-key build with no
/// explicit `allowed_key_ids` still verifies (the policy layer enforces the
/// "≥2 keys ⇒ allowed_key_ids required" rule, FIX-54).
///
/// Structurally unable to filter on the sidecar's `key_id` hint: it only ever
/// sees full fingerprints.
#[must_use]
pub fn resolve_allowed_keys<'a>(baked: &'a [BakedKey], allowed: &[FullFp]) -> Vec<&'a BakedKey> {
    if allowed.is_empty() {
        return baked.iter().collect();
    }
    baked
        .iter()
        .filter(|k| allowed.contains(&k.fingerprint()))
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on the key-parsing and narrowing behaviour"
)]
mod tests {
    use super::*;
    use fips204::traits::{KeyGen, SerDes};

    /// Deterministic key material for the parse/narrow tests via the
    /// seed-based keygen (no external RNG crate ⇒ no `rand_core` version skew
    /// with `fips204`'s pinned `rand_core`).
    fn gen65_bytes(seed: u8) -> Vec<u8> {
        let (pk, _sk) = ml_dsa_65::KG::keygen_from_seed(&[seed; 32]);
        pk.into_bytes().to_vec()
    }

    fn baked65(seed: u8) -> BakedKey {
        BakedKey::parse(&gen65_bytes(seed), AlgId::MlDsa65).unwrap()
    }

    #[test]
    fn parse_accepts_well_formed_key() {
        let bytes = gen65_bytes(1);
        let key = BakedKey::parse(&bytes, AlgId::MlDsa65).unwrap();
        assert_eq!(key.alg, AlgId::MlDsa65);
        assert_eq!(key.pubkey, bytes);
    }

    #[test]
    fn parse_rejects_wrong_length() {
        let err = BakedKey::parse(&[0u8; 10], AlgId::MlDsa65).unwrap_err();
        assert!(matches!(
            err,
            NmblError::Signature {
                stage: "baked-key-length",
                ..
            }
        ));
    }

    #[test]
    fn parse_rejects_mismatched_alg() {
        // A valid ML-DSA-65 key declared as ML-DSA-87 is the wrong length for
        // 87 and must fail the length check (whole-set fail-closed).
        let bytes = gen65_bytes(2);
        let err = BakedKey::parse(&bytes, AlgId::MlDsa87).unwrap_err();
        assert!(matches!(err, NmblError::Signature { .. }));
    }

    #[test]
    fn fingerprint_matches_wire_fp() {
        let key = baked65(3);
        assert_eq!(key.fingerprint(), wire::fp(&key.pubkey));
    }

    #[test]
    fn resolve_empty_allowed_is_no_restriction() {
        let keys = [baked65(4), baked65(5)];
        assert_eq!(resolve_allowed_keys(&keys, &[]).len(), 2);
    }

    #[test]
    fn resolve_narrows_on_full_fingerprint() {
        let keys = [baked65(6), baked65(7)];
        let wanted = keys[1].fingerprint();
        let got = resolve_allowed_keys(&keys, &[wanted]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].fingerprint(), wanted);
    }

    #[test]
    fn resolve_ignores_unknown_fingerprint() {
        let keys = [baked65(8)];
        assert!(resolve_allowed_keys(&keys, &[[0xFFu8; 32]]).is_empty());
    }

    #[test]
    fn order_by_hint_never_drops_keys() {
        let keys = [baked65(9), baked65(10), baked65(11)];
        // A wrong/zero hint leaves every key present.
        assert_eq!(order_by_hint(&keys, 0).len(), 3);
        assert_eq!(order_by_hint(&keys, 0xDEAD_BEEF).len(), 3);
    }

    #[test]
    fn order_by_hint_promotes_matching_key() {
        let keys = [baked65(12), baked65(13), baked65(14)];
        // Derive the hint from the LAST key's fingerprint head.
        let head = keys[2].fingerprint();
        let hint = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
        let ordered = order_by_hint(&keys, hint);
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].fingerprint(), keys[2].fingerprint());
    }

    #[test]
    fn parse_baked_keys_on_empty_stub_is_ok_empty() {
        // The committed stub bakes no keys; the parser returns an empty Vec.
        let keys = parse_baked_keys().unwrap();
        assert!(keys.is_empty());
    }
}
