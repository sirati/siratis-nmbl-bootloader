//! Per-generation signature verification (split from the verify core).
//!
//! Resolves a generation's `<boot>/nmbl/sigs/<gen-id>/{kernel,initrd}.sig`
//! sidecars and verifies each blob under its own domain ([`DOMAIN_GEN_KERNEL`]
//! / [`DOMAIN_GEN_INITRD`]). Every blob open + sidecar read is routed through
//! the [`FsOps`] ops layer so a `--validate-initrm` dry-run streams + verifies
//! the EXTRACTED-closure copy rather than the live boot partition; on
//! `RealSys` the I/O is byte-identical to a direct `std::fs` open/read. The
//! kernel/initrd fds opened here are the EXACT ones stream-hashed AND (on the
//! secure-boot path) retained for the kexec load — never re-opened by path
//! (FIX-02 / MED-1 / LOW-A).
//!
//! [`DOMAIN_GEN_KERNEL`]: super::DOMAIN_GEN_KERNEL
//! [`DOMAIN_GEN_INITRD`]: super::DOMAIN_GEN_INITRD

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::sys::ops::FsOps;

use super::super::scan::generation_sig_dir;
use super::{DOMAIN_GEN_INITRD, DOMAIN_GEN_KERNEL, verify_image_fd_digest_bytes};

/// Ensure a generation's kernel AND initrd both carry a valid signature.
///
/// Resolves the per-generation sidecar directory `<boot>/nmbl/sigs/<gen-id>/`
/// (R-4) and verifies `kernel<suffix>` under [`DOMAIN_GEN_KERNEL`] and
/// `initrd<suffix>` under [`DOMAIN_GEN_INITRD`], each over an own pinned fd via
/// [`super::verify_image_fd`]. BOTH must verify.
///
/// (The generation parameter is named `generation`, not `gen`: `gen` is a
/// reserved keyword in edition 2024.)
///
/// Every blob open and sidecar read is routed through `fs` (an [`FsOps`]) so a
/// `--validate-initrm` dry-run streams + verifies the EXTRACTED-closure copy
/// rather than touching the live boot partition; on `RealSys` the I/O is
/// byte-identical to a direct `std::fs` open/read.
pub fn ensure_generation_signed(
    fs: &dyn FsOps,
    config: &Config,
    generation: &Generation,
) -> Result<()> {
    let sig_dir = generation_sig_dir(config, generation)?;
    let suffix = config.signing.sig_path_suffix.as_str();

    verify_generation_blob(
        fs,
        config,
        &generation.kernel,
        &sig_dir.join(format!("kernel{suffix}")),
        "generation kernel",
        DOMAIN_GEN_KERNEL,
    )?;
    verify_generation_blob(
        fs,
        config,
        &generation.initrd,
        &sig_dir.join(format!("initrd{suffix}")),
        "generation initrd",
        DOMAIN_GEN_INITRD,
    )?;
    Ok(())
}

/// Open `blob` read-only and verify it against `sig_path` under `domain`.
/// Opens ONCE via [`FsOps::open_ro`] and stream-hashes the pinned fd — the
/// path is never reopened for hashing (FIX-02/FIX-64). The sidecar is read
/// through [`FsOps::read_file`] so the dry-run verifies the closure copy.
fn verify_generation_blob(
    fs: &dyn FsOps,
    config: &Config,
    blob: &Path,
    sig_path: &Path,
    desc: &str,
    domain: &'static [u8],
) -> Result<()> {
    let file = fs.open_ro(blob).map_err(|source| NmblError::Io {
        source,
        context: format!("open {desc} {} for verify", blob.display()),
    })?;
    let sig_bytes = read_sidecar(fs, sig_path, desc)?;
    verify_image_fd_digest_bytes(file.as_fd(), desc, &sig_bytes, domain, config).map(|_digest| ())
}

/// Read a sidecar's bytes through `fs`, shaping the I/O error to match the
/// path-based [`super::verify_image_fd_digest`] so the dry-run and the real
/// path surface identical context.
fn read_sidecar(fs: &dyn FsOps, sig_path: &Path, desc: &str) -> Result<Vec<u8>> {
    fs.read_file(sig_path).map_err(|source| NmblError::Io {
        source,
        context: format!("read sidecar {} for {desc}", sig_path.display()),
    })
}

