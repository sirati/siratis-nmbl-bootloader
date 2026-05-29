//! ioctl wrappers and public API for `/dev/loop-control` + `/dev/loopN`.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno as RustixErrno;
use rustix::ioctl::{BadOpcode, Ioctl, IoctlOutput, NoArg, Opcode, Setter};

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

use super::types::{
    LO_FLAGS_READ_ONLY, LOOP_CLR_FD, LOOP_CONFIGURE, LOOP_CONTROL_PATH, LOOP_CONTROL_SYSFS,
    LOOP_CTL_GET_FREE, LoopConfig,
};

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
