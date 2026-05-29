//! Thin wrappers around the two `/dev/loop-control` + `/dev/loopN`
//! ioctls NMBL needs to mount the rescue squashfs without dragging in
//! `losetup(8)` from util-linux.
//!
//! The flow at runtime is:
//!   1. [`allocate_loop_device`] opens `/dev/loop-control` and issues
//!      `LOOP_CTL_GET_FREE`, returning the index of a free loop node.
//!   2. The caller opens `/dev/loopN` (RW for read-write attachment,
//!      RO is fine when only mounting read-only backing files).
//!   3. [`configure_loop_device`] hands the backing-file fd + the
//!      `LO_FLAGS_READ_ONLY` bit to `LOOP_CONFIGURE` (Linux ≥ 5.8),
//!      which atomically binds the fd and sets the device parameters
//!      — replacing the old `LOOP_SET_FD` + `LOOP_SET_STATUS64`
//!      two-step.
//!   4. The caller mounts `/dev/loopN` as usual.
//!   5. [`detach_loop_device`] (`LOOP_CLR_FD`) is available for the
//!      cases where we want to release the binding, even though the
//!      rescue path normally never unwinds.
//!
//! Project rule: minimize unsafe. The opcodes are flat legacy numbers
//! (no `_IOR`/`_IOW` direction bits) so we drive `rustix::ioctl` with
//! `BadOpcode` — every unsafe block has a SAFETY comment per the
//! convention set in `sys::kexec`.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno as RustixErrno;
use rustix::ioctl::{BadOpcode, Ioctl, IoctlOutput, NoArg, Opcode, RawOpcode, Setter};

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

/// `/dev/loop-control` — single global control node used to allocate
/// and release loop indices.
pub const LOOP_CONTROL_PATH: &str = "/dev/loop-control";

/// sysfs path exposing `major:minor` for the loop-control misc device.
/// Read at runtime so we never hard-code the (10:237) pair.
const LOOP_CONTROL_SYSFS: &str = "/sys/class/misc/loop-control/dev";

/// Loop-control ioctl: return the index of an unused loop device,
/// allocating one if none is free. Result is the index (≥0); negative
/// means failure.
pub const LOOP_CTL_GET_FREE: RawOpcode = 0x4C82;

/// Per-loop ioctl: atomically bind a backing file fd and configure
/// the device parameters in one shot. Added in Linux 5.8 (commit
/// 3448914e8cc5, "loop: add LOOP_CONFIGURE ioctl"). NMBL targets
/// kernels ≥ 5.8 so we never need the legacy `LOOP_SET_FD` +
/// `LOOP_SET_STATUS64` fallback.
pub const LOOP_CONFIGURE: RawOpcode = 0x4C0A;

/// Per-loop ioctl: detach the backing file (the inverse of the
/// `LOOP_SET_FD` half of the old configure path). Takes no argument.
pub const LOOP_CLR_FD: RawOpcode = 0x4C01;

/// `LO_FLAGS_READ_ONLY` — set in `loop_info64.lo_flags` /
/// `loop_config.info.lo_flags` to mark the device read-only.
pub const LO_FLAGS_READ_ONLY: u32 = 1;

/// Size of the `lo_file_name` / `lo_crypt_name` fields in
/// `struct loop_info64` — `LO_NAME_SIZE` from `<linux/loop.h>`.
pub const LO_NAME_SIZE: usize = 64;

/// Size of the `lo_encrypt_key` field in `struct loop_info64` —
/// `LO_KEY_SIZE` from `<linux/loop.h>`.
pub const LO_KEY_SIZE: usize = 32;

/// Mirror of `struct loop_info64` from `<linux/loop.h>`.
///
/// Field order, sizes, and the trailing buffers must match the kernel
/// UAPI exactly: the kernel will read this verbatim and reject any
/// mismatched layout with `EINVAL`. The `info_size_matches_uapi`
/// unit test pins the size to 232 bytes (8*8 + 4*4 + 64 + 64 + 32 +
/// 2*8) so an accidental field re-order is caught at `cargo test`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LoopInfo64 {
    pub lo_device: u64,
    pub lo_inode: u64,
    pub lo_rdevice: u64,
    pub lo_offset: u64,
    pub lo_sizelimit: u64,
    pub lo_number: u32,
    pub lo_encrypt_type: u32,
    pub lo_encrypt_key_size: u32,
    pub lo_flags: u32,
    pub lo_file_name: [u8; LO_NAME_SIZE],
    pub lo_crypt_name: [u8; LO_NAME_SIZE],
    pub lo_encrypt_key: [u8; LO_KEY_SIZE],
    pub lo_init: [u64; 2],
}

