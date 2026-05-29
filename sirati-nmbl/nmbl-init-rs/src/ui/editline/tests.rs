use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::EditableLine;
use super::ops::handle_key_on;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn alt(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::ALT)
}

#[test]
fn with_text_parks_cursor_at_end() {
    let line = EditableLine::with_text("hello");
    assert_eq!(line.text(), "hello");
    assert_eq!(line.cursor(), 5);
}

#[test]
fn insert_at_cursor_in_middle_of_string() {
    let mut line = EditableLine::with_text("helo");
    line.handle_key(key(KeyCode::Home));
    // Walk to between 'e' and 'l' (byte 2).
    line.handle_key(key(KeyCode::Right));
    line.handle_key(key(KeyCode::Right));
    line.handle_key(key(KeyCode::Char('l')));
    assert_eq!(line.text(), "hello");
    // Cursor advanced past the inserted 'l'.
    assert_eq!(line.cursor(), 3);
}

#[test]
fn left_right_home_end_navigation() {
    let mut line = EditableLine::with_text("abcd");
    assert_eq!(line.cursor(), 4);
    line.handle_key(key(KeyCode::Home));
    assert_eq!(line.cursor(), 0);
    line.handle_key(key(KeyCode::Right));
    assert_eq!(line.cursor(), 1);
    line.handle_key(key(KeyCode::End));
    assert_eq!(line.cursor(), 4);
    line.handle_key(key(KeyCode::Left));
    assert_eq!(line.cursor(), 3);
    // Saturation: Left at 0 stays at 0, Right at end stays at end.
    line.handle_key(key(KeyCode::Home));
    line.handle_key(key(KeyCode::Left));
    assert_eq!(line.cursor(), 0);
    line.handle_key(key(KeyCode::End));
    line.handle_key(key(KeyCode::Right));
    assert_eq!(line.cursor(), 4);
}

#[test]
fn backspace_at_cursor_removes_preceding_char() {
    let mut line = EditableLine::with_text("hello");
    line.handle_key(key(KeyCode::Home));
    line.handle_key(key(KeyCode::Right));
    line.handle_key(key(KeyCode::Right));
    line.handle_key(key(KeyCode::Right)); // cursor after first 'l' (byte 3)
    line.handle_key(key(KeyCode::Backspace)); // removes 'l' at byte 2
    assert_eq!(line.text(), "helo");
    assert_eq!(line.cursor(), 2);
    // Backspace at column 0 is a no-op.
    line.handle_key(key(KeyCode::Home));
    line.handle_key(key(KeyCode::Backspace));
    assert_eq!(line.text(), "helo");
    assert_eq!(line.cursor(), 0);
}

#[test]
fn delete_at_cursor_removes_following_char() {
    let mut line = EditableLine::with_text("hello");
    line.handle_key(key(KeyCode::Home));
    line.handle_key(key(KeyCode::Delete)); // removes 'h'
    assert_eq!(line.text(), "ello");
    assert_eq!(line.cursor(), 0);
    // Delete at end is a no-op.
    line.handle_key(key(KeyCode::End));
    line.handle_key(key(KeyCode::Delete));
    assert_eq!(line.text(), "ello");
    assert_eq!(line.cursor(), 4);
}

#[test]
fn multibyte_navigation_and_edit_never_panics() {
    // "héllo" — 'é' is two bytes. Walking by char must land on
    // boundaries, and backspacing across the 'é' must remove the
    // whole codepoint.
    let mut line = EditableLine::with_text("héllo");
    line.handle_key(key(KeyCode::Home));
    line.handle_key(key(KeyCode::Right)); // past 'h' → byte 1
    assert_eq!(line.cursor(), 1);
    line.handle_key(key(KeyCode::Right)); // past 'é' → byte 3
    assert_eq!(line.cursor(), 3);
    line.handle_key(key(KeyCode::Backspace)); // remove 'é'
    assert_eq!(line.text(), "hllo");
    assert_eq!(line.cursor(), 1);
}

#[test]
fn ctrl_a_and_ctrl_e_jump_to_ends() {
    let mut line = EditableLine::with_text("abcd");
    assert!(line.handle_key(ctrl(KeyCode::Char('a'))));
    assert_eq!(line.cursor(), 0);
    assert!(line.handle_key(ctrl(KeyCode::Char('e'))));
    assert_eq!(line.cursor(), 4);
}

#[test]
fn ctrl_d_deletes_at_cursor() {
    let mut line = EditableLine::with_text("abcd");
    line.handle_key(key(KeyCode::Home));
    assert!(line.handle_key(ctrl(KeyCode::Char('d'))));
    assert_eq!(line.text(), "bcd");
}

