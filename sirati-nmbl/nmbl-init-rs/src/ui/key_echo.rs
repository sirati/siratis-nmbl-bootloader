//! Key-echo diagnostic loop.
//!
//! Hidden test screen: not reachable from normal boot. The orchestrator
//! enters this loop only when the kernel cmdline carries
//! `nmbl.key_echo=1`. It renders [`crate::ui::app::Screen::KeyEcho`],
//! polls the boot console for keys, and appends every event to two
//! ring buffers — parsed `KeyEvent`s on the left, raw bytes from the
//! lower layer on the right (the splash input layer also tees the raw
//! bytes to `nmbl_warn!`, so the same trace lands on serial).
//!
//! Why this exists: VNC → splash key delivery has had cascading bugs
//! (VT_ACTIVATE without WAITACTIVE, keyboard layer in K_RAW, parser
//! dropping CSI sequences, …). Hypothesis-stacking didn't fix it; this
//! harness lets the operator (or an automated `sendkey` driver) see
//! exactly what each layer produces and where the chain breaks.
//!
//! The loop exits on Ctrl+C (the only safe universal escape: Esc is a
//! meaningful key event we want to *display*, not consume as an exit
//! signal). Returning lets the caller route to whatever shutdown path
//! it likes — typically [`crate::shell::drop_to_emergency`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::error::Result;
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::Console;

/// Drive the key-echo screen on the supplied console until the
/// operator hits Ctrl+C. Every keypress is appended to both ring
/// buffers and re-rendered immediately. Polling errors propagate so
/// the caller can log and drop to emergency.
///
/// `Ctrl+C` is `KeyCode::Char('c')` with `CONTROL`. We also accept
/// `Ctrl+\\` (the SIGQUIT key) as a backup escape in case a particular
/// VNC client mangles `^C` — same reasoning as why the loop avoids
/// using Esc.
pub fn run_key_echo_loop(console: &mut dyn Console) -> Result<()> {
    let mut app = App::key_echo();

    // Paint the empty screen once so the operator immediately sees the
    // chrome (two panels + footer) and knows the harness is live —
    // otherwise the screen would stay blank until the first keypress.
    console.render(&app)?;

    loop {
        let maybe = console.poll_key(POLL_SLICE)?;
        let Some(key) = maybe else {
            continue;
        };
        if is_exit_chord(&key) {
            return Ok(());
        }
        app.push_key_echo_event(describe_key(&key));
        console.render(&app)?;
    }
}

/// Return `true` for the exit chords: Ctrl+C and Ctrl+\\. Anything
/// else is logged to the screen so the operator can see what the
/// console emits.
fn is_exit_chord(key: &KeyEvent) -> bool {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('\\'))
}

/// Human-readable rendering of a `KeyEvent` for the events panel.
fn describe_key(key: &KeyEvent) -> String {
    format!("{:?} {:?}", key.code, key.modifiers)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_exit_chord() {
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_exit_chord(&k));
    }

    #[test]
    fn plain_c_is_not_exit_chord() {
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_exit_chord(&k));
    }

    #[test]
    fn esc_is_not_exit_chord() {
        // Esc is a key we want to *display*, not consume. Pin it.
        let k = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(!is_exit_chord(&k));
        let k = KeyEvent::new(KeyCode::Esc, KeyModifiers::CONTROL);
        assert!(!is_exit_chord(&k));
    }

    #[test]
    fn describe_key_prints_code_and_modifiers() {
        // crossterm's Debug for `KeyModifiers::NONE` renders as the
        // bitflags form (`KeyModifiers(0x0)`), not the name `NONE`.
        // The assertion just pins that the modifiers field shows up
        // *somewhere* in the rendering; the surrounding diagnostic
        // loop also tees the event to `nmbl_warn!`, so the operator
        // gets the textual form on serial regardless of how
        // `describe_key` formats it.
        let k = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let s = describe_key(&k);
        assert!(s.contains("Char('a')"), "missing code: {s}");
        assert!(s.contains("KeyModifiers"), "missing modifiers field: {s}");
    }
}
