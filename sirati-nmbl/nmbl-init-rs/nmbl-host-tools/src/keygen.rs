//! `keygen` — generate an ML-DSA keypair for signing.
//!
//! Writes the PRIVATE key bytes to `--out-priv` and the RAW public-key bytes to
//! `--out-pub`. The public file is exactly `alg.pk_len()` bytes of the encoded
//! ML-DSA public key — the precise blob `boot.nmbl.signing.publicKeys` bakes and
//! `nmbl_init::sig::wire::fp` fingerprints (FIX-65), so no post-processing is
//! needed to wire a generated key into the trust anchor.
//!
//! Private-key bytes live in [`Zeroizing`] from generation until they are
//! written and dropped, so the secret never lingers in freed heap memory.

use std::path::Path;

use fips204::traits::SerDes;
use fips204::{ml_dsa_65, ml_dsa_87};
use nmbl_init::sig::AlgId;
use zeroize::Zeroizing;

use crate::error::{Result, SignError};
use crate::keyfile;

/// Generate a keypair for `alg`, writing the private key to `out_priv` and the
/// raw public-key bytes to `out_pub`. Uses the OS RNG (`OsRng`, via fips204's
/// `default-rng`) so each call yields fresh key material.
pub fn run(alg: AlgId, out_priv: &Path, out_pub: &Path) -> Result<()> {
    let (priv_bytes, pub_bytes) = generate(alg)?;

    // Write the public key first: it carries no secret, and a partial private
    // file is the one we most want to avoid leaving behind.
    keyfile::write_public(out_pub, alg, &pub_bytes)?;
    keyfile::write_private(out_priv, alg, &priv_bytes)?;

    println!(
        "wrote {:?} keypair: private -> {}, public ({} bytes) -> {}",
        alg,
        out_priv.display(),
        pub_bytes.len(),
        out_pub.display(),
    );
    Ok(())
}

/// Generate raw `(private, public)` byte blobs for `alg`. The private bytes are
/// returned in a [`Zeroizing`] wrapper so they are wiped on drop.
fn generate(alg: AlgId) -> Result<(Zeroizing<Vec<u8>>, Vec<u8>)> {
    match alg {
        AlgId::MlDsa65 => {
            let (pk, sk) =
                ml_dsa_65::try_keygen().map_err(|e| SignError::crypto("ML-DSA-65 keygen", e))?;
            Ok((
                Zeroizing::new(sk.into_bytes().to_vec()),
                pk.into_bytes().to_vec(),
            ))
        }
        AlgId::MlDsa87 => {
            let (pk, sk) =
                ml_dsa_87::try_keygen().map_err(|e| SignError::crypto("ML-DSA-87 keygen", e))?;
            Ok((
                Zeroizing::new(sk.into_bytes().to_vec()),
                pk.into_bytes().to_vec(),
            ))
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "tests assert on known vectors and may panic on failure"
)]
mod tests {
    use super::*;

    #[test]
    fn generate_emits_correct_lengths() {
        for alg in [AlgId::MlDsa65, AlgId::MlDsa87] {
            let (sk, pk) = generate(alg).unwrap();
            // Public key is exactly the frozen pk_len; private is non-empty.
            assert_eq!(pk.len(), alg.pk_len());
            assert!(!sk.is_empty());
        }
    }
}
