//! `SigSidecar` — a borrowed, parsed view over a sidecar buffer.
//!
//! This is the `secure-boot`-gated reader the verify path consumes. It layers
//! a typed view over the always-compiled [`wire`](super::wire) leaf: [`parse`]
//! decodes the fixed-width header, validates the trailing signature width
//! against `AlgId::sig_len()` (the SINGLE source — FIX-46), and exposes the
//! signature as a borrowed slice. Panic-free throughout: header decoding goes
//! through `wire::decode` (checked) and the signature carve uses
//! `split_at_checked`, never an index.

use super::alg::{AlgId, HashId};
use super::wire::{self, DOMAIN_LEN, DecodeError, HEADER_LEN, Header};

/// Reasons a sidecar buffer fails to [`parse`]. Panic-free typed errors only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarError {
    /// The fixed-width header failed to decode (short/magic/version/alg/hash).
    Header(DecodeError),
    /// The buffer is shorter than `HEADER_LEN + alg.sig_len()`: the signature
    /// field is truncated. Carries the expected and actual total lengths.
    SignatureTruncated { expected: usize, actual: usize },
    /// Trailing bytes beyond exactly one signature field. The sidecar is
    /// fixed-size; extra bytes mean a malformed or concatenated file.
    TrailingBytes { expected: usize, actual: usize },
}

impl core::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Header(e) => write!(f, "sidecar header invalid: {e:?}"),
            Self::SignatureTruncated { expected, actual } => write!(
                f,
                "sidecar signature truncated: expected {expected} bytes, got {actual}"
            ),
            Self::TrailingBytes { expected, actual } => write!(
                f,
                "sidecar has trailing bytes: expected {expected} bytes, got {actual}"
            ),
        }
    }
}

impl std::error::Error for SidecarError {}

/// A parsed, borrowed view over a sidecar buffer. Holds the decoded header and
/// a slice over the signature bytes; owns nothing.
#[derive(Debug, Clone, Copy)]
pub struct SigSidecar<'a> {
    header: Header,
    signature: &'a [u8],
}

impl<'a> SigSidecar<'a> {
    /// Parse a complete sidecar buffer. Validates the header via
    /// `wire::decode`, then requires EXACTLY `HEADER_LEN + alg.sig_len()`
    /// bytes — a short buffer is `SignatureTruncated`, a long one is
    /// `TrailingBytes`. Panic-free: `split_at_checked` only.
    pub fn parse(buf: &'a [u8]) -> Result<Self, SidecarError> {
        let header = wire::decode(buf).map_err(SidecarError::Header)?;
        let expected = wire::sidecar_len(header.alg);

        // Split off the fixed header; `wire::decode` already guaranteed the
        // buffer is at least HEADER_LEN, but re-check via split_at_checked so
        // this stays index-free and independently sound.
        let (_, rest) =
            buf.split_at_checked(HEADER_LEN)
                .ok_or(SidecarError::SignatureTruncated {
                    expected,
                    actual: buf.len(),
                })?;

        let sig_len = header.alg.sig_len();
        let signature = match rest.split_at_checked(sig_len) {
            Some((sig, [])) => sig,
            Some(_) => {
                return Err(SidecarError::TrailingBytes {
                    expected,
                    actual: buf.len(),
                });
            }
            None => {
                return Err(SidecarError::SignatureTruncated {
                    expected,
                    actual: buf.len(),
                });
            }
        };

        Ok(Self { header, signature })
    }

    /// Signature algorithm; drives `sig_len()`/`pk_len()`.
    #[must_use]
    pub fn alg(&self) -> AlgId {
        self.header.alg
    }

    /// Pre-hash digest identifier (SHA-512 in v1).
    #[must_use]
    pub fn hash(&self) -> HashId {
        self.header.hash
    }

    /// The `key_id` ORDER HINT. NEVER used to narrow trust on the generation
    /// path (PLAN-SOUND); c4's gate narrows only on full fingerprints.
    #[must_use]
    pub fn key_id(&self) -> u32 {
        self.header.key_id
    }

    /// The 32-byte domain tag recorded in the sidecar
    /// (`SHA-256(DOMAIN_TAG_PREFIX || role-domain)`). The verifier compares
    /// this against the expected per-role domain to reject cross-role replay.
    #[must_use]
    pub fn domain_tag(&self) -> &[u8; DOMAIN_LEN] {
        &self.header.domain
    }

    /// The raw signature bytes (`alg.sig_len()` long by construction).
    #[must_use]
    pub fn signature(&self) -> &'a [u8] {
        self.signature
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on the frozen sidecar parse behaviour"
)]
mod tests {
    use super::*;
    use crate::sig::wire::encode;

    fn full_sidecar(alg: AlgId) -> Vec<u8> {
        let header = Header {
            alg,
            hash: HashId::Sha512,
            key_id: 42,
            domain: [3u8; DOMAIN_LEN],
        };
        let mut buf = encode(&header).to_vec();
        buf.extend(std::iter::repeat_n(0xABu8, alg.sig_len()));
        buf
    }

    #[test]
    fn parses_valid_sidecar() {
        let buf = full_sidecar(AlgId::MlDsa65);
        let s = SigSidecar::parse(&buf).unwrap();
        assert_eq!(s.alg(), AlgId::MlDsa65);
        assert_eq!(s.hash(), HashId::Sha512);
        assert_eq!(s.key_id(), 42);
        assert_eq!(s.domain_tag(), &[3u8; DOMAIN_LEN]);
        assert_eq!(s.signature().len(), AlgId::MlDsa65.sig_len());
        assert!(s.signature().iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn rejects_truncated_signature() {
        let mut buf = full_sidecar(AlgId::MlDsa87);
        buf.truncate(buf.len() - 1);
        let err = SigSidecar::parse(&buf).unwrap_err();
        assert!(matches!(err, SidecarError::SignatureTruncated { .. }));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut buf = full_sidecar(AlgId::MlDsa65);
        buf.push(0x00);
        let err = SigSidecar::parse(&buf).unwrap_err();
        assert!(matches!(err, SidecarError::TrailingBytes { .. }));
    }

    #[test]
    fn rejects_bad_header_without_panic() {
        let mut buf = full_sidecar(AlgId::MlDsa65);
        buf[0] = b'Z';
        let err = SigSidecar::parse(&buf).unwrap_err();
        assert!(matches!(err, SidecarError::Header(_)));
    }

    #[test]
    fn sig_len_carved_matches_alg() {
        // The parser carves the signature at exactly alg.sig_len(); pin that
        // the parser and the alg table agree (FIX-46).
        for alg in [AlgId::MlDsa65, AlgId::MlDsa87] {
            let buf = full_sidecar(alg);
            let s = SigSidecar::parse(&buf).unwrap();
            assert_eq!(s.signature().len(), alg.sig_len());
        }
    }
}
