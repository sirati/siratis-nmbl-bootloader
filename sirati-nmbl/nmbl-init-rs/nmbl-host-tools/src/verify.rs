//! Host-side verification for restricted update receivers.

use std::fs;
use std::io::Read;
use std::path::Path;

use nmbl_init::sig::{BakedKey, SigSidecar, VerifyPolicy, verify_digest};
use sha2::{Digest, Sha512};

use crate::error::{Result, SignError};

pub fn run(input: &Path, public_key: &Path, domain: &'static [u8], sig: &Path) -> Result<()> {
    let sidecar_bytes =
        fs::read(sig).map_err(|e| SignError::io(format!("read sidecar {}", sig.display()), e))?;
    let public_bytes = fs::read(public_key)
        .map_err(|e| SignError::io(format!("read public key {}", public_key.display()), e))?;
    let file =
        fs::File::open(input).map_err(|e| SignError::io(format!("open {}", input.display()), e))?;
    let mut reader = std::io::BufReader::new(file);
    verify_reader(&mut reader, &public_bytes, domain, &sidecar_bytes)?;
    println!("verified {}", input.display());
    Ok(())
}

/// Verify an already-open input. Restricted receivers use this to keep the
/// bytes they checked pinned across the later copy, avoiding pathname races.
pub fn verify_reader(
    reader: &mut impl Read,
    public_bytes: &[u8],
    domain: &'static [u8],
    sidecar_bytes: &[u8],
) -> Result<()> {
    let sidecar = SigSidecar::parse(sidecar_bytes)
        .map_err(|e| SignError::Key(format!("invalid sidecar: {e}")))?;
    let key = BakedKey::from_pubkey(public_bytes, sidecar.alg())
        .map_err(|e| SignError::Key(format!("invalid public key: {e}")))?;
    let mut hasher = Sha512::new();
    std::io::copy(reader, &mut hasher).map_err(|e| SignError::io("hash input", e))?;
    let digest: [u8; 64] = hasher.finalize().into();
    verify_digest(&digest, domain, &sidecar, &[key], VerifyPolicy::Enforce).map_err(|e| {
        SignError::Crypto {
            context: "verify input".into(),
            reason: e.to_string(),
        }
    })?;
    Ok(())
}
