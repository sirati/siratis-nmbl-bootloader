//! `sign` / `sign-image` — produce a detached `NMBLSIG1` sidecar for a file.
//!
//! The signer is the literal inverse of the verifier's decoder. For an input
//! file it:
//!
//! 1. streams the file through SHA-512 (the SAME pre-hash digest the verifier
//!    computes over a pinned fd in `nmbl_init::util::hash`);
//! 2. ML-DSA-signs that 64-byte digest with `Ph::SHA512` under the caller's
//!    per-role domain (`--domain`) — exactly the `(message=digest, ctx=domain,
//!    Ph::SHA512)` triple `nmbl_init::sig::verify_digest` checks (FIX-01/50);
//! 3. assembles the sidecar with `nmbl_init::sig::wire::encode` — NOT a
//!    hand-rolled header — so the bytes are byte-for-byte what the verifier's
//!    `wire::decode`/`SigSidecar::parse` reads back (FIX-25);
//! 4. writes the sidecar next to the input (`<input>.sig`) or to `--out`.
//!
//! The header's `domain` field is `wire::domain_tag(domain)` and the `key_id`
//! is the first four little-endian bytes of `wire::fp(pubkey)` — the order hint
//! the verifier uses (never a trust narrowing).

use std::fs;
use std::path::{Path, PathBuf};

use fips204::traits::{SerDes, Signer};
use fips204::{Ph, ml_dsa_65, ml_dsa_87};
use nmbl_init::sig::wire::{self, Header};
use nmbl_init::sig::{AlgId, HashId};
use zeroize::Zeroizing;

use crate::error::{Result, SignError};
use crate::keyfile::{self, PrivateKeyFile};

/// Default sidecar extension appended to the input path when `--out` is absent.
const SIG_EXT: &str = "sig";

/// Sign `input` under `domain` with the private key at `priv_path`, writing the
/// sidecar to `out` (or `<input>.sig` when `out` is `None`). Returns the path
/// the sidecar was written to.
pub fn run(
    input: &Path,
    priv_path: &Path,
    domain: &'static [u8],
    out: Option<&Path>,
) -> Result<PathBuf> {
    let key = keyfile::read_private(priv_path)?;
    let sidecar = sign_file(input, &key, domain)?;
    let out_path = match out {
        Some(p) => p.to_path_buf(),
        None => default_sig_path(input),
    };
    fs::write(&out_path, &sidecar)
        .map_err(|e| SignError::io(format!("write sidecar {}", out_path.display()), e))?;
    println!(
        "signed {} -> {} ({} bytes, role {})",
        input.display(),
        out_path.display(),
        sidecar.len(),
        String::from_utf8_lossy(domain),
    );
    Ok(out_path)
}

/// Append `.sig` to `input` for the default sidecar location.
fn default_sig_path(input: &Path) -> PathBuf {
    let mut name = input.as_os_str().to_owned();
    name.push(".");
    name.push(SIG_EXT);
    PathBuf::from(name)
}

/// Stream `input` through SHA-512, sign the digest under `domain`, and return
/// the full sidecar buffer (`wire::encode(header) || signature`).
fn sign_file(input: &Path, key: &PrivateKeyFile, domain: &'static [u8]) -> Result<Vec<u8>> {
    let digest = sha512_file(input)?;
    sidecar_for_digest(&digest, key, domain)
}

/// Compute the SHA-512 digest of `path`, streaming so a large image never lands
/// in RAM whole. Mirrors `nmbl_init::util::hash::sha512_file` so the signer and
/// verifier hash identically.
fn sha512_file(path: &Path) -> Result<[u8; 64]> {
    use sha2::{Digest, Sha512};

    let file =
        fs::File::open(path).map_err(|e| SignError::io(format!("open {}", path.display()), e))?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha512::new();
    std::io::copy(&mut reader, &mut hasher)
        .map_err(|e| SignError::io(format!("hash {}", path.display()), e))?;
    Ok(hasher.finalize().into())
}

