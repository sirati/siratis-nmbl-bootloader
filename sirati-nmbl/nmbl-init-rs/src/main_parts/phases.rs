use std::path::Path;

use nmbl_init::activation::{KeyInjection, run_all_activations};
use nmbl_init::config::{BootstrapConfig, Config, resolve_full_config_path};
use nmbl_init::devices::mount_system_filesystems;
use nmbl_init::error::{NmblError, Result};
use nmbl_init::modules::{load_early_modules, load_explicit_modules, load_modules};
use nmbl_init::mount::mount_pseudo_filesystems;
use nmbl_init::sys::{blkid, mount as sys_mount};
use nmbl_init::ui::{BootReporter, SessionInteraction, TuiPasswordSupplier};
use nmbl_init::{nmbl_info, nmbl_warn};

/// Phase 1: mount /proc, /sys, /dev. Lives at the top of `main` so the
/// optional bootstrap phase (0.5) can see those pseudo-filesystems
/// before it touches blkid or mounts the boot partition. Uses a
/// [`NoopConsole`] sentinel because the real console is not open yet.
pub(super) fn run_phase_1(reporter: &mut BootReporter<'_, '_>) -> Result<()> {
    nmbl_info!("phase 1: mount pseudo-filesystems");
    mount_pseudo_filesystems(reporter)
}

