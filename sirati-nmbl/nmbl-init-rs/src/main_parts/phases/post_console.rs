use nmbl_init::activation::{KeyInjection, PasswordSupplier, run_all_activations};
use nmbl_init::config::Config;
use nmbl_init::devices::mount_system_filesystems;
use nmbl_init::error::Result;
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::sys::ops::SysOps;
use nmbl_init::ui::{BootReporter, SessionInteraction, SkipSelector};
use nmbl_init::{nmbl_info, nmbl_warn};

#[cfg(feature = "secure-boot")]
use nmbl_init::policy::GatePhase;

/// Feature-free stand-in for [`nmbl_init::policy::GatePhase`] so the two gate
/// hook call sites compile identically in the default build, where the gate is
/// a no-op (the `secure-boot` verifier is absent).
#[cfg(not(feature = "secure-boot"))]
#[derive(Clone, Copy)]
enum GatePhase {
    PrePlainBoot,
    PostUnlock,
}

/// Execute the post-console phases (2b, 3, 3b). The caller has already
/// opened the live console; we wrap it in a [`BootReporter`] so every
/// phase pushes its current "what am I doing" string through the
/// reporter and the operator sees progress on the splash framebuffer
/// or raw-mode tty. The reporter is dropped on return so the caller
/// can reuse the underlying console for the generation selector.
///
/// Returns the LUKS-passphrase injections that the kexec phase must
/// thread into the chained initrd (one per `luks-password` activation
/// whose TOML sets `pass_to_stage1`; empty when none opted in).
///
/// Takes `&mut Config` unconditionally so the staged-boot merge can mutate the
/// config in place; the non-staged phases only read it (auto-reborrowed as
/// `&Config` per call).
///
/// The module loads, blkid sweep, activations, and mount all route through the
/// `ops` [`SysOps`](nmbl_init::sys::ops::SysOps) seam so a dry-run impl can
/// no-op their side effects (`--validate-initrm`). `supplier` is supplied by the
/// caller (rather than built here) for the same reason: the dry-run substitutes
/// a scripted supplier that never touches the console. The priority-gate hooks +
/// staged-boot apply still carry the security params (`session`, `skip_selector`,
/// `sender`, `driver_images`).
#[allow(
    clippy::too_many_arguments,
    reason = "the post-console phase threads both the ops seam and the security \
              context (gate hooks + staged-boot); each is load-bearing"
)]
pub(crate) async fn run_phases_post_console<S: SysOps>(
    ops: &mut S,
    config: &mut Config,
    console: &mut dyn nmbl_init::ui::console::Console,
    supplier: &mut dyn PasswordSupplier,
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    sender: &nmbl_init::sys::poller::LocalSender,
    driver_images: &mut nmbl_init::imageload::DriverImagesHandle,
) -> Result<Vec<KeyInjection>> {
    let mut reporter = BootReporter::new(console, "phase 2b: loading kernel modules");
    // Paint the first frame so the operator sees a populated screen
    // before any work happens — otherwise a fast phase 2b would race
    // the first kmsg push and the log panel would be empty for one
    // frame. The pre-console phases already populated the log ring,
    // so the snapshot we pull here already shows phase 1 + 2a output.
    let _ = reporter.refresh_log();

    // Priority gate, hook #1 (FIX-34): the plain-boot-FS gate. Runs with the
    // live console up but before any interactive work, so a refuse returns
    // `Err(PolicyRefused)` that propagates to the `run_tui_session` Err arm —
    // the ONE shared refuse-render entry, NEVER the emergency shell (FIX-35).
    // The attested volume is dropped here (the plain boot FS owns no staged
    // set; staged-boot is consumed only at the post-unlock hook below).
    let _ = run_priority_gate_hook(
        ops,
        GatePhase::PrePlainBoot,
        config,
        &mut reporter,
        session,
        skip_selector,
        sender,
        driver_images,
    )
    .await?;

    nmbl_info!("phase 2b: load explicit kernel modules");
    load_explicit_modules(ops, config, &mut reporter)?;

    // The splash backend opens /dev/tty1 and calls VT_ACTIVATE itself
    // (see `splash::input::SplashInput::open`); the tty backend uses
    // `/dev/console`, which already points at the kernel-chosen VT.
    // Neither path needs an extra VT switch here.

    // Populate /dev/disk/by-{partlabel,label,uuid,partuuid} BEFORE storage
    // activations. NMBL has no udev, so a `luks-password` activation whose
    // `device` is a `/dev/disk/by-partlabel/...` path (the common disko
    // shape) would otherwise hand cryptsetup a non-existent path and fail
    // with exit 4. The external-config bootstrap (phase 0.5) already does
    // this, but the embedded-config path reaches activations first, so we
    // sweep blkid here too. `mount_system_filesystems` repeats the sweep
    // (idempotent) before the post-unlock mounts.
    let _ = reporter.set_phase("phase 2c: scanning /dev/disk/by-* symlinks");
    nmbl_info!("phase 2c: populating /dev/disk/by-* symlinks");
    if let Err(err) = ops.populate_disk_symlinks().await {
        nmbl_warn!("phase 2c: blkid sweep failed (continuing): {err}");
    }

    nmbl_info!("phase 3: storage activations");
    let mut injections =
        run_all_activations(ops, config, &mut reporter, Some(supplier), sender).await?;

    // Priority gate, hook #2 (FIX-34): the inside-LUKS gate. The storage
    // activations above opened the priority volume's backing mapper, so the
    // gate can now mount it read-only and verify its signed file. Same
    // deferred-refuse shape as hook #1 — a refuse surfaces as
    // `Err(PolicyRefused)` up through here into the shared Err arm (FIX-35).
    //
    // The attested-volume witness this hook yields is consumed by staged-boot
    // (#33): the SINGLE call site where a verified fragment is applied. It runs
    // here, AFTER the gate attests the volume, so staged-boot can never consume
    // an unverified volume (FIX-26); its extra key injections join the base set.
    let staged = run_priority_gate_hook(
        ops,
        GatePhase::PostUnlock,
        config,
        &mut reporter,
        session,
        skip_selector,
        sender,
        driver_images,
    )
    .await?;
    injections.extend(staged);

    nmbl_info!("phase 3b: mount system filesystems");
    mount_system_filesystems(ops, config, &mut reporter).await?;

    Ok(injections)
}

