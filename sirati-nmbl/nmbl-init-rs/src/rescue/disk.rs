//! External rescue squashfs path (PLAN.md §Option 2 "Rust-side flow",
//! Phase C.1).
//!
//! When `rescue.mode = "external"` the rescue toolkit lives on the
//! boot partition as `nmbl-rescue.sfs`. This module performs the full
//! lazy-mount dance:
//!
//! 1. Locate the squashfs via [`super::locate_sfs`].
//! 2. Allocate a free loop minor via
//!    [`crate::sys::loopdev::allocate_loop_device`].
//! 3. Open `/dev/loopN` read-write (LOOP_CONFIGURE refuses an RO fd
//!    even when the backing is RO; the RO-ness is set independently
//!    via `LO_FLAGS_READ_ONLY`).
//! 4. Open the squashfs `O_RDONLY | CLOEXEC` and feed both fds to
//!    [`crate::sys::loopdev::configure_loop_device`].
//! 5. Mount `/dev/loopN` at `/rescue` as `squashfs,ro`.
//! 6. `pivot_root("/rescue", "/rescue/oldroot")` so the operator's
//!    `cd /` lands in the rescue image rather than the initramfs.
//!    `/oldroot` is intentionally LEFT MOUNTED so the operator can
//!    inspect what failed (`ls /oldroot/etc/nmbl/`, the panic report,
//!    the in-progress generation tree under `/mnt/system`, …).
//! 7. `execve("/bin/sh", …)` with a minimal TERM+PATH environment.
//!
//! Every failure point is wrapped in [`NmblError::Rescue`] with a
//! `stage` string the emergency-shell banner surfaces verbatim.

use std::convert::Infallible;
use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

use nix::unistd::{chdir, execve, pivot_root};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno as RustixErrno;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::loopdev::{allocate_loop_device, configure_loop_device, open_loop_device};
use crate::sys::mount::mount_fs;

/// Mountpoint where the rescue squashfs is staged before `pivot_root`.
/// Lives at the initramfs root because (a) `/rescue` is unlikely to
/// collide with anything the initramfs created and (b) after pivot it
/// becomes `/`, so the path itself is ephemeral.
const RESCUE_MOUNT: &str = "/rescue";

/// Subdirectory inside the rescue squashfs's root that receives the
/// old initramfs after `pivot_root`. `/oldroot` is the path the
/// operator will see from the rescue shell.
const OLDROOT_NAME: &str = "oldroot";

/// Shell binary expected inside the rescue squashfs. The squashfs ships
/// busybox under `/bin/sh`, so this path is the post-pivot one.
const RESCUE_SHELL: &str = "/bin/sh";