/// Phase 2a: load early (graphics) kernel modules so the splash backend
/// has a DRM card to attach to when `open_console` runs. Reads
/// `config.kernel_modules.early`. The reporter wraps a [`NoopConsole`];
/// status pushes do nothing visible, but the underlying log ring is
/// still populated for the post-console reporter to surface.
pub(super) fn run_phase_2a(config: &Config, reporter: &mut BootReporter<'_, '_>) -> Result<()> {
    let _ = reporter.set_phase("phase 2a: load early kernel modules");
    nmbl_info!("phase 2a: load early kernel modules");
    load_early_modules(config, reporter)
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
pub(super) async fn run_phases_post_console(
    config: &Config,
    console: &mut dyn nmbl_init::ui::console::Console,
    session: &SessionInteraction,
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
    if let Err(err) = blkid::populate_disk_by_symlinks() {
        nmbl_warn!("phase 2c: blkid sweep failed (continuing): {err}");
    }

    nmbl_info!("phase 3: storage activations");
    let mut supplier = TuiPasswordSupplier::new(config, session);
    let injections = run_all_activations(config, &mut reporter, Some(&mut supplier)).await?;

    nmbl_info!("phase 3b: mount system filesystems");
    mount_system_filesystems(config, &mut reporter)?;

    Ok(injections)
}

/// Phase 0.5: two-tier bootstrap. Loads the embedded
/// `/etc/nmbl/bootstrap.toml`, brings up the minimum kernel modules it
/// names, sweeps blkid to populate `/dev/disk/by-*`, mounts the boot
/// filesystem, and reads the full `Config` from there.
///
/// On any failure the returned `NmblError::Bootstrap` carries a `stage`
/// string the emergency-shell banner surfaces. Once `boot_fs` is
/// mounted we intentionally leave it mounted on the error path so the
/// operator's shell still sees it under `bootstrap.boot_fs.mountpoint`.
pub(super) fn run_bootstrap_phase(bootstrap_path: &Path) -> Result<Config> {
    nmbl_info!(
        "phase 0.5: loading bootstrap config {}",
        bootstrap_path.display()
    );
    let bootstrap = BootstrapConfig::load(bootstrap_path)?;
    let section = &bootstrap.bootstrap;

    nmbl_info!(
        "phase 0.5: loading {} bootstrap kernel modules from {}",
        section.kernel_modules.explicit.len(),
        section.kernel_modules.modules_dir.display(),
    );
    load_modules(
        &section.kernel_modules.modules_dir,
        &section.kernel_modules.explicit,
        &[],
    )
    .map_err(|source| NmblError::Bootstrap {
        stage: "load-modules",
        source: Box::new(source),
    })?;

    nmbl_info!("phase 0.5: populating /dev/disk/by-* symlinks");
    blkid::populate_disk_by_symlinks().map_err(|source| NmblError::Bootstrap {
        stage: "blkid-sweep",
        source: Box::new(source),
    })?;

    let boot_fs = &section.boot_fs;
    // When stateful storage is configured the runtime needs to rewrite
    // `state.bin` on this same device. We mount the boot fs read-write
    // ONCE here and later bind a writable view at the state mountpoint;
    // mounting the same block device twice fails with EBUSY on vfat. The
    // operator's `ro` default is only honoured when no state mount is
    // configured.
    let stateful_rw = section.state.is_some();
    let boot_options = if stateful_rw {
        if boot_fs.options.is_empty() {
            "rw,nosuid,noexec,nodev".to_string()
        } else {
            format!("{},rw,nosuid,noexec,nodev", boot_fs.options)
        }
    } else {
        boot_fs.options.clone()
    };
    nmbl_info!(
        "phase 0.5: mounting boot fs {} at {} (type {}, options {})",
        boot_fs.device,
        boot_fs.mountpoint.display(),
        boot_fs.fstype,
        boot_options,
    );
    std::fs::create_dir_all(&boot_fs.mountpoint).map_err(|source| NmblError::Bootstrap {
        stage: "mount-boot",
        source: Box::new(NmblError::Io {
            source,
            context: format!("creating boot mountpoint {}", boot_fs.mountpoint.display()),
        }),
    })?;
    sys_mount::mount_fs(
        Some(Path::new(&boot_fs.device)),
        &boot_fs.mountpoint,
        &boot_fs.fstype,
        &boot_options,
    )
    .map_err(|source| NmblError::Bootstrap {
        stage: "mount-boot",
        source: Box::new(source),
    })?;

    // boot_fs is mounted; from here on, any failure must NOT unmount
    // it — the operator's emergency shell needs to see it.
    let full_path = resolve_full_config_path(&boot_fs.mountpoint, &section.config_path);
    nmbl_info!(
        "phase 0.5: loading full config from {}",
        full_path.display()
    );
    let mut config = Config::load(&full_path).map_err(|source| NmblError::Bootstrap {
        stage: "read-config",
        source: Box::new(source),
    })?;

    // Hand the runtime boot mountpoint to the rescue dispatcher so
    // `rescue::locate_sfs` can resolve `sfs_path` against it instead of
    // the build-time `/boot` convention. This must be set before
    // `run_inner` evaluates the `force_on_boot` rescue trigger, which
    // only needs the boot mount — never the stateful state bind below.
    config.runtime_boot_mountpoint = Some(boot_fs.mountpoint.clone());

    Ok(config)
}

/// Stateful side of Phase 0.5: expose a writable view of the boot fs at
/// `state.mountpoint` so `state.bin` can be rewritten between boots. The
/// boot device is already mounted read-write at `boot_fs.mountpoint` by
/// [`run_bootstrap_phase`] when stateful is enabled; we `MS_BIND` that
/// mount here rather than mounting the block device a second time, which
/// fails with EBUSY on vfat. A bind shares the existing RW mount, so the
/// state view is writable without re-opening the device.
///
/// Split out of `run_bootstrap_phase` so `run_inner` can evaluate the
/// `force_on_boot` rescue trigger BEFORE this mount runs: the force path
/// skips generation boot entirely, so it never touches `state.bin` and
/// must not be blocked by a state-mount failure.
#[cfg(feature = "stateful")]
pub(super) fn mount_state_twin(config: &mut Config, bootstrap_path: &Path) -> Result<()> {
    let bootstrap = BootstrapConfig::load(bootstrap_path)?;
    let section = &bootstrap.bootstrap;
    let boot_fs = &section.boot_fs;
    let Some(state_mount) = &section.state else {
        return Ok(());
    };
    let mp = &state_mount.mountpoint;
    nmbl_info!(
        "phase 0.5: bind-mounting {} at {} for state.bin",
        boot_fs.mountpoint.display(),
        mp.display(),
    );
    std::fs::create_dir_all(mp).map_err(|source| NmblError::Bootstrap {
        stage: "mount-state",
        source: Box::new(NmblError::Io {
            source,
            context: format!("creating state mountpoint {}", mp.display()),
        }),
    })?;
    sys_mount::mount_fs(Some(&boot_fs.mountpoint), mp, &boot_fs.fstype, "bind").map_err(
        |source| NmblError::Bootstrap {
            stage: "mount-state",
            source: Box::new(source),
        },
    )?;
    config.runtime_state_mountpoint = Some(mp.clone());
    Ok(())
}
