//! VT graphics-mode helpers: `KDGETMODE` / `KDSETMODE` ioctls.
//!
//! When `/dev/console` is bound to a kernel VT (the framebuffer case,
//! not a serial line), the kernel keeps writing printk output to the
//! same framebuffer the TUI is drawing to. We `ioctl(KDSETMODE,
//! KD_GRAPHICS)` to suppress that until [`Drop`]; on non-VT lines
//! (serial console) the ioctl returns `ENOTTY` and we tolerate it.
//!
//! rustix 0.38 does not expose a wrapper for the kd ioctls, so this
//! module contains one tightly-scoped `unsafe { libc::ioctl(...) }` per
//! direction, each documented with a SAFETY comment naming the kernel
//! contract (linux/kd.h).

use std::os::fd::{AsRawFd, BorrowedFd};

use crate::nmbl_warn;

/// `linux/kd.h` ioctl numbers. Stable kernel ABI.
pub(super) const KDGETMODE: libc::Ioctl = 0x4B3B;
pub(super) const KDSETMODE: libc::Ioctl = 0x4B3A;
/// VT in graphics mode: kernel stops painting printk to the framebuffer.
pub(super) const KD_GRAPHICS: libc::c_long = 0x01;
/// VT in text mode (the default). Only referenced by tests; production
/// code never hard-codes `KD_TEXT` — it always restores the mode value
/// captured by `KDGETMODE` so we don't clobber a pre-graphics setup.
#[cfg(test)]
pub(super) const KD_TEXT: libc::c_long = 0x00;

/// Try to switch `fd`'s VT into `KD_GRAPHICS`.
pub(super) fn enter_kd_graphics(fd: BorrowedFd<'_>) -> Option<libc::c_long> {
    let mut mode: libc::c_long = 0;
    // SAFETY: KDGETMODE (linux/kd.h) reads an `unsigned long` through
    // the pointer in the third ioctl argument. `&mut mode` is a valid,
    // properly-aligned pointer to a live `c_long` that outlives the
    // call. The fd is a live open file descriptor by contract.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDGETMODE, &mut mode) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOTTY) {
            nmbl_warn!(
                "KDGETMODE on console fd {} failed: {err}; \
                 leaving VT in current mode (printk may bleed into TUI)",
                fd.as_raw_fd()
            );
        }
        return None;
    }
    if mode == KD_GRAPHICS {
        return None;
    }
    // SAFETY: KDSETMODE (linux/kd.h) takes its third argument as an
    // `unsigned long` value. The fd is a live open VT.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSETMODE, KD_GRAPHICS) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "KDSETMODE(KD_GRAPHICS) on console fd {} failed: {err}; \
             printk may bleed into TUI",
            fd.as_raw_fd()
        );
        return None;
    }
    Some(mode)
}

pub(super) fn restore_kd_mode(fd: BorrowedFd<'_>, previous: libc::c_long) {
    // SAFETY: same contract as `enter_kd_graphics`.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSETMODE, previous) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "KDSETMODE restore on console fd {} failed: {err}; \
             VT may remain in graphics mode (try `kbd_mode -a`)",
            fd.as_raw_fd()
        );
    }
}
