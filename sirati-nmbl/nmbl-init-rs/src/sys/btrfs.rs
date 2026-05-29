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
//! `/dev/btrfs-control` is a misc char device (major 10, minor 234)
//! registered when `btrfs.ko` initialises. NMBL has no udev so the
//! node may not exist yet even after the module loads. We create it
//! on demand by reading the actual major:minor from
//! `/sys/class/misc/btrfs-control/dev` so the code stays correct if
//! the kernel ever reassigns the minor.
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

/// Path to the btrfs control device.
const BTRFS_CONTROL_DEVICE: &str = "/dev/btrfs-control";

/// sysfs path that exposes `major:minor` for the btrfs-control misc device.
const BTRFS_CONTROL_SYSFS: &str = "/sys/class/misc/btrfs-control/dev";

/// Ensure `/dev/btrfs-control` exists. NMBL has no udev, so the node
/// may be absent even after `btrfs.ko` loads. We read the actual
/// `major:minor` from sysfs (avoiding a hard-coded 10:234) and call
/// `mknod(2)` to create the char-device node if it isn't already there.
///
/// Returns `Ok(())` if the node already exists or was created
/// successfully. Returns `Ok(())` (with a warning) if the sysfs path
/// is missing — that means btrfs.ko didn't load at all, so the open()
/// that follows will also fail and log its own warning.
fn ensure_btrfs_control() {
    // If the node already exists (e.g. devtmpfs created it), skip mknod.
    if std::path::Path::new(BTRFS_CONTROL_DEVICE).exists() {
        return;
    }

    // Read "major:minor\n" from sysfs.
    let raw = match std::fs::read_to_string(BTRFS_CONTROL_SYSFS) {
        Ok(s) => s,
        Err(e) => {
            nmbl_warn!(
                "btrfs: cannot read {} ({}); /dev/btrfs-control will be absent",
                BTRFS_CONTROL_SYSFS,
                e,
            );
            return;
        }
    };
    let trimmed = raw.trim();
    let (maj_str, min_str) = match trimmed.split_once(':') {
        Some(pair) => pair,
        None => {
            nmbl_warn!(
                "btrfs: unexpected format in {} ({:?}); skipping mknod",
                BTRFS_CONTROL_SYSFS,
                trimmed,
            );
            return;
        }
    };
    let (major, minor) = match (maj_str.parse::<u32>(), min_str.parse::<u32>()) {
        (Ok(ma), Ok(mi)) => (ma, mi),
        _ => {
            nmbl_warn!(
                "btrfs: cannot parse major:minor from {:?}; skipping mknod",
                trimmed,
            );
            return;
        }
    };

    // S_IFCHR | 0o600
    let mode: libc::mode_t = libc::S_IFCHR | 0o600;
    let dev = libc::makedev(major, minor);
    let path_cstr = match std::ffi::CString::new(BTRFS_CONTROL_DEVICE) {
        Ok(c) => c,
        Err(_) => {
            nmbl_warn!("btrfs: BTRFS_CONTROL_DEVICE path contains NUL; skipping mknod");
            return;
        }
    };
    // SAFETY: mknod(2) with a char-device type. `path_cstr` is a valid
    // NUL-terminated CString that outlives the call. `dev` is the
    // numeric major:minor obtained from libc::makedev. The kernel creates
    // or rejects the node; no user-space buffers are written by the
    // syscall. No safe rustix wrapper for mknod exists in rustix 0.38.
    let rc = unsafe { libc::mknod(path_cstr.as_ptr(), mode, dev) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        // EEXIST is fine — another thread/path created it.
        if e.raw_os_error() != Some(libc::EEXIST) {
            nmbl_warn!("btrfs: mknod {} failed: {}", BTRFS_CONTROL_DEVICE, e);
        }
    }
}

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
    // `libc::ioctl`'s request argument is `c_int` on musl (our production
    // target) but `c_ulong` on glibc; cast to whichever this build needs.
    // Casting the u32 ioctl code is safe either way — the kernel treats it
    // as unsigned and the bit pattern is preserved through the syscall.
    #[cfg(target_env = "musl")]
    let request = BTRFS_IOC_SCAN_DEV as libc::c_int;
    #[cfg(not(target_env = "musl"))]
    let request = BTRFS_IOC_SCAN_DEV as libc::c_ulong;
    let rc = unsafe {
        libc::ioctl(
            control_fd,
            request,
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
/// `/dev/btrfs-control`. Logs `[nmbl] btrfs: registered N of M devices`
/// on completion. Individual device errors are logged at warn level
/// and do not abort the sweep.
pub fn scan_devices(devs: &[std::path::PathBuf]) -> Result<()> {
    let total = devs.len();
    if total == 0 {
        nmbl_info!("btrfs: registered 0 of 0 devices");
        return Ok(());
    }

    // Create /dev/btrfs-control if absent (NMBL has no udev).
    ensure_btrfs_control();

    // Open /dev/btrfs-control once and reuse it. If it is missing the
    // btrfs module didn't load — surface the underlying error so the
    // operator sees why the scan was skipped.
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
            nmbl_info!("btrfs: registered {}", dev.display());
        } else {
            nmbl_warn!(
                "btrfs: BTRFS_IOC_SCAN_DEV on {} returned errno {} ({})",
                dev.display(),
                errno,
                std::io::Error::from_raw_os_error(errno),
            );
        }
    }
    nmbl_info!("btrfs: registered {} of {} devices", count, total);
    Ok(())
}