/// Invoke the priority gate at `phase`; for the post-unlock phase, consume the
/// attested-volume witness into staged-boot (#33), returning any key injections
/// the staged activations produced. A bad signature anywhere (the gate or the
/// staged apply) is an `Err(PolicyRefused)` routed to the shared refuse screen
/// (FIX-35). Behind `secure-boot`: the feature-free build has no gate to run.
#[cfg(feature = "secure-boot")]
#[cfg_attr(
    not(feature = "staged-boot"),
    allow(
        clippy::unused_async,
        reason = "the await lives in the staged-boot-gated branch; the seam stays async so the call site is uniform across features"
    )
)]
#[allow(
    clippy::too_many_arguments,
    reason = "threads the ops seam (gate verify + staged apply dry-run) alongside the security context"
)]
async fn run_priority_gate_hook<S: SysOps>(
    ops: &mut S,
    phase: GatePhase,
    config: &mut Config,
    reporter: &mut BootReporter<'_, '_>,
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    sender: &nmbl_init::sys::poller::LocalSender,
    driver_images: &mut nmbl_init::imageload::DriverImagesHandle,
) -> Result<Vec<KeyInjection>> {
    let Some(attested) = nmbl_init::policy::run_priority_gate_at(ops, phase, config)? else {
        return Ok(Vec::new());
    };
    #[cfg(feature = "staged-boot")]
    {
        // Only the inside-LUKS (post-unlock) phase carries the staged fragment;
        // the pre-plain-boot witness is dropped (its volume owns no staged set).
        // Staged-rerun driver images are appended to `driver_images` so they are
        // measured + torn down alongside the base set (#28 / LOW-B).
        if matches!(phase, GatePhase::PostUnlock) {
            return nmbl_init::staged::apply_staged_boot(
                ops,
                attested,
                config,
                reporter,
                session,
                skip_selector,
                sender,
                driver_images,
            )
            .await;
        }
    }
    // No staged-boot feature, or the pre-plain-boot phase: drop the witness.
    #[cfg(not(feature = "staged-boot"))]
    let _ = (reporter, session, skip_selector, sender, driver_images);
    drop(attested);
    Ok(Vec::new())
}

#[cfg(not(feature = "secure-boot"))]
#[allow(
    clippy::unused_async,
    reason = "mirrors the secure-boot async hook for a uniform call site"
)]
#[allow(
    clippy::too_many_arguments,
    reason = "mirrors the secure-boot hook signature for a uniform call site"
)]
async fn run_priority_gate_hook<S: SysOps>(
    _ops: &mut S,
    _phase: GatePhase,
    _config: &mut Config,
    _reporter: &mut BootReporter<'_, '_>,
    _session: &SessionInteraction,
    _skip_selector: &SkipSelector,
    _sender: &nmbl_init::sys::poller::LocalSender,
    _driver_images: &mut nmbl_init::imageload::DriverImagesHandle,
) -> Result<Vec<KeyInjection>> {
    Ok(Vec::new())
}
