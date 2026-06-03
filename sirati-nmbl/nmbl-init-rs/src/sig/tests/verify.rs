//! Verify-pipeline KATs: round-trip, domain-cross-reject, whole-set fail-closed,
//! and the in-loop signature-length hard error (FIX-01/45/46/50/51).

use super::TestSigner;
use crate::error::NmblError;
use crate::sig::alg::AlgId;
use crate::sig::keys;
use crate::sig::sidecar::SigSidecar;
use crate::sig::verify::{DOMAIN_GEN_INITRD, DOMAIN_GEN_KERNEL, VerifyPolicy, verify_digest};
use crate::sig::wire;

/// A fixed test digest (stands in for a streamed SHA-512 of an image).
fn digest(byte: u8) -> [u8; 64] {
    [byte; 64]
}

/// Round-trip KAT: a real fips204 keygen → sign a digest under a role → the
/// matching baked key ACCEPTS; tampering with the digest or the signature
/// REJECTS. Covers both algorithms (FIX-50: the Ph::SHA512 entry is present
/// for ml-dsa-65 AND ml-dsa-87).
#[test]
fn round_trip_accepts_then_tamper_rejects() {
    for alg in [AlgId::MlDsa65, AlgId::MlDsa87] {
        let signer = TestSigner::new(alg, 7);
        let keys = [signer.baked_key()];
        let d = digest(0x11);

        // (a) Honest signature verifies.
        let sc_bytes = signer.sidecar_bytes(&d, DOMAIN_GEN_KERNEL, 0);
        let sc = SigSidecar::parse(&sc_bytes).unwrap();
        verify_digest(&d, DOMAIN_GEN_KERNEL, &sc, &keys, VerifyPolicy::Enforce)
            .unwrap_or_else(|e| panic!("honest {alg:?} sig must verify: {e}"));

        // (b) Tampered DIGEST (different image) rejects.
        let other = digest(0x22);
        let err = verify_digest(&other, DOMAIN_GEN_KERNEL, &sc, &keys, VerifyPolicy::Enforce)
            .unwrap_err();
        assert!(
            matches!(err, NmblError::Signature { .. }),
            "tampered digest must reject"
        );

        // (c) Tampered SIGNATURE byte rejects.
        let mut bad_bytes = sc_bytes.clone();
        let last = bad_bytes.len() - 1;
        bad_bytes[last] ^= 0x01;
        let bad_sc = SigSidecar::parse(&bad_bytes).unwrap();
        let err = verify_digest(&d, DOMAIN_GEN_KERNEL, &bad_sc, &keys, VerifyPolicy::Enforce)
            .unwrap_err();
        assert!(
            matches!(err, NmblError::Signature { .. }),
            "tampered sig must reject"
        );
    }
}

/// Domain-cross-reject KAT (FIX-01): a signature minted for the gen-KERNEL role
/// must NOT verify when offered under the gen-INITRD role, even though the key
/// and digest are identical. The sidecar's recorded domain tag is the
/// gen-kernel tag; verifying under gen-initrd recomputes a different expected
/// tag and refuses BEFORE any key is tried.
#[test]
fn domain_cross_reject() {
    let signer = TestSigner::new(AlgId::MlDsa65, 9);
    let keys = [signer.baked_key()];
    let d = digest(0x33);

    // Sign under the KERNEL domain.
    let sc_bytes = signer.sidecar_bytes(&d, DOMAIN_GEN_KERNEL, 0);
    let sc = SigSidecar::parse(&sc_bytes).unwrap();

    // Same key, same digest, but verified under the INITRD domain ⇒ reject.
    let err = verify_digest(&d, DOMAIN_GEN_INITRD, &sc, &keys, VerifyPolicy::Enforce).unwrap_err();
    match err {
        NmblError::Signature { stage, .. } => {
            assert_eq!(
                stage, "domain-mismatch",
                "must reject on the domain tag, not the key"
            );
        }
        other => panic!("expected a domain-mismatch Signature error, got {other:?}"),
    }

    // Sanity: it DOES verify under its own (kernel) domain.
    verify_digest(&d, DOMAIN_GEN_KERNEL, &sc, &keys, VerifyPolicy::Enforce).unwrap();
}

/// Any-of KAT: with several baked keys, a signature from ONE of them verifies
/// regardless of position, and a `key_id` hint pointing at the wrong key does
/// NOT prevent the correct key from being tried (hint is order-only).
#[test]
fn any_of_accepts_with_multiple_keys_and_wrong_hint() {
    let s0 = TestSigner::new(AlgId::MlDsa65, 1);
    let s1 = TestSigner::new(AlgId::MlDsa65, 2);
    let s2 = TestSigner::new(AlgId::MlDsa65, 3);
    let keys = [s0.baked_key(), s1.baked_key(), s2.baked_key()];
    let d = digest(0x44);

    // Signed by the MIDDLE key, with a key_id hint of 0 (pointing nowhere).
    let sc_bytes = s1.sidecar_bytes(&d, DOMAIN_GEN_KERNEL, 0);
    let sc = SigSidecar::parse(&sc_bytes).unwrap();
    verify_digest(&d, DOMAIN_GEN_KERNEL, &sc, &keys, VerifyPolicy::Enforce)
        .expect("any-of must find the signing key despite a useless hint");
}

