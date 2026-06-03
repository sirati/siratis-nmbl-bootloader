//! Single-fd verification of the staged image + the signed fragment (#33).
//!
//! Both blobs live under the attested volume the priority gate mounted (FIX-26).
//! Each is verified over its OWN pinned fd (FIX-02) against the baked trust
//! anchor, under its role domain (FIX-01): the staged image under
//! [`DOMAIN_DRIVER_IMAGE`] (it is a driver squashfs), the config fragment under
//! [`DOMAIN_STAGED_FRAGMENT`]. The verify result is mapped through the operator's
//! enforce/audit posture via [`crate::sig::apply_policy`] — the SAME gate the
//! generation and driver-image paths use, so there is no allow-unsigned fork
//! (FIX-04). A refuse becomes an `Err` the caller surfaces as
//! `PolicyRefused` against the pristine base config.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::policy::AttestedVolume;
use crate::sig::{self, DOMAIN_DRIVER_IMAGE, DOMAIN_STAGED_FRAGMENT, PolicyDecision};
use crate::sys::ops::FsOps;

/// Verify BOTH staged blobs — the image and the fragment — each single-fd.
///
/// Resolves the `[staged]` pointer set against the attested mountpoint, opens
/// each blob `O_RDONLY | CLOEXEC` ONCE, and runs the frozen fd-only verify
/// pipeline under the blob's role domain, then applies the signing posture.
/// Returns `Ok(())` only when BOTH verify (or audit-mode downgrades the
/// failure); the first refusal is an `Err` the caller routes to refuse.
///
/// # Errors
/// [`NmblError::Signature`] (wrapped) when either blob fails to open or its
/// signature is refused under enforcement.
pub(super) fn verify_staged_blobs(
    fs: &dyn FsOps,
    attested: &AttestedVolume,
    config: &Config,
) -> Result<()> {
    let staged = config.staged.as_ref().ok_or_else(missing_staged)?;
    let mp = attested.mountpoint();

    // The staged image (driver squashfs) under the driver-image role domain.
    let image_path = join_under(mp, &staged.image);
    let image_sig = sidecar_path(&image_path, &config.signing.sig_path_suffix);
    verify_one(
        fs,
        &image_path,
        &image_sig,
        DOMAIN_DRIVER_IMAGE,
        "staged-image",
        config,
    )?;

    // The signed config fragment under the staged-fragment role domain. The
    // sidecar is the explicit `[staged].sig` path (not a suffix sibling), since
    // the fragment + its detached signature are declared as a pair.
    let fragment_path = join_under(mp, &staged.fragment);
    let fragment_sig = join_under(mp, &staged.sig);
    verify_one(
        fs,
        &fragment_path,
        &fragment_sig,
        DOMAIN_STAGED_FRAGMENT,
        "staged-fragment",
        config,
    )
}

/// Resolve the on-disk path of the config fragment under the attested mount, for
/// the caller's [`crate::config::load_fragment`] step (after verification).
pub(super) fn resolve_fragment_path(attested: &AttestedVolume, config: &Config) -> PathBuf {
    let fragment = config
        .staged
        .as_ref()
        .map_or_else(PathBuf::new, |s| s.fragment.clone());
    join_under(attested.mountpoint(), &fragment)
}

/// Open `blob` read-only ONCE and verify it against `sig_path` under `domain`,
/// then apply the operator's signing posture. The pinned fd hashed here is the
/// only fd opened for the blob (FIX-02); the open routes through
/// [`FsOps::open_ro`] so `--validate-initrm` verifies the SHIPPED bytes from the
/// closure with no path-reopen, while the real boot opens the on-disk blob.
fn verify_one(
    fs: &dyn FsOps,
    blob: &Path,
    sig_path: &Path,
    domain: &'static [u8],
    label: &'static str,
    config: &Config,
) -> Result<()> {
    use std::os::fd::AsFd;

    let file = fs.open_ro(blob).map_err(|e| NmblError::Signature {
        stage: label,
        detail: format!("opening {} for verify: {e}", blob.display()),
    })?;

    // Read the sidecar through the ops seam too (not the path-based
    // `verify_image_fd`, which `std::fs::read`s the LIVE host fs): both the blob
    // and its sidecar then come from the SAME source, so a `--validate-initrm`
    // dry-run verifies the closure copy of both.
    let desc = blob.display().to_string();
    let verify_result = fs
        .read_file(sig_path)
        .map_err(|source| NmblError::Io {
            source,
            context: format!("read {label} sidecar {} for verify", sig_path.display()),
        })
        .and_then(|sig_bytes| {
            sig::verify_image_fd_digest_bytes(file.as_fd(), &desc, &sig_bytes, domain, config)
                .map(|_digest| ())
        });

    match sig::apply_policy(config, verify_result) {
        PolicyDecision::Proceed => Ok(()),
        PolicyDecision::Refuse(cause) => Err(NmblError::Signature {
            stage: label,
            detail: format!("{label} signature refused: {cause}"),
        }),
    }
}

/// Join a boot-relative `rel` under the attested mountpoint, stripping a single
/// leading `/` so [`Path::join`] keeps the mountpoint (mirrors the driver-image
/// + rescue path resolution).
fn join_under(mountpoint: &Path, rel: &Path) -> PathBuf {
    mountpoint.join(rel.strip_prefix("/").unwrap_or(rel))
}

/// The sidecar path for a signed image: `<path><suffix>` (the configured
/// `signing.sig_path_suffix`), matching how the signer names siblings.
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// `[staged].enable` was true but the table vanished between the dispatcher
/// check and here — an internal inconsistency, surfaced as a hard refuse.
fn missing_staged() -> NmblError {
    NmblError::Signature {
        stage: "staged-missing",
        detail: "staged-boot enabled but the [staged] table is absent".to_string(),
    }
}