/// Mount the rescue squashfs from the boot partition, `pivot_root` into
/// it, and `execve` its `/bin/sh`. On success this never returns.
///
/// `cause` is the error that triggered the rescue. It is logged
/// before the loop-mount dance so the operator can see what failed
/// even if the squashfs mount itself misbehaves.
pub fn try_disk_rescue(config: &Config, cause: &NmblError) -> Result<Infallible> {
    let sfs_path = super::locate_sfs(config)?;
    eprintln!(
        "[nmbl] external rescue: mounting {} (triggered by: {})",
        sfs_path.display(),
        cause
    );

    if !sfs_path.exists() {
        return Err(NmblError::Rescue {
            stage: "locate-sfs",
            source: Box::new(NmblError::Io {
                source: io::Error::from(io::ErrorKind::NotFound),
                context: format!(
                    "rescue squashfs {} not found on boot partition",
                    sfs_path.display(),
                ),
            }),
        });
    }

    let index = allocate_loop_device().map_err(|source| NmblError::Rescue {
        stage: "loop-alloc",
        source: Box::new(source),
    })?;

    let loop_fd = open_loop_device(index, true).map_err(|source| NmblError::Rescue {
        stage: "loop-open",
        source: Box::new(source),
    })?;

    let sfs_fd = rustix::fs::open(&sfs_path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| NmblError::Rescue {
            stage: "sfs-open",
            source: Box::new(NmblError::Io {
                source: io_error_from_rustix(e),
                context: format!("opening {}", sfs_path.display()),
            }),
        })?;

    configure_loop_device(&loop_fd, &sfs_fd, true).map_err(|source| NmblError::Rescue {
        stage: "loop-configure",
        source: Box::new(source),
    })?;

    let rescue_dir = Path::new(RESCUE_MOUNT);
    ensure_dir(rescue_dir).map_err(|source| NmblError::Rescue {
        stage: "mount-rescue",
        source: Box::new(source),
    })?;

    let loop_dev = PathBuf::from(format!("/dev/loop{index}"));
    mount_fs(Some(&loop_dev), rescue_dir, "squashfs", "ro").map_err(|source| {
        NmblError::Rescue {
            stage: "mount-rescue",
            source: Box::new(source),
        }
    })?;

    let oldroot = rescue_dir.join(OLDROOT_NAME);
    ensure_dir(&oldroot).map_err(|source| NmblError::Rescue {
        stage: "pivot-root",
        source: Box::new(source),
    })?;
    pivot_root(rescue_dir, &oldroot).map_err(|source| NmblError::Rescue {
        stage: "pivot-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: format!(
                "pivot_root({} -> {})",
                rescue_dir.display(),
                oldroot.display(),
            ),
        }),
    })?;
    chdir("/").map_err(|source| NmblError::Rescue {
        stage: "pivot-root",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: "chdir(/) after pivot_root".to_string(),
        }),
    })?;
    // The old initramfs stays mounted at `/oldroot` on purpose — the
    // operator wants to inspect the panic report, the half-mounted
    // /mnt/system tree, and /etc/nmbl/* without a separate mount step.
    // A pre-kexec sweep is unnecessary because the rescue shell is the
    // operator's final destination on this path.

    let path_c = CString::new(RESCUE_SHELL).map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "rescue shell path contains interior NUL".to_string(),
            context: format!("preparing execve of {RESCUE_SHELL}"),
        }),
    })?;
    let argv0_c = CString::new("sh").map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "rescue argv0 contains interior NUL".to_string(),
            context: format!("preparing execve of {RESCUE_SHELL}"),
        }),
    })?;
    let term_c = CString::new("TERM=linux").map_err(|_| NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "TERM environment string contains interior NUL".to_string(),
            context: format!("preparing execve of {RESCUE_SHELL}"),
        }),
    })?;
    let path_env_c =
        CString::new("PATH=/bin:/sbin:/usr/bin:/usr/sbin").map_err(|_| NmblError::Rescue {
            stage: "exec-shell",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "PATH environment string contains interior NUL".to_string(),
                context: format!("preparing execve of {RESCUE_SHELL}"),
            }),
        })?;

    let argv: [&CString; 1] = [&argv0_c];
    let env: [&CString; 2] = [&term_c, &path_env_c];

    // execve only returns on error.
    let exec_err = execve(&path_c, &argv, &env).err();
    if let Some(source) = exec_err {
        return Err(NmblError::Rescue {
            stage: "exec-shell",
            source: Box::new(NmblError::Shell { source }),
        });
    }
    // execve returned Ok without replacing the process image. The
    // kernel docs say this cannot happen; the type system demands a
    // value, so surface it as an exec failure.
    Err(NmblError::Rescue {
        stage: "exec-shell",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "execve returned Ok without replacing the process image".to_string(),
            context: format!("execve {RESCUE_SHELL}"),
        }),
    })
}

/// Create `path` (and parents) on the rescue mountpoint side. Mirrors
/// the `ensure_dir` helper from `src/mount.rs`; we keep a copy here
/// because that one is module-private and the rescue path wants a
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

