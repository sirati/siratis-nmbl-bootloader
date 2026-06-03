//! Cross-crate round-trip KAT (FIX-25 / FIX-52).
//!
//! This is the keystone test: `nmbl-host-tools` SIGNS a blob and
//! `nmbl_init::sig::verify_digest` (the in-initramfs verifier) ACCEPTS it,
//! exercising BOTH crates' code on the SAME bytes. It pins the
//! `(domain, Ph::SHA512, AlgId)` triple end-to-end and asserts the negatives:
//! wrong key, wrong domain, and a truncated sidecar all REJECT.
//!
//! The whole flow goes through the real CLI surfaces: `keygen` writes the key
//! files to disk, `keyfile::read_private` loads the private key, and
//! `sign::sidecar_for_digest` mints the sidecar with `nmbl_init::sig::wire`. The
//! verify side parses with `SigSidecar::parse` and checks with `verify_digest` —
//! the literal decoder for the signer's encoder.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "KATs assert on known vectors and may panic on failure"
)]

use std::fs;
use std::path::Path;

use nmbl_host_tools::keyfile::{self, PrivateKeyFile};
use nmbl_host_tools::keygen;
use nmbl_host_tools::sign;

use nmbl_init::sig::{
    AlgId, BakedKey, DOMAIN_GEN_INITRD, DOMAIN_GEN_KERNEL, SigSidecar, VerifyPolicy, verify_digest,
};

/// A fixed 64-byte test digest (stands in for a streamed SHA-512 of an image).
fn digest(byte: u8) -> [u8; 64] {
    [byte; 64]
}

/// Generate a keypair for `alg` into a temp dir, returning the loaded private
/// key and the raw public-key bytes — the exact files the CLI writes/reads.
fn keypair(dir: &Path, alg: AlgId) -> (PrivateKeyFile, Vec<u8>) {
    let priv_path = dir.join("sk.key");
    let pub_path = dir.join("pk.bin");
    keygen::run(alg, &priv_path, &pub_path).expect("keygen must succeed");

    let key = keyfile::read_private(&priv_path).expect("read back private key");
    let pubkey = fs::read(&pub_path).expect("read back public key");
    assert_eq!(
        pubkey.len(),
        alg.pk_len(),
        "public key is the frozen pk_len"
    );
    (key, pubkey)
}

/// Round-trip: sign under a role with `nmbl-sign`, verify with `nmbl-init`.
#[test]
fn signer_output_verifies_in_nmbl_init() {
    for alg in [AlgId::MlDsa65, AlgId::MlDsa87] {
        let dir = tempfile::tempdir().unwrap();
        let (key, pubkey) = keypair(dir.path(), alg);
        let baked = [BakedKey::from_pubkey(&pubkey, alg).unwrap()];
        let d = digest(0x11);

        // SIGN with nmbl-host-tools.
        let sidecar_bytes = sign::sidecar_for_digest(&d, &key, DOMAIN_GEN_KERNEL).unwrap();

        // VERIFY with nmbl-init's decoder + verifier — the literal inverse.
        let sidecar = SigSidecar::parse(&sidecar_bytes).expect("verifier parses signer output");
        verify_digest(
            &d,
            DOMAIN_GEN_KERNEL,
            &sidecar,
            &baked,
            VerifyPolicy::Enforce,
        )
        .unwrap_or_else(|e| panic!("{alg:?}: honest signature must verify: {e}"));
    }
}

/// fp(host pubkey) == fp(baked) — the fingerprint pre-image is the raw public
/// bytes both sides hash (FIX-65), so the `allowed_key_ids` narrowing can never
/// silently empty on an encoding mismatch.
#[test]
fn fingerprint_agrees_across_crates() {
    let dir = tempfile::tempdir().unwrap();
    let (_key, pubkey) = keypair(dir.path(), AlgId::MlDsa65);
    let baked = BakedKey::from_pubkey(&pubkey, AlgId::MlDsa65).unwrap();
    assert_eq!(baked.fingerprint(), nmbl_init::sig::fp(&pubkey));
}

