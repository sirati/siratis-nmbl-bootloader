use crossterm::event::{KeyCode as CtKeyCode, KeyEvent as CtKeyEvent, KeyModifiers as CtMods};
use termwiz::input::{
    InputEvent, InputParser as TwInputParser, KeyCode as TwKeyCode, Modifiers as TwMods,
    MouseButtons as TwMouseButtons,
};

use crate::ui::console::ConsoleEvent;

/// Stateful translator wrapping [`termwiz::input::InputParser`] so the
/// caller hands in raw bytes and pulls out
/// `Option<crossterm::event::KeyEvent>` per recognised key plus any
/// mouse-wheel scroll notches. Non-wheel mouse events (motion, clicks)
/// and paste are dropped; the App state machine doesn't bind them.
pub(crate) struct TermwizToCrossterm {
    inner: TwInputParser,
}

impl TermwizToCrossterm {
    pub(crate) fn new() -> Self {
        Self {
            inner: TwInputParser::new(),
        }
    }

    /// Feed bytes into termwiz; drain matching events into `out` as
    /// crossterm `KeyEvent`s. Non-key events (mouse, paste, resize
    /// from SIGWINCH) are discarded — callers that only bind keys (the
    /// splash and mock backends) use this. The CSI 8t pre-filter handles
    /// the resize path that matters.
    ///
    /// `maybe_more` is forwarded to termwiz so it knows whether a
    /// dangling `ESC` should commit as Esc (false) or wait for more
    /// bytes (true).
    ///
    /// Gated to the backends that actually use the key-only shape: the
    /// splash kernel-VT path (no xterm mouse sequences) and the mock
    /// console. The always-compiled tty backend uses [`Self::feed_events`]
    /// directly, so without this gate `feed` would be dead code in the
    /// default-feature lib build that crane clippy enforces.
    #[cfg(any(feature = "image-splash", feature = "mocking"))]
    pub(crate) fn feed(&mut self, bytes: &[u8], maybe_more: bool, out: &mut Vec<CtKeyEvent>) {
        // Reuse the richer path with a throwaway scroll sink so the two
        // entry points can never drift; key-only callers just discard
        // wheel notches (the splash kernel-VT path never produces any).
        let mut scrolls = Vec::new();
        self.feed_events(bytes, maybe_more, out, &mut scrolls);
    }

    /// Like [`Self::feed`], but also surfaces vertical mouse-wheel
    /// notches as [`ConsoleEvent::Scroll`] into `scrolls`. Used by the
    /// tty backend, the only input path that carries xterm mouse
    /// sequences. Non-wheel mouse events (motion, clicks) and paste are
    /// still dropped.
    pub(crate) fn feed_events(
        &mut self,
        bytes: &[u8],
        maybe_more: bool,
        keys: &mut Vec<CtKeyEvent>,
        scrolls: &mut Vec<ConsoleEvent>,
    ) {
        let events = self.inner.parse_as_vec(bytes, maybe_more);
        for ev in events {
            if let Some(k) = to_crossterm_key(&ev) {
                keys.push(k);
            } else if let Some(s) = to_scroll_event(&ev) {
                scrolls.push(s);
            }
        }
    }
}

/// Map a termwiz `InputEvent::Mouse` carrying a vertical-wheel notch
/// into a [`ConsoleEvent::Scroll`]. Wheel-up (xterm button 64 / termwiz
/// `VERT_WHEEL | WHEEL_POSITIVE`) scrolls toward older scrollback;
/// wheel-down (button 65 / `VERT_WHEEL` without `WHEEL_POSITIVE`)
/// scrolls toward the live tail. Returns `None` for non-wheel mouse
/// events and for horizontal-wheel notches (NMBL has no horizontal
/// scrollback).
fn to_scroll_event(ev: &InputEvent) -> Option<ConsoleEvent> {
    let InputEvent::Mouse(m) = ev else {
        return None;
    };
    if !m.mouse_buttons.contains(TwMouseButtons::VERT_WHEEL) {
        return None;
    }
    let up = m.mouse_buttons.contains(TwMouseButtons::WHEEL_POSITIVE);
    Some(ConsoleEvent::Scroll { up })
}

/// Map a single termwiz `InputEvent::Key` into the
/// `crossterm::event::KeyEvent` shape the rest of the UI matches
/// against. Returns `None` for non-key variants and for key variants
/// the App state machine never binds (function keys, modifier-only
/// presses, etc.).
fn to_crossterm_key(ev: &InputEvent) -> Option<CtKeyEvent> {
    let InputEvent::Key(k) = ev else { return None };
    let modifiers = to_crossterm_mods(k.modifiers);
    let code = match k.key {
        TwKeyCode::Char(c) => CtKeyCode::Char(c),
        TwKeyCode::Enter => CtKeyCode::Enter,
        TwKeyCode::Tab => CtKeyCode::Tab,
        TwKeyCode::Backspace => CtKeyCode::Backspace,
        TwKeyCode::Escape => CtKeyCode::Esc,
        TwKeyCode::LeftArrow => CtKeyCode::Left,
        TwKeyCode::RightArrow => CtKeyCode::Right,
        TwKeyCode::UpArrow => CtKeyCode::Up,
        TwKeyCode::DownArrow => CtKeyCode::Down,
        TwKeyCode::Home | TwKeyCode::KeyPadHome => CtKeyCode::Home,
        TwKeyCode::End | TwKeyCode::KeyPadEnd => CtKeyCode::End,
        TwKeyCode::PageUp | TwKeyCode::KeyPadPageUp => CtKeyCode::PageUp,
        TwKeyCode::PageDown | TwKeyCode::KeyPadPageDown => CtKeyCode::PageDown,
        TwKeyCode::Insert => CtKeyCode::Insert,
        TwKeyCode::Delete => CtKeyCode::Delete,
        TwKeyCode::Function(n) => CtKeyCode::F(n),
        // Everything else (modifier-only, media keys, application
        // arrow variants, etc.) — NMBL doesn't bind them; drop.
        _ => return None,
    };
    Some(CtKeyEvent::new(code, modifiers))
}

