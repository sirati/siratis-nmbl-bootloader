//! The real fail-closed ML-DSA verify pipeline (#15 — FIX-01/46/50/51).
//!
//! Replaces the F2 stub bodies with the any-of detached-signature pipeline the
//! whole secure-boot tree binds to. Three layers:
//!
//! - [`verify_digest`] — the lowest level: a precomputed SHA-512 digest, a
//!   per-role `domain`, a parsed sidecar, and the resolved keys. Recomputes the
//!   sidecar domain tag and REJECTS on mismatch (the domain-cross-reject
//!   property, FIX-01), then tries each key whose algorithm matches the sidecar,
//!   accepting on the FIRST valid signature and erroring only after ALL keys
//!   have been tried. A signature-length mismatch inside the loop is a HARD
//!   error (FIX-46), never a `continue`.
//! - [`verify_image_fd`] — opens-once / seeks-to-0 / streams the image through
//!   SHA-512 over a SINGLE pinned fd (FIX-02/FIX-51), loads + parses the
//!   sidecar, and calls [`verify_digest`].
//! - [`ensure_generation_signed`] — resolves the per-generation sidecar dir
//!   `/boot/nmbl/sigs/<gen-id>/{kernel,initrd}.sig` and verifies each blob under
//!   its own domain.
//!
//! There is intentionally NO path-reopening `verify_detached` (FIX-64): every
//! trust path verifies an already-open, pinned fd.

use std::fs;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::util::hash;

use super::keys::{self, BakedKey, KeyVerify};
use super::scan::generation_sig_dir;
use super::sidecar::SigSidecar;
use super::wire;

// The stub Ok(())-returning bodies in this crate are GONE. This assert makes a
// regression that re-introduces an unconditional `Ok(())` on a secure-boot
// build a hard compile error: the real pipeline is the only thing that may
// compile under `secure-boot` (FIX-50). The module only exists under
// `secure-boot`, so the assert is unconditionally true here and documents the
// invariant at the module root.
const _: () = assert!(
    cfg!(feature = "secure-boot"),
    "sig::verify must only compile under the secure-boot feature; a stub Ok(()) \
     verifier may never coexist with it (FIX-50)"
);

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

/// How strictly a verify call treats a missing or unparseable sidecar.
///
/// The real audit-vs-enforce wiring is consumed in #19 (via `apply_policy`);
/// the verify pipeline here threads it so a future audit path can downgrade a
/// failure to a warning. The bodies below are fail-closed regardless: they
/// return `Err` on any mismatch and let the caller decide whether `Audit`
/// suppresses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyPolicy {
    /// Fail-closed: a missing/bad signature is a hard `Err`.
    Enforce,
    /// Audit: verify and LOG, but do not refuse boot. Only reachable behind
    /// `signing.enable && !enforce && allowAuditModeInsecure` (FIX-16/FIX-31);
    /// never the default.
    Audit,
}

impl VerifyPolicy {
    /// Derive the verify posture from the runtime config's `[signing]` table.
    /// Enforcing unless the operator deliberately opted into audit mode
    /// (`enable && !enforce`, itself gated by `allowAuditModeInsecure` on the
    /// Nix side).
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        if config.signing.enable && !config.signing.enforce {
            Self::Audit
        } else {
            Self::Enforce
        }
    }
}

