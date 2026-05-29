//! VT ioctl helpers: activate the virtual terminal and pin the keyboard
//! layer to `K_XLATE`.

use std::os::fd::OwnedFd;

use crate::nmbl_warn;

/// Force the VT bound to `fd` into the foreground via `VT_ACTIVATE`,
/// then block until the switch completes via `VT_WAITACTIVE`.
///
/// `VT_ACTIVATE` is asynchronous: the kernel schedules the switch but
/// returns immediately. PS/2 / VNC keystrokes get demultiplexed to the
/// *currently foreground* VT at delivery time, so the first reads on
/// the splash fd race the switch and the early keys land on whichever
/// VT was foreground before us. `VT_WAITACTIVE` blocks until VT 1 is
/// actually the active VT, after which every subsequent keystroke
/// arrives on this fd.
///
/// On x86 the constants are `VT_ACTIVATE = 0x5606` and `VT_WAITACTIVE
/// = 0x5607`, with the third ioctl arg the 1-based VT number. Both
/// failures are non-fatal: we log and continue — the worst case is the
/// pre-fix behaviour where the operator sees the splash but can't drive
/// it. The two unsafe calls are documented in docs/architecture.md
/// alongside the other accepted ioctls (finit_module, kexec_file_load).
pub(super) fn activate_vt(fd: &OwnedFd) {
    use std::os::fd::AsRawFd as _;
    const VT_ACTIVATE: libc::Ioctl = 0x5606;
    const VT_WAITACTIVE: libc::Ioctl = 0x5607;
    // /dev/tty1 → VT 1. We always open VT1 (see splash::INPUT_TTY_PATH)
    // so the VT number is fixed.
    let vt_number: libc::c_int = 1;
    // SAFETY: VT_ACTIVATE takes an integer argument as the third ioctl
    // parameter; the kernel reads `vt_number` by value. The fd is a
    // live, open tty char device per the contract on this function.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), VT_ACTIVATE, vt_number) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "VT_ACTIVATE({vt_number}) on splash input fd failed: {err}; \
             keystrokes may not reach the splash"
        );
        // No point waiting for a switch we couldn't schedule.
        return;
    }
    // SAFETY: VT_WAITACTIVE has the same ABI as VT_ACTIVATE — third arg
    // is the target VT number as an integer value. The kernel blocks
    // until that VT is the foreground console (or returns EINTR on a
    // pending signal — early userspace has no async signal sources we
    // care about, but a stray EINTR is non-fatal and just collapses to
    // the warning path below).
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), VT_WAITACTIVE, vt_number) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "VT_WAITACTIVE({vt_number}) on splash input fd failed: {err}; \
             early keystrokes may race the VT switch"
        );
    }
}

/// Pin the VT keyboard layer to `K_XLATE` (the default mode that
/// translates scancodes to ANSI escape sequences). Defensive: an
/// earlier boot stage that left the line in `K_RAW` / `K_MEDIUMRAW`
/// would feed raw scancodes to our parser, which expects the ANSI
/// CSI forms (see [`parse_event`]) and would silently drop them.
///
/// Failure is non-fatal: on a non-VT fd `KDSKBMODE` returns `ENOTTY`,
/// which is the expected behaviour on serial consoles — log and move
/// on. Other failures (EPERM, EINVAL) are likewise tolerated because
/// the most common state is already-K_XLATE.
pub(super) fn set_kbd_xlate(fd: &OwnedFd) {
    use std::os::fd::AsRawFd as _;
    const KDSKBMODE: libc::Ioctl = 0x4B45;
    const K_XLATE: libc::c_long = 0x01;
    // SAFETY: KDSKBMODE (linux/kd.h) takes its third argument as an
    // `unsigned long` value (not a pointer). The kernel validates the
    // mode against the K_* set. The fd is a live open tty char device
    // by the function contract; non-VT fds return ENOTTY which we
    // tolerate below.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSKBMODE, K_XLATE) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOTTY) {
            nmbl_warn!(
                "KDSKBMODE(K_XLATE) on splash input fd failed: {err}; \
                 keystrokes may arrive as raw scancodes the parser ignores"
            );
        }
    }
}
