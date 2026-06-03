//! The MEASURE (b) and LOAD (c) steps of the verify→measure→load handoff,
//! split out of `boot/handoff.rs` to keep each file within the size limit.
//!
//! These two steps are the back half of
//! [`crate::boot::handoff::verify_measure_then_load`]: after the signature gate
//! produces an optional [`VerifiedGeneration`] (the pinned kernel fd + reused
//! digests), [`measure_handoff`] extends PCR-11 (#27 / FIX-12) and
//! [`load_handoff`] hands the verified fd to `kexec_file_load(2)` (FIX-02 /
//! MED-1). Keeping them next to each other preserves the fixed verify → measure
//! → load order and the byte-identity contract between them.

use crate::config::Config;
use crate::error::Result;
use crate::generations::Generation;
use crate::sys;

#[cfg(feature = "secure-boot")]
use std::os::fd::AsFd;

use super::handoff::VerifiedGeneration;

/// Whether this build/config requires the boot handoff to be measured into
/// PCR-11 (R-8): the operator turned on `tpm.measure`, or the secure-boot
/// priority gate is enabled. When neither is set, measuring is a NO-OP.
pub(super) fn measure_required(config: &Config) -> bool {
    // `secure_boot.enable` only exists under the feature; default it to false
    // otherwise so the expression is single-pathed (no needless `return`).
    #[cfg(feature = "secure-boot")]
    let secure_boot_enabled = config.secure_boot.enable;
    #[cfg(not(feature = "secure-boot"))]
    let secure_boot_enabled = false;

    config.tpm.measure || secure_boot_enabled
}

/// (b) MEASURE the handoff into PCR-11 (#27), gated on the measure posture.
///
/// * Measuring OFF ⇒ NO-OP (no extend), regardless of feature.
/// * Measuring ON, secure-boot, a `VerifiedGeneration` in hand ⇒ extend PCR-11
///   with the identity marker, the REUSED kernel+initrd digests (FIX-02), the
///   byte-exact `cmdline` (FIX-14), and the (currently empty) driver-image
///   slice (#28 fills it). A TPM/measure failure is fail-closed: it surfaces as
///   `PolicyRefused` so the boot routes to refuse rather than booting
///   unmeasured (FIX-27).
/// * Measuring ON but NO verified generation (audit-mode failure, or signing
///   disabled while `tpm.measure` is on) ⇒ fail closed: a measure-required boot
///   with no verified inputs cannot be honestly measured, so refuse.
#[cfg_attr(
    not(feature = "secure-boot"),
    expect(
        unused_variables,
        reason = "measure path only compiles under the secure-boot feature"
    )
)]
pub(super) fn measure_handoff(
    config: &Config,
    generation: &Generation,
    verified: Option<&VerifiedGeneration>,
    cmdline: &str,
) -> Result<()> {
    if !measure_required(config) {
        return Ok(());
    }
    #[cfg(feature = "secure-boot")]
    {
        use crate::error::NmblError;
        use crate::tpm::measure;

        let refuse = |cause: NmblError| -> NmblError {
            NmblError::PolicyRefused {
                cause: Box::new(cause),
            }
        };

        // A measure-required boot MUST have verified inputs to measure (FIX-27):
        // no honest measurement exists for an unverified/audit-bypassed image.
        let Some(vg) = verified else {
            return Err(refuse(NmblError::TpmProto {
                context: "measure".to_string(),
                reason: "measure required but generation was not verified \
                         (refusing an unmeasured boot)"
                    .to_string(),
            }));
        };

        // #28 (Wave-4) supplies the ordered+hashed driver-image refs here; #27
        // accepts an empty slice (event #4 is then absent).
        let driver_images: &[measure::DriverImageRef] = &[];
        // Fail CLOSED on any TPM/measure error: a present-but-uncappable or
        // absent TPM on a measure-required build must refuse, never boot
        // silently unmeasured (FIX-27).
        measure::extend_handoff(
            config,
            generation,
            &vg.kernel_digest,
            &vg.initrd_digest,
            cmdline,
            driver_images,
        )
        .map_err(refuse)?;
        Ok(())
    }
    #[cfg(not(feature = "secure-boot"))]
    {
        // `tpm.measure` can be set on a non-secure-boot build, but the extend
        // path is secure-boot-gated; a measure-required boot here cannot be
        // honoured, so refuse rather than boot unmeasured.
        Err(crate::error::NmblError::PolicyRefused {
            cause: Box::new(crate::error::NmblError::TpmProto {
                context: "measure".to_string(),
                reason: "tpm.measure is set but this build lacks the secure-boot \
                         measure path (refusing an unmeasured boot)"
                    .to_string(),
            }),
        })
    }
}

/// (c) LOAD the verified image. On the secure-boot path the PINNED kernel fd
/// from `verified` is handed straight to `kexec_file_load(2)` (FIX-02 / MED-1)
/// so the loaded kernel is the exact one verified+measured; otherwise the
/// loader opens the kernel by path. The initrd is always combined with the
/// NMBL cpio `fragment` in a memfd (the typed passphrases + log transcript
/// never touch disk).
#[cfg_attr(
    not(feature = "secure-boot"),
    expect(
        unused_variables,
        reason = "verified is always None without the verify pipeline"
    )
)]
pub(super) fn load_handoff(
    generation: &Generation,
    verified: Option<VerifiedGeneration>,
    fragment: &[u8],
    cmdline: &str,
) -> Result<()> {
    #[cfg(feature = "secure-boot")]
    if let Some(vg) = verified {
        // Pin the verified fd into the load — no re-open by path (MED-1).
        return sys::kexec::load_with_kernel_fd_and_extra_initrd_cpio(
            &generation.kernel,
            vg.kernel_fd.as_fd(),
            &generation.initrd,
            fragment,
            cmdline,
            0,
        );
    }
    // Non-secure-boot / audit / signing-disabled: open the kernel by path.
    sys::kexec::load_with_extra_initrd_cpio(
        &generation.kernel,
        &generation.initrd,
        fragment,
        cmdline,
        0,
    )
}