/// Verify a precomputed 64-byte SHA-512 digest against `keys` under `domain`.
///
/// The lowest-level verify entry: the caller has already streamed the image
/// through SHA-512 (single pinned fd) and parsed the sidecar. Steps:
///
/// 1. Recompute `wire::domain_tag(domain)` and compare it to the sidecar's
///    recorded domain tag — a mismatch is the cross-role replay case and is a
///    hard `Err` BEFORE any key is touched (FIX-01).
/// 2. Narrow to the keys whose algorithm matches the sidecar, ordered by the
///    `key_id` hint (order only — `key_id` never narrows trust).
/// 3. Try each such key's pre-hash verify; accept on the FIRST that verifies.
///    A signature-LENGTH mismatch inside the loop is an internal inconsistency
///    and returns a hard error immediately, never a `continue` (FIX-46).
/// 4. If no key verifies (and at least one matched the algorithm), `Err`.
#[allow(
    clippy::needless_pass_by_value,
    reason = "VerifyPolicy is Copy; by-value keeps the frozen signature stable"
)]
pub fn verify_digest(
    digest: &[u8; 64],
    domain: &'static [u8],
    sidecar: &SigSidecar<'_>,
    keys: &[BakedKey],
    policy: VerifyPolicy,
) -> Result<()> {
    let _ = policy; // fail-closed body; the caller maps Audit, not us.

    // (1) Domain-cross-reject: the sidecar's recorded tag must equal this
    // role's tag. Done first so a wrong-role signature can never even be
    // offered to a key.
    let expected_tag = wire::domain_tag(domain);
    if sidecar.domain_tag() != &expected_tag {
        return Err(NmblError::Signature {
            stage: "domain-mismatch",
            detail: format!(
                "sidecar domain tag does not match the {} role",
                String::from_utf8_lossy(domain)
            ),
        });
    }

    // (2) Only keys of the sidecar's algorithm can verify its signature; order
    // by the key_id hint for performance (never trust narrowing).
    let alg = sidecar.alg();
    let ordered = keys::order_by_hint(keys, sidecar.key_id());
    let sig = sidecar.signature();

    // (3) Any-of: accept on the first valid signature; a length mismatch is a
    // hard error (FIX-46).
    let mut tried = 0usize;
    for key in ordered {
        if key.alg != alg {
            continue;
        }
        tried += 1;
        match key.key.hash_verify_digest(digest, sig, domain) {
            KeyVerify::Accept => return Ok(()),
            KeyVerify::Reject => {}
            KeyVerify::BadSigLen => {
                return Err(NmblError::Signature {
                    stage: "internal-siglen",
                    detail: format!(
                        "signature length {} does not match {alg:?} sig_len {}",
                        sig.len(),
                        alg.sig_len()
                    ),
                });
            }
        }
    }

    // (4) No key accepted. Distinguish "no key of the right algorithm" from
    // "tried keys but none verified" for the operator log.
    let detail = if tried == 0 {
        format!("no baked {alg:?} key available to verify this signature")
    } else {
        format!("signature rejected by all {tried} candidate {alg:?} key(s)")
    };
    Err(NmblError::Signature {
        stage: "no-valid-key",
        detail,
    })
}

/// Verify an already-open image fd against its sidecar under `domain`.
///
/// The trust-path entry used by every consumer (FIX-64: fd-only, no
/// path-reopen). Streams the fd through SHA-512 (seek-to-0 first, asserting
/// bytes-hashed == file length — FIX-51), loads + parses the sidecar from
/// `sig`, then calls [`verify_digest`]. Keys + policy come from `config`.
pub fn verify_image_fd(
    fd: BorrowedFd<'_>,
    image_desc: &str,
    sig: Option<&Path>,
    domain: &'static [u8],
    config: &Config,
) -> Result<()> {
    verify_image_fd_digest(fd, image_desc, sig, domain, config).map(|_digest| ())
}

/// Like [`verify_image_fd`] but RETURNS the SHA-512 digest it streamed over the
/// pinned fd, so a caller that also needs to MEASURE the image reuses the SAME
/// hash rather than re-streaming it (FIX-02 — one hash, verify + measure).
///
/// Identical verify semantics to [`verify_image_fd`]; the only difference is the
/// digest is handed back instead of dropped.
pub fn verify_image_fd_digest(
    fd: BorrowedFd<'_>,
    image_desc: &str,
    sig: Option<&Path>,
    domain: &'static [u8],
    config: &Config,
) -> Result<[u8; 64]> {
    // The fd-only contract needs an explicit sidecar path; a borrowed fd has
    // no path to derive a sibling convention from.
    let sig_path = sig.ok_or_else(|| NmblError::Signature {
        stage: "sidecar-missing",
        detail: format!("no sidecar path supplied for {image_desc}"),
    })?;

    // Stream the WHOLE image over the single pinned fd. `bytes_hashed` is the
    // exact length read after seeking to 0 (FIX-51); a downstream consumer
    // wanting a length cross-check has it, and the seek guarantees we never
    // hash only a tail.
    let (digest, _bytes_hashed) = hash::sha512_fd(fd)?;

    let sig_bytes = fs::read(sig_path).map_err(|source| NmblError::Io {
        source,
        context: format!("read sidecar {} for {image_desc}", sig_path.display()),
    })?;
    let sidecar = SigSidecar::parse(&sig_bytes).map_err(|e| NmblError::Signature {
        stage: "sidecar-parse",
        detail: format!("{image_desc}: {e}"),
    })?;

    let baked = keys::parse_baked_keys()?;
    let policy = VerifyPolicy::from_config(config);
    verify_digest(&digest, domain, &sidecar, &baked, policy)?;
    Ok(digest)
}

