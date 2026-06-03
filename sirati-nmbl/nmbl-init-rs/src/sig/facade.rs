//! The FROZEN signature-verification API surface (§B.3).
//!
//! Holds the public types ([`BakedKey`], [`VerifyPolicy`], [`FullFp`]), the
//! per-role domain constants, the helper fns ([`fp`]/[`resolve_allowed_keys`]),
//! and the verify ENTRY POINTS whose signatures are KEYSTONE-frozen. The verify
//! BODIES are stubbed for F2 #14/#15: they return
//! `Err(NmblError::Signature{stage:"stub-f2", …})` so the contract COMPILES and
//! PUBLISHES without a real ML-DSA pipeline yet. Every stub is marked
//! `// F2 #14/#15: real impl`.
//!
//! FIX-64: there is intentionally NO `verify_detached(image_path, sig_path, …)`
//! on this surface. Path-reopening verify is a TOCTOU footgun — every trust
//! path verifies an already-open, pinned fd via [`verify_image_fd`], so the
//! path-based entry is removed from the frozen API entirely.

use std::os::fd::BorrowedFd;
use std::path::Path;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;

use super::alg::AlgId;
use super::sidecar::SigSidecar;
use super::wire;

// ---- Per-role domain constants (FIX-01) -------------------------------------
//
// Each trust path threads its OWN domain into the verify `ctx`, and the sidecar
// records the matching domain tag (`wire::domain_tag(domain)`). A signature
// minted for one role can therefore NEVER verify under another (the
// domain-cross-reject property). These byte strings are frozen.

/// Domain for a generation kernel signature.
pub const DOMAIN_GEN_KERNEL: &[u8] = b"nmbl:gen-kernel:v1";
/// Domain for a generation initrd signature.
pub const DOMAIN_GEN_INITRD: &[u8] = b"nmbl:gen-initrd:v1";
/// Domain for a driver-image (squashfs) signature.
pub const DOMAIN_DRIVER_IMAGE: &[u8] = b"nmbl:driver-image:v1";
/// Domain for a staged config-fragment signature.
pub const DOMAIN_STAGED_FRAGMENT: &[u8] = b"nmbl:staged-fragment:v1";
/// Domain for the priority-volume signed file.
pub const DOMAIN_PRIORITY_FILE: &[u8] = b"nmbl:priority-file:v1";
/// Domain for the rescue squashfs signature.
pub const DOMAIN_RESCUE_SFS: &[u8] = b"nmbl:rescue-sfs:v1";

/// A full 32-byte public-key fingerprint (`fp` output). Used by c4's gate to
/// narrow `allowed_key_ids` on the WHOLE fingerprint, never a truncation or the
/// sidecar's `key_id` hint (FIX-08).
pub type FullFp = [u8; 32];

/// A trusted public key baked into the measured initramfs.
///
/// PLACEHOLDER for F2: the real `VerifyingKeyEnum`-backed shape (the parsed
/// `fips204` key + its algorithm) lands with `sig/keys.rs` in #14. The frozen
/// API references `&[BakedKey]` only, so #14 can fill in the body without
/// touching any consumer. The raw key bytes + algorithm are retained so [`fp`]
/// and the future verifier have what they need.
#[derive(Debug, Clone)]
pub struct BakedKey {
    /// Raw encoded public-key bytes — the EXACT pre-image [`fp`] hashes
    /// (FIX-65), `alg.pk_len()` long.
    pub pubkey: Vec<u8>,
    /// Algorithm this key verifies under.
    pub alg: AlgId,
}

impl BakedKey {
    /// Full fingerprint of this key (`fp(self.pubkey)`).
    #[must_use]
    pub fn fingerprint(&self) -> FullFp {
        fp(&self.pubkey)
    }
}

