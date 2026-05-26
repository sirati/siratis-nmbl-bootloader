//! Phase 1: mount the pseudo-filesystems the rest of `nmbl-init` needs.
//!
//! Replaces the top half of `scripts/mount-and-kernel.sh.nix` (the proc/sys/
//! dev/run/tmp block). Idempotent: a fresh boot has none of these mounted;
//! a re-exec after panic recovery typically has all of them mounted and the
//! kernel returns `EBUSY`, which we treat as success.

use std::io;
use std::path::Path;

use nix::errno::Errno;

use crate::error::{NmblError, Result};
use crate::sys::mount::mount_fs;
use crate::{nmbl_verbose, nmbl_warn};

/// One pseudo-filesystem we mount during phase 1. `source` stays `None`
/// because every entry below is a pseudo-fs that ignores the source string.
struct PseudoFs {
    target: &'static str,
    fstype: &'static str,
    options: &'static str,
}

const PSEUDO_FILESYSTEMS: &[PseudoFs] = &[
    PseudoFs {
        target: "/proc",
        fstype: "proc",
        options: "nosuid,noexec,nodev",
    },
    PseudoFs {
        target: "/sys",
        fstype: "sysfs",
        options: "nosuid,noexec,nodev",
    },
    PseudoFs {
        target: "/dev",
        fstype: "devtmpfs",
        options: "mode=755,nosuid",
    },
    PseudoFs {
        target: "/run",
        fstype: "tmpfs",
        options: "nosuid,nodev,mode=755",
    },
    PseudoFs {
        target: "/tmp",
        fstype: "tmpfs",
        options: "nosuid,nodev",
    },
];

/// Mount the pseudo-filesystems the rest of init needs: /proc, /sys,
/// /dev (devtmpfs), /run (tmpfs), /tmp (tmpfs). Idempotent — a mount
/// that fails with `EBUSY` is logged at warn level and treated as
/// success, since re-mounting one of these is a benign no-op.
pub fn mount_pseudo_filesystems() -> Result<()> {
    for fs in PSEUDO_FILESYSTEMS {
        let target = Path::new(fs.target);
        ensure_dir(target)?;
        match mount_fs(None, target, fs.fstype, fs.options) {
            Ok(()) => nmbl_verbose!("mounted {} on {}", fs.fstype, fs.target),
            Err(NmblError::Mount {
                source: Errno::EBUSY,
                ..
            }) => {
                nmbl_warn!(
                    "{} already mounted on {} (EBUSY) — continuing",
                    fs.fstype,
                    fs.target
                );
            }
            Err(other) => return Err(other),
        }
    }
    Ok(())
}

/// Create `path` (and any missing parents) on the initramfs root. Returns
/// `Ok(())` if the directory already exists, since the cpio image often
/// pre-creates these mountpoints; any other `io::Error` is wrapped with a
/// path-aware context string.
fn ensure_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(NmblError::Io {
            source: e,
            context: format!("creating {}", path.display()),
        }),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests {
    use super::*;

    #[test]
    fn ensure_dir_succeeds_on_existing_path() {
        // `/` always exists on any host that can run cargo test; create_dir_all
        // on it must collapse to Ok(()), not surface a permission or
        // already-exists failure.
        ensure_dir(Path::new("/")).expect("root always exists");
    }

    #[test]
    fn ensure_dir_creates_nested_in_tmp() {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir().join(format!("nmbl-ensure-dir-{pid}-{nonce}"));
        let nested = base.join("a/b/c");

        ensure_dir(&nested).expect("nested create");
        // Idempotency: a second call on the same path must also be Ok.
        ensure_dir(&nested).expect("second call is a no-op");

        let _ = std::fs::remove_dir_all(&base);
    }
}
