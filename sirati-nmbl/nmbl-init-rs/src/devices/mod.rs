//! Block-device readiness polling and system-root mount cascade.
//!
//! NMBL runs without udev, so PID 1 has to busy-poll until the path the
//! user named in `fileSystems[].device` actually appears. Replaces the
//! `while [ ! -b "$device" ]; do sleep 0.1; …; done` loop at the bottom
//! of `sirati-nmbl/scripts/mount-and-kernel.sh.nix`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{FileType, stat};

use nix::errno::Errno;

use crate::config::{Config, FilesystemEntry};
use crate::error::{NmblError, Result};
use crate::nmbl_info;
use crate::ui::{BootReporter, ProgressSink, TickOutcome};

/// Sleep granularity between polls. Matches the 100 ms cadence of the
/// shell loop this module replaces.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Poll for `device` to appear and be a block or character device.
///
/// Anything that is not `S_IFBLK` or `S_IFCHR` (including a transient
/// regular-file placeholder some udev-free setups create) counts as
/// "not yet" and we keep polling. Returns [`NmblError::DeviceTimeout`]
/// once the deadline passes. Exposed `pub` so `src/activation.rs` can
/// call it after LVM/cryptsetup produces a new device.
///
/// `operation` is a short verb plus phase context like
/// `"phase 3b: waiting for"` that gets joined with the device path to
/// build the boot-status phase string when `progress` is `Some`. The
/// spinner advances on every poll iteration so the operator sees the
/// boot is alive alongside the `Ns / Ms` countdown. Pass `progress =
/// None` to poll without driving a UI (tests, headless contexts).
pub fn wait_for(
    device: &Path,
    timeout: Duration,
    operation: &str,
    progress: Option<&mut dyn ProgressSink>,
) -> Result<()> {
    let start = Instant::now();
    let deadline = start.checked_add(timeout).unwrap_or_else(Instant::now);

    let mut progress = progress;

    loop {
        if device_ready(device) {
            return Ok(());
        }

        if Instant::now() >= deadline {
            return Err(NmblError::DeviceTimeout {
                device: device.to_path_buf(),
                timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
            });
        }

        // The sink's `tick` itself blocks ~POLL_INTERVAL on `poll_key`,
        // so when one is wired we let it provide the inter-poll cadence
        // (and an Esc-abort hook). Without a sink (headless / tests)
        // we still need the sleep so the loop doesn't busy-spin.
        if let Some(sink) = progress.as_deref_mut() {
            let elapsed = start.elapsed();
            let phase = format_wait_phase(operation, &device.display(), elapsed, timeout);
            if let TickOutcome::Aborted = sink.tick(&phase) {
                return Err(NmblError::OperatorAborted {
                    context: format!("waiting for {}", device.display()),
                });
            }
        } else {
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Build the canonical wait-status string used by every blocking wait
/// loop. Single source of truth so device-wait, activation-wait, and any
/// future polling loop produce the same `"<op> <target> (Ns / Ms)"`
/// shape — operators can grep one format instead of three.
///
/// The duration argument is rendered as whole seconds; sub-second
/// precision is noise on a 100 ms poll cadence.
pub fn format_wait_phase(
    operation: &str,
    target: &dyn std::fmt::Display,
    elapsed: Duration,
    timeout: Duration,
) -> String {
    format!(
        "{operation} {target} ({}s / {}s)",
        elapsed.as_secs(),
        timeout.as_secs(),
    )
}

/// `true` if `device` exists and is a block- or char-device node.
/// Stat failures collapse to `false` so the caller keeps polling
/// rather than aborting on a startup race.
fn device_ready(device: &Path) -> bool {
    match device.try_exists() {
        Ok(true) => {}
        Ok(false) | Err(_) => return false,
    }

    let Ok(st) = stat(device) else {
        return false;
    };

    matches!(
        FileType::from_raw_mode(st.st_mode as rustix::fs::RawMode),
        FileType::BlockDevice | FileType::CharacterDevice,
    )
}

/// Resolve the on-disk mountpoint for a single filesystem entry.
///
/// * `is_root=true` pins the target to `system_root` itself.
/// * Non-root absolute `mountpoint` already under `system_root` is
///   used as-is (lets configs spell out the post-pivot path).
/// * Anything else (relative, or absolute outside the root such as
///   the natural `/boot`) is joined with `system_root`.
///
/// Exposed `pub` so `src/boot.rs` resolves unmount targets via the
/// exact same logic — any drift would silently miss live mounts.
pub fn resolve_mountpoint(system_root: &Path, entry: &FilesystemEntry) -> PathBuf {
    if entry.is_root {
        return system_root.to_path_buf();
    }

    let mp = entry.mountpoint.as_path();
    if mp.is_absolute() && mp.starts_with(system_root) {
        return mp.to_path_buf();
    }

    if mp.is_absolute() {
        // Strip the leading `/` so `Path::join` doesn't replace the root.
        let stripped = mp.strip_prefix("/").unwrap_or(mp);
        return system_root.join(stripped);
    }

    system_root.join(mp)
}

/// Create `dir` (and parents) if absent. `create_dir_all` already
/// treats `AlreadyExists` as success; a regular file in the way still
/// surfaces as an error.
fn ensure_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).map_err(|source| NmblError::Io {
        source,
        context: format!("creating mountpoint directory {}", dir.display()),
    })
}

/// Wait for every configured device and mount each filesystem under
/// `config.paths.system_root` in declaration order. The root entry
/// (`is_root=true`) is mounted at `system_root` itself regardless of
/// its `mountpoint` field; other entries land beneath it per
/// [`resolve_mountpoint`].
///
/// `reporter` carries the live boot console; we surface the current
/// device / mountpoint as the boot-status phase label so the operator
/// sees what we're waiting on (especially when a slow device drags out
/// the configured per-device budget — see
/// [`crate::config::General::device_timeout_secs`]).
///
/// When Phase 0.5 (bootstrap) already mounted the boot partition at
/// `config.runtime_boot_mountpoint`, a second `mount(2)` of the same
/// block device to a different path returns `EBUSY`. In that case we
/// bind-mount from the bootstrap mountpoint so the system root sees the
/// boot partition at its expected location without remounting the device.
pub fn mount_system_filesystems(
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
) -> Result<()> {
    let system_root = config.paths.system_root.as_path();
    let device_timeout = Duration::from_secs(config.general.device_timeout_secs);
    ensure_dir(system_root)?;

    let _ = reporter.set_phase("phase 3b: scanning /dev/disk/by-* symlinks");
    // NMBL has no udev, so /dev/disk/by-{partlabel,label,uuid,partuuid}/
    // is empty unless we populate it ourselves. Do that BEFORE the
    // wait_for loop below — disko-style configs reference paths
    // under /dev/disk/by-*, and waiting for a symlink we'll never
    // create just burns the 30 s budget.
    let btrfs_devs = crate::sys::blkid::populate_disk_by_symlinks()?;

    // Issue BTRFS_IOC_SCAN_DEV on every btrfs member found by blkid so
    // the kernel assembles multi-device btrfs filesystems (RAID0/RAID1)
    // before mount(2) is called. Without this, mount fails with
    // "devid N uuid ... is missing" when only the first member is known.
    if !btrfs_devs.is_empty() {
        crate::sys::btrfs::scan_devices(&btrfs_devs)?;
    }

    for entry in &config.filesystems {
        let dev = Path::new(&entry.device);
        let _ = reporter.set_phase(format!(
            "phase 3b: waiting for {} -> {}",
            dev.display(),
            entry.mountpoint.display(),
        ));
        // Animate the wait so the operator sees the boot is alive (and an
        // "elapsed / timeout" countdown) instead of a frozen phase label.
        wait_for(
            dev,
            device_timeout,
            "phase 3b: waiting for",
            Some(&mut *reporter),
        )?;

        let target = resolve_mountpoint(system_root, entry);
        ensure_dir(&target)?;

        let _ = reporter.set_phase(format!(
            "phase 3b: mounting {} on {}",
            dev.display(),
            target.display(),
        ));
        match crate::sys::mount::mount_fs(Some(dev), &target, &entry.fstype, &entry.options) {
            Ok(()) => {}
            Err(NmblError::Mount {
                source: Errno::EBUSY,
                ..
            }) => {
                // The device is already mounted — most likely the boot
                // partition that Phase 0.5 mounted at
                // `runtime_boot_mountpoint`. Bind-mount from there so the
                // system root still gets the directory at the expected path.
                if let Some(bootstrap_mp) = &config.runtime_boot_mountpoint {
                    nmbl_info!(
                        "device {} already mounted (EBUSY); bind-mounting {} -> {}",
                        dev.display(),
                        bootstrap_mp.display(),
                        target.display(),
                    );
                    crate::sys::mount::mount_fs(
                        Some(bootstrap_mp.as_path()),
                        &target,
                        &entry.fstype,
                        "bind",
                    )?;
                } else {
                    // No bootstrap mountpoint to fall back to — propagate.
                    return Err(NmblError::Mount {
                        src: Some(dev.to_path_buf()),
                        dst: target,
                        fstype: entry.fstype.clone(),
                        source: Errno::EBUSY,
                    });
                }
            }
            Err(e) => return Err(e),
        }
    }

    nmbl_info!("system filesystems mounted under {}", system_root.display());
    Ok(())
}

#[cfg(test)]
mod tests;