/// A generation whose kernel+initrd were verified over PINNED fds, carrying the
/// artefacts every downstream step must REUSE rather than recompute (FIX-02).
///
/// Closing the verify→measure→load TOCTOU (MED-1) requires that the SAME bytes
/// be verified, measured, AND loaded. This witness holds:
///
/// * `kernel_fd` — the kernel's OWN, still-open `O_RDONLY` fd. The verifier
///   opened the kernel ONCE, hashed it over this fd, and verified its
///   signature; the loader hands THIS fd to `kexec_file_load(2)` (never
///   re-opening the path), so the loaded kernel is byte-identical to the
///   verified+measured one.
/// * `initrd_fd` — the (pristine) initrd's OWN, still-open `O_RDONLY` fd, kept
///   on the SAME footing as the kernel fd (LOW-A). The loader splices the NMBL
///   cpio fragment onto the initrd bytes read from THIS fd — never re-reading
///   the path — so the initrd bytes spliced into the kexec memfd are
///   byte-identical to the ones verified + measured. Closing the verify→load
///   window for the initrd too, not just the kernel.
/// * `kernel_digest` / `initrd_digest` — the SHA-512 digests the verifier
///   already streamed over the pinned fds. The PCR-11 measure reuses these
///   verbatim (no second hash — FIX-02).
///
/// Holding the fds in the witness keeps them alive for the whole verify→measure
/// →load window: dropping the witness closes them, so the loader must consume
/// them within that window.
#[derive(Debug)]
pub struct VerifiedGeneration {
    /// The kernel's pinned fd — opened once for verify, reused for load.
    pub kernel_fd: OwnedFd,
    /// The (pristine) initrd's pinned fd — opened once for verify, reused for
    /// the kexec initrd splice (LOW-A — no path re-read).
    pub initrd_fd: OwnedFd,
    /// SHA-512 of the kernel, reused by the measure step (no re-hash).
    pub kernel_digest: [u8; 64],
    /// SHA-512 of the (pristine) initrd, reused by the measure step.
    pub initrd_digest: [u8; 64],
}

/// Verify a generation's kernel+initrd AND return the pinned kernel fd + reused
/// digests (FIX-02 / MED-1).
///
/// Unlike [`ensure_generation_signed`] (which drops every fd once it has a
/// verdict), this opens the kernel ONCE and KEEPS that fd, streams it through
/// SHA-512 (the digest the sidecar verify uses AND the measure reuses), then
/// verifies the signature over that one fd. The initrd is opened, hashed, and
/// verified the same way, AND its fd is likewise RETAINED (LOW-A): the loader
/// splices the NMBL cpio fragment onto the initrd bytes read from that pinned
/// fd, so the bytes loaded == verified == measured for the initrd too (only the
/// pristine initrd is measured, never the fragment — FIX-42).
///
/// On success the returned [`VerifiedGeneration`] owns the live kernel + initrd
/// fds; the caller loads from THOSE fds. On any verify failure the fds are
/// dropped and the error propagates (the gate maps audit-vs-enforce).
pub fn verify_generation_pinned(
    fs: &dyn FsOps,
    config: &Config,
    generation: &Generation,
) -> Result<VerifiedGeneration> {
    let sig_dir = generation_sig_dir(config, generation)?;
    let suffix = config.signing.sig_path_suffix.as_str();

    // Open the kernel ONCE via the ops layer and keep its fd for the load
    // (FIX-02). Verify + hash both happen over this exact fd; the fd handed to
    // kexec is THIS one, never a re-open by path. On a dry-run `open_ro` yields
    // the extracted-closure copy, so the verify runs against the shipped bytes.
    let kernel_file = fs
        .open_ro(&generation.kernel)
        .map_err(|source| NmblError::Io {
            source,
            context: format!(
                "open generation kernel {} for verify+load",
                generation.kernel.display()
            ),
        })?;
    let kernel_sig = sig_dir.join(format!("kernel{suffix}"));
    let kernel_sig_bytes = read_sidecar(fs, &kernel_sig, "generation kernel")?;
    // ONE hash over the pinned fd serves both verify and measure (FIX-02).
    let kernel_digest = verify_image_fd_digest_bytes(
        kernel_file.as_fd(),
        "generation kernel",
        &kernel_sig_bytes,
        DOMAIN_GEN_KERNEL,
        config,
    )?;

    // The initrd is verified over its OWN pinned fd, its digest captured for the
    // measure, AND its fd RETAINED for the load splice (LOW-A — no path re-read).
    let (initrd_fd, initrd_digest) = verify_generation_blob_pinned(
        fs,
        config,
        &generation.initrd,
        &sig_dir.join(format!("initrd{suffix}")),
        "generation initrd",
        DOMAIN_GEN_INITRD,
    )?;

    Ok(VerifiedGeneration {
        kernel_fd: kernel_file.into(),
        initrd_fd,
        kernel_digest,
        initrd_digest,
    })
}