/// Negative — WRONG KEY: a sidecar from key A is rejected by key B.
#[test]
fn wrong_key_is_rejected() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let (key_a, _pub_a) = keypair(dir_a.path(), AlgId::MlDsa65);
    let (_key_b, pub_b) = keypair(dir_b.path(), AlgId::MlDsa65);

    let baked_b = [BakedKey::from_pubkey(&pub_b, AlgId::MlDsa65).unwrap()];
    let d = digest(0x22);

    let sidecar_bytes = sign::sidecar_for_digest(&d, &key_a, DOMAIN_GEN_KERNEL).unwrap();
    let sidecar = SigSidecar::parse(&sidecar_bytes).unwrap();
    let err = verify_digest(
        &d,
        DOMAIN_GEN_KERNEL,
        &sidecar,
        &baked_b,
        VerifyPolicy::Enforce,
    )
    .expect_err("a signature from a foreign key must reject");
    let msg = format!("{err}");
    assert!(
        msg.contains("no-valid-key") || msg.contains("rejected"),
        "got: {msg}"
    );
}

/// Negative — WRONG DOMAIN: a sidecar minted for gen-kernel must not verify
/// under gen-initrd, even with the correct key (the domain-cross-reject).
#[test]
fn wrong_domain_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (key, pubkey) = keypair(dir.path(), AlgId::MlDsa65);
    let baked = [BakedKey::from_pubkey(&pubkey, AlgId::MlDsa65).unwrap()];
    let d = digest(0x33);

    let sidecar_bytes = sign::sidecar_for_digest(&d, &key, DOMAIN_GEN_KERNEL).unwrap();
    let sidecar = SigSidecar::parse(&sidecar_bytes).unwrap();

    // Same key + digest, verified under the INITRD role ⇒ reject before any key.
    let err = verify_digest(
        &d,
        DOMAIN_GEN_INITRD,
        &sidecar,
        &baked,
        VerifyPolicy::Enforce,
    )
    .expect_err("cross-role replay must reject");
    assert!(format!("{err}").contains("domain"), "got: {err}");

    // Sanity: it DOES verify under its own role.
    verify_digest(
        &d,
        DOMAIN_GEN_KERNEL,
        &sidecar,
        &baked,
        VerifyPolicy::Enforce,
    )
    .unwrap();
}

/// Negative — TRUNCATED SIDECAR: dropping the last byte makes the verifier's
/// parser refuse it as a truncated signature, never a panic.
#[test]
fn truncated_sidecar_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (key, _pubkey) = keypair(dir.path(), AlgId::MlDsa65);
    let d = digest(0x44);

    let mut sidecar_bytes = sign::sidecar_for_digest(&d, &key, DOMAIN_GEN_KERNEL).unwrap();
    sidecar_bytes.truncate(sidecar_bytes.len() - 1);
    assert!(
        SigSidecar::parse(&sidecar_bytes).is_err(),
        "a truncated sidecar must fail to parse"
    );
}

/// The `sign::run` file path writes a sidecar next to the input that the
/// verifier accepts — proving the on-disk artifact (not just the in-memory
/// buffer) round-trips.
#[test]
fn sign_to_file_then_verify() {
    let dir = tempfile::tempdir().unwrap();
    let (_key_unused, pubkey) = keypair(dir.path(), AlgId::MlDsa87);
    let baked = [BakedKey::from_pubkey(&pubkey, AlgId::MlDsa87).unwrap()];

    // Write a small input file and sign it through the public `run` path.
    let input = dir.path().join("image.bin");
    fs::write(&input, b"hello nmbl image").unwrap();
    let priv_path = dir.path().join("sk.key");
    let out = sign::run(&input, &priv_path, DOMAIN_GEN_KERNEL, None).unwrap();
    assert_eq!(out, dir.path().join("image.bin.sig"));

    // Recompute the digest the verifier would and check the on-disk sidecar.
    use sha2::{Digest, Sha512};
    let mut hasher = Sha512::new();
    hasher.update(fs::read(&input).unwrap());
    let d: [u8; 64] = hasher.finalize().into();

    let sidecar_bytes = fs::read(&out).unwrap();
    let sidecar = SigSidecar::parse(&sidecar_bytes).unwrap();
    verify_digest(
        &d,
        DOMAIN_GEN_KERNEL,
        &sidecar,
        &baked,
        VerifyPolicy::Enforce,
    )
    .expect("the on-disk sidecar must verify");
}
