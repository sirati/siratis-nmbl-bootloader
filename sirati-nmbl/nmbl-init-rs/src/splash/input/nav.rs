//! Navigation-key helpers and VT shift-state recovery.
//!
//! The kernel VT keyboard layer in `K_XLATE` collapses Ctrl/Shift+cursor
//! chords onto the bare CSI form. These helpers detect navigation keys
//! and re-attach the live shift-state queried via `TIOCLINUX`.

use std::os::fd::OwnedFd;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// True for the cursor / paging keys whose Ctrl/Shift chords the kernel
/// VT collapses onto the bare CSI form (see [`super::SplashInput::poll`]).
/// These are exactly the keys the pretty shell binds to scrollback in
/// `pretty_shell::handle_key`; recovering modifiers for anything else
/// would risk mis-tagging keys whose `K_XLATE` byte already encodes the
/// modifier (e.g. `0x03` → Ctrl+C, capital letters → Shift).
pub(super) fn is_navigation_key(k: &KeyEvent) -> bool {
    matches!(
        k.code,
        KeyCode::Up
            | KeyCode::Down
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End
    )
}

/// Merge `recovered` modifiers onto every navigation key in `keys`.
/// Non-navigation keys are left untouched (their `K_XLATE` byte already
/// carries any modifier the parser could derive). Pure so it can be
/// unit-tested without a VT fd.
pub(super) fn merge_recovered_mods(keys: &mut [KeyEvent], recovered: KeyModifiers) {
    for k in keys {
        if is_navigation_key(k) {
            k.modifiers |= recovered;
        }
    }
}

/// Recover the live Ctrl/Shift modifier state from the kernel VT via
/// `TIOCLINUX` subcode 6 (`TIOCL_GETSHIFTSTATE`).
///
/// The ioctl writes a single-byte bitmask back through the buffer whose
/// first byte we seed with the subcode. The kernel's `shift_state` bits
/// are `1=Shift`, `2=AltGr`, `4=Control`, `8=Alt` (linux/keyboard.h
/// `KG_*`). We map Shift and Control onto crossterm; AltGr/Alt are not
/// bound by the pretty shell and are ignored.
///
/// Failure is non-fatal and common (serial lines / non-VT fds return
/// `ENOTTY`, an unprivileged caller may get `EPERM`): we return
/// `KeyModifiers::NONE`, leaving the key unmodified exactly as before
/// this fix. No warning is logged because this runs on every navigation
/// keypress and a non-VT line would spam the log.
pub(super) fn read_shift_state(fd: &OwnedFd) -> KeyModifiers {
    use std::os::fd::AsRawFd as _;
    const TIOCL_GETSHIFTSTATE: u8 = 6;
    const KG_SHIFT: u8 = 0x01;
    const KG_CTRL: u8 = 0x04;
    // The buffer doubles as input (subcode in byte 0) and output (the
    // kernel overwrites byte 0 with the shift-state bitmask).
    let mut arg: [u8; 1] = [TIOCL_GETSHIFTSTATE];
    // SAFETY: TIOCLINUX (linux/tiocl.h) takes a pointer to a buffer in
    // the third ioctl argument; for subcode 6 the kernel reads the
    // subcode from byte 0 and writes the 1-byte shift-state result back
    // into the same byte. `arg` is a live, properly-aligned 1-byte
    // buffer that outlives the call. The fd is a live open char device
    // by the function contract; non-VT fds return ENOTTY which we
    // tolerate below.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCLINUX, arg.as_mut_ptr()) };
    if rc < 0 {
        return KeyModifiers::NONE;
    }
    let bits = arg.first().copied().unwrap_or(0);
    let mut out = KeyModifiers::NONE;
    if bits & KG_SHIFT != 0 {
        out |= KeyModifiers::SHIFT;
    }
    if bits & KG_CTRL != 0 {
        out |= KeyModifiers::CONTROL;
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn navigation_keys_are_recognised() {
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            assert!(is_navigation_key(&press(code)), "{code:?} must be nav");
        }
        for code in [KeyCode::Char('a'), KeyCode::Enter, KeyCode::Esc] {
            assert!(!is_navigation_key(&press(code)), "{code:?} must not be nav");
        }
    }

    /// The regression: Ctrl+Shift+Up arrives off the kernel VT as the
    /// bare `ESC [ A` (`KeyCode::Up` + NONE — see the captured bytes in
    /// the commit message). Merging the recovered shift-state must
    /// reattach CONTROL|SHIFT so `pretty_shell::handle_key`'s scroll
    /// binding fires.
    #[test]
    fn merge_reattaches_ctrl_shift_to_arrow() {
        let mut keys = vec![press(KeyCode::Up)];
        merge_recovered_mods(&mut keys, KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        assert_eq!(
            keys.first().expect("one key").modifiers,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        );
    }

    /// Recovered modifiers must NOT leak onto non-navigation keys: a
    /// typed character whose `K_XLATE` byte already encodes the shift
    /// (capital letter) must keep the parser's own modifiers.
    #[test]
    fn merge_leaves_char_keys_untouched() {
        let mut keys = vec![press(KeyCode::Char('a')), press(KeyCode::Down)];
        merge_recovered_mods(&mut keys, KeyModifiers::CONTROL);
        assert_eq!(
            keys.first().expect("char key").modifiers,
            KeyModifiers::NONE,
            "char key must be untouched"
        );
        assert_eq!(
            keys.get(1).expect("arrow key").modifiers,
            KeyModifiers::CONTROL,
            "arrow key must gain the recovered modifier"
        );
    }

    /// No recovered modifiers (the unmodified-Up case, or a non-VT fd
    /// where `read_shift_state` returns NONE) must leave keys as-is.
    #[test]
    fn merge_none_is_identity() {
        let mut keys = vec![press(KeyCode::Up)];
        merge_recovered_mods(&mut keys, KeyModifiers::NONE);
        assert_eq!(keys.first().expect("one key").modifiers, KeyModifiers::NONE);
    }
}