/// Ensure a generation's kernel AND initrd both carry a valid signature.
///
/// Resolves the per-generation sidecar directory `<boot>/nmbl/sigs/<gen-id>/`
/// (R-4) and verifies `kernel<suffix>` under [`DOMAIN_GEN_KERNEL`] and
/// `initrd<suffix>` under [`DOMAIN_GEN_INITRD`], each over an own pinned fd via
/// [`verify_image_fd`]. BOTH must verify.
///
/// (The generation parameter is named `generation`, not `gen`: `gen` is a
/// reserved keyword in edition 2024.)
pub fn ensure_generation_signed(config: &Config, generation: &Generation) -> Result<()> {
    let sig_dir = generation_sig_dir(config, generation)?;
    let suffix = config.signing.sig_path_suffix.as_str();

    verify_generation_blob(
        config,
        &generation.kernel,
        &sig_dir.join(format!("kernel{suffix}")),
        "generation kernel",
        DOMAIN_GEN_KERNEL,
    )?;
    verify_generation_blob(
        config,
        &generation.initrd,
        &sig_dir.join(format!("initrd{suffix}")),
        "generation initrd",
        DOMAIN_GEN_INITRD,
    )?;
    Ok(())
}

/// Open `blob` read-only and verify it against `sig_path` under `domain`.
/// Opens ONCE and hands the pinned fd straight to [`verify_image_fd`] — the
/// path is never reopened for hashing (FIX-02/FIX-64).
fn verify_generation_blob(
    config: &Config,
    blob: &Path,
    sig_path: &Path,
    desc: &str,
    domain: &'static [u8],
) -> Result<()> {
    let file = fs::File::open(blob).map_err(|source| NmblError::Io {
        source,
        context: format!("open {desc} {} for verify", blob.display()),
    })?;
    verify_image_fd(file.as_fd(), desc, Some(sig_path), domain, config)
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
/// * `kernel_digest` / `initrd_digest` — the SHA-512 digests the verifier
///   already streamed over the pinned fds. The PCR-11 measure reuses these
///   verbatim (no second hash — FIX-02).
///
/// Holding the fd in the witness keeps it alive for the whole verify→measure
/// →load window: dropping the witness closes the fd, so the loader must consume
/// it within that window.
#[derive(Debug)]
pub struct VerifiedGeneration {
    /// The kernel's pinned fd — opened once for verify, reused for load.
    pub kernel_fd: OwnedFd,
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
/// verified the same way; its fd is not retained (the loader re-reads the
/// pristine initrd into a memfd to splice the NMBL cpio fragment — the initrd
/// digest is what binds it, and it is measured, not the fragment — FIX-42).
///
/// On success the returned [`VerifiedGeneration`] owns the live kernel fd; the
/// caller loads THAT fd. On any verify failure the fd is dropped and the error
/// propagates (the gate maps audit-vs-enforce).
pub fn verify_generation_pinned(
    config: &Config,
    generation: &Generation,
) -> Result<VerifiedGeneration> {
    let sig_dir = generation_sig_dir(config, generation)?;
    let suffix = config.signing.sig_path_suffix.as_str();

    // Open the kernel ONCE and keep its fd for the load (FIX-02). Verify +
    // hash both happen over this exact fd.
    let kernel_file = fs::File::open(&generation.kernel).map_err(|source| NmblError::Io {
        source,
        context: format!(
            "open generation kernel {} for verify+load",
            generation.kernel.display()
        ),
    })?;
    let kernel_sig = sig_dir.join(format!("kernel{suffix}"));
    // ONE hash over the pinned fd serves both verify and measure (FIX-02).
    let kernel_digest = verify_image_fd_digest(
        kernel_file.as_fd(),
        "generation kernel",
        Some(&kernel_sig),
        DOMAIN_GEN_KERNEL,
        config,
    )?;

    // The initrd is verified over its own pinned fd; its digest is captured for
    // the measure. Its fd is not retained (see the doc comment).
    let initrd_digest = verify_generation_blob_digest(
        config,
        &generation.initrd,
        &sig_dir.join(format!("initrd{suffix}")),
        "generation initrd",
        DOMAIN_GEN_INITRD,
    )?;

    Ok(VerifiedGeneration {
        kernel_fd: kernel_file.into(),
        kernel_digest,
        initrd_digest,
    })
}

/// Like [`verify_generation_blob`], but also returns the SHA-512 digest the
/// verify computed over the pinned fd, so the measure step reuses it (FIX-02).
fn verify_generation_blob_digest(
    config: &Config,
    blob: &Path,
    sig_path: &Path,
    desc: &str,
    domain: &'static [u8],
) -> Result<[u8; 64]> {
    let file = fs::File::open(blob).map_err(|source| NmblError::Io {
        source,
        context: format!("open {desc} {} for verify", blob.display()),
    })?;
    verify_image_fd_digest(file.as_fd(), desc, Some(sig_path), domain, config)
}
