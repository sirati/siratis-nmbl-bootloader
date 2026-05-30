//! Udev-less population of /dev/disk/by-{partlabel,label,uuid,partuuid}/
//! symlinks. We walk /sys/class/block, run blkid -o export on each
//! resulting /dev/<name>, parse the KEY=VALUE output, and create the
//! corresponding by-* symlinks. Mirrors what udev would do but
//! without dragging in udev itself (which would balloon the
//! initramfs by an order of magnitude).
//!
//! Replaces the bash loop that lived in `mount-and-kernel.sh.nix`
//! (commit 534fe5d, "sirati-nmbl: udev-less stage-0 …").

mod parse;
mod symlink;

pub use parse::parse_blkid_export;

use std::path::{Path, PathBuf};

use crate::error::{NmblError, Result};
use crate::{nmbl_info, nmbl_warn};

use std::collections::HashMap;

use parse::blkid_for;
use symlink::create_links_for;

use crate::sys::poller::LocalSender;

/// Filesystem locations + blkid attribute keys we care about. Kept as
/// a slice constant so tests can iterate the same set the production
/// path uses, with no chance of drift.
const CATEGORIES: &[(&str, &str)] = &[
    ("by-partlabel", "PARTLABEL"),
    ("by-label", "LABEL"),
    ("by-uuid", "UUID"),
    ("by-partuuid", "PARTUUID"),
];

/// Absolute path to the `blkid` binary in the NMBL initramfs.
///
/// The Nix side wires `pkgs.util-linux`'s `bin/blkid` into `/bin/blkid`
/// inside the initrd (see `lib/config.nix` baseContents). Production
/// always invokes that path; tests skip when it is missing.
pub(crate) const BLKID_BINARY: &str = "/bin/blkid";

/// Where /sys exposes the kernel-known block devices.
const SYSFS_BLOCK_DIR: &str = "/sys/class/block";

/// Where the by-* symlink tree lives.
const DISK_DIR: &str = "/dev/disk";

/// Exit code blkid uses for "no superblock found" — common for
/// unformatted partitions and raw whole-disk nodes. Treat as "no
/// attributes", not a failure.
const BLKID_EXIT_NO_SUPERBLOCK: i32 = 2;

/// Populate /dev/disk/by-{partlabel,label,uuid,partuuid}/ symlinks
/// for every block device in /sys/class/block. Idempotent — re-runs
/// just overwrite the same target. Errors from individual devices
/// are logged via `nmbl_warn!` and do not fail the whole call; only
/// catastrophic errors (e.g. /sys/class/block not readable) bubble.
///
/// Also returns the list of block devices whose blkid TYPE is "btrfs"
/// so the caller can issue `BTRFS_IOC_SCAN_DEV` before mounting.
///
/// Async because each per-device blkid reap goes through the poller's
/// non-blocking `waitpid` op, so the single-threaded runtime keeps
/// serving concurrent work while we scan. Both the early-boot bootstrap
/// sweep and the interactive-phase sweep run inside the interactive
/// runtime and use this path; there is no blocking variant.
pub async fn populate_disk_by_symlinks(sender: &LocalSender) -> Result<Vec<PathBuf>> {
    let dev_paths = collect_block_dev_paths()?;
    let mut acc = SymlinkAcc::default();
    for dev_path in dev_paths {
        let attrs = match blkid_for(&dev_path, sender).await {
            Ok(map) => map,
            Err(e) => {
                nmbl_warn!("blkid: scanning {} failed: {}", dev_path.display(), e);
                continue;
            }
        };
        acc.record(&dev_path, &attrs);
    }
    Ok(acc.finish())
}