impl LoopInfo64 {
    /// All-zeroes default. Plain `Default::default()` won't derive for
    /// the long byte arrays without `serde`/`bytemuck`, so spell it
    /// out — every field zero is also exactly what `LOOP_CONFIGURE`
    /// wants when the caller has no special parameters to set.
    #[inline]
    pub const fn zeroed() -> Self {
        Self {
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: 0,
            lo_sizelimit: 0,
            lo_number: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: 0,
            lo_file_name: [0; LO_NAME_SIZE],
            lo_crypt_name: [0; LO_NAME_SIZE],
            lo_encrypt_key: [0; LO_KEY_SIZE],
            lo_init: [0; 2],
        }
    }
}

/// Mirror of `struct loop_config` from `<linux/loop.h>` (Linux 5.8+).
///
/// Layout is `fd, block_size, loop_info64 info, __u64 __reserved[8]`.
/// The trailing reserved field MUST be present and zero — the kernel
/// reads `sizeof(struct loop_config)` bytes and would `EINVAL` on a
/// short struct. The `config_size_matches_uapi` test pins the total
/// at 304 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LoopConfig {
    pub fd: u32,
    pub block_size: u32,
    pub info: LoopInfo64,
    // Maps to the C struct's `__u64 __reserved[8]`; named without the
    // leading underscores because Rust reserves those for the language.
    pub reserved: [u64; 8],
}

impl LoopConfig {
    /// Zeroed-out config with only `fd` populated. `block_size = 0`
    /// asks the kernel to pick its default (typically the backing
    /// file's I/O block size); `info` is all zeroes which means
    /// "no offset, no size limit, no flags".
    #[inline]
    pub const fn for_fd(fd: u32) -> Self {
        Self {
            fd,
            block_size: 0,
            info: LoopInfo64::zeroed(),
            reserved: [0; 8],
        }
    }
}

/// Custom `rustix::ioctl::Ioctl` for `LOOP_CTL_GET_FREE`.
///
/// The two stock helpers don't fit: `NoArg` throws away the syscall
/// return value, but `LOOP_CTL_GET_FREE` *uses* that return value
/// (the free-device index). So we implement the trait by hand —
/// `as_ptr` returns null (no userspace buffer), and `output_from_ptr`
/// just hands back the integer that `ioctl(2)` returned.
struct LoopCtlGetFree;

// SAFETY: `LOOP_CTL_GET_FREE` ioctl on `/dev/loop-control` takes no
// argument pointer (the kernel ignores it) and writes nothing to
// userspace. The `out` value returned by the kernel is the chosen
// loop index (≥0) when ioctl(2) reports success — rustix has already
// converted the `-1 / errno` failure case into a `Result::Err`.
unsafe impl Ioctl for LoopCtlGetFree {
    type Output = IoctlOutput;

    const IS_MUTATING: bool = false;
    const OPCODE: Opcode = Opcode::old(LOOP_CTL_GET_FREE);

    fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        core::ptr::null_mut()
    }

    unsafe fn output_from_ptr(
        out: IoctlOutput,
        _arg: *mut core::ffi::c_void,
    ) -> rustix::io::Result<Self::Output> {
        Ok(out)
    }
}

