use nmbl_init::activation::{KeyInjection, run_all_activations};
use nmbl_init::config::Config;
use nmbl_init::devices::mount_system_filesystems;
use nmbl_init::error::Result;
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::sys::blkid;
use nmbl_init::ui::{BootReporter, SessionInteraction, SkipSelector, TuiPasswordSupplier};
use nmbl_init::{nmbl_info, nmbl_warn};

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

    nmbl_info!("phase 3b: mount system filesystems");
    mount_system_filesystems(config, &mut reporter, sender).await?;

    Ok(injections)
}