/// Read /sys/class/block, pre-create the four by-* target directories,
/// and return the `/dev/<name>` paths for every block device that has a
/// materialised node.
fn collect_block_dev_paths() -> Result<Vec<PathBuf>> {
    let sysfs = Path::new(SYSFS_BLOCK_DIR);
    let entries = std::fs::read_dir(sysfs).map_err(|source| NmblError::Io {
        source,
        context: format!("reading {}", sysfs.display()),
    })?;

    // Pre-create the four target directories once. `create_dir_all`
    // already treats AlreadyExists as success.
    for (dir_name, _) in CATEGORIES {
        let dir = Path::new(DISK_DIR).join(dir_name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            nmbl_warn!(
                "blkid: could not create {}: {} — symlinks for this category will be skipped",
                dir.display(),
                e,
            );
        }
    }

    let mut dev_paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                nmbl_warn!("blkid: dir entry in {} unreadable: {}", sysfs.display(), e);
                continue;
            }
        };

        let dev_path = Path::new("/dev").join(entry.file_name());

        // Some block devices the kernel exposes in /sys have no
        // materialised /dev node yet — notably GPT partitions when
        // devtmpfs/udev did not auto-create `/dev/<disk><N>`. Without the
        // node, blkid cannot probe the partition and its PARTLABEL is
        // never turned into a by-partlabel symlink, so a disko-style
        // `device = /dev/disk/by-partlabel/...` activation fails. Try to
        // mknod the node from the sysfs `dev` (major:minor) attribute
        // before probing. Device-mapper aliases and other entries without
        // a usable `dev` attribute are still skipped.
        if std::fs::symlink_metadata(&dev_path).is_err() {
            match ensure_dev_node(&entry.path(), &dev_path) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    nmbl_warn!(
                        "blkid: could not create node {}: {} — skipping",
                        dev_path.display(),
                        e,
                    );
                    continue;
                }
            }
        }
        dev_paths.push(dev_path);
    }
    Ok(dev_paths)
}

/// Accumulator for a by-* sweep: counts scanned devices and created
/// links, and collects btrfs members. Folds the shared post-blkid logic
/// (btrfs detection + symlink creation) behind the per-device reap.
#[derive(Default)]
struct SymlinkAcc {
    device_count: usize,
    link_count: usize,
    btrfs_devs: Vec<PathBuf>,
}

impl SymlinkAcc {
    fn record(&mut self, dev_path: &Path, attrs: &HashMap<String, String>) {
        self.device_count = self.device_count.saturating_add(1);
        if attrs.get("TYPE").map(String::as_str) == Some("btrfs") {
            self.btrfs_devs.push(dev_path.to_path_buf());
        }
        self.link_count = self
            .link_count
            .saturating_add(create_links_for(dev_path, attrs));
    }

    fn finish(self) -> Vec<PathBuf> {
        nmbl_info!(
            "blkid: scanned {} block device(s), created/updated {} by-* symlink(s)",
            self.device_count,
            self.link_count,
        );
        self.btrfs_devs
    }
}

/// Ensure a block-device node exists at `dev_path`, creating it from the
/// sysfs `dev` (`major:minor`) attribute under `sysfs_entry` when udev /
/// devtmpfs did not. Returns `Ok(true)` when the node exists (already or
/// freshly created), `Ok(false)` when `sysfs_entry/dev` is absent or
/// unparseable (e.g. a device-mapper alias with no usable node — caller
/// skips it). Mirrors the mknod pattern in `sys::btrfs::ensure_btrfs_control`.
pub(crate) fn ensure_dev_node(sysfs_entry: &Path, dev_path: &Path) -> Result<bool> {
    if dev_path.exists() {
        return Ok(true);
    }

    let dev_attr = sysfs_entry.join("dev");
    let raw = match std::fs::read_to_string(&dev_attr) {
        Ok(s) => s,
        // No `dev` attribute → not a node-backed device (alias/holder).
        Err(_) => return Ok(false),
    };
    let trimmed = raw.trim();
    let Some((maj_str, min_str)) = trimmed.split_once(':') else {
        return Ok(false);
    };
    let (Ok(major), Ok(minor)) = (maj_str.parse::<u32>(), min_str.parse::<u32>()) else {
        return Ok(false);
    };

    let mode: libc::mode_t = libc::S_IFBLK | 0o600;
    let dev = libc::makedev(major, minor);
    let path_cstr =
        std::ffi::CString::new(dev_path.as_os_str().as_encoded_bytes()).map_err(|_| {
            NmblError::Io {
                source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
                context: format!("device path {} contains NUL", dev_path.display()),
            }
        })?;
    // SAFETY: mknod(2) with a block-device type. `path_cstr` is a valid
    // NUL-terminated CString that outlives the call; `dev` is the numeric
    // major:minor from libc::makedev. The kernel creates or rejects the
    // node and writes no user-space buffers. No safe rustix wrapper for
    // mknod exists in rustix 0.38 (matches sys::btrfs::ensure_btrfs_control).
    let rc = unsafe { libc::mknod(path_cstr.as_ptr(), mode, dev) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EEXIST) {
            return Ok(true);
        }
        return Err(NmblError::Io {
            source: e,
            context: format!(
                "mknod block node {} ({}:{})",
                dev_path.display(),
                major,
                minor
            ),
        });
    }
    Ok(true)
}
