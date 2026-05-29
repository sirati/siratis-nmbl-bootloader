use crossterm::event::{KeyCode as CtKeyCode, KeyEvent as CtKeyEvent, KeyModifiers as CtMods};
use termwiz::input::{
    InputEvent, InputParser as TwInputParser, KeyCode as TwKeyCode, Modifiers as TwMods,
};

/// Stateful translator wrapping [`termwiz::input::InputParser`] so the
/// caller hands in raw bytes and pulls out
/// `Option<crossterm::event::KeyEvent>` per recognised key. Mouse and
/// paste events are dropped; the App state machine doesn't bind them.
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
    /// from SIGWINCH) are discarded — NMBL doesn't bind them and the
    /// CSI 8t pre-filter handles the resize path that matters.
    ///
    /// `maybe_more` is forwarded to termwiz so it knows whether a
    /// dangling `ESC` should commit as Esc (false) or wait for more
    /// bytes (true).
    pub(crate) fn feed(&mut self, bytes: &[u8], maybe_more: bool, out: &mut Vec<CtKeyEvent>) {
        let events = self.inner.parse_as_vec(bytes, maybe_more);
        for ev in events {
            if let Some(k) = to_crossterm_key(&ev) {
                out.push(k);
            }
        }
    }
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

    #[test]
    fn termwiz_to_crossterm_arrow_up() {
        let mut t = TermwizToCrossterm::new();
        let mut out = Vec::new();
        t.feed(b"\x1b[A", false, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Up);
    }

    #[test]
    fn termwiz_to_crossterm_enter_via_cr() {
        let mut t = TermwizToCrossterm::new();
        let mut out = Vec::new();
        t.feed(b"\r", false, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Enter);
    }

    #[test]
    fn termwiz_to_crossterm_plain_a() {
        let mut t = TermwizToCrossterm::new();
        let mut out = Vec::new();
        t.feed(b"a", false, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Char('a'));
        assert_eq!(out[0].modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn termwiz_to_crossterm_ctrl_c() {
        let mut t = TermwizToCrossterm::new();
        let mut out = Vec::new();
        t.feed(&[0x03], false, &mut out);
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
        let mut out = Vec::new();
        t.feed(b"\x1b[1;6A", false, &mut out);
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
        let mut out = Vec::new();
        t.feed(b"\x1b[A", false, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Up);
        assert_eq!(out[0].modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn termwiz_to_crossterm_esc_alone() {
        let mut t = TermwizToCrossterm::new();
        let mut out = Vec::new();
        // `maybe_more = false` tells termwiz to commit Esc.
        t.feed(b"\x1b", false, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, KeyCode::Esc);
    }
}