fn to_crossterm_mods(m: TwMods) -> CtMods {
    let mut out = CtMods::NONE;
    if m.contains(TwMods::SHIFT) {
        out |= CtMods::SHIFT;
    }
    if m.contains(TwMods::CTRL) {
        out |= CtMods::CONTROL;
    }
    if m.contains(TwMods::ALT) {
        out |= CtMods::ALT;
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    /// Feed bytes and return the key events — most parser tests only
    /// care about the key path. Uses `feed_events` (always compiled)
    /// rather than the feature-gated key-only `feed`.
    fn feed_keys(t: &mut TermwizToCrossterm, bytes: &[u8], maybe_more: bool) -> Vec<CtKeyEvent> {
        let mut keys = Vec::new();
        let mut scrolls = Vec::new();
        t.feed_events(bytes, maybe_more, &mut keys, &mut scrolls);
        keys
    }

    #[test]
    fn termwiz_to_crossterm_arrow_up() {
        let mut t = TermwizToCrossterm::new();
        let out = feed_keys(&mut t, b"\x1b[A", false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Up);
    }

    #[test]
    fn termwiz_to_crossterm_enter_via_cr() {
        let mut t = TermwizToCrossterm::new();
        let out = feed_keys(&mut t, b"\r", false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Enter);
    }

    #[test]
    fn termwiz_to_crossterm_plain_a() {
        let mut t = TermwizToCrossterm::new();
        let out = feed_keys(&mut t, b"a", false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Char('a'));
        assert_eq!(out[0].modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn termwiz_to_crossterm_ctrl_c() {
        let mut t = TermwizToCrossterm::new();
        let out = feed_keys(&mut t, &[0x03], false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Char('c'));
        assert!(out[0].modifiers.contains(KeyModifiers::CONTROL));
    }

    /// A userspace terminal emulator (the proven tty/console path)
    /// emits the modifier-encoded `ESC [ 1 ; 6 A` for Ctrl+Shift+Up,
    /// and the shared parser decodes the modifiers correctly. The
    /// kernel VT (splash path) instead collapses the chord onto the
    /// bare `ESC [ A`; this pins both facts so the splash-side
    /// shift-state recovery (`splash::input::read_shift_state`) stays
    /// justified.
    #[test]
    fn modifier_encoded_ctrl_shift_up_decodes() {
        let mut t = TermwizToCrossterm::new();
        let out = feed_keys(&mut t, b"\x1b[1;6A", false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Up);
        assert_eq!(
            out[0].modifiers,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT
        );
    }

    #[test]
    fn bare_csi_up_carries_no_modifiers() {
        // What the kernel VT actually delivers for Ctrl+Shift+Up.
        let mut t = TermwizToCrossterm::new();
        let out = feed_keys(&mut t, b"\x1b[A", false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Up);
        assert_eq!(out[0].modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn termwiz_to_crossterm_esc_alone() {
        let mut t = TermwizToCrossterm::new();
        // `maybe_more = false` tells termwiz to commit Esc.
        let out = feed_keys(&mut t, b"\x1b", false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Esc);
    }

    /// An xterm SGR mouse wheel-up report (`ESC [ < 64 ; col ; row M`)
    /// surfaces as `ConsoleEvent::Scroll { up: true }` and produces no
    /// key event; wheel-down (button 65) surfaces `up: false`.
    #[test]
    fn sgr_mouse_wheel_up_and_down_surface_scroll() {
        let mut t = TermwizToCrossterm::new();
        let mut keys = Vec::new();
        let mut scrolls = Vec::new();
        t.feed_events(b"\x1b[<64;10;10M", false, &mut keys, &mut scrolls);
        assert!(keys.is_empty(), "wheel must not produce a key event");
        assert_eq!(scrolls, vec![ConsoleEvent::Scroll { up: true }]);

        keys.clear();
        scrolls.clear();
        t.feed_events(b"\x1b[<65;10;10M", false, &mut keys, &mut scrolls);
        assert!(keys.is_empty(), "wheel must not produce a key event");
        assert_eq!(scrolls, vec![ConsoleEvent::Scroll { up: false }]);
    }

    /// A non-wheel mouse click (`ESC [ < 0 ; col ; row M`, left button)
    /// is dropped: no key and no scroll.
    #[test]
    fn sgr_mouse_click_dropped() {
        let mut t = TermwizToCrossterm::new();
        let mut keys = Vec::new();
        let mut scrolls = Vec::new();
        t.feed_events(b"\x1b[<0;10;10M", false, &mut keys, &mut scrolls);
        assert!(keys.is_empty());
        assert!(scrolls.is_empty(), "clicks are not in scope");
    }
}