/// Ensure `/dev/loop-control` exists. NMBL ships no udev, so the node
/// may be absent even after `loop.ko` loads. We read the actual
/// `major:minor` from sysfs and `mknod(2)` the char-device node when it
/// isn't already there. A no-op when the node exists (the common case:
/// devtmpfs created it on module load). When sysfs is missing the loop
/// module isn't loaded at all; we warn and let the `open` below surface
/// the failure.
fn ensure_loop_control() {
    if Path::new(LOOP_CONTROL_PATH).exists() {
        return;
    }

    let raw = match std::fs::read_to_string(LOOP_CONTROL_SYSFS) {
        Ok(s) => s,
        Err(e) => {
            nmbl_warn!(
                "loop: cannot read {} ({}); {} will be absent",
                LOOP_CONTROL_SYSFS,
                e,
                LOOP_CONTROL_PATH,
            );
            return;
        }
    };
    let trimmed = raw.trim();
    let Some((maj_str, min_str)) = trimmed.split_once(':') else {
        nmbl_warn!(
            "loop: unexpected format in {} ({:?}); skipping mknod",
            LOOP_CONTROL_SYSFS,
            trimmed,
        );
        return;
    };
    let (Ok(major), Ok(minor)) = (maj_str.parse::<u32>(), min_str.parse::<u32>()) else {
        nmbl_warn!(
            "loop: cannot parse major:minor from {:?}; skipping mknod",
            trimmed,
        );
        return;
    };

    let mode: libc::mode_t = libc::S_IFCHR | 0o600;
    let dev = libc::makedev(major, minor);
    let Ok(path_cstr) = std::ffi::CString::new(LOOP_CONTROL_PATH) else {
        nmbl_warn!("loop: LOOP_CONTROL_PATH contains NUL; skipping mknod");
        return;
    };
    // SAFETY: mknod(2) with a char-device type. `path_cstr` is a valid
    // NUL-terminated CString that outlives the call; `dev` is the numeric
    // major:minor obtained from libc::makedev. The kernel creates or
    // rejects the node; no user-space buffers are written. rustix 0.38
    // exposes no safe mknod wrapper.
    let rc = unsafe { libc::mknod(path_cstr.as_ptr(), mode, dev) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            nmbl_warn!("loop: mknod {} failed: {}", LOOP_CONTROL_PATH, e);
        }
    }
}

/// Open `/dev/loop-control` and run `LOOP_CTL_GET_FREE`.
///
/// Returns the index of a free `/dev/loopN`. The caller is expected
/// to immediately open `/dev/loopN` and feed it to
/// [`configure_loop_device`]; the chosen index can race with another
/// process if `/dev/loop-control` is shared, but in the NMBL initrd
/// nothing else is running yet.
pub fn allocate_loop_device() -> Result<u32> {
    // NMBL has no udev. On most kernels devtmpfs materialises
    // `/dev/loop-control` the moment the `loop` module loads, but
    // belt-and-braces: if the node is still absent (and the module is
    // loaded, so sysfs exposes its major:minor) create it with mknod,
    // mirroring `sys::btrfs::ensure_btrfs_control`. Without this the
    // open below would fail with ENOENT.
    ensure_loop_control();
    let control = rustix::fs::open(
        LOOP_CONTROL_PATH,
        OFlags::RDWR | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| NmblError::Io {
        source: io_error_from_rustix(e),
        context: format!("opening {LOOP_CONTROL_PATH}"),
    })?;

    // SAFETY: `LOOP_CTL_GET_FREE` is a legacy-style opcode (just a
    // raw number, no direction/size bits) and `control` is a valid
    // fd to `/dev/loop-control`. `LoopCtlGetFree`'s `Ioctl` impl
    // matches the kernel contract (null arg pointer, integer return
    // value).
    let raw =
        unsafe { rustix::ioctl::ioctl(&control, LoopCtlGetFree) }.map_err(|e| NmblError::Io {
            source: io_error_from_rustix(e),
            context: format!("LOOP_CTL_GET_FREE on {LOOP_CONTROL_PATH}"),
        })?;

    if raw < 0 {
        return Err(NmblError::Io {
            source: std::io::Error::from_raw_os_error(libc::EINVAL),
            context: format!("LOOP_CTL_GET_FREE on {LOOP_CONTROL_PATH} returned negative ({raw})"),
        });
    }
    // Cast through u32 — Linux's loop minor numbers fit comfortably.
    Ok(raw as u32)
}