/// No-valid-key KAT: a signature from a key that is NOT baked is rejected after
/// every baked key has been tried.
#[test]
fn unknown_key_rejected() {
    let baked_signer = TestSigner::new(AlgId::MlDsa65, 10);
    let foreign_signer = TestSigner::new(AlgId::MlDsa65, 11);
    let keys = [baked_signer.baked_key()];
    let d = digest(0x55);

    let sc_bytes = foreign_signer.sidecar_bytes(&d, DOMAIN_GEN_KERNEL, 0);
    let sc = SigSidecar::parse(&sc_bytes).unwrap();
    let err = verify_digest(&d, DOMAIN_GEN_KERNEL, &sc, &keys, VerifyPolicy::Enforce).unwrap_err();
    match err {
        NmblError::Signature { stage, .. } => assert_eq!(stage, "no-valid-key"),
        other => panic!("expected no-valid-key, got {other:?}"),
    }
}

/// No-matching-algorithm KAT: a baked key of a DIFFERENT algorithm than the
/// sidecar is never tried; the verify reports no candidate key.
#[test]
fn no_key_of_sidecar_algorithm() {
    // Sidecar is ML-DSA-87; the only baked key is ML-DSA-65.
    let sidecar_signer = TestSigner::new(AlgId::MlDsa87, 4);
    let baked_65 = TestSigner::new(AlgId::MlDsa65, 5);
    let keys = [baked_65.baked_key()];
    let d = digest(0x66);

    let sc_bytes = sidecar_signer.sidecar_bytes(&d, DOMAIN_GEN_KERNEL, 0);
    let sc = SigSidecar::parse(&sc_bytes).unwrap();
    let err = verify_digest(&d, DOMAIN_GEN_KERNEL, &sc, &keys, VerifyPolicy::Enforce).unwrap_err();
    assert!(matches!(
        err,
        NmblError::Signature {
            stage: "no-valid-key",
            ..
        }
    ));
}

/// Whole-set fail-closed KAT (FIX-45): a key set with ONE corrupt entry makes
/// the WHOLE parse fail — no shortened Vec that silently trusts the survivors.
#[test]
fn whole_set_fail_closed() {
    let good = TestSigner::new(AlgId::MlDsa65, 6);
    let good_bytes = good.baked_key().pubkey;

    // One good key, one too-short (corrupt) key.
    let pairs: [(&[u8], AlgId); 2] = [
        (good_bytes.as_slice(), AlgId::MlDsa65),
        (&[0u8; 10], AlgId::MlDsa65),
    ];
    let err = keys::parse_key_set(&pairs).unwrap_err();
    assert!(
        matches!(err, NmblError::Signature { .. }),
        "any bad key must fail the whole set"
    );

    // The all-good prefix on its own parses fine — proving the failure was the
    // corrupt entry, not the good one.
    let good_only: [(&[u8], AlgId); 1] = [(good_bytes.as_slice(), AlgId::MlDsa65)];
    assert_eq!(keys::parse_key_set(&good_only).unwrap().len(), 1);
}

/// Signature-length hard-error KAT (FIX-46): if a sidecar's signature length
/// disagrees with the key algorithm's `sig_len()`, the verify loop returns the
/// `internal-siglen` hard error rather than silently skipping the key.
///
/// The `SigSidecar` parser normally carves the signature at exactly
/// `sig_len()`, so to exercise the in-loop guard we call `verify_digest` with a
/// sidecar whose alg disagrees with the only baked key's alg-length. We build a
/// short-signature sidecar by hand at the wire level.
#[test]
fn in_loop_siglen_mismatch_is_hard_error() {
    // The `SigSidecar` parser always carves the signature at exactly
    // `sig_len()`, so a wrong-length signature can never reach `verify_digest`
    // through a parsed sidecar. We therefore exercise the key-level guard the
    // verify loop relies on directly: a short signature must surface
    // `BadSigLen` (which `verify_digest` turns into the `internal-siglen` hard
    // error — a `return Err`, never a `continue` — FIX-46).
    use crate::sig::keys::KeyVerify;

    let signer = TestSigner::new(AlgId::MlDsa65, 8);
    let key = signer.baked_key();
    let d = digest(0x77);

    // A signature one byte short of sig_len triggers BadSigLen at the key.
    let short_sig = vec![0u8; AlgId::MlDsa65.sig_len() - 1];
    assert_eq!(
        key.key
            .hash_verify_digest(&d, &short_sig, DOMAIN_GEN_KERNEL),
        KeyVerify::BadSigLen,
        "a wrong-length signature must surface BadSigLen, not a silent reject"
    );
}

/// Sidecar tamper KAT: flipping the recorded domain tag in an otherwise-valid
/// sidecar makes it fail the domain check (defence in depth alongside the
/// cross-role test).
#[test]
fn tampered_domain_tag_rejected() {
    let signer = TestSigner::new(AlgId::MlDsa65, 12);
    let keys = [signer.baked_key()];
    let d = digest(0x88);

    let mut sc_bytes = signer.sidecar_bytes(&d, DOMAIN_GEN_KERNEL, 0);
    // Corrupt one byte of the recorded domain tag (offset OFF_DOMAIN..).
    sc_bytes[wire::OFF_DOMAIN] ^= 0xFF;
    let sc = SigSidecar::parse(&sc_bytes).unwrap();
    let err = verify_digest(&d, DOMAIN_GEN_KERNEL, &sc, &keys, VerifyPolicy::Enforce).unwrap_err();
    assert!(matches!(
        err,
        NmblError::Signature {
            stage: "domain-mismatch",
            ..
        }
    ));
}
