//! Sidecar wire format — the ALWAYS-COMPILED leaf (FIX-25).
//!
//! This module is compiled in EVERY build, with or without the `secure-boot`
//! feature, because it is the single definition of the on-disk sidecar layout
//! shared between `nmbl-init` (decode, on the verify path) and the host signer
//! `nmbl-sign` (encode). One definition, two crates ⇒ writer and reader can
//! never drift (FIX-25).
//!
//! Keep this leaf dependency-light: the fixed-width header codec
//! ([`encode`]/[`decode`] + the offset consts) touches no crypto crate and is
//! panic-free (every read is a checked `.get`/`split_at_checked`, never an
//! index). The only crypto here is [`fp`] and [`domain_tag`], which hash with
//! SHA-256 and are therefore gated on the same feature axis as the `sha2`
//! dependency (`secure-boot`/`network-rescue`); the host signer enables
//! `sha2` independently.
//!
//! ## Sidecar v1 layout (frozen)
//!
//! Fixed-width header, then the variable-width signature. Multi-byte integers
//! are little-endian. The per-role domain is folded into v1 NOW (FIX-44 /
//! FIX-01) as a fixed 32-byte tag — `SHA-256(b"nmbl:sigdom:v1" || domain)` —
//! so the header stays fixed-width and panic-free regardless of how long the
//! role string is, while still binding each signature to exactly one role.
//!
//! ```text
//! offset  size  field
//!   0       8   magic      = b"NMBLSIG1"
//!   8       1   version    = 1
//!   9       1   alg_id     (AlgId on-wire byte: 1=ML-DSA-65, 2=ML-DSA-87)
//!  10       1   hash_id    (HashId on-wire byte: 1=SHA-512)
//!  11       1   reserved   = 0
//!  12       4   key_id     u32 LE (ORDER HINT only — never narrows trust)
//!  16      32   domain     SHA-256(b"nmbl:sigdom:v1" || role-domain)
//!  48       N   signature  (N = AlgId::sig_len(); not part of the header)
//! ```
//!
//! `HEADER_LEN` is offset 48 (everything before the signature). The total
//! sidecar length is `HEADER_LEN + alg.sig_len()`.

#[cfg(any(feature = "secure-boot", feature = "network-rescue"))]
use sha2::{Digest, Sha256};

use super::alg::{AlgId, HashId};

/// Magic bytes at offset 0 — `b"NMBLSIG1"`, the literal sidecar v1 tag.
pub const MAGIC: [u8; 8] = *b"NMBLSIG1";

/// Sidecar format version stored at [`OFF_VERSION`]. Frozen at 1; the domain
/// fold is part of v1, NOT a version bump (FIX-44).
pub const VERSION: u8 = 1;

/// Pre-image prefix domain-separating the sidecar domain tag from any other
/// SHA-256 use. The tag is `SHA-256(DOMAIN_TAG_PREFIX || role-domain)`.
pub const DOMAIN_TAG_PREFIX: &[u8] = b"nmbl:sigdom:v1";

/// Pre-image prefix for the public-key fingerprint [`fp`] (FIX-65: this is the
/// EXACT pre-image both the baked-key fingerprint and the host tool hash).
pub const KEYFP_PREFIX: &[u8] = b"nmbl:keyfp:v1";

// ---- Fixed-width header field offsets (frozen sidecar v1) ----

/// Offset of the 8-byte magic.
pub const OFF_MAGIC: usize = 0;
/// Offset of the 1-byte version.
pub const OFF_VERSION: usize = 8;
/// Offset of the 1-byte `alg_id`.
pub const OFF_ALG_ID: usize = 9;
/// Offset of the 1-byte `hash_id`.
pub const OFF_HASH_ID: usize = 10;
/// Offset of the 1-byte reserved padding (must be 0).
pub const OFF_RESERVED: usize = 11;
/// Offset of the 4-byte little-endian `key_id` (order hint).
pub const OFF_KEY_ID: usize = 12;
/// Offset of the 32-byte domain tag.
pub const OFF_DOMAIN: usize = 16;
/// Width of the domain tag (a SHA-256 digest).
pub const DOMAIN_LEN: usize = 32;
/// Total fixed header length; the signature begins here.
pub const HEADER_LEN: usize = OFF_DOMAIN + DOMAIN_LEN; // 48

/// Total sidecar length for `alg`: fixed header plus the signature field.
#[must_use]
pub const fn sidecar_len(alg: AlgId) -> usize {
    HEADER_LEN + alg.sig_len()
}

/// Decoded fixed-width sidecar header (the bytes before the signature).
///
/// `Copy` and crypto-free: produced by [`decode`] purely from offset reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Signature algorithm; determines the trailing signature width.
    pub alg: AlgId,
    /// Pre-hash digest identifier (SHA-512 in v1).
    pub hash: HashId,
    /// Key-selection ORDER HINT only — verify NEVER narrows trust on it.
    pub key_id: u32,
    /// 32-byte domain tag = `SHA-256(DOMAIN_TAG_PREFIX || role-domain)`.
    pub domain: [u8; DOMAIN_LEN],
}