/// Like [`verify_generation_blob`], but opens the blob ONCE via [`FsOps`],
/// verifies + hashes it over that single pinned fd, and RETURNS both the live
/// fd and the SHA-512 digest. The kept fd lets the loader splice from the exact
/// verified+measured bytes (LOW-A); the digest is reused by the measure step
/// (FIX-02).
fn verify_generation_blob_pinned(
    fs: &dyn FsOps,
    config: &Config,
    blob: &Path,
    sig_path: &Path,
    desc: &str,
    domain: &'static [u8],
) -> Result<(OwnedFd, [u8; 64])> {
    let file = fs.open_ro(blob).map_err(|source| NmblError::Io {
        source,
        context: format!("open {desc} {} for verify+load", blob.display()),
    })?;
    let sig_bytes = read_sidecar(fs, sig_path, desc)?;
    let digest = verify_image_fd_digest_bytes(file.as_fd(), desc, &sig_bytes, domain, config)?;
    Ok((file.into(), digest))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on the dry-run verify-against-closure contract"
)]
mod tests {
    use super::*;
    use crate::sys::ops::dryrun::{ClosureView, DryRunScenario, DryRunSys};
    use fips204::traits::{KeyGen, SerDes, Signer};
    use fips204::{Ph, ml_dsa_65};
    use std::path::PathBuf;
    use tempfile::TempDir;

    use crate::sig::alg::{AlgId, HashId};
    use crate::sig::wire::{self, Header};

    /// A `DryRunSys` rooted at `root` — its `open_ro`/`read_file` resolve a boot
    /// path UNDER `root`, so the verify runs against the extracted-closure copy
    /// exactly as `--validate-initrm` would.
    fn closure_ops(root: &std::path::Path) -> DryRunSys {
        DryRunSys::new(
            ClosureView::new(root.to_path_buf()),
            DryRunScenario::NormalBoot,
        )
    }

    /// A signing config whose boot mountpoint is the boot path the closure maps
    /// `/boot` to (so sidecar resolution lands inside the closure).
    fn signing_config() -> Config {
        toml::from_str::<Config>("[paths]\nshell = \"/bin/sh\"\n[signing]\nenable = true\n")
            .expect("config parses")
    }

    /// A generation whose kernel/initrd are CLOSURE-relative boot paths (so
    /// `open_ro` grafts them under the closure root) but whose `toplevel` is the
    /// REAL on-disk store dir under `root` — `gen_id` canonicalizes it directly
    /// (it is not routed through ops) and takes its basename for the sidecar dir.
    fn closure_generation(root: &std::path::Path) -> Generation {
        let top = root.join("nix/store/abc123-nixos-system-7");
        Generation {
            number: 7,
            profile_link: top.clone(),
            toplevel: top.clone(),
            kernel: PathBuf::from("/boot/vmlinuz"),
            initrd: PathBuf::from("/boot/initrd"),
            init_path: top.join("init"),
            kernel_params: Vec::new(),
            label: String::new(),
        }
    }