/// How strictly a verify call treats a missing or unparseable sidecar.
///
/// PLACEHOLDER for F2: the real audit-vs-enforce semantics are wired in #15/#19
/// (via `apply_policy`). Frozen now so every entry point can take it by value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyPolicy {
    /// Fail-closed: a missing/bad signature is a hard `Err`.
    Enforce,
    /// Audit: verify and LOG, but do not refuse boot. Only reachable behind
    /// `signing.enable && !enforce && allowAuditModeInsecure` (FIX-16/FIX-31);
    /// never the default.
    Audit,
}

/// Full public-key fingerprint: `SHA-256(b"nmbl:keyfp:v1" || pubkey)`.
///
/// Thin re-export of the always-compiled [`wire::fp`] so consumers depend on
/// ONE definition (FIX-08/FIX-65). The pre-image is the exact raw baked-key
/// bytes.
#[must_use]
pub fn fp(pubkey: &[u8]) -> FullFp {
    wire::fp(pubkey)
}

/// Narrow a baked-key set to those whose FULL fingerprint is in `allowed`
/// (FIX-08). Returns borrows in baked order. An empty `allowed` means "no
/// restriction" — every baked key is allowed — so a build with one key and no
/// explicit `allowed_key_ids` still verifies (the policy layer enforces the
/// "≥2 keys ⇒ allowed_key_ids required" rule, FIX-54).
///
/// Structurally unable to filter on the sidecar's `key_id` hint: it only sees
/// full fingerprints.
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

/// Verify a precomputed 64-byte digest against `keys` under `domain`.
///
/// The lowest-level verify entry: the caller has already streamed the image
/// through SHA-512 (`util::hash`, single pinned fd) and parsed the sidecar.
/// Fail-closed any-of over the resolved keys; the sidecar's recorded domain tag
/// must equal `wire::domain_tag(domain)` (FIX-01 cross-reject). `key_id` is an
/// order hint only.
///
/// FROZEN signature; body stubbed for #14/#15.
#[allow(
    clippy::needless_pass_by_value,
    reason = "VerifyPolicy is Copy; by-value keeps the frozen signature stable for #15"
)]
pub fn verify_digest(
    digest: &[u8; 64],
    domain: &'static [u8],
    sidecar: &SigSidecar<'_>,
    keys: &[BakedKey],
    policy: VerifyPolicy,
) -> Result<()> {
    // F2 #14/#15: real impl — recompute `wire::domain_tag(domain)`, reject on
    // mismatch with the sidecar tag, then fail-closed any-of ML-DSA verify of
    // `sidecar.signature()` over each resolved key via `Ph::SHA512`, hashing
    // `digest`. Until then the gate refuses by construction.
    let _ = (digest, domain, sidecar, keys, policy);
    Err(NmblError::Signature {
        stage: "stub-f2",
        detail: "verify_digest body lands in F2 #14/#15".to_string(),
    })
}

/// Verify an already-open image fd against its sidecar under `domain`.
///
/// The trust-path entry used by EVERY consumer (FIX-64: fd-only, no
/// path-reopen). Streams the fd through SHA-512 (seek-to-0 first, asserting
/// bytes-hashed == file length — FIX-51), parses the sidecar (`sig` = the
/// sidecar path, or a convention next to the image when `None`), and calls
/// [`verify_digest`]. `config` supplies the baked keys + policy.
///
/// FROZEN signature; body stubbed for #14/#15.
pub fn verify_image_fd(
    fd: BorrowedFd<'_>,
    image_desc: &str,
    sig: Option<&Path>,
    domain: &'static [u8],
    config: &Config,
) -> Result<()> {
    // F2 #14/#15: real impl — seek fd to 0, stream SHA-512 over the whole
    // image (single pinned fd, no path reopen — FIX-02/FIX-51), load the
    // sidecar bytes, parse via SigSidecar, then verify_digest under `domain`
    // with the config's baked keys + policy.
    let _ = (fd, sig, domain, config);
    Err(NmblError::Signature {
        stage: "stub-f2",
        detail: format!("verify_image_fd({image_desc}) body lands in F2 #14/#15"),
    })
}

