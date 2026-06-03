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
use crate::imageload::load_driver_images;
use crate::modules::load_explicit_modules;
use crate::sys::poller::LocalSender;
use crate::ui::{BootReporter, SessionInteraction, SkipSelector, TuiPasswordSupplier};

/// Apply the merged config's module/driver/activation effects and return the
/// staged activations' key injections.
///
/// # Errors
/// Propagates the first failing effect (module load, driver-image
/// verify/mount/load, or activation) so the caller refuses the boot.
pub(super) async fn rerun_merged_effects(
    config: &mut Config,
    reporter: &mut BootReporter<'_, '_>,
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    sender: &LocalSender,
) -> Result<Vec<KeyInjection>> {
    // (1) Explicit modules the fragment may have added.
    let _ = reporter.set_phase("staged-boot: re-loading explicit kernel modules");
    load_explicit_modules(config, reporter)?;

    // (2) The (now-verified) staged driver images. The loader single-fd verifies
    // every declared image under the driver-image domain before loop-mounting —
    // the staged drivers go through the exact same #23 verify as a baseline set.
    // The handle is intentionally not torn down here: like the baseline images
    // it stays mounted into kexec (the kexec teardown owns the unmount), and
    // driver images carry no secrets (FIX-55).
    let _ = reporter.set_phase("staged-boot: loading staged driver images");
    let _driver_handle = load_driver_images(config)?;

    // (3) Activations the fragment added (e.g. a new LUKS volume the staged
    // drivers expose). Run through the same activation runner as the base phase
    // so a staged passphrase modal renders identically; return the injections
    // for the kexec path. A config with no activations is a cheap no-op.
    let _ = reporter.set_phase("staged-boot: running staged activations");
    let mut supplier = TuiPasswordSupplier::new(config, session, skip_selector);
    run_all_activations(config, reporter, Some(&mut supplier), sender).await
}
