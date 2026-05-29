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
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Home));
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Right));
    // Insert 'e' → "secret", cursor advances past it to byte 2.
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Char('e')));
    assert_eq!(buf, "secret");
    assert_eq!(cur, 2);
}

#[test]
fn handle_key_on_backspace_and_delete_at_cursor() {
    let mut buf = String::from("abc");
    let mut cur = 1; // between 'a' and 'b'
    let handled;
    (cur, handled) = handle_key_on(&mut buf, cur, key(KeyCode::Backspace));
    assert!(handled);
    assert_eq!(buf, "bc");
    assert_eq!(cur, 0);
    // Delete at cursor removes 'b'.
    (cur, _) = handle_key_on(&mut buf, cur, key(KeyCode::Delete));
    assert_eq!(buf, "c");
    assert_eq!(cur, 0);
}