/// Ensure a generation's kernel AND initrd both carry a valid signature.
///
/// Looks up the sidecars under `/boot/nmbl/sigs/<gen-id>/{kernel.sig,
/// initrd.sig}` (R-4) and verifies each over its own domain
/// ([`DOMAIN_GEN_KERNEL`]/[`DOMAIN_GEN_INITRD`]) via [`verify_image_fd`].
///
/// FROZEN signature; body stubbed for #14/#15 (sidecar lookup + the boot-flow
/// wiring lands with `scan.rs`/`gate.rs` in #18/#19).
///
/// (The generation parameter is named `generation`, not `gen`: `gen` is a
/// reserved keyword in edition 2024. The frozen contract is the type
/// signature `(&Config, &Generation) -> Result<()>`; the binding name is not.)
pub fn ensure_generation_signed(config: &Config, generation: &Generation) -> Result<()> {
    // F2 #14/#15: real impl — resolve the per-gen sidecar dir via the shared
    // gen_id() helper, open kernel/initrd fds, and verify_image_fd each under
    // its domain. Until then the gate refuses by construction.
    let _ = (config, generation);
    Err(NmblError::Signature {
        stage: "stub-f2",
        detail: "ensure_generation_signed body lands in F2 #14/#15".to_string(),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on the frozen facade API and stub behaviour"
)]
mod tests {
    use super::*;

    fn key(byte: u8, alg: AlgId) -> BakedKey {
        BakedKey {
            pubkey: vec![byte; alg.pk_len()],
            alg,
        }
    }

    #[test]
    fn fp_matches_wire_definition() {
        let pk = vec![9u8; 64];
        assert_eq!(fp(&pk), wire::fp(&pk));
    }

    #[test]
    fn empty_allowed_means_no_restriction() {
        let keys = [key(1, AlgId::MlDsa65), key(2, AlgId::MlDsa87)];
        let got = resolve_allowed_keys(&keys, &[]);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn resolve_narrows_on_full_fingerprint() {
        let keys = [key(1, AlgId::MlDsa65), key(2, AlgId::MlDsa87)];
        let wanted = keys[1].fingerprint();
        let got = resolve_allowed_keys(&keys, &[wanted]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].fingerprint(), wanted);
    }

    #[test]
    fn resolve_ignores_unknown_fingerprint() {
        let keys = [key(1, AlgId::MlDsa65)];
        let got = resolve_allowed_keys(&keys, &[[0xFFu8; 32]]);
        assert!(got.is_empty());
    }

    #[test]
    fn per_role_domains_are_distinct() {
        let all = [
            DOMAIN_GEN_KERNEL,
            DOMAIN_GEN_INITRD,
            DOMAIN_DRIVER_IMAGE,
            DOMAIN_STAGED_FRAGMENT,
            DOMAIN_PRIORITY_FILE,
            DOMAIN_RESCUE_SFS,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "per-role domains must be distinct (FIX-01)");
            }
        }
    }

    #[test]
    fn verify_digest_stub_refuses() {
        let keys: [BakedKey; 0] = [];
        // Build a minimal valid sidecar to parse.
        let header = wire::Header {
            alg: AlgId::MlDsa65,
            hash: crate::sig::alg::HashId::Sha512,
            key_id: 0,
            domain: wire::domain_tag(DOMAIN_GEN_KERNEL),
        };
        let mut buf = wire::encode(&header).to_vec();
        buf.extend(std::iter::repeat_n(0u8, AlgId::MlDsa65.sig_len()));
        let sc = SigSidecar::parse(&buf).unwrap();
        let err = verify_digest(
            &[0u8; 64],
            DOMAIN_GEN_KERNEL,
            &sc,
            &keys,
            VerifyPolicy::Enforce,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            NmblError::Signature {
                stage: "stub-f2",
                ..
            }
        ));
    }
}
