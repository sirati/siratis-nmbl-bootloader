//! Kernel-VT keyboard-LED queries.
//!
//! The LUKS passphrase prompt wants to warn the operator when Caps Lock
//! is engaged (a wrong-passphrase magnet). On a kernel VT — both the
//! splash framebuffer (input on `/dev/tty1`) and the tty backend
//! (`/dev/console` when it is a VT) — the current keyboard lock state is
//! readable with the `KDGKBLED` ioctl: it returns the keyboard's logical
//! lock flags in the low three bits, with Caps Lock at [`LED_CAP`].
//!
//! This module exposes a single best-effort query,
//! [`caps_lock_active`], that takes a borrowed fd. On a non-VT line (a
//! serial console, or the mock harness's stdin) the ioctl fails with
//! `ENOTTY`; we map that — and any other failure — to `None`
//! ("unknown"), so the caller simply never shows the warning rather than
//! erroring out. The passphrase prompt must never break because the
//! keyboard couldn't be queried.
//!
//! ## `KDGKBLED` vs `KDGETLED`
//!
//! `KDGETLED` reports the *physical* LED state, which userspace can
//! override with `KDSETLED` and which therefore may not reflect the real
//! lock state. `KDGKBLED` reports the keyboard driver's *logical* lock
//! flags — the actual Caps/Num/Scroll lock the next keystroke will be
//! translated under — which is exactly what we want for the warning.
//! See `linux/kd.h`.
//!
//! ## Why raw `libc::ioctl`
//!
//! rustix 0.38 (the pinned version) exposes no wrapper for the `kd`
//! keyboard ioctls, mirroring the situation in
//! [`crate::ui::console::tty`] for `KDGETMODE`/`KDSETMODE`. We keep the
//! single `unsafe` call tightly scoped and documented with a SAFETY
//! comment naming the kernel contract.

use std::os::fd::{AsFd, AsRawFd};

/// `linux/kd.h`: read the current keyboard lock flags. The third ioctl
/// argument is a pointer to a `char` the kernel writes the flag bits
/// into. Stable kernel ABI.
const KDGKBLED: libc::Ioctl = 0x4B64;

/// Caps-Lock bit within the `KDGKBLED` result (`linux/kd.h`: `LED_CAP`).
const LED_CAP: libc::c_char = 0x04;

/// Best-effort query of the keyboard's Caps-Lock lock state on the VT
/// bound to `fd`.
///
/// Returns:
/// - `Some(true)`  — Caps Lock is engaged on this VT keyboard.
/// - `Some(false)` — Caps Lock is off.
/// - `None`        — the state is unknown: `fd` is not a VT (serial line
///   → `ENOTTY`) or the ioctl otherwise failed. Callers must treat
///   `None` as "do not show the warning".
///
/// Never panics; never logs (it is polled every render tick, so a noisy
/// failure path would flood the ring). The caller decides what to do
/// with `None`.
pub fn caps_lock_active<F: AsFd>(fd: F) -> Option<bool> {
    let mut flags: libc::c_char = 0;
    // SAFETY: KDGKBLED (linux/kd.h) writes the keyboard lock flags as a
    // single `char` through the pointer in the third ioctl argument.
    // `&mut flags` is a valid, properly-aligned pointer to a live
    // `c_char` that outlives the call. The fd is a live open file
    // descriptor by the `AsFd` contract. On a non-VT fd the kernel
    // returns -1/ENOTTY without touching the pointer, which we handle
    // below.
    let rc = unsafe { libc::ioctl(fd.as_fd().as_raw_fd(), KDGKBLED, &mut flags) };
    if rc < 0 {
        // ENOTTY (serial line, /dev/null, …) and any other error map to
        // "unknown" — the prompt degrades to never showing the warning.
        return None;
    }
    Some(flags & LED_CAP != 0)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    /// `/dev/null` is not a VT, so `KDGKBLED` returns ENOTTY and the
    /// query must degrade to `None` (unknown) — the exact graceful path
    /// the serial backend relies on.
    #[test]
    fn caps_lock_on_non_vt_is_none() {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            Ok(f) => f,
            // Extremely sandboxed env without /dev/null — skip.
            Err(_) => return,
        };
        assert_eq!(
            caps_lock_active(&file),
            None,
            "non-VT fd must yield None (unknown), never panic or error"
        );
    }

    /// `LED_CAP` must be the documented 0x04 bit. A regression here would
    /// silently mask the wrong lock (Num/Scroll) and the warning would
    /// fire on the wrong key.
    #[test]
    fn led_cap_constant_is_0x04() {
        assert_eq!(LED_CAP, 0x04);
    }
}
