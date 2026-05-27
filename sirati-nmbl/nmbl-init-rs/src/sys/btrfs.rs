//! `BTRFS_IOC_SCAN_DEV` wrapper.
//!
//! Issues the btrfs device-scan ioctl on a block device so the kernel
//! registers it as a member of a multi-device btrfs filesystem before
//! `mount(2)` is called. Equivalent to `btrfs device scan <dev>` but
//! without requiring the userspace `btrfs` binary in the initramfs.
//!
//! BTRFS_IOC_SCAN_DEV = _IOW(BTRFS_IOCTL_MAGIC, 4, struct btrfs_ioctl_vol_args)
//!   BTRFS_IOCTL_MAGIC = 0x94
//!   struct btrfs_ioctl_vol_args: { __s64 fd; char name[BTRFS_PATH_NAME_MAX+1] }
//!   BTRFS_PATH_NAME_MAX = 4087
//!   sizeof(btrfs_ioctl_vol_args) = 8 + 4088 = 4096
//!   _IOW(type, nr, size): ((3 << 30) | (type << 8) | nr | (size << 16))
//!     = (0xC0001094 with size=4096=0x1000) => 0xC400_9410 on 64-bit
//!
//! See linux/btrfs.h in the kernel UAPI headers.

use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::error::{NmblError, Result};
use crate::{nmbl_info, nmbl_warn};

/// `BTRFS_IOCTL_MAGIC` from linux/btrfs.h.
const BTRFS_IOCTL_MAGIC: u8 = 0x94;

/// `BTRFS_PATH_NAME_MAX` from linux/btrfs.h.
const BTRFS_PATH_NAME_MAX: usize = 4087;

/// `struct btrfs_ioctl_vol_args` from linux/btrfs.h.
/// Layout: { __s64 fd; char name[BTRFS_PATH_NAME_MAX + 1] }
#[repr(C)]
struct BtrfsIoctlVolArgs {
    fd: i64,
    name: [u8; BTRFS_PATH_NAME_MAX + 1],
}

/// Compute `BTRFS_IOC_SCAN_DEV` at compile time.
/// `_IOW(magic, nr, T)` = `((_IOC_WRITE << 30) | ((size) << 16) | ((type) << 8) | (nr))`
/// `_IOC_WRITE` = 1 (linux/ioctl.h), size = size_of::<BtrfsIoctlVolArgs>() = 4096.
const BTRFS_IOC_SCAN_DEV: libc::c_ulong = {
    let size = std::mem::size_of::<BtrfsIoctlVolArgs>();
    ((1u64 << 30) | ((size as u64) << 16) | ((BTRFS_IOCTL_MAGIC as u64) << 8) | 4u64)
        as libc::c_ulong
};

/// Issue `BTRFS_IOC_SCAN_DEV` on `dev`. The ioctl tells the kernel to
/// scan this device and register it in the btrfs multi-device registry
/// so that a subsequent `mount(2)` can assemble all members.
///
/// Returns `Ok(())` on success. Returns `Err` only for hard failures
/// (open(2) failed); ENOTTY / EINVAL (non-btrfs device) are treated as
/// warnings and collapsed to `Ok(())` since we call this on every
/// device whose blkid TYPE=btrfs.
fn scan_one(dev: &Path) -> Result<()> {
    // SAFETY: BtrfsIoctlVolArgs is a plain C struct; zeroing it is valid.
    let mut args = BtrfsIoctlVolArgs {
        fd: -1,
        name: [0u8; BTRFS_PATH_NAME_MAX + 1],
    };

    // Fill args.name with the device path, truncated to BTRFS_PATH_NAME_MAX.
    let path_str = dev.to_string_lossy();
    let bytes = path_str.as_bytes();
    let copy_len = bytes.len().min(BTRFS_PATH_NAME_MAX);
    // Copy byte-by-byte: indexing_slicing lint requires bounds-checked access.
    for (i, b) in bytes.iter().take(copy_len).enumerate() {
        // SAFETY: i < copy_len <= BTRFS_PATH_NAME_MAX < args.name.len()
        if let Some(slot) = args.name.get_mut(i) {
            *slot = *b;
        }
    }

    // Open the device O_RDONLY | O_CLOEXEC.
    let fd = rustix::fs::open(
        dev,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| NmblError::Io {
        source: std::io::Error::from_raw_os_error(e.raw_os_error()),
        context: format!("opening {} for btrfs scan", dev.display()),
    })?;

    // SAFETY: Unavoidable raw ioctl.
    //   * No safe Rust wrapper exists for BTRFS_IOC_SCAN_DEV in nix 0.29
    //     or rustix 0.38.
    //   * `fd` is a live OwnedFd held for the duration of this call.
    //   * `args` is stack-allocated, correctly sized, and zero-initialized;
    //     the kernel reads the `name` field and does not write to our buffer.
    //   * The ioctl number matches linux/btrfs.h BTRFS_IOC_SCAN_DEV.
    let rc = unsafe {
        libc::ioctl(
            fd.as_raw_fd(),
            BTRFS_IOC_SCAN_DEV,
            &mut args as *mut BtrfsIoctlVolArgs,
        )
    };

    if rc < 0 {
        let errno = std::io::Error::last_os_error();
        nmbl_warn!(
            "btrfs: BTRFS_IOC_SCAN_DEV on {} returned error: {} — skipping",
            dev.display(),
            errno,
        );
    }

    Ok(())
}

/// Scan every device in `devs` with `BTRFS_IOC_SCAN_DEV`. Logs
/// `[nmbl] btrfs: scanned N device(s)` on completion. Individual
/// device errors are logged at warn level and do not abort the sweep.
pub fn scan_devices(devs: &[std::path::PathBuf]) -> Result<()> {
    let mut count: usize = 0;
    for dev in devs {
        match scan_one(dev) {
            Ok(()) => {
                count = count.saturating_add(1);
                nmbl_warn!("btrfs: registered {} with kernel", dev.display());
            }
            Err(e) => {
                nmbl_warn!("btrfs: scan of {} failed: {}", dev.display(), e);
            }
        }
    }
    nmbl_info!("btrfs: scanned {} device(s)", count);
    Ok(())
}
