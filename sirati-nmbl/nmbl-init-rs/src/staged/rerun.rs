//! Re-run the modules/activations the merged config implies (#33, step e).
//!
//! After the transactional merge has swapped the fragment's tables into the base
//! `Config`, the runtime must apply the parts of that config the fragment may
//! have changed. Three effects, in the SAME order the post-console phase runs
//! them so the staged config behaves identically to a config loaded that way
//! from the start:
//!
//! 1. **Explicit kernel modules** — the fragment may add `[kernel_modules]`
//!    entries; re-loading is idempotent for already-loaded modules.
//! 2. **Driver images** — the (now-verified) staged driver squashfs blobs the
//!    fragment declared in `[driver_images]`, loaded through the #23 loader,
//!    which itself single-fd verifies every image under the driver-image domain
//!    before loop-mounting it (so a staged driver is held to the same crypto
//!    bar as a baseline one).
//! 3. **Activations** — any `[[activations]]` the fragment added, run through
//!    the same [`run_all_activations`] the base phase uses; their key injections
//!    are returned for the kexec path.
//!
//! Any step's error propagates up; the caller maps it to `PolicyRefused`. The
//! merge already validated the candidate, so these run against a known-good
//! config.

use crate::activation::{KeyInjection, run_all_activations};
use crate::config::Config;
use crate::error::Result;
use crate::imageload::{DriverImagesHandle, load_driver_images};
use crate::modules::load_explicit_modules;
use crate::nmbl_info;
use crate::sys::ops::SysOps;
use crate::sys::poller::LocalSender;
use crate::ui::{BootReporter, SessionInteraction, SkipSelector, TuiPasswordSupplier};

/// Apply the merged config's module/driver/activation effects and return the
/// staged activations' key injections.
///
/// # Errors
/// Propagates the first failing effect (module load, driver-image
/// verify/mount/load, or activation) so the caller refuses the boot.
#[allow(
    clippy::too_many_arguments,
    reason = "the staged re-run threads the ops seam alongside the security \
              context it shares with the base post-console phase"
)]
pub(super) async fn rerun_merged_effects<S: SysOps>(
    ops: &mut S,
    config: &mut Config,
    reporter: &mut BootReporter<'_, '_>,
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    sender: &LocalSender,
    driver_images: &mut DriverImagesHandle,
) -> Result<Vec<KeyInjection>> {
    // The staged re-run routes its module loads + activations through the SAME
    // `SysOps` seam the base post-console phase uses (rather than a hardcoded
    // `RealSys`), so `--validate-initrm` dry-runs the staged effects too.

    // (1) Explicit modules the fragment may have added.
    let _ = reporter.set_phase("staged-boot: re-loading explicit kernel modules");
    load_explicit_modules(ops, config, reporter)?;
    // Direct proof the merged-config modules ACTUALLY loaded (the set_phase
    // above only proves the phase was entered). Names the explicit list the
    // loader just walked — a fragment-added module (e.g. `dummy`) shows up here
    // only after `load_explicit_modules` returned Ok. Plain format over the
    // names, no unwrap/panic.
    let explicit = &config.kernel_modules.explicit;
    nmbl_info!(
        "staged rerun: loaded {} explicit module(s) [{}]",
        explicit.len(),
        explicit.join(", ")
    );

    // (2) The (now-verified) staged driver images. The loader single-fd verifies
    // every declared image under the driver-image domain before loop-mounting —
    // the staged drivers go through the exact same #23 verify as a baseline set.
    //
    // The loaded images are APPENDED to the shared `driver_images` accumulator
    // (LOW-B): that handle is the one the #24 hook tears down on the normal
    // pre-kexec terminus (and leaves mounted only on the capped-shell divert,
    // FIX-55), so a staged image is now registered for the SAME teardown as a
    // baseline one — no benign loop/mount leak across kexec. Their verified
    // refs also join the PCR-11 measure event #4 (#28), so a staged driver is
    // measured exactly like a baseline one.
    let _ = reporter.set_phase("staged-boot: loading staged driver images");
    let staged_handle = load_driver_images(ops, config)?;
    for image in staged_handle.images() {
        driver_images.push(image.clone());
    }

    // (3) Activations the fragment added (e.g. a new LUKS volume the staged
    // drivers expose). Run through the same activation runner as the base phase
    // so a staged passphrase modal renders identically; return the injections
    // for the kexec path. A config with no activations is a cheap no-op.
    let _ = reporter.set_phase("staged-boot: running staged activations");
    let mut supplier = TuiPasswordSupplier::new(config, session, skip_selector);
    run_all_activations(ops, config, reporter, Some(&mut supplier), sender).await
}
