//! Classify why a generation scan found nothing.
//!
//! The bare "no generations found" error is useless to an operator who
//! reached the emergency screen, hand-mounted a filesystem, and re-ran
//! "Verify kexec readiness". The real cause is almost always one of:
//!
//!   1. nothing is mounted at the system-root mountpoint at all, or
//!   2. *something* is mounted there but it's the wrong filesystem /
//!      mounted at the wrong place, so the `nix/var/nix/profiles`
//!      directory NMBL needs simply isn't present.
//!
//! [`classify_scan_failure`] distinguishes those from the genuine
//! "mounted, readable, but empty" case and returns the matching
//! [`NmblError`] variant so the banner can print actionable guidance.

use std::path::Path;

use crate::error::NmblError;

/// Decide whether `path` is itself a mount point.
///
/// Uses the classic device-number test: a directory whose `st_dev`
/// differs from its parent's `st_dev` is the root of a mounted
/// filesystem. Done with `rustix::fs::stat` (no raw `unsafe`); falls
/// back to "not a mount point" on any stat error, which keeps the
/// caller's classification conservative (it will say "nothing mounted"
/// rather than mis-blame the layout).
pub(super) fn is_mountpoint(path: &Path) -> bool {
    let Ok(here) = rustix::fs::stat(path) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        // No parent (path is "/"): the root filesystem is always mounted.
        return true;
    };
    // A relative path with no parent component ("foo") — treat the
    // current directory as the parent.
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    match rustix::fs::stat(parent) {
        Ok(up) => here.st_dev != up.st_dev,
        Err(_) => false,
    }
}

/// Map a failed/empty profiles-dir scan onto the most specific
/// [`NmblError`] variant, given whether the system-root mountpoint is
/// actually a mount.
///
/// `mounted` is injected (rather than calling [`is_mountpoint`]
/// directly) so the classification is unit-testable without real
/// mounts: tests drive the three branches by toggling `mounted` and the
/// on-disk presence of `profiles_dir`.
///
/// Decision table (only reached once the scan has found zero
/// generations):
///
/// | profiles_dir exists | mountpoint mounted | result               |
/// |---------------------|--------------------|----------------------|
/// | no                  | no                 | SystemRootNotMounted |
/// | no                  | yes                | ProfilesDirMissing   |
/// | yes                 | (either)           | NoGenerations        |
pub(super) fn classify_scan_failure(
    profiles_dir: &Path,
    mountpoint: &Path,
    mounted: bool,
) -> NmblError {
    let dir_present = profiles_dir.try_exists().unwrap_or(false);
    if dir_present {
        // The directory is there and we got far enough to read it (or
        // tried to) but no `system-N-link` entries qualified.
        return NmblError::NoGenerations {
            searched: profiles_dir.to_path_buf(),
        };
    }
    if mounted {
        // A filesystem is mounted at the system root, but it does not
        // contain the profiles directory — wrong filesystem / wrong
        // mountpoint.
        NmblError::ProfilesDirMissing {
            path: profiles_dir.to_path_buf(),
            mountpoint: mountpoint.to_path_buf(),
        }
    } else {
        // Nothing is mounted at the expected system root.
        NmblError::SystemRootNotMounted {
            mountpoint: mountpoint.to_path_buf(),
        }
    }
}

/// Convenience wrapper used by the scanner: classify using the live
/// [`is_mountpoint`] probe.
pub(super) fn classify_scan_failure_live(profiles_dir: &Path, mountpoint: &Path) -> NmblError {
    classify_scan_failure(profiles_dir, mountpoint, is_mountpoint(mountpoint))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn not_mounted_and_missing_dir_is_system_root_not_mounted() {
        let mp = PathBuf::from("/mnt/system");
        let profiles = PathBuf::from("/mnt/system/nix/var/nix/profiles");
        match classify_scan_failure(&profiles, &mp, false) {
            NmblError::SystemRootNotMounted { mountpoint } => assert_eq!(mountpoint, mp),
            other => panic!("expected SystemRootNotMounted, got {other:?}"),
        }
    }

    #[test]
    fn mounted_but_missing_dir_is_profiles_dir_missing() {
        let mp = PathBuf::from("/mnt/system");
        let profiles = PathBuf::from("/mnt/system/nix/var/nix/profiles");
        match classify_scan_failure(&profiles, &mp, true) {
            NmblError::ProfilesDirMissing { path, mountpoint } => {
                assert_eq!(path, profiles);
                assert_eq!(mountpoint, mp);
            }
            other => panic!("expected ProfilesDirMissing, got {other:?}"),
        }
    }

    #[test]
    fn existing_empty_dir_is_no_generations() {
        // The profiles dir is present on disk; regardless of the
        // mount-probe result, an existing-but-empty dir means
        // NoGenerations (the scan read it and found nothing).
        let tmp = TempDir::new().expect("temp dir");
        let profiles = tmp.path().to_path_buf();
        for mounted in [false, true] {
            match classify_scan_failure(&profiles, tmp.path(), mounted) {
                NmblError::NoGenerations { searched } => assert_eq!(searched, profiles),
                other => panic!("expected NoGenerations (mounted={mounted}), got {other:?}"),
            }
        }
    }

    #[test]
    fn is_mountpoint_true_for_root() {
        assert!(
            is_mountpoint(Path::new("/")),
            "/ must read as a mount point"
        );
    }

    #[test]
    fn is_mountpoint_false_for_ordinary_subdir() {
        // A freshly-created tempdir lives on the same filesystem as its
        // parent, so it is not a mount point.
        let tmp = TempDir::new().expect("temp dir");
        let sub = tmp.path().join("child");
        std::fs::create_dir(&sub).expect("subdir");
        assert!(
            !is_mountpoint(&sub),
            "an ordinary subdirectory is not a mount point"
        );
    }
}
