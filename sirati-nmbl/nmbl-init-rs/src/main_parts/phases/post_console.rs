use nmbl_init::activation::{KeyInjection, run_all_activations};
use nmbl_init::config::Config;
use nmbl_init::devices::mount_system_filesystems;
use nmbl_init::error::Result;
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::sys::blkid;
use nmbl_init::ui::{BootReporter, SessionInteraction, SkipSelector, TuiPasswordSupplier};
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
/// Takes `&mut Config` unconditionally so a later staged-boot merge can
/// mutate the config in place without a signature change; the current
/// phases only read it (auto-reborrowed as `&Config` per call).
pub(crate) async fn run_phases_post_console(
    config: &mut Config,
    console: &mut dyn nmbl_init::ui::console::Console,
    session: &SessionInteraction,
    skip_selector: &SkipSelector,
    sender: &nmbl_init::sys::poller::LocalSender,
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
    // The attested volume is dropped here (the plain boot FS owns no mount);
    // #33's staged-boot apply consumes the witness in a later wave.
    run_priority_gate_hook(GatePhase::PrePlainBoot, config)?;

    nmbl_info!("phase 2b: load explicit kernel modules");
    load_explicit_modules(config, &mut reporter)?;

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
    if let Err(err) = blkid::populate_disk_by_symlinks(sender).await {
        nmbl_warn!("phase 2c: blkid sweep failed (continuing): {err}");
    }

    nmbl_info!("phase 3: storage activations");
    let mut supplier = TuiPasswordSupplier::new(config, session, skip_selector);
    let injections =
        run_all_activations(config, &mut reporter, Some(&mut supplier), sender).await?;

    // Priority gate, hook #2 (FIX-34): the inside-LUKS gate. The storage
    // activations above opened the priority volume's backing mapper, so the
    // gate can now mount it read-only and verify its signed file. Same
    // deferred-refuse shape as hook #1 — a refuse surfaces as
    // `Err(PolicyRefused)` up through here into the shared Err arm (FIX-35).
    run_priority_gate_hook(GatePhase::PostUnlock, config)?;

    nmbl_info!("phase 3b: mount system filesystems");
    mount_system_filesystems(config, &mut reporter, sender).await?;

    Ok(injections)
}

/// Invoke the priority gate at `phase` and DROP the attested volume. The
/// witness is consumed by #33's staged-boot apply (a later wave); until then
/// the gate is run purely for its refuse side-effect (a bad signature is an
/// `Err(PolicyRefused)` routed to the shared refuse screen). Behind
/// `secure-boot`: the feature-free build has no gate to run.
#[cfg(feature = "secure-boot")]
fn run_priority_gate_hook(phase: GatePhase, config: &Config) -> Result<()> {
    let _attested = nmbl_init::policy::run_priority_gate_at(phase, config)?;
    Ok(())
}

#[cfg(not(feature = "secure-boot"))]
fn run_priority_gate_hook(_phase: GatePhase, _config: &Config) -> Result<()> {
    Ok(())
}