#[test]
fn word_motion_left_and_right() {
    let mut line = EditableLine::with_text("foo bar baz");
    // From the end, Ctrl+Left lands at the start of "baz" (byte 8).
    assert!(line.handle_key(ctrl(KeyCode::Left)));
    assert_eq!(line.cursor(), 8);
    // Again → start of "bar" (byte 4).
    assert!(line.handle_key(alt(KeyCode::Char('b'))));
    assert_eq!(line.cursor(), 4);
    // Ctrl+Right → start of "baz" (byte 8).
    assert!(line.handle_key(ctrl(KeyCode::Right)));
    assert_eq!(line.cursor(), 8);
    // Alt+F from byte 8 → end (byte 11).
    assert!(line.handle_key(alt(KeyCode::Char('f'))));
    assert_eq!(line.cursor(), 11);
}

#[test]
fn handle_key_inserts_plain_chars_and_ignores_unknown() {
    let mut line = EditableLine::new();
    assert!(line.handle_key(key(KeyCode::Char('h'))));
    assert!(line.handle_key(key(KeyCode::Char('i'))));
    assert_eq!(line.text(), "hi");
    // Enter / Esc / Tab are NOT owned by the line.
    assert!(!line.handle_key(key(KeyCode::Enter)));
    assert!(!line.handle_key(key(KeyCode::Esc)));
    assert!(!line.handle_key(key(KeyCode::Tab)));
    // Ctrl+C must not insert a literal 'c'.
    assert!(!line.handle_key(ctrl(KeyCode::Char('c'))));
    assert_eq!(line.text(), "hi");
}

#[test]
fn into_text_returns_buffer() {
    let line = EditableLine::with_text("payload");
    assert_eq!(line.into_text(), "payload");
}

// --- The free-function path used by the (masked) passphrase prompt.

#[test]
fn handle_key_on_mirrors_editable_line_for_insert_and_nav() {
    // The passphrase prompt keeps its secret in a Zeroizing<String>
    // and drives edits through `handle_key_on`; pin that the free
    // function produces the same buffer + cursor an EditableLine
    // would, including a mid-string insert (the masking is purely a
    // render-time concern, so the buffer logic is identical).
    let mut buf = String::from("scret");
    let mut cur = buf.len();
    // Home, then Right once → between 's' and 'c' (byte 1).
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Home), true);
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Right), true);
    // Insert 'e' → "secret", cursor advances past it to byte 2.
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Char('e')), true);
    assert_eq!(buf, "secret");
    assert_eq!(cur, 2);
}

#[test]
fn handle_key_on_backspace_and_delete_at_cursor() {
    let mut buf = String::from("abc");
    let mut cur = 1; // between 'a' and 'b'
    let handled;
    (cur, handled) = handle_key_on(&mut buf, cur, key(KeyCode::Backspace), true);
    assert!(handled);
    assert_eq!(buf, "bc");
    assert_eq!(cur, 0);
    // Delete at cursor removes 'b'.
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Delete), true);
    assert_eq!(buf, "c");
    assert_eq!(cur, 0);
}

#[test]
fn secret_mode_disables_word_jump_but_keeps_char_and_absolute_nav() {
    // Masked-passphrase semantics (`allow_word_motion = false`):
    // Alt+F / Ctrl+Right MUST NOT land on a word boundary (which
    // would leak a space position) and MUST NOT insert a literal
    // char. They degrade to a single-char move. Plain
    // Left/Right/Home/End and Ctrl+A/E still work.
    let mut buf = String::from("foo bar baz");
    let mut cur = 0;
    let handled;

    // Alt+F from byte 0: word motion would jump to byte 4 ("bar").
    // With it disabled we expect a single-char move to byte 1.
    (cur, handled) = handle_key_on(&mut buf, cur, alt(KeyCode::Char('f')), false);
    assert!(handled, "Alt+F must still be handled (not fall through)");
    assert_eq!(cur, 1, "Alt+F must move one char, not jump to a word");
    assert_eq!(buf, "foo bar baz", "Alt+F must not insert a literal 'f'");

    // Ctrl+Right from byte 1: word motion would jump to byte 4.
    // Disabled → single-char move to byte 2.
    (cur, _) = handle_key_on(&mut buf, cur, ctrl(KeyCode::Right), false);
    assert_eq!(cur, 2, "Ctrl+Right must move one char, not jump to a word");

    // Alt+B never reaches a word boundary either; from byte 2 it is
    // a single char left to byte 1.
    (cur, _) = handle_key_on(&mut buf, cur, alt(KeyCode::Char('b')), false);
    assert_eq!(cur, 1, "Alt+B must move one char left, not jump");

    // Absolute navigation is unaffected.
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Home), false);
    assert_eq!(cur, 0);
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::End), false);
    assert_eq!(cur, buf.len());
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Left), false);
    assert_eq!(cur, buf.len().saturating_sub(1));
    (cur, _) = handle_key_on(&mut buf, cur, ctrl(KeyCode::Char('a')), false);
    assert_eq!(cur, 0);
    (cur, _) = handle_key_on(&mut buf, cur, ctrl(KeyCode::Char('e')), false);
    assert_eq!(cur, buf.len());
}
