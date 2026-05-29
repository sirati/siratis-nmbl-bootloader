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
        // The raw bytes that drove this event live inside SplashInput
        // and are teed to serial via `nmbl_warn!` (see
        // `splash::input::log_raw_bytes`), but the [`Console`] trait
        // intentionally has no "what bytes did you just read" accessor
        // — adding one would burden every backend. As a substitute,
        // we derive the canonical VT byte sequence for the parsed
        // `KeyCode` and push that into the byte_log panel. It is the
        // *expected* byte stream the parser would receive in K_XLATE
        // mode for this code; mismatches against the
        // `SplashInput raw bytes:` serial trace are themselves a
        // diagnostic signal (parser disagreement, modifier loss, …).
        app.push_key_echo_bytes(synthesise_bytes(&key));
        console.render(&app)?;
    }
}

/// Build the canonical K_XLATE byte sequence for a `KeyEvent`, formatted
/// as space-separated lowercase hex (e.g. `"1b 5b 41"` for Up). For
/// codes the parser doesn't have a 1:1 byte mapping for (function keys,
/// modifiers, etc.) we return a textual `"<code>"` marker so the panel
/// still surfaces the event.
fn synthesise_bytes(key: &KeyEvent) -> String {
    let bytes: Vec<u8> = match key.code {
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Enter => vec![0x0d],
        KeyCode::Tab => vec![0x09],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Char(c) if c.is_ascii() => {
            // Ctrl+letter collapses to a C0 control byte; otherwise the
            // ASCII codepoint is the byte value.
            if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii_alphabetic() {
                let lc = c.to_ascii_lowercase() as u8;
                vec![lc.wrapping_sub(0x60)]
            } else {
                vec![c as u8]
            }
        }
        _ => return format!("<{:?}>", key.code),
    };
    let mut s = String::with_capacity(bytes.len().saturating_mul(3));
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02x}"));
    }
    s
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
    fn synthesise_bytes_known_codes() {
        let plain_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(synthesise_bytes(&plain_a), "61");

        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(synthesise_bytes(&up), "1b 5b 41");

        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(synthesise_bytes(&enter), "0d");

        // Ctrl+c -> C0 0x03.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(synthesise_bytes(&ctrl_c), "03");

        let bs = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(synthesise_bytes(&bs), "7f");
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