/// Map a `rustix::io::Errno` to `std::io::Error` so it can ride inside
/// `NmblError::Io`. Same shape as the helper in `sys::loopdev`.
fn io_error_from_rustix(e: RustixErrno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use crate::rescue::RescueMode;
    use crate::sys::loopdev::LOOP_CONTROL_PATH;

    fn cfg_with_sfs(sfs: Option<PathBuf>, mountpoint: Option<PathBuf>) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = RescueConfig {
            mode: RescueMode::External,
            sfs_path: sfs,
            ..RescueConfig::default()
        };
        c.runtime_boot_mountpoint = mountpoint;
        c
    }

    #[test]
    fn try_disk_rescue_missing_sfs_is_locate_sfs_error() {
        // Point at a path we know cannot exist so the locate-sfs guard
        // fires before we touch /dev/loop-control. This lets the test
        // assert error shape on every host, not just ones with a loop
        // control node available.
        let dir = tempfile::tempdir().expect("tempdir");
        let bogus_name = format!(
            "nmbl-rescue-missing-{}-{}.sfs",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let bogus = dir.path().join(&bogus_name);
        assert!(
            !bogus.exists(),
            "test precondition: bogus path must be absent"
        );

        let cause = NmblError::ConfigInvalid {
            reason: "synthetic".to_string(),
            context: "test".to_string(),
        };
        let cfg = cfg_with_sfs(
            Some(PathBuf::from(&bogus_name)),
            Some(dir.path().to_path_buf()),
        );
        let err = try_disk_rescue(&cfg, &cause).expect_err("missing sfs must error");
        match err {
            NmblError::Rescue { stage, source } => {
                assert_eq!(stage, "locate-sfs");
                match *source {
                    NmblError::Io { context, .. } => {
                        assert!(context.contains(&bogus.display().to_string()), "{context}");
                    }
                    other => panic!("expected Io inside Rescue, got {other:?}"),
                }
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
    }

    #[test]
    fn try_disk_rescue_without_mountpoint_is_locate_sfs_error() {
        // Legacy embedded-config mode: no runtime boot mountpoint is set,
        // so the locate-sfs guard must short-circuit before any disk I/O.
        let cause = NmblError::ConfigInvalid {
            reason: "synthetic".to_string(),
            context: "test".to_string(),
        };
        let cfg = cfg_with_sfs(None, None);
        let err =
            try_disk_rescue(&cfg, &cause).expect_err("missing runtime boot mountpoint must error");
        match err {
            NmblError::Rescue { stage, source } => {
                assert_eq!(stage, "locate-sfs");
                assert!(
                    matches!(*source, NmblError::ConfigInvalid { .. }),
                    "expected ConfigInvalid inside Rescue, got {source:?}",
                );
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn try_disk_rescue_no_loop_control_is_loop_alloc_error() {
        // Mirrors the skip pattern from sys::loopdev::tests: only
        // exercise the loop-alloc arm on hosts that lack
        // /dev/loop-control. Where the node exists the unprivileged
        // sandbox would still take us through allocate_loop_device,
        // which we don't want to depend on.
        if Path::new(LOOP_CONTROL_PATH).exists() {
            eprintln!("skipping: {LOOP_CONTROL_PATH} present");
            return;
        }

        // Stage a real squashfs-shaped tempfile so the locate-sfs guard
        // passes and we reach the loop-alloc step.
        let dir = tempfile::tempdir().expect("tempdir");
        let sfs = dir.path().join("nmbl-rescue.sfs");
        std::fs::write(&sfs, b"placeholder").expect("write sfs");

        let cfg = cfg_with_sfs(
            Some(PathBuf::from("nmbl-rescue.sfs")),
            Some(dir.path().to_path_buf()),
        );
        let cause = NmblError::ConfigInvalid {
            reason: "synthetic".to_string(),
            context: "test".to_string(),
        };
        let err = try_disk_rescue(&cfg, &cause).expect_err("no loop-control must error");
        match err {
            NmblError::Rescue { stage, .. } => {
                assert_eq!(stage, "loop-alloc");
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
    }

    #[test]
    fn ensure_dir_handles_existing_path() {
        ensure_dir(Path::new("/")).expect("root always exists");
    }
}