/// Reasons [`decode`] rejects a header. Panic-free: every failure is a typed
/// variant, never an index-out-of-bounds or unwrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Buffer shorter than [`HEADER_LEN`].
    TooShort,
    /// Magic at [`OFF_MAGIC`] is not [`MAGIC`].
    BadMagic,
    /// Version at [`OFF_VERSION`] is not [`VERSION`].
    BadVersion(u8),
    /// `alg_id` byte does not decode to a known [`AlgId`].
    BadAlg(u8),
    /// `hash_id` byte does not decode to a known [`HashId`].
    BadHash(u8),
}

/// Decode the fixed-width header from the front of `buf`. Panic-free: bounded
/// entirely by `split_at_checked`/`.get`. Returns the parsed [`Header`]; the
/// caller carves the trailing signature using `header.alg.sig_len()`.
#[must_use = "a decoded header must be inspected before trusting the sidecar"]
pub fn decode(buf: &[u8]) -> Result<Header, DecodeError> {
    let (head, _) = buf
        .split_at_checked(HEADER_LEN)
        .ok_or(DecodeError::TooShort)?;

    let magic = head
        .get(OFF_MAGIC..OFF_VERSION)
        .ok_or(DecodeError::TooShort)?;
    if magic != MAGIC {
        return Err(DecodeError::BadMagic);
    }

    let version = *head.get(OFF_VERSION).ok_or(DecodeError::TooShort)?;
    if version != VERSION {
        return Err(DecodeError::BadVersion(version));
    }

    let alg_byte = *head.get(OFF_ALG_ID).ok_or(DecodeError::TooShort)?;
    let alg = AlgId::from_u8(alg_byte).ok_or(DecodeError::BadAlg(alg_byte))?;

    let hash_byte = *head.get(OFF_HASH_ID).ok_or(DecodeError::TooShort)?;
    let hash = HashId::from_u8(hash_byte).ok_or(DecodeError::BadHash(hash_byte))?;

    let key_id_bytes: [u8; 4] = head
        .get(OFF_KEY_ID..OFF_DOMAIN)
        .ok_or(DecodeError::TooShort)?
        .try_into()
        .map_err(|_| DecodeError::TooShort)?;
    let key_id = u32::from_le_bytes(key_id_bytes);

    let domain: [u8; DOMAIN_LEN] = head
        .get(OFF_DOMAIN..HEADER_LEN)
        .ok_or(DecodeError::TooShort)?
        .try_into()
        .map_err(|_| DecodeError::TooShort)?;

    Ok(Header {
        alg,
        hash,
        key_id,
        domain,
    })
}

/// Encode the fixed-width header (without the signature) into a fresh
/// `[u8; HEADER_LEN]`. Crypto-free: the caller supplies the already-computed
/// 32-byte domain tag (via [`domain_tag`]). Used by the host signer; kept here
/// so writer and reader share ONE layout (FIX-25). Panic-free: every write is
/// a `get_mut`-guarded `copy_from_slice`, never an index assignment.
#[must_use]
pub fn encode(header: &Header) -> [u8; HEADER_LEN] {
    let mut out = [0u8; HEADER_LEN];
    // Each range is a compile-time-constant sub-slice of the fixed array; the
    // `if let` guards keep this index-free (the lint forbids `out[a..b] = …`).
    if let Some(s) = out.get_mut(OFF_MAGIC..OFF_VERSION) {
        s.copy_from_slice(&MAGIC);
    }
    if let Some(s) = out.get_mut(OFF_VERSION..OFF_ALG_ID) {
        s.copy_from_slice(&[VERSION]);
    }
    if let Some(s) = out.get_mut(OFF_ALG_ID..OFF_HASH_ID) {
        s.copy_from_slice(&[header.alg.to_u8()]);
    }
    if let Some(s) = out.get_mut(OFF_HASH_ID..OFF_RESERVED) {
        s.copy_from_slice(&[header.hash.to_u8()]);
    }
    // OFF_RESERVED stays 0 from the zero-init.
    if let Some(s) = out.get_mut(OFF_KEY_ID..OFF_DOMAIN) {
        s.copy_from_slice(&header.key_id.to_le_bytes());
    }
    if let Some(s) = out.get_mut(OFF_DOMAIN..HEADER_LEN) {
        s.copy_from_slice(&header.domain);
    }
    out
}

/// Compute the 32-byte sidecar domain tag for a role domain string:
/// `SHA-256(DOMAIN_TAG_PREFIX || domain)` (FIX-44 / FIX-01). The verifier
/// recomputes this from the per-role `domain` const and compares it against
/// the header's `domain` field, so a signature minted for one role can never
/// be replayed under another (the domain-cross-reject property).
///
/// Gated like `sha2`: always available in any build that compiles the verify
/// path or the host signer.
#[cfg(any(feature = "secure-boot", feature = "network-rescue"))]
#[must_use]
pub fn domain_tag(domain: &[u8]) -> [u8; DOMAIN_LEN] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_TAG_PREFIX);
    hasher.update(domain);
    hasher.finalize().into()
}

