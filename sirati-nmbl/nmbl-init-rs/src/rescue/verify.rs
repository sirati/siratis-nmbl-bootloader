//! External-rescue squashfs signature verification (#21, R-1).
//!
//! Before NMBL loop-mounts and enters the on-disk `nmbl-rescue.sfs`, this
//! module verifies its detached signature over a SINGLE pinned fd under the
//! frozen `nmbl:rescue-sfs:v1` domain ([`DOMAIN_RESCUE_SFS`]). A tampered or
//! unsigned rescue image must NEVER be entered: under enforcement a
//! bad/missing/wrong-key signature routes the caller through
//! [`crate::policy::refuse_unsigned`] → `RebootIntoRescue` (R-1) — a refuse,
//! NOT a silent halt and NOT entering the rescue.
//!
//! The image is opened ONCE by [`super::image::open`]; the signature is
//! verified over that descriptor, which is then the one bound to the loop
//! device, and the SHA-512 streamed here is reused for the stage-2 pin.
//!
//! The hook mirrors the generation guard's shape ([`crate::sig::gate`]): the
//! cryptographic decision is entirely the frozen [`crate::sig::verify`]
//! pipeline's; this module only resolves the sidecar, opens the image once,
//! and maps the verify result through the operator's `[signing]` posture:
//!
//! * **feature-off** — this whole module is `secure-boot`-gated, so a binary
//!   built without `secure-boot` performs no rescue-image verification at all
//!   (the legacy behaviour).
//! * **`signing.enable = false`** — verification is declined; the dispatcher
//!   proceeds to mount (the operator opted out, not an allow-unsigned bypass).
//! * **Enforce** (`enable && enforce`) — a bad/missing/wrong-key signature is
//!   a hard refuse; the caller routes the cause to `refuse_unsigned`.
//! * **Audit** (`enable && !enforce`) — the SAME verify runs, but a failure
//!   only WARNs and the dispatcher proceeds to mount (FIX-16/FIX-31).
//!
//! ## Sidecar resolution
//!
//! The rescue squashfs lives on the boot partition as a single blob (resolved
//! by [`super::locate_sfs`]); its detached sidecar is the SIBLING file
//! `<sfs-path><signing.sig_path_suffix>` (e.g. `nmbl-rescue.sfs.sig`). This
//! mirrors how the generation guard appends `signing.sig_path_suffix` to a
//! blob stem, but keeps the sidecar next to the image it signs rather than in
//! the per-generation `nmbl/sigs/<gen-id>/` directory — the rescue image is
//! not part of any NixOS generation.

use std::ffi::OsString;
use std::os::fd::AsFd;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sig::{self, DOMAIN_RESCUE_SFS, PolicyDecision};

/// Verify the external rescue squashfs and apply the `[signing]` policy gate.
///
/// Short-circuits to [`PolicyDecision::Proceed`] when `signing.enable` is
/// `false` (the operator declined the feature — NOT an allow-unsigned bypass,
/// FIX-04). Otherwise resolves the sidecar, opens the squashfs ONCE, streams
/// it through [`sig::verify_image_fd`] under [`DOMAIN_RESCUE_SFS`] over that
/// single pinned fd, and maps the result through [`sig::apply_policy`]:
/// enforce ⇒ [`PolicyDecision::Refuse`] (the caller hands the cause to
/// `refuse_unsigned`), audit ⇒ WARN + [`PolicyDecision::Proceed`].
///
/// The caller MUST act on a [`PolicyDecision::Refuse`] by routing the cause
/// through `policy::refuse_unsigned` BEFORE any loop-mount/switch into the
/// image — this function only produces the decision, it never mounts, caps,
/// or constructs a `TerminalAction`.
#[must_use]
pub fn verify_rescue_image_gated(
    config: &Config,
    image: &Result<super::image::Stage2Image>,
) -> (PolicyDecision, Option<[u8; 64]>) {
    // signing safety: signing-disabled is the operator declining the feature,
    // NOT an allow-unsigned bypass of an enabled one (FIX-04). The legacy
    // (feature-free) rescue verifies nothing; this matches that posture.
    if !config.signing.enable {
        crate::nmbl_info!(
            "rescue: signature verification disabled (signing.enable = false); skipping gate"
        );
        return (PolicyDecision::Proceed, None);
    }
    let result = verify_rescue_image(config, image);
    let digest = result.as_ref().ok().copied();
    (sig::apply_policy(config, result.map(|_| ())), digest)
}