    /// Lay out `<root>/boot/{vmlinuz,initrd}` and `<root>/nix/store/...` so the
    /// closure has the kernel+initrd a dry-run streams + verifies.
    fn lay_out_closure(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("boot")).expect("boot dir");
        std::fs::create_dir_all(root.join("nix/store/abc123-nixos-system-7")).expect("store dir");
        std::fs::write(root.join("boot/vmlinuz"), b"closure-kernel-bytes").expect("kernel");
        std::fs::write(root.join("boot/initrd"), b"closure-initrd-bytes").expect("initrd");
    }

    /// Mint a real ML-DSA-65 sidecar over the SHA-512 of `blob` under `domain`,
    /// returning the sidecar bytes plus the verifying-key bytes. Mirrors the
    /// host signer (pre-hash `Ph::SHA512`, per-role ctx).
    fn sign_blob(blob: &std::path::Path, domain: &[u8], seed: u8) -> (Vec<u8>, Vec<u8>) {
        let (pk, sk) = ml_dsa_65::KG::keygen_from_seed(&[seed; 32]);
        let file = std::fs::File::open(blob).expect("open blob");
        let (digest, _len) = crate::util::hash::sha512_fd(file.as_fd()).expect("hash");
        let sig = sk
            .try_hash_sign_with_seed(&[0x42u8; 32], &digest, domain, &Ph::SHA512)
            .expect("sign");
        let header = Header {
            alg: AlgId::MlDsa65,
            hash: HashId::Sha512,
            key_id: 0,
            domain: wire::domain_tag(domain),
        };
        let mut sidecar = wire::encode(&header).to_vec();
        sidecar.extend_from_slice(&sig);
        (sidecar, pk.into_bytes().to_vec())
    }

    #[test]
    fn dry_run_verify_fails_closed_on_missing_closure_sidecar() {
        // The kernel+initrd ship in the closure but NO sidecar does. The dry-run
        // verify must therefore FAIL while reading the sidecar — proving it ran
        // against the closure copy (and would catch a missing signature at
        // validate time), not silently pass.
        let tmp = TempDir::new().expect("temp");
        lay_out_closure(tmp.path());
        let mut cfg = signing_config();
        // Closure-relative boot mount: `read_file`/`open_ro` graft it under the
        // closure root, so the sidecar path resolves inside the extracted tree.
        cfg.runtime_boot_mountpoint = Some(PathBuf::from("/boot"));
        let fs = closure_ops(tmp.path());

        let err = verify_generation_pinned(&fs, &cfg, &closure_generation(tmp.path()))
            .expect_err("missing closure sidecar must fail closed");
        assert!(
            matches!(
                err,
                NmblError::Io {
                    ref context,
                    ..
                } if context.contains("read sidecar")
            ),
            "expected a sidecar-read Io error, got {err:?}",
        );
    }

    #[test]
    fn dry_run_verify_rejects_wrong_key_against_closure() {
        // A correctly-formed sidecar minted under a key that is NOT baked into
        // THIS build must be rejected: there are no baked keys, so the any-of
        // loop finds no candidate and refuses. This proves the dry-run runs the
        // real ML-DSA verify against the closure bytes (catches a wrong-key sig).
        let tmp = TempDir::new().expect("temp");
        lay_out_closure(tmp.path());
        let sig_dir = tmp.path().join("boot/nmbl/sigs/abc123-nixos-system-7");
        std::fs::create_dir_all(&sig_dir).expect("sig dir");
        let (k_sidecar, _pk) = sign_blob(&tmp.path().join("boot/vmlinuz"), DOMAIN_GEN_KERNEL, 9);
        std::fs::write(sig_dir.join("kernel.sig"), &k_sidecar).expect("kernel sig");

        let mut cfg = signing_config();
        cfg.runtime_boot_mountpoint = Some(PathBuf::from("/boot"));
        let fs = closure_ops(tmp.path());

        let err = verify_generation_pinned(&fs, &cfg, &closure_generation(tmp.path()))
            .expect_err("a sig from an un-baked key must be refused");
        assert!(
            matches!(err, NmblError::Signature { .. }),
            "expected a Signature rejection (no baked key), got {err:?}",
        );
    }
}