/// Bind `backing_fd` to the loop device referred to by `loop_fd`
/// using `LOOP_CONFIGURE`.
///
/// `read_only=true` sets `LO_FLAGS_READ_ONLY`, which is what the
/// rescue path wants when the backing squashfs is itself read-only.
/// All other parameters (offset, size limit, block size, name) are
/// left at the kernel defaults.
pub fn configure_loop_device(
    loop_fd: &impl AsFd,
    backing_fd: &impl AsFd,
    read_only: bool,
) -> Result<()> {
    let backing = backing_fd.as_fd();
    // `loop_config.fd` is a u32 — Linux fds fit but a paranoid u32
    // cast catches any future move to a signed/wider fd type.
    let raw_fd: u32 = raw_fd_as_u32(backing)?;

    let mut config = LoopConfig::for_fd(raw_fd);
    if read_only {
        config.info.lo_flags |= LO_FLAGS_READ_ONLY;
    }

    // SAFETY: `LOOP_CONFIGURE` is a legacy-style opcode reading a
    // `struct loop_config` from userspace. `Setter` keeps `config`
    // alive (by value) for the duration of the ioctl, so the pointer
    // it hands to the kernel stays valid. `LoopConfig` is `#[repr(C)]`
    // with the kernel-mandated layout (asserted by
    // `config_size_matches_uapi`).
    let setter = unsafe { Setter::<BadOpcode<LOOP_CONFIGURE>, LoopConfig>::new(config) };

    // SAFETY: `loop_fd` is a borrowed fd to an opened `/dev/loopN`;
    // the ioctl number matches what that fd's driver expects.
    unsafe { rustix::ioctl::ioctl(loop_fd.as_fd(), setter) }.map_err(|e| NmblError::Io {
        source: io_error_from_rustix(e),
        context: format!("LOOP_CONFIGURE on loop fd (read_only={read_only})"),
    })?;
    Ok(())
}

/// Release the binding of a loop device via `LOOP_CLR_FD`.
///
/// Used by the rescue teardown path; the normal NMBL flow does not
/// unwind from the loop-mount, so this helper is provided mostly so
/// tests can clean up after themselves on hosts where they actually
/// got privileged access.
pub fn detach_loop_device(loop_fd: &impl AsFd) -> Result<()> {
    // SAFETY: `LOOP_CLR_FD` is a legacy-style opcode that takes no
    // argument pointer. `NoArg` matches that contract.
    let noarg = unsafe { NoArg::<BadOpcode<LOOP_CLR_FD>>::new() };
    // SAFETY: `loop_fd` is a borrowed fd to an opened `/dev/loopN`.
    unsafe { rustix::ioctl::ioctl(loop_fd.as_fd(), noarg) }.map_err(|e| NmblError::Io {
        source: io_error_from_rustix(e),
        context: "LOOP_CLR_FD on loop fd".to_string(),
    })?;
    Ok(())
}

/// Convenience for callers: open `/dev/loopN` after [`allocate_loop_device`]
/// returned `N`. `read_write=true` opens RW; `LOOP_CONFIGURE` (and the
/// other set-state ioctls) refuse to run on an RO fd unless the caller
/// holds `CAP_SYS_ADMIN`, so the bind path opens RW even when the
/// resulting block device will be read-only — the device's RO-ness is
/// set independently via `LO_FLAGS_READ_ONLY` plus the backing file's
/// own open mode.
pub fn open_loop_device(index: u32, read_write: bool) -> Result<OwnedFd> {
    let path = format!("/dev/loop{index}");
    let flags = if read_write {
        OFlags::RDWR | OFlags::CLOEXEC
    } else {
        OFlags::RDONLY | OFlags::CLOEXEC
    };
    rustix::fs::open(Path::new(&path), flags, Mode::empty()).map_err(|e| NmblError::Io {
        source: io_error_from_rustix(e),
        context: format!("opening {path}"),
    })
}

/// Map a `rustix::io::Errno` to a `std::io::Error` so we can stash it
/// in `NmblError::Io { source }`.
fn io_error_from_rustix(e: RustixErrno) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.raw_os_error())
}

