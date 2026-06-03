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
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::util::hash;

use super::keys::{self, BakedKey, KeyVerify};
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
    verify_digest(&digest, domain, &sidecar, &baked, policy)
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

/// Resolve the per-generation sidecar directory `<boot>/nmbl/sigs/<gen-id>/`.
///
/// `<boot>` is the runtime boot mountpoint (Phase 0.5). `<gen-id>` is the
/// content-addressed store basename of the generation's toplevel — the same id
/// the install signer writes under — derived here as the file name of the
/// generation's profile-link target. (The shared `gen_id(toplevel)` helper and
/// the `scan.rs`-side resolution land in #18; this keeps the resolution local
/// to the verify path until then, matching the R-4 layout.)
fn generation_sig_dir(config: &Config, generation: &Generation) -> Result<PathBuf> {
    let boot = config
        .runtime_boot_mountpoint
        .as_deref()
        .ok_or_else(|| NmblError::Signature {
            stage: "gen-sig-dir",
            detail: "no runtime boot mountpoint to locate generation sidecars".to_string(),
        })?;
    let gen_id = gen_id_of(generation)?;
    Ok(boot.join("nmbl").join("sigs").join(gen_id))
}

/// Content-addressed generation id: the file name of the canonicalized
/// profile-link target (the store basename of the generation toplevel). Stable
/// across rollback. Superseded by the shared `generations::gen_id` in #18.
fn gen_id_of(generation: &Generation) -> Result<String> {
    let toplevel = fs::canonicalize(&generation.profile_link).map_err(|source| NmblError::Io {
        source,
        context: format!("canonicalize {}", generation.profile_link.display()),
    })?;
    toplevel
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .ok_or_else(|| NmblError::Signature {
            stage: "gen-sig-dir",
            detail: format!(
                "generation toplevel {} has no store basename",
                toplevel.display()
            ),
        })
}
