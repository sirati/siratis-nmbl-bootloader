//! `keygen` — generate an ML-DSA keypair for signing.
//!
//! Two output modes:
//!
//! - **Files** (`--out-priv`/`--out-pub`): the private-key container goes to
//!   `--out-priv` (created `0600`), the RAW public-key bytes to `--out-pub`.
//! - **Pipes** (`--stdio`): the private-key container goes to stdout and the RAW
//!   public-key bytes to file descriptor 3. Nothing is written to disk, so a
//!   secrets store can capture the private key straight from the pipe. This is
//!   the generic generator contract (private on stdout, public on fd 3).
//!
//! The public key is exactly `alg.pk_len()` bytes of the encoded ML-DSA public
//! key — the precise blob `boot.nmbl.signing.publicKeys` bakes and
//! `nmbl_init::sig::wire::fp` fingerprints (FIX-65), so no post-processing is
//! needed to wire a generated key into the trust anchor.
//!
//! Private-key bytes live in [`Zeroizing`] from generation until they are
//! written and dropped, so the secret never lingers in freed heap memory.

use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::path::Path;

use fips204::traits::SerDes;
use fips204::{ml_dsa_65, ml_dsa_87};
use nmbl_init::sig::AlgId;
use zeroize::Zeroizing;

use crate::error::{Result, SignError};
use crate::keyfile;

/// The descriptor `--stdio` writes the public key to.
pub const PUBLIC_KEY_FD: RawFd = 3;

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

/// `keygen --stdio`: private-key container to stdout, raw public key to fd 3.
/// Fails before generating anything when fd 3 is not open, so a caller that
/// forgot the public channel never receives an orphaned private key.
pub fn run_stdio(alg: AlgId) -> Result<()> {
    let mut public_out = open_public_fd(PUBLIC_KEY_FD)?;
    let stdout = io::stdout();
    let mut private_out = stdout.lock();
    run_to(alg, &mut private_out, &mut public_out)
}

/// Generate a keypair and write the private container to `private_out` and the
/// raw public key to `public_out`. Exposed so tests can drive the exact
/// `--stdio` byte layout without juggling process descriptors.
pub fn run_to(alg: AlgId, private_out: &mut impl Write, public_out: &mut impl Write) -> Result<()> {
    let (priv_bytes, pub_bytes) = generate(alg)?;
    if pub_bytes.len() != alg.pk_len() {
        return Err(SignError::Key(format!(
            "public key is {} bytes, expected {} for {alg:?}",
            pub_bytes.len(),
            alg.pk_len()
        )));
    }
    let container = keyfile::encode_private(alg, &priv_bytes);
    // Public first, mirroring the file mode: if the public channel fails, no
    // private key has left the process.
    public_out
        .write_all(&pub_bytes)
        .and_then(|()| public_out.flush())
        .map_err(|e| SignError::io(format!("write public key to fd {PUBLIC_KEY_FD}"), e))?;
    private_out
        .write_all(container.as_slice())
        .and_then(|()| private_out.flush())
        .map_err(|e| SignError::io("write private key to stdout", e))
}

/// Take ownership of a duplicate of `fd`, failing with a clear message when the
/// caller did not open it. Duplicating (rather than adopting `fd` itself) keeps
/// the check and the later `File` drop free of any double-close hazard.
fn open_public_fd(fd: RawFd) -> Result<File> {
    // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` only inspects `fd` and, on success,
    // returns a NEW descriptor this process exclusively owns. It has no memory
    // side effects; an unopened `fd` yields -1/EBADF.
    #[allow(unsafe_code, reason = "fcntl dup of the fd-3 public-key channel")]
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup < 0 {
        let err = io::Error::last_os_error();
        return Err(SignError::Usage(format!(
            "keygen --stdio writes the public key to fd {fd}, which is not open ({err}); \
             run it as e.g. `nmbl-sign keygen --alg ml-dsa-65 --stdio >priv 3>pub`"
        )));
    }
    // SAFETY: `dup` is a freshly duplicated, valid descriptor owned by nobody
    // else; `File` takes sole ownership and closes it exactly once on drop.
    #[allow(unsafe_code, reason = "adopting the descriptor fcntl just returned")]
    let file = unsafe { File::from_raw_fd(dup) };
    Ok(file)
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

    #[test]
    fn unopened_public_fd_is_a_clear_error() {
        // A descriptor number far above anything the test harness opens.
        let err = open_public_fd(987_654).err().unwrap();
        assert!(err.to_string().contains("not open"), "{err}");
    }

    #[test]
    fn run_to_emits_container_and_raw_public() {
        let mut private = Vec::new();
        let mut public = Vec::new();
        run_to(AlgId::MlDsa87, &mut private, &mut public).unwrap();
        assert_eq!(public.len(), AlgId::MlDsa87.pk_len());
        assert!(private.starts_with(b"NMBLSK01"));
        let key = keyfile::parse_private(&private).unwrap();
        assert_eq!(key.alg, AlgId::MlDsa87);
    }
}
