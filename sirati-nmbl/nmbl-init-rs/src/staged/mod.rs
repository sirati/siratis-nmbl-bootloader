//! Signed staged-boot fragment apply (#33 — FIX-26/FIX-32/FIX-33).
//!
//! After the priority gate (#31) attests the first volume it hands an
//! unforgeable [`AttestedVolume`] witness to [`apply_staged_boot`]. Only a
//! passed gate can produce that witness, so the staged path is structurally
//! unreachable on an unverified volume (FIX-26). Under that attested mount this
//! module:
//!
//! 1. **Verifies, single-fd, BOTH** the staged image (the squashfs carrying the
//!    extra drivers) AND the signed config fragment — each over its OWN pinned
//!    fd, against the baked trust anchor: the image under
//!    [`crate::sig::DOMAIN_DRIVER_IMAGE`], the fragment under
//!    [`crate::sig::DOMAIN_STAGED_FRAGMENT`] (FIX-01 domain separation, so a
//!    signature minted for any other role cannot verify here). The verify
//!    result is mapped through the operator's enforce/audit posture
//!    ([`crate::sig::apply_policy`]) — there is NO allow-unsigned fork (FIX-04).
//! 2. **Loads** the fragment ([`crate::config::load_fragment`]).
//! 3. **Transactionally merges** the fragment into the base `Config`
//!    ([`merge::merge_fragment`], FIX-32): build the merged candidate, validate
//!    it in full, and only keep the swap on success — on ANY failure the base
//!    `config` is left BYTE-FOR-BYTE untouched (no partial apply).
//! 4. **Re-runs the effects** the merged config implies: the explicit kernel
//!    modules, the (now-verified) staged driver images via the #23 loader, and
//!    any activations the fragment added — threading their key injections back
//!    to the kexec path.
//!
//! ## Failure ⇒ refuse against the PRISTINE base (R-1 / FIX-35)
//!
//! ANY failure — a bad staged-image or fragment signature, an unparseable
//! fragment, a merge that fails validation, or a re-run step that errors — is
//! surfaced as [`NmblError::PolicyRefused`]. Because the merge is transactional
//! the `config` is the UNMODIFIED base at that point, so the shared
//! `run_tui_session` Err arm renders the refuse / relock against
//! `config_before`: it caps the lock PCR, closes the TPM-unsealed mappers,
//! relocks, writes the sentinel, and reboots into rescue (R-1). The refuse is
//! NEVER taken inline and NEVER drops to the emergency shell (FIX-35) — exactly
//! the priority gate's deferral shape.
//!
//! ## The fragment cannot relax policy (FIX-53)
//!
//! [`crate::config::ConfigFragment`] OMITS the `[signing]`, `[secure_boot]` and
//! `[staged]` tables by construction (its `deny_unknown_fields` decode rejects
//! any such key as a hard parse error). There is therefore NO field through
//! which a fragment could disable signature enforcement, widen the trust
//! anchor, or re-point the staged source it was itself loaded through. This
//! module never reads a staged source from anything but the BASE config's
//! `[staged]` table, so even a hostile-but-valid fragment cannot redirect a
//! later load.

#[cfg(feature = "staged-boot")]
mod merge;

#[cfg(all(test, feature = "staged-boot"))]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests;

#[cfg(feature = "staged-boot")]
use crate::activation::KeyInjection;
#[cfg(feature = "staged-boot")]
use crate::config::Config;
#[cfg(feature = "staged-boot")]
use crate::error::Result;
#[cfg(feature = "staged-boot")]
use crate::policy::AttestedVolume;
#[cfg(feature = "staged-boot")]
use crate::sys::poller::LocalSender;
#[cfg(feature = "staged-boot")]
use crate::ui::{BootReporter, SessionInteraction, SkipSelector};