/// Coerce a `BorrowedFd`'s raw integer to `u32` for the `loop_config.fd`
/// slot. Returns `EINVAL` if the fd doesn't fit (it always will on
/// Linux, but the explicit branch makes the cast lossless).
fn raw_fd_as_u32(fd: BorrowedFd<'_>) -> Result<u32> {
    use std::os::fd::AsRawFd;
    let raw = fd.as_raw_fd();
    if raw < 0 {
        return Err(NmblError::Io {
            source: std::io::Error::from_raw_os_error(libc::EBADF),
            context: format!("backing fd is negative ({raw})"),
        });
    }
    Ok(raw as u32)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::*;
    use std::mem::size_of;

    #[test]
    fn ioctl_constants_match_kernel_uapi() {
        // Pin the opcodes from <linux/loop.h> so a stray edit gets
        // caught in CI without booting a kernel.
        assert_eq!(LOOP_CTL_GET_FREE, 0x4C82);
        assert_eq!(LOOP_CONFIGURE, 0x4C0A);
        assert_eq!(LOOP_CLR_FD, 0x4C01);
        assert_eq!(LO_FLAGS_READ_ONLY, 1);
        assert_eq!(LO_NAME_SIZE, 64);
        assert_eq!(LO_KEY_SIZE, 32);
    }

    #[test]
    fn info_size_matches_uapi() {
        // 5*8 (u64s) + 4*4 (u32s) + 64 + 64 + 32 + 2*8 = 232.
        assert_eq!(size_of::<LoopInfo64>(), 232);
    }

    #[test]
    fn config_size_matches_uapi() {
        // Layout of `struct loop_config`:
        //   fd:         4
        //   block_size: 4
        //   info:     232  (LoopInfo64)
        //   reserved:  64  (8 * u64)
        //   total:    304
        // Must match what the running kernel reads.
        assert_eq!(size_of::<LoopConfig>(), 304);
    }

    #[test]
    fn loop_info64_zeroed_is_all_zero() {
        let info = LoopInfo64::zeroed();
        assert_eq!(info.lo_device, 0);
        assert_eq!(info.lo_inode, 0);
        assert_eq!(info.lo_rdevice, 0);
        assert_eq!(info.lo_offset, 0);
        assert_eq!(info.lo_sizelimit, 0);
        assert_eq!(info.lo_number, 0);
        assert_eq!(info.lo_encrypt_type, 0);
        assert_eq!(info.lo_encrypt_key_size, 0);
        assert_eq!(info.lo_flags, 0);
        assert!(info.lo_file_name.iter().all(|b| *b == 0));
        assert!(info.lo_crypt_name.iter().all(|b| *b == 0));
        assert!(info.lo_encrypt_key.iter().all(|b| *b == 0));
        assert_eq!(info.lo_init, [0, 0]);
    }

    #[test]
    fn loop_config_for_fd_only_sets_fd() {
        let cfg = LoopConfig::for_fd(7);
        assert_eq!(cfg.fd, 7);
        assert_eq!(cfg.block_size, 0);
        assert_eq!(cfg.info.lo_flags, 0);
        assert_eq!(cfg.info.lo_offset, 0);
        assert_eq!(cfg.info.lo_sizelimit, 0);
        assert_eq!(cfg.reserved, [0; 8]);
    }

    /// Skip the test body when `/dev/loop-control` isn't present
    /// (sandboxed CI, unprivileged container). Returning `true` from
    /// the helper means "skipped".
    fn no_loop_control() -> bool {
        !Path::new(LOOP_CONTROL_PATH).exists()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn allocate_loop_device_returns_ok_when_control_present() {
        if no_loop_control() {
            eprintln!("skipping: {LOOP_CONTROL_PATH} not present");
            return;
        }
        match allocate_loop_device() {
            Ok(n) => {
                // Loop minors are conventionally < 1<<20; just sanity
                // check we got a plausible value.
                assert!(n < 1_000_000, "loop index {n} looks bogus");
            }
            Err(e) => {
                // Unprivileged sandboxes can still see the node but
                // refuse the ioctl with EPERM/EACCES. Treat that as
                // a skip rather than a hard failure.
                eprintln!("skipping: LOOP_CTL_GET_FREE failed: {e}");
            }
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn configure_loop_device_against_tempfile() {
        if no_loop_control() {
            eprintln!("skipping: {LOOP_CONTROL_PATH} not present");
            return;
        }
        let index = match allocate_loop_device() {
            Ok(n) => n,
            Err(e) => {
                eprintln!("skipping: allocate failed: {e}");
                return;
            }
        };
        let loop_fd = match open_loop_device(index, true) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("skipping: open /dev/loop{index} failed: {e}");
                return;
            }
        };

        // 1 MiB temporary backing file — big enough for the kernel
        // to accept as a loop backing.
        let mut tmp = tempfile::tempfile().expect("tempfile");
        use std::io::Write as _;
        tmp.write_all(&vec![0u8; 1024 * 1024]).expect("fill tmp");
        tmp.flush().expect("flush tmp");

        match configure_loop_device(&loop_fd, &tmp, true) {
            Ok(()) => {
                // Tidy up so we don't leak the binding for subsequent
                // test runs on the same host.
                let _ = detach_loop_device(&loop_fd);
            }
            Err(e) => {
                eprintln!("skipping: LOOP_CONFIGURE failed: {e}");
            }
        }
    }
}