/// Sign a precomputed 64-byte digest under `domain` and assemble the sidecar.
/// Exposed to the round-trip KAT so it can drive the exact encode path the CLI
/// uses on a digest it shares with the verifier.
pub fn sidecar_for_digest(
    digest: &[u8; 64],
    key: &PrivateKeyFile,
    domain: &'static [u8],
) -> Result<Vec<u8>> {
    let (signature, pubkey) = ml_dsa_sign(digest, key, domain)?;
    let header = Header {
        alg: key.alg,
        hash: HashId::Sha512,
        // Order hint only (never narrows trust): the first 4 LE bytes of the
        // public-key fingerprint, matching `keys::order_by_hint`.
        key_id: key_id_hint(&pubkey),
        domain: wire::domain_tag(domain),
    };
    let mut buf = wire::encode(&header).to_vec();
    buf.extend_from_slice(&signature);
    Ok(buf)
}

/// ML-DSA-sign `digest` with `Ph::SHA512` under `domain`, returning the raw
/// signature bytes AND the derived public-key bytes (for the `key_id` hint).
fn ml_dsa_sign(
    digest: &[u8; 64],
    key: &PrivateKeyFile,
    domain: &[u8],
) -> Result<(Vec<u8>, Vec<u8>)> {
    match key.alg {
        AlgId::MlDsa65 => {
            let arr: [u8; ml_dsa_65::SK_LEN] = key
                .sk
                .as_slice()
                .try_into()
                .map_err(|_| length_error(key.alg, key.sk.len()))?;
            // The decoded secret-key wrapper is dropped at end of scope; the
            // source bytes in `key.sk` stay zeroized by their `Zeroizing`.
            let sk = Zeroizing::new(
                ml_dsa_65::PrivateKey::try_from_bytes(arr)
                    .map_err(|e| SignError::crypto("decode ML-DSA-65 private key", e))?,
            );
            let sig = sk
                .try_hash_sign(digest, domain, &Ph::SHA512)
                .map_err(|e| SignError::crypto("ML-DSA-65 sign", e))?;
            let pk = sk.get_public_key().into_bytes().to_vec();
            Ok((sig.to_vec(), pk))
        }
        AlgId::MlDsa87 => {
            let arr: [u8; ml_dsa_87::SK_LEN] = key
                .sk
                .as_slice()
                .try_into()
                .map_err(|_| length_error(key.alg, key.sk.len()))?;
            let sk = Zeroizing::new(
                ml_dsa_87::PrivateKey::try_from_bytes(arr)
                    .map_err(|e| SignError::crypto("decode ML-DSA-87 private key", e))?,
            );
            let sig = sk
                .try_hash_sign(digest, domain, &Ph::SHA512)
                .map_err(|e| SignError::crypto("ML-DSA-87 sign", e))?;
            let pk = sk.get_public_key().into_bytes().to_vec();
            Ok((sig.to_vec(), pk))
        }
    }
}

/// First four little-endian bytes of `wire::fp(pubkey)` — the verifier's
/// `key_id` order hint.
fn key_id_hint(pubkey: &[u8]) -> u32 {
    let fp = wire::fp(pubkey);
    let head = fp.get(..4).unwrap_or(&[0u8; 4]);
    let mut bytes = [0u8; 4];
    let n = head.len().min(4);
    if let (Some(dst), Some(src)) = (bytes.get_mut(..n), head.get(..n)) {
        dst.copy_from_slice(src);
    }
    u32::from_le_bytes(bytes)
}

fn length_error(alg: AlgId, got: usize) -> SignError {
    SignError::Key(format!(
        "private key body is {got} bytes, wrong length for {alg:?}"
    ))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "tests assert on known vectors and may panic on failure"
)]
mod tests {
    use super::*;

    #[test]
    fn default_sig_path_appends_sig() {
        let p = default_sig_path(Path::new("/boot/kernel"));
        assert_eq!(p, PathBuf::from("/boot/kernel.sig"));
    }

    #[test]
    fn key_id_hint_matches_fp_head() {
        let pk = b"some-public-key-bytes";
        let fp = wire::fp(pk);
        let want = u32::from_le_bytes([fp[0], fp[1], fp[2], fp[3]]);
        assert_eq!(key_id_hint(pk), want);
    }
}
