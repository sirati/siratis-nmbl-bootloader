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
use crate::sys::ops::FsOps;

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
pub fn verify_rescue_sfs_gated(fs: &dyn FsOps, config: &Config) -> PolicyDecision {
    // signing safety: signing-disabled is the operator declining the feature,
    // NOT an allow-unsigned bypass of an enabled one (FIX-04). The legacy
    // (feature-free) rescue verifies nothing; this matches that posture.
    if !config.signing.enable {
        crate::nmbl_info!(
            "rescue: signature verification disabled (signing.enable = false); skipping gate"
        );
        return PolicyDecision::Proceed;
    }
    sig::apply_policy(config, verify_rescue_sfs(fs, config))
}

/// Resolve the sidecar, open the squashfs once, and verify it over a single
/// pinned fd under [`DOMAIN_RESCUE_SFS`]. Returns the raw verify `Result` for
/// [`verify_rescue_sfs_gated`] to map through the policy gate.
fn verify_rescue_sfs(fs: &dyn FsOps, config: &Config) -> Result<()> {
    let sfs_path = super::locate_sfs(config)?;
    let sig_path = rescue_sig_sidecar(&sfs_path, &config.signing.sig_path_suffix);

    // Open the image ONCE through the ops seam and hand the pinned fd straight
    // to the verify pipeline (FIX-02/FIX-64): the path is never reopened for
    // hashing, so the bytes verified are exactly the bytes this fd refers to.
    // Routing through `FsOps` means a `--validate-initrm` dry-run verifies the
    // closure copy of the rescue sfs. `prepare_disk_rescue` opens its own fd for
    // the loop bind afterwards; both resolve the same boot-partition path, and
    // the enforce-refuse short-circuits before any mount so a tampered image is
    // never loop-bound.
    let file = fs.open_ro(&sfs_path).map_err(|source| NmblError::Io {
        source,
        context: format!("open rescue squashfs {} for verify", sfs_path.display()),
    })?;
    // Read the sidecar through the SAME seam so blob + sidecar come from one
    // source (closure on a dry-run, live boot fs on a real boot).
    let sig_bytes = fs.read_file(&sig_path).map_err(|source| NmblError::Io {
        source,
        context: format!(
            "read rescue squashfs sidecar {} for verify",
            sig_path.display()
        ),
    })?;
    sig::verify_image_fd_digest_bytes(
        file.as_fd(),
        "rescue squashfs",
        &sig_bytes,
        DOMAIN_RESCUE_SFS,
        config,
    )
    .map(|_digest| ())
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
    use crate::sys::ops::dryrun::{ClosureView, DryRunScenario, DryRunSys};
    use std::path::Path;

    /// A real-FS-backed [`FsOps`] for these tests: a [`DryRunSys`] rooted at
    /// `/`, so `open_ro`/`read_file` are identity opens over the absolute
    /// tempdir paths — the same I/O the real boot performs.
    fn runtime_fs() -> DryRunSys {
        DryRunSys::new(
            ClosureView::new(PathBuf::from("/")),
            DryRunScenario::NormalBoot,
        )
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
        assert!(verify_rescue_sfs_gated(&runtime_fs(), &c).is_proceed());
    }

    #[test]
    fn enforce_missing_image_is_refuse() {
        // enable && enforce ⇒ a missing rescue image (so a missing sidecar /
        // unopenable blob) is a hard Refuse the caller routes to
        // refuse_unsigned. No baked keys are needed: the open fails first.
        let dir = tempfile::tempdir().expect("tempdir");
        let c = cfg(true, true, Some(dir.path().to_path_buf()));
        assert!(matches!(
            verify_rescue_sfs_gated(&runtime_fs(), &c),
            PolicyDecision::Refuse(_)
        ));
    }

    #[test]
    fn audit_missing_image_proceeds_with_warning() {
        // enable && !enforce ⇒ audit: the SAME verify runs and FAILS (no
        // image), but the failure only warns and the dispatcher proceeds. This
        // is the ONLY relaxation (FIX-16/FIX-31).
        let dir = tempfile::tempdir().expect("tempdir");
        let c = cfg(true, false, Some(dir.path().to_path_buf()));
        assert!(
            verify_rescue_sfs_gated(&runtime_fs(), &c).is_proceed(),
            "audit mode must proceed on a missing/bad rescue signature"
        );
    }

    #[test]
    fn enforce_present_image_unsigned_is_refuse() {
        // A present squashfs blob with NO sidecar under enforce must refuse: the
        // sidecar read fails, and enforce maps that to Refuse (a bad/missing
        // signature never enters the rescue — R-1).
        let dir = tempfile::tempdir().expect("tempdir");
        let sfs = dir.path().join("nmbl-rescue.sfs");
        std::fs::write(&sfs, b"not-really-a-squashfs").expect("write sfs");
        let c = cfg(true, true, Some(dir.path().to_path_buf()));
        assert!(matches!(
            verify_rescue_sfs_gated(&runtime_fs(), &c),
            PolicyDecision::Refuse(_)
        ));
    }
}