/// Apply the signed staged-boot fragment under the attested volume (#33).
///
/// Consumes the [`AttestedVolume`] witness BY VALUE (FIX-26): the volume's mount
/// lifetime is held for the duration of the verify + load, then dropped. Takes
/// `&mut Config` UNCONDITIONALLY regardless of feature (FIX-33), so the seam is
/// borrow-stable across builds.
///
/// Returns the additional [`KeyInjection`]s any staged activation produced, for
/// the caller to thread into the kexec'd initrd alongside the base injections.
///
/// A no-op returning `Ok(Vec::new())` when staged-boot is disabled (the
/// `staged-boot` feature off, or `[staged].enable = false`, or no `[staged]`
/// table): the base boot continues unchanged.
///
/// # Errors
/// Returns [`NmblError::PolicyRefused`](crate::error::NmblError::PolicyRefused)
/// on ANY failure (bad signature, unparseable fragment, merge-validation
/// failure, or a failed re-run). The transactional merge guarantees `config` is
/// the PRISTINE base at that point, so the caller's shared refuse render relocks
/// against `config_before` (FIX-32/FIX-35).
#[cfg(feature = "staged-boot")]
pub async fn apply_staged_boot(
    attested: AttestedVolume,
    config: &mut Config,
    reporter: &mut BootReporter<'_, '_>,
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    sender: &LocalSender,
) -> Result<Vec<KeyInjection>> {
    // `[staged]` disabled / absent ⇒ nothing to do (the attested volume is
    // dropped here, releasing its mount).
    if !staged_boot_enabled(config) {
        drop(attested);
        return Ok(Vec::new());
    }
    if let Some(staged) = config.staged.as_ref() {
        crate::nmbl_info!(
            "staged-boot: applying signed fragment {} (image {})",
            staged.fragment.display(),
            staged.image.display()
        );
    }

    // Everything below either succeeds or is funnelled to a single refuse:
    // collect the result and map a hard error to PolicyRefused so the merge's
    // transactional guarantee (base untouched) backs the refuse against the
    // pristine config (FIX-32/FIX-35).
    match apply_inner(&attested, config, reporter, session, skip_selector, sender).await {
        Ok(injections) => {
            crate::nmbl_info!("staged-boot: fragment applied; merged config in effect");
            Ok(injections)
        }
        Err(cause) => Err(crate::error::NmblError::PolicyRefused {
            cause: Box::new(cause),
        }),
    }
}

/// Whether staged-boot should run for this config: a present `[staged]` table
/// with `enable = true`. Factored out so the disabled-no-op short-circuit is the
/// EXACT predicate the tests pin (an absent table or `enable = false` is a
/// no-op).
#[cfg(feature = "staged-boot")]
#[must_use]
pub(super) fn staged_boot_enabled(config: &Config) -> bool {
    config.staged.as_ref().is_some_and(|s| s.enable)
}

/// The fallible body: verify both blobs single-fd, merge transactionally, then
/// re-run the merged config's effects. Any `Err` here leaves `config` pristine
/// (the merge is the only mutation, and it rolls itself back on a validate
/// failure — every other step runs either before the merge or on the already
/// merged-and-validated config).
#[cfg(feature = "staged-boot")]
async fn apply_inner(
    attested: &AttestedVolume,
    config: &mut Config,
    reporter: &mut BootReporter<'_, '_>,
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    sender: &LocalSender,
) -> Result<Vec<KeyInjection>> {
    // (a) SINGLE-fd verify the staged image AND the fragment — each over its
    // own pinned fd, before anything is loaded or merged.
    verify::verify_staged_blobs(attested, config)?;

    // (b)+(c) load the fragment, then TRANSACTIONALLY merge it into the base
    // config (validate-then-swap; base untouched on a validate failure).
    let fragment_path = verify::resolve_fragment_path(attested, config);
    let fragment = crate::config::load_fragment(&fragment_path)?;
    merge::merge_fragment(config, fragment)?;

    // (d) re-run the modules/activations the merged config now implies.
    rerun::rerun_merged_effects(config, reporter, session, skip_selector, sender).await
}

#[cfg(feature = "staged-boot")]
mod verify;

#[cfg(feature = "staged-boot")]
mod rerun;