/// Resolve the sidecar and verify the image over the descriptor
/// [`super::image::open`] pinned, under [`DOMAIN_RESCUE_SFS`]. Returns the
/// SHA-512 streamed over that descriptor so the stage-2 pin reuses it.
fn verify_rescue_image(
    config: &Config,
    image: &Result<super::image::Stage2Image>,
) -> Result<[u8; 64]> {
    let image = image.as_ref().map_err(|e| NmblError::Rescue {
        stage: "locate-sfs",
        source: Box::new(NmblError::ConfigInvalid {
            reason: e.to_string(),
            context: "locating the rescue image for signature verification".to_string(),
        }),
    })?;
    let sig_path = rescue_sig_sidecar(&image.path, &config.signing.sig_path_suffix);
    // The SAME descriptor is loop-bound afterwards by `prepare_disk_rescue`
    // (FIX-02/FIX-64): the bytes verified are exactly the bytes mounted.
    let file = image.file.as_ref().map_err(|e| NmblError::Io {
        source: std::io::Error::new(e.kind(), e.to_string()),
        context: format!("open rescue image {} for verify", image.path.display()),
    })?;
    sig::verify_image_fd_digest(
        file.as_fd(),
        "rescue image",
        Some(&sig_path),
        DOMAIN_RESCUE_SFS,
        config,
    )
}

/// Build the detached-sidecar path for the rescue squashfs: the sibling file
/// `<sfs-path><suffix>` (e.g. `/mnt/boot/nmbl-rescue.sfs` ⇒
/// `/mnt/boot/nmbl-rescue.sfs.sig`). Appends `suffix` to the FULL file name so
/// the sidecar sits next to the image, matching the host signer's layout.
fn rescue_sig_sidecar(sfs_path: &std::path::Path, suffix: &str) -> PathBuf {
    let mut name: OsString = sfs_path
        .file_name()
        .map_or_else(OsString::new, OsString::from);
    name.push(suffix);
    sfs_path.with_file_name(name)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use crate::rescue::RescueMode;
    use std::path::Path;

    fn gate(c: &Config) -> PolicyDecision {
        verify_rescue_image_gated(c, &crate::rescue::image::open(c)).0
    }

    fn cfg(enable: bool, enforce: bool, mountpoint: Option<PathBuf>) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = RescueConfig {
            mode: RescueMode::External,
            ..RescueConfig::default()
        };
        c.runtime_boot_mountpoint = mountpoint;
        c.signing.enable = enable;
        c.signing.enforce = enforce;
        c
    }

    #[test]
    fn sidecar_is_sibling_with_suffix() {
        assert_eq!(
            rescue_sig_sidecar(Path::new("/mnt/boot/nmbl-rescue.sfs"), ".sig"),
            PathBuf::from("/mnt/boot/nmbl-rescue.sfs.sig"),
        );
    }

    #[test]
    fn sidecar_honours_custom_suffix() {
        assert_eq!(
            rescue_sig_sidecar(Path::new("/mnt/boot/r.sfs"), ".mldsa"),
            PathBuf::from("/mnt/boot/r.sfs.mldsa"),
        );
    }

    #[test]
    fn disabled_signing_proceeds_without_touching_disk() {
        // signing.enable = false ⇒ the gate short-circuits to Proceed WITHOUT
        // resolving or opening any image (the operator declined the feature,
        // not an allow-unsigned bypass — FIX-04). A mountpoint pointing at a
        // path with no rescue image proves verify never ran.
        let c = cfg(false, false, Some(PathBuf::from("/nonexistent")));
        assert!(gate(&c).is_proceed());
    }

    #[test]
    fn enforce_missing_image_is_refuse() {
        // enable && enforce ⇒ a missing rescue image (so a missing sidecar /
        // unopenable blob) is a hard Refuse the caller routes to
        // refuse_unsigned. No baked keys are needed: the open fails first.
        let dir = tempfile::tempdir().expect("tempdir");
        let c = cfg(true, true, Some(dir.path().to_path_buf()));
        assert!(matches!(gate(&c), PolicyDecision::Refuse(_)));
    }

    #[test]
    fn audit_missing_image_proceeds_with_warning() {
        // enable && !enforce ⇒ audit: the SAME verify runs and FAILS (no
        // image), but the failure only warns and the dispatcher proceeds. This
        // is the ONLY relaxation (FIX-16/FIX-31).
        let dir = tempfile::tempdir().expect("tempdir");
        let c = cfg(true, false, Some(dir.path().to_path_buf()));
        assert!(
            gate(&c).is_proceed(),
            "audit mode must proceed on a missing/bad rescue signature"
        );
    }

    #[test]
    fn enforce_present_image_unsigned_is_refuse() {
        // A present squashfs blob with NO sidecar under enforce must refuse:
        // verify_image_fd fails to read the sidecar, and enforce maps that to
        // Refuse (a bad/missing signature never enters the rescue — R-1).
        let dir = tempfile::tempdir().expect("tempdir");
        let sfs = dir.path().join("nmbl-rescue.sfs");
        std::fs::write(&sfs, b"not-really-a-squashfs").expect("write sfs");
        let c = cfg(true, true, Some(dir.path().to_path_buf()));
        assert!(matches!(gate(&c), PolicyDecision::Refuse(_)));
    }
}
