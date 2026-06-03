use std::path::Path;

use nmbl_init::config::{BootstrapConfig, Config, resolve_full_config_path};
use nmbl_init::error::{NmblError, Result};
use nmbl_init::modules::load_early_modules;
use nmbl_init::mount::mount_pseudo_filesystems;
use nmbl_init::nmbl_info;
use nmbl_init::sys::ops::{FsOps, ModuleOps, SysOps};
use nmbl_init::ui::BootReporter;

mod post_console;

pub(crate) use post_console::run_phases_post_console;

/// Phase 1: mount /proc, /sys, /dev. Lives at the top of `main` so the
/// optional bootstrap phase (0.5) can see those pseudo-filesystems
/// before it touches blkid or mounts the boot partition. Uses a
/// [`NoopConsole`] sentinel because the real console is not open yet.
pub(super) fn run_phase_1(ops: &mut impl FsOps, reporter: &mut BootReporter<'_, '_>) -> Result<()> {
    nmbl_info!("phase 1: mount pseudo-filesystems");
    mount_pseudo_filesystems(ops, reporter)
}

/// Phase 2a: load early (graphics) kernel modules so the splash backend
/// has a DRM card to attach to when `open_console` runs. Reads
/// `config.kernel_modules.early`. The reporter wraps a [`NoopConsole`];
/// status pushes do nothing visible, but the underlying log ring is
/// still populated for the post-console reporter to surface.
pub(super) fn run_phase_2a(
    ops: &mut impl ModuleOps,
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
) -> Result<()> {
    let _ = reporter.set_phase("phase 2a: load early kernel modules");
    nmbl_info!("phase 2a: load early kernel modules");
    load_early_modules(ops, config, reporter)
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
///
/// Async because the blkid sweep reaps each child through the poller's
/// non-blocking `waitpid` op rather than blocking the runtime thread.
/// Runs inside the interactive [`crate::ui::block_on_tui_with_poller`]
/// region; `module`/`mount` syscalls below are atomic and stay
/// synchronous within the async region.
pub(super) async fn run_bootstrap_phase<S: SysOps>(
    ops: &mut S,
    bootstrap_path: &Path,
) -> Result<Config> {
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
    ops.load_modules(
        &section.kernel_modules.modules_dir,
        &section.kernel_modules.explicit,
        &[],
    )
    .map_err(|source| NmblError::Bootstrap {
        stage: "load-modules",
        source: Box::new(source),
    })?;

    nmbl_info!("phase 0.5: populating /dev/disk/by-* symlinks");
    ops.populate_disk_symlinks()
        .await
        .map_err(|source| NmblError::Bootstrap {
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
    ops.ensure_dir(&boot_fs.mountpoint)
        .map_err(|source| NmblError::Bootstrap {
            stage: "mount-boot",
            source: Box::new(NmblError::Io {
                source,
                context: format!("creating boot mountpoint {}", boot_fs.mountpoint.display()),
            }),
        })?;
    ops.mount(
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
pub(super) fn mount_state_twin(
    ops: &mut impl FsOps,
    config: &mut Config,
    bootstrap_path: &Path,
) -> Result<()> {
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
    ops.ensure_dir(mp).map_err(|source| NmblError::Bootstrap {
        stage: "mount-state",
        source: Box::new(NmblError::Io {
            source,
            context: format!("creating state mountpoint {}", mp.display()),
        }),
    })?;
    ops.mount(Some(&boot_fs.mountpoint), mp, &boot_fs.fstype, "bind")
        .map_err(|source| NmblError::Bootstrap {
            stage: "mount-state",
            source: Box::new(source),
        })?;
    config.runtime_state_mountpoint = Some(mp.clone());
    Ok(())
}
