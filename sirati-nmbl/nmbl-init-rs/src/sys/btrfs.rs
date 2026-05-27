//! `BTRFS_IOC_SCAN_DEV` wrapper.
//!
//! Issues the btrfs device-scan ioctl so the kernel registers a block
//! device as a member of a multi-device btrfs filesystem before
//! `mount(2)` is called. Equivalent to `btrfs device scan <dev>` but
//! without requiring the userspace `btrfs` binary in the initramfs.
//!
//! IMPORTANT: this ioctl is a *control* ioctl handled by
//! `fs/btrfs/super.c:btrfs_control_ioctl()`. It MUST be invoked on the
//! `/dev/btrfs-control` misc char device, NOT on the block device.
//! The device-to-scan path is passed via the `name` field of
//! `struct btrfs_ioctl_vol_args`. Invoking it on a block device fd
//! returns ENOTTY because the block device's file_operations does not
//! recognize BTRFS_IOC_SCAN_DEV.
//!
//! BTRFS_IOC_SCAN_DEV = _IOW(BTRFS_IOCTL_MAGIC, 4, struct btrfs_ioctl_vol_args)
//!   BTRFS_IOCTL_MAGIC = 0x94
//!   struct btrfs_ioctl_vol_args: { __s64 fd; char name[BTRFS_PATH_NAME_MAX+1] }
//!   BTRFS_PATH_NAME_MAX = 4087
//!   sizeof(btrfs_ioctl_vol_args) = 8 + 4088 = 4096
//!
//! See linux/btrfs.h in the kernel UAPI headers.

use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::error::Result;
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

/// `BTRFS_IOC_SCAN_DEV` ioctl request number.
///
/// `_IOW(BTRFS_IOCTL_MAGIC, 4, struct btrfs_ioctl_vol_args)` where
/// `_IOW(type, nr, T)` = `((_IOC_WRITE << 30) | (sizeof(T) << 16) | ((type) << 8) | nr)`,
/// `_IOC_WRITE` = 1, `sizeof(BtrfsIoctlVolArgs)` = 4096 = 0x1000.
/// Stored as `u32` (the type libc::ioctl expects on Linux/musl as `Ioctl = c_int`).
const BTRFS_IOC_SCAN_DEV: u32 = {
    let size = std::mem::size_of::<BtrfsIoctlVolArgs>() as u32;
    (1u32 << 30) | (size << 16) | ((BTRFS_IOCTL_MAGIC as u32) << 8) | 4u32
};

/// Path to the btrfs control device. Created by the kernel via
/// devtmpfs when the btrfs module loads; NMBL pre-loads btrfs in
/// phase 2 so this node exists by the time phase 3b runs.
const BTRFS_CONTROL_DEVICE: &str = "/dev/btrfs-control";

/// Fill `args.name` with the device path (NUL-padded, truncated to
/// `BTRFS_PATH_NAME_MAX`). Bounds-checked so the `indexing_slicing`
/// lint stays happy.
fn fill_args_name(args: &mut BtrfsIoctlVolArgs, dev: &Path) {
    let path_str = dev.to_string_lossy();
    let bytes = path_str.as_bytes();
    let copy_len = bytes.len().min(BTRFS_PATH_NAME_MAX);
    for (i, b) in bytes.iter().take(copy_len).enumerate() {
        if let Some(slot) = args.name.get_mut(i) {
            *slot = *b;
        }
    }
}

/// Issue `BTRFS_IOC_SCAN_DEV` against `control_fd` (the
/// `/dev/btrfs-control` device) with `dev` in the `name` field.
/// Returns the raw errno on failure; `0` on success.
fn ioctl_scan(control_fd: libc::c_int, dev: &Path) -> i32 {
    let mut args = BtrfsIoctlVolArgs {
        fd: -1,
        name: [0u8; BTRFS_PATH_NAME_MAX + 1],
    };
    fill_args_name(&mut args, dev);

    // SAFETY: Unavoidable raw ioctl.
    //   * No safe Rust wrapper exists for BTRFS_IOC_SCAN_DEV in nix 0.29
    //     or rustix 0.38.
    //   * `control_fd` is a live OwnedFd held by the caller for the
    //     duration of this call (we pass the raw fd through, the caller
    //     owns it).
    //   * `args` is stack-allocated, correctly sized, and zero-initialized;
    //     the kernel reads the `name` field and does not write to our buffer.
    //   * The ioctl number matches linux/btrfs.h BTRFS_IOC_SCAN_DEV.
    // `libc::ioctl` on musl takes the request as `c_int`; casting a
    // u32 ioctl code to i32 is safe — the kernel treats it as unsigned,
    // and the bit pattern is preserved through the system-call argument.
    let rc = unsafe {
        libc::ioctl(
            control_fd,
            BTRFS_IOC_SCAN_DEV as libc::c_int,
            &mut args as *mut BtrfsIoctlVolArgs,
        )
    };

    if rc < 0 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
    } else {
        0
    }
}

/// Scan every device in `devs` via `BTRFS_IOC_SCAN_DEV` on
/// `/dev/btrfs-control`. Logs `[nmbl] btrfs: scanned N device(s)`
/// on completion. Individual device errors are logged at warn level
/// and do not abort the sweep.
pub fn scan_devices(devs: &[std::path::PathBuf]) -> Result<()> {
    if devs.is_empty() {
        nmbl_info!("btrfs: scanned 0 device(s)");
        return Ok(());
    }

    // Open /dev/btrfs-control once and reuse it. If it is missing the
    // btrfs module didn't load (or its devtmpfs entry hasn't appeared
    // yet) — surface the underlying error so the operator sees why the
    // scan was skipped.
    let control = match rustix::fs::open(
        BTRFS_CONTROL_DEVICE,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            nmbl_warn!(
                "btrfs: cannot open {} ({}); skipping scan — multi-device btrfs mounts will likely fail",
                BTRFS_CONTROL_DEVICE,
                e,
            );
            return Ok(());
        }
    };

    let mut count: usize = 0;
    for dev in devs {
        let errno = ioctl_scan(control.as_raw_fd(), dev);
        if errno == 0 {
            count = count.saturating_add(1);
            nmbl_info!("btrfs: registered {} with kernel", dev.display());
        } else {
            nmbl_warn!(
                "btrfs: BTRFS_IOC_SCAN_DEV on {} returned {} ({})",
                dev.display(),
                errno,
                std::io::Error::from_raw_os_error(errno),
            );
        }
    }
    nmbl_info!("btrfs: scanned {} device(s)", count);
    Ok(())
}
