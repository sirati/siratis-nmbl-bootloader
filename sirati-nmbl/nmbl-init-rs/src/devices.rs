//! Block-device readiness polling and system-root mount cascade.
//!
//! NMBL runs without udev, so PID 1 has to busy-poll until the path the
//! user named in `fileSystems[].device` actually appears. Replaces the
//! `while [ ! -b "$device" ]; do sleep 0.1; …; done` loop at the bottom
//! of `sirati-nmbl/scripts/mount-and-kernel.sh.nix`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustix::fs::{FileType, stat};

use crate::config::{Config, FilesystemEntry};
use crate::error::{NmblError, Result};
use crate::nmbl_info;

/// Default per-device readiness deadline used by
/// [`mount_system_filesystems`]. Held here (not in `Config`) until the
/// schema grows a dedicated knob.
const DEFAULT_DEVICE_TIMEOUT: Duration = Duration::from_secs(30);

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
pub fn wait_for(device: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);

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

        std::thread::sleep(POLL_INTERVAL);
    }
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
pub fn mount_system_filesystems(config: &Config) -> Result<()> {
    let system_root = config.paths.system_root.as_path();
    ensure_dir(system_root)?;

    for entry in &config.filesystems {
        let dev = Path::new(&entry.device);
        wait_for(dev, DEFAULT_DEVICE_TIMEOUT)?;

        let target = resolve_mountpoint(system_root, entry);
        ensure_dir(&target)?;

        crate::sys::mount::mount_fs(Some(dev), &target, &entry.fstype, &entry.options)?;
    }

    nmbl_info!("system filesystems mounted under {}", system_root.display());
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fs_entry(device: &str, mountpoint: &str, is_root: bool) -> FilesystemEntry {
        FilesystemEntry {
            device: device.to_string(),
            mountpoint: PathBuf::from(mountpoint),
            fstype: "ext4".to_string(),
            options: String::new(),
            is_root,
        }
    }

    #[test]
    fn wait_for_missing_path_times_out() {
        let missing = Path::new("/nonexistent/path/nmbl-devices-test");
        let err =
            wait_for(missing, Duration::from_millis(200)).expect_err("missing path must time out");
        match err {
            NmblError::DeviceTimeout { device, timeout_ms } => {
                assert_eq!(device, missing.to_path_buf());
                assert_eq!(timeout_ms, 200);
            }
            other => panic!("expected DeviceTimeout, got {other:?}"),
        }
    }

    #[test]
    fn wait_for_dev_null_returns_quickly() {
        let dev_null = Path::new("/dev/null");
        if !dev_null.exists() {
            eprintln!("skipping: /dev/null missing in this sandbox");
            return;
        }
        let start = Instant::now();
        wait_for(dev_null, Duration::from_secs(1)).expect("/dev/null should be ready");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "wait_for(/dev/null) took {elapsed:?}, expected <1s",
        );
    }

    #[test]
    fn resolve_mountpoint_is_root_overrides() {
        let root = PathBuf::from("/mnt/system");
        let entry = fs_entry("/dev/sda1", "/whatever", true);
        assert_eq!(resolve_mountpoint(&root, &entry), root);
    }

    #[test]
    fn resolve_mountpoint_relative_is_joined() {
        let root = PathBuf::from("/mnt/system");
        let entry = fs_entry("/dev/sda1", "boot", false);
        assert_eq!(
            resolve_mountpoint(&root, &entry),
            PathBuf::from("/mnt/system/boot"),
        );
    }

    #[test]
    fn resolve_mountpoint_absolute_already_under_root_kept() {
        let root = PathBuf::from("/mnt/system");
        let entry = fs_entry("/dev/sda1", "/mnt/system/boot", false);
        assert_eq!(
            resolve_mountpoint(&root, &entry),
            PathBuf::from("/mnt/system/boot"),
        );
    }

    #[test]
    fn resolve_mountpoint_absolute_not_under_root_joined() {
        let root = PathBuf::from("/mnt/system");
        let entry = fs_entry("/dev/sda1", "/boot", false);
        assert_eq!(
            resolve_mountpoint(&root, &entry),
            PathBuf::from("/mnt/system/boot"),
        );
    }
}