/// Full public-key fingerprint: `SHA-256(KEYFP_PREFIX || pubkey)` (FIX-08 /
/// FIX-65). The pre-image is the EXACT raw baked-key bytes — the same bytes
/// stored in `baked_keys.rs` and read by the host tool — so the
/// `allowed_key_ids` narrowing in c4's gate can never silently empty on an
/// encoding mismatch. Returns the full 32-byte digest; callers compare on the
/// whole fingerprint, never a truncation.
///
/// Gated like `sha2` (`fp` is only ever called on a verify/sign path).
#[cfg(any(feature = "secure-boot", feature = "network-rescue"))]
#[must_use]
pub fn fp(pubkey: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(KEYFP_PREFIX);
    hasher.update(pubkey);
    hasher.finalize().into()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on the frozen sidecar layout with known vectors"
)]
mod tests {
    use super::*;

    fn sample_header() -> Header {
        Header {
            alg: AlgId::MlDsa87,
            hash: HashId::Sha512,
            key_id: 0x0102_0304,
            domain: [7u8; DOMAIN_LEN],
        }
    }

    #[test]
    fn offsets_are_frozen() {
        assert_eq!(OFF_MAGIC, 0);
        assert_eq!(OFF_VERSION, 8);
        assert_eq!(OFF_ALG_ID, 9);
        assert_eq!(OFF_HASH_ID, 10);
        assert_eq!(OFF_RESERVED, 11);
        assert_eq!(OFF_KEY_ID, 12);
        assert_eq!(OFF_DOMAIN, 16);
        assert_eq!(HEADER_LEN, 48);
    }

    #[test]
    fn sidecar_len_is_header_plus_sig() {
        assert_eq!(sidecar_len(AlgId::MlDsa65), 48 + 3309);
        assert_eq!(sidecar_len(AlgId::MlDsa87), 48 + 4627);
    }

    #[test]
    fn encode_decode_roundtrip() {
        let h = sample_header();
        let bytes = encode(&h);
        assert_eq!(bytes.len(), HEADER_LEN);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded, h);
    }

    #[test]
    fn encode_places_fields_at_frozen_offsets() {
        let h = sample_header();
        let b = encode(&h);
        assert_eq!(&b[OFF_MAGIC..OFF_VERSION], &MAGIC);
        assert_eq!(b[OFF_VERSION], VERSION);
        assert_eq!(b[OFF_ALG_ID], AlgId::MlDsa87.to_u8());
        assert_eq!(b[OFF_HASH_ID], HashId::Sha512.to_u8());
        assert_eq!(b[OFF_RESERVED], 0);
        assert_eq!(&b[OFF_KEY_ID..OFF_DOMAIN], &0x0102_0304u32.to_le_bytes());
        assert_eq!(&b[OFF_DOMAIN..HEADER_LEN], &[7u8; DOMAIN_LEN]);
    }

    #[test]
    fn decode_rejects_short_buffer_without_panic() {
        assert_eq!(decode(&[]), Err(DecodeError::TooShort));
        assert_eq!(decode(&[0u8; HEADER_LEN - 1]), Err(DecodeError::TooShort));
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let mut b = encode(&sample_header());
        b[0] = b'X';
        assert_eq!(decode(&b), Err(DecodeError::BadMagic));
    }

    #[test]
    fn decode_rejects_bad_version() {
        let mut b = encode(&sample_header());
        b[OFF_VERSION] = 2;
        assert_eq!(decode(&b), Err(DecodeError::BadVersion(2)));
    }

    #[test]
    fn decode_rejects_unknown_alg_and_hash() {
        let mut b = encode(&sample_header());
        b[OFF_ALG_ID] = 9;
        assert_eq!(decode(&b), Err(DecodeError::BadAlg(9)));

        let mut b = encode(&sample_header());
        b[OFF_HASH_ID] = 9;
        assert_eq!(decode(&b), Err(DecodeError::BadHash(9)));
    }

    #[cfg(any(feature = "secure-boot", feature = "network-rescue"))]
    #[test]
    fn fp_is_prefixed_sha256_and_stable() {
        // Pin fp() against the canonical SHA-256(prefix || pubkey).
        let pubkey = b"some-public-key-bytes";
        let mut h = Sha256::new();
        h.update(KEYFP_PREFIX);
        h.update(pubkey);
        let want: [u8; 32] = h.finalize().into();
        assert_eq!(fp(pubkey), want);
    }

    #[cfg(any(feature = "secure-boot", feature = "network-rescue"))]
    #[test]
    fn domain_tag_distinguishes_roles() {
        let a = domain_tag(b"nmbl:gen-kernel:v1");
        let b = domain_tag(b"nmbl:gen-initrd:v1");
        assert_ne!(a, b, "different roles must tag differently");
        // And stable / prefixed.
        let mut h = Sha256::new();
        h.update(DOMAIN_TAG_PREFIX);
        h.update(b"nmbl:gen-kernel:v1");
        let want: [u8; 32] = h.finalize().into();
        assert_eq!(a, want);
    }
}
