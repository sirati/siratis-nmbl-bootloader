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
use crate::ui::{BootReporter, ProgressSink};

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
    let deadline = start
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);

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

        if let Some(sink) = progress.as_deref_mut() {
            let elapsed = start.elapsed();
            let phase = format_wait_phase(operation, &device.display(), elapsed, timeout);
            sink.tick(&phase);
        }

        std::thread::sleep(POLL_INTERVAL);
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
/// the 30s budget).
pub fn mount_system_filesystems(
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
) -> Result<()> {
    let system_root = config.paths.system_root.as_path();
    ensure_dir(system_root)?;

    let _ = reporter.set_phase("phase 3b: scanning /dev/disk/by-* symlinks");
    // NMBL has no udev, so /dev/disk/by-{partlabel,label,uuid,partuuid}/
    // is empty unless we populate it ourselves. Do that BEFORE the
    // wait_for loop below — disko-style configs reference paths
    // under /dev/disk/by-*, and waiting for a symlink we'll never
    // create just burns the 30 s budget.
    crate::sys::blkid::populate_disk_by_symlinks()?;

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
            DEFAULT_DEVICE_TIMEOUT,
            "phase 3b: waiting for",
            Some(reporter as &mut dyn ProgressSink),
        )?;

        let target = resolve_mountpoint(system_root, entry);
        ensure_dir(&target)?;

        let _ = reporter.set_phase(format!(
            "phase 3b: mounting {} on {}",
            dev.display(),
            target.display(),
        ));
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

    /// Counting ProgressSink for tests. Records every call and the most
    /// recent phase string so we can assert both the cadence (~N ticks
    /// per second of wait) and the format of the status line.
    struct CountingSink {
        ticks: u32,
        last_phase: Option<String>,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                ticks: 0,
                last_phase: None,
            }
        }
    }

    impl ProgressSink for CountingSink {
        fn tick(&mut self, phase: &str) {
            self.ticks = self.ticks.saturating_add(1);
            self.last_phase = Some(phase.to_string());
        }
    }

    #[test]
    fn wait_for_missing_path_times_out() {
        let missing = Path::new("/nonexistent/path/nmbl-devices-test");
        let err = wait_for(missing, Duration::from_millis(200), "waiting for", None)
            .expect_err("missing path must time out");
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
        wait_for(dev_null, Duration::from_secs(1), "waiting for", None)
            .expect("/dev/null should be ready");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "wait_for(/dev/null) took {elapsed:?}, expected <1s",
        );
    }

    #[test]
    fn wait_for_ticks_progress_sink_during_wait() {
        // A 500 ms timeout on a 100 ms poll cadence should fire ~5 ticks
        // (give or take one for scheduler jitter). The exact bound is
        // intentionally loose — CI VMs run hot.
        let missing = Path::new("/nonexistent/nmbl-devices-tick-test");
        let mut sink = CountingSink::new();
        let _ = wait_for(
            missing,
            Duration::from_millis(500),
            "waiting for",
            Some(&mut sink),
        )
        .expect_err("missing path must time out");
        assert!(
            sink.ticks >= 2,
            "expected at least 2 ticks during a 500 ms wait, got {}",
            sink.ticks
        );
        assert!(
            sink.ticks <= 15,
            "expected at most 15 ticks during a 500 ms wait (defensive upper bound), got {}",
            sink.ticks
        );
    }

    #[test]
    fn wait_for_phase_string_includes_target_elapsed_and_timeout() {
        // Wait long enough for at least one whole-second tick to fire so
        // the elapsed counter increments off zero.
        let missing = Path::new("/nonexistent/nmbl-devices-phase-test");
        let mut sink = CountingSink::new();
        let _ = wait_for(
            missing,
            Duration::from_millis(1100),
            "phase 3b: waiting for",
            Some(&mut sink),
        )
        .expect_err("missing path must time out");

        let phase = sink
            .last_phase
            .as_deref()
            .expect("at least one tick must fire during a 1.1 s wait");
        assert!(
            phase.starts_with("phase 3b: waiting for"),
            "phase string must lead with the operation verb + phase context: {phase:?}"
        );
        assert!(
            phase.contains("nmbl-devices-phase-test"),
            "phase string must name the target device: {phase:?}"
        );
        assert!(
            phase.contains("/ 1s)"),
            "phase string must include timeout in seconds: {phase:?}"
        );
    }

    #[test]
    fn format_wait_phase_renders_canonical_shape() {
        // Lock the visible format so a downstream activation-wait caller
        // can rely on the exact string the operator greps for.
        let phase = format_wait_phase(
            "phase 3b: waiting for",
            &"/dev/disk/by-uuid/abc",
            Duration::from_secs(12),
            Duration::from_secs(30),
        );
        assert_eq!(
            phase,
            "phase 3b: waiting for /dev/disk/by-uuid/abc (12s / 30s)"
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
