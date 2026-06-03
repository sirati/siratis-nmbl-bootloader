//! Key-file I/O for `nmbl-sign`.
//!
//! Two on-disk shapes:
//!
//! - **Public key** — RAW encoded ML-DSA public-key bytes, nothing else. This
//!   is exactly the blob `boot.nmbl.signing.publicKeys` bakes and
//!   `nmbl_init::sig::wire::fp` fingerprints (FIX-65); writing it bare means a
//!   generated key drops straight into the trust anchor with no re-encoding.
//!
//! - **Private key** — a tiny self-describing container: an 8-byte magic, a
//!   1-byte `AlgId` tag, then the raw encoded private-key bytes. The tag lets
//!   `sign` pick the right algorithm without a separate flag and rejects a
//!   wrong-length or wrong-magic file fail-closed. Secret bytes are held in
//!   [`Zeroizing`] end to end.
//!
//! The public-key length is validated against `AlgId::pk_len()` — the SINGLE
//! length source the verifier also reads (FIX-46) — so a generated public file
//! can never bake at the wrong width.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nmbl_init::sig::AlgId;
use zeroize::Zeroizing;

use crate::error::{Result, SignError};

/// Magic prefix of a private-key file. Distinct from the `NMBLSIG1` sidecar
/// magic so the two artifacts can never be confused.
const PRIV_MAGIC: [u8; 8] = *b"NMBLSK01";
/// Offset of the 1-byte `AlgId` tag in a private-key file.
const PRIV_ALG_OFF: usize = 8;
/// Offset where the raw private-key bytes begin.
const PRIV_KEY_OFF: usize = 9;

/// On-disk `AlgId` tag byte (the same on-wire value the sidecar header uses).
fn alg_tag(alg: AlgId) -> u8 {
    alg.to_u8()
}

/// Decode an `AlgId` tag byte; `None` for an unknown value.
fn alg_from_tag(tag: u8) -> Option<AlgId> {
    AlgId::from_u8(tag)
}

/// Write the RAW public-key bytes to `path`, after asserting they are exactly
/// `alg.pk_len()` long (FIX-46) so a baked key can never be the wrong width.
pub fn write_public(path: &Path, alg: AlgId, pubkey: &[u8]) -> Result<()> {
    if pubkey.len() != alg.pk_len() {
        return Err(SignError::Key(format!(
            "public key is {} bytes, expected {} for {:?}",
            pubkey.len(),
            alg.pk_len(),
            alg
        )));
    }
    fs::write(path, pubkey)
        .map_err(|e| SignError::io(format!("write public key {}", path.display()), e))
}

/// Write a private-key container (`magic || alg || raw-sk`) to `path`. The
/// secret bytes arrive in [`Zeroizing`] and the assembled buffer is also
/// zeroized on drop, so the private material is wiped from this process's memory
/// after the write.
///
/// The file is created `0o600` from the start (via `OpenOptionsExt::mode`), so
/// the secret never exists with world-readable permissions even for an instant
/// — a chmod-after-write would leave that window open. An existing file is
/// truncated and rewritten; `mode` only applies on creation, so on a re-keygen
/// we re-assert `0o600` explicitly to repair a file that was created insecurely.
pub fn write_private(path: &Path, alg: AlgId, sk: &Zeroizing<Vec<u8>>) -> Result<()> {
    let mut buf = Zeroizing::new(Vec::with_capacity(PRIV_KEY_OFF + sk.len()));
    buf.extend_from_slice(&PRIV_MAGIC);
    buf.push(alg_tag(alg));
    buf.extend_from_slice(sk);
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| SignError::io(format!("create private key {}", path.display()), e))?;
    // `mode` is a no-op when the file already existed; re-assert 0600 so a
    // re-keygen over an insecure file still ends locked down.
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| SignError::io(format!("chmod private key {}", path.display()), e))?;
    f.write_all(buf.as_slice())
        .map_err(|e| SignError::io(format!("write private key {}", path.display()), e))
}

/// A loaded private key: its algorithm and the raw secret bytes (zeroized on
/// drop).
pub struct PrivateKeyFile {
    /// The algorithm the private key signs under.
    pub alg: AlgId,
    /// Raw encoded private-key bytes (held in `Zeroizing`).
    pub sk: Zeroizing<Vec<u8>>,
}

/// Read and validate a private-key container from `path`. Fail-closed: a bad
/// magic, an unknown algorithm tag, or a truncated body is a typed error, never
/// a partial key.
pub fn read_private(path: &Path) -> Result<PrivateKeyFile> {
    let raw = Zeroizing::new(
        fs::read(path)
            .map_err(|e| SignError::io(format!("read private key {}", path.display()), e))?,
    );
    let magic = raw
        .get(..PRIV_ALG_OFF)
        .ok_or_else(|| SignError::Key("private-key file shorter than its header".into()))?;
    if magic != PRIV_MAGIC {
        return Err(SignError::Key("private-key file has a bad magic".into()));
    }
    let tag = *raw
        .get(PRIV_ALG_OFF)
        .ok_or_else(|| SignError::Key("private-key file missing its algorithm tag".into()))?;
    let alg = alg_from_tag(tag).ok_or_else(|| {
        SignError::Key(format!("private-key file has unknown algorithm tag {tag}"))
    })?;
    let body = raw
        .get(PRIV_KEY_OFF..)
        .ok_or_else(|| SignError::Key("private-key file has no key body".into()))?;
    Ok(PrivateKeyFile {
        alg,
        sk: Zeroizing::new(body.to_vec()),
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert on known vectors and may panic on failure"
)]
mod tests {
    use super::*;

    #[test]
    fn private_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk.key");
        let sk = Zeroizing::new(vec![0xAB; 100]);
        write_private(&path, AlgId::MlDsa87, &sk).unwrap();

        let loaded = read_private(&path).unwrap();
        assert_eq!(loaded.alg, AlgId::MlDsa87);
        assert_eq!(loaded.sk.as_slice(), sk.as_slice());
    }

    #[test]
    fn private_is_written_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk.key");
        let sk = Zeroizing::new(vec![0xCD; 64]);
        write_private(&path, AlgId::MlDsa87, &sk).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "private key must be 0600, got {mode:o}"
        );
    }

    #[test]
    fn private_rekeygen_stays_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk.key");
        // Pre-create a world-readable file to mimic an insecure prior keygen.
        fs::write(&path, b"stale").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let sk = Zeroizing::new(vec![0xCD; 64]);
        write_private(&path, AlgId::MlDsa87, &sk).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "re-keygen must end 0600, got {mode:o}");
    }

    #[test]
    fn bad_magic_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk.key");
        fs::write(&path, b"XXXXXXXX\x01rest").unwrap();
        assert!(read_private(&path).is_err());
    }

    #[test]
    fn unknown_alg_tag_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sk.key");
        let mut buf = PRIV_MAGIC.to_vec();
        buf.push(0x09); // not a valid AlgId byte
        buf.extend_from_slice(&[0u8; 4]);
        fs::write(&path, &buf).unwrap();
        assert!(read_private(&path).is_err());
    }

    #[test]
    fn wrong_length_public_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pk.bin");
        assert!(write_public(&path, AlgId::MlDsa65, &[0u8; 10]).is_err());
    }
}
