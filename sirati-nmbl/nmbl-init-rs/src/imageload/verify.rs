//! Driver-image signature verification + policy gate (#23, FIX-05).
//!
//! Step 1 of the per-image pipeline. Over the SINGLE pinned fd (FIX-02), run
//! the frozen ML-DSA verify pipeline under the driver-image role domain, then
//! map the result through the operator's signing posture. This is the ONLY
//! place the driver-image loader decides "is this image trusted?" — it holds no
//! crypto of its own; the decision is entirely [`crate::sig`]'s.
//!
//! The verify primitive is ALWAYS-ON: even though `driver_images.enable`
//! requires a secure-boot build to be set (FIX-05), the loader still verifies
//! every image rather than trusting the config flag. An enforce-mode failure
//! becomes a [`NmblError::DriverImage`] the caller routes through
//! `refuse_unsigned`; in the (insecure, opt-in) audit posture
//! [`crate::sig::apply_policy`] downgrades it to a warning and the load
//! proceeds — the same audit-vs-enforce semantics as the generation guard.

use std::os::fd::BorrowedFd;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sig::{self, DOMAIN_DRIVER_IMAGE, PolicyDecision};

use super::locate::ResolvedImage;

/// Verify the driver image referred to by `image_fd` against its sidecar and
/// apply the signing policy.
///
/// `image_fd` is the single pinned fd opened by `locate::open_image_ro`; the
/// bytes hashed here are exactly the bytes the loop device will serve to the
/// kernel (FIX-02). On a [`PolicyDecision::Proceed`] returns `Ok(())`; on a
/// [`PolicyDecision::Refuse`] returns the verify error wrapped as a
/// `verify`-stage [`NmblError::DriverImage`] so the caller can route it through
/// `refuse_unsigned` (R-1).
///
/// # Errors
/// [`NmblError::DriverImage`] (`stage = "verify"`) when enforcement refuses the
/// image's signature.
pub(super) fn verify_driver_image(
    image_fd: BorrowedFd<'_>,
    resolved: &ResolvedImage,
    config: &Config,
) -> Result<()> {
    let desc = resolved.image_path.display().to_string();

    // Run the frozen fd-only verify pipeline under the driver-image role
    // domain — a signature minted for any other role cannot verify here
    // (domain-cross-reject, FIX-01).
    let verify_result = sig::verify_image_fd(
        image_fd,
        &desc,
        Some(&resolved.sig_path),
        DOMAIN_DRIVER_IMAGE,
        config,
    );

    // Map the verify result through the operator's enforce/audit posture. There
    // is NO allow-unsigned branch (FIX-04): apply_policy only ever proceeds on a
    // pass, or (audit only) downgrades a failure to a warning.
    match sig::apply_policy(config, verify_result) {
        PolicyDecision::Proceed => Ok(()),
        PolicyDecision::Refuse(cause) => Err(NmblError::DriverImage {
            stage: "verify",
            source: Box::new(cause),
        }),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use std::os::fd::AsFd;
    use std::path::PathBuf;

    use super::*;

    /// A secure-boot config with the given enable/enforce posture.
    fn config_with_posture(enable: bool, enforce: bool) -> Config {
        let text = format!(
            "[paths]\nshell = \"/bin/sh\"\n[signing]\nenable = {enable}\nenforce = {enforce}\n",
        );
        toml::from_str::<Config>(&text).expect("config parses")
    }

    fn resolved(image: PathBuf, sig: PathBuf) -> ResolvedImage {
        ResolvedImage {
            image_path: image,
            sig_path: sig,
        }
    }

    #[test]
    fn enforce_with_missing_sidecar_refuses() {
        // No baked keys + a missing sidecar ⇒ verify fails; enforce posture
        // ⇒ a verify-stage DriverImage refusal (the caller's refuse_unsigned
        // route). Crucially this returns BEFORE any mount happens.
        let dir = tempfile::tempdir().expect("tempdir");
        let img = dir.path().join("d.sfs");
        std::fs::write(&img, b"not-a-real-squashfs").expect("write image");
        // sig path deliberately absent.
        let res = resolved(img.clone(), dir.path().join("d.sfs.sig"));
        let cfg = config_with_posture(true, true);

        let file = std::fs::File::open(&img).expect("open image");
        let err = verify_driver_image(file.as_fd(), &res, &cfg)
            .expect_err("enforce + bad sig must refuse");
        match err {
            NmblError::DriverImage { stage, .. } => assert_eq!(stage, "verify"),
            other => panic!("expected DriverImage(verify), got {other:?}"),
        }
    }

    #[test]
    fn audit_with_missing_sidecar_proceeds() {
        // Same failing verify, but audit posture (enable && !enforce) downgrades
        // it to a warning and proceeds — the only relaxation (FIX-16/FIX-31).
        let dir = tempfile::tempdir().expect("tempdir");
        let img = dir.path().join("d.sfs");
        std::fs::write(&img, b"not-a-real-squashfs").expect("write image");
        let res = resolved(img.clone(), dir.path().join("d.sfs.sig"));
        let cfg = config_with_posture(true, false);

        let file = std::fs::File::open(&img).expect("open image");
        verify_driver_image(file.as_fd(), &res, &cfg)
            .expect("audit mode proceeds on a bad signature");
    }
}
