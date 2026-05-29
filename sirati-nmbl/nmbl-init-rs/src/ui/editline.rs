//! A small backend-agnostic single-line text buffer with a cursor.
//!
//! Both the kernel-cmdline editor ([`crate::ui::app::Screen::Editing`])
//! and the LUKS passphrase prompt
//! ([`crate::ui::app::Screen::Passphrase`]) need the same line-editing
//! semantics: insert at the cursor, Backspace/Delete at the cursor,
//! Left/Right by one char, Home/End to the extremes, plus the
//! readline-flavoured Ctrl+A/Ctrl+E and word-wise
//! Ctrl+Left/Right / Alt+B/F. Rather than duplicate the byte-boundary
//! bookkeeping in two places (and rather than hand the whole line over
//! to `termwiz::lineedit::LineEditor`, which drives its OWN blocking
//! termwiz `Terminal` read + render loop that doesn't match our
//! poll-based `Console` abstraction across the splash/serial/tty
//! backends), we keep a tiny shared helper that operates purely on the
//! `crossterm::event::KeyEvent`s the app already receives. The renderer
//! stays in [`crate::ui::view`]; this module owns only buffer + cursor.
//!
//! ## Why not `termwiz::lineedit::LineEditor`?
//!
//! `LineEditor::read_line` takes ownership of a `termwiz::Terminal`,
//! blocks on its own input read, and renders the line itself. NMBL
//! renders ratatui frames into a `&mut dyn Console` whose splash backend
//! is a DRM framebuffer (no termwiz terminal at all) and whose input is
//! delivered one poll-tick at a time so the surrounding loop can animate
//! spinners and react to resize events. A blind `read_line` would
//! bypass all of that and would not paint on the splash backend. The
//! editing *semantics* termwiz offers are simple enough that mirroring
//! them over our own `KeyEvent` stream is both smaller and uniform
//! across all three backends.
//!
//! ## Two consumers, one core
//!
//! [`EditableLine`] is the cmdline editor's owned buffer+cursor type.
//! The passphrase prompt cannot use it directly: the secret must live in
//! a [`zeroize::Zeroizing`] `String` so it is scrubbed on drop. To avoid
//! forking the editing logic, the actual edits live in free functions
//! that operate on `(&mut String, usize) -> usize` (buffer + cursor in,
//! new cursor out); both [`EditableLine`] and the passphrase handler
//! ([`handle_key_on`]) delegate to them. The cursor is a **byte** index
//! and always sits on a char boundary.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A single editable text line: an owned `String` plus a byte-index
/// cursor that always sits on a char boundary.
///
/// The cursor is stored as a **byte** index (not a char count) so
/// insertion / deletion are O(1) `String` ops and never need a
/// char-walk to find the splice point. The renderer converts the byte
/// cursor to a display column via
/// [`crate::ui::view`]'s `char_column_for_byte_cursor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditableLine {
    buffer: String,
    /// Byte offset into `buffer`; invariant: always a char boundary in
    /// `0..=buffer.len()`.
    cursor: usize,
}

impl EditableLine {
    /// Empty line with the cursor at column 0.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from existing text with the cursor parked at the end — the
    /// natural landing spot when entering the cmdline editor pre-filled
    /// with a generation's kernel params.
    #[must_use]
    pub fn with_text(text: impl Into<String>) -> Self {
        let buffer = text.into();
        let cursor = buffer.len();
        Self { buffer, cursor }
    }

    /// Borrow the current text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.buffer
    }

    /// The cursor's byte offset (always a char boundary).
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// `true` when the buffer holds no characters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Consume the line and return the owned buffer (drops the cursor).
    #[must_use]
    pub fn into_text(self) -> String {
        self.buffer
    }

    /// Apply a [`KeyEvent`] to the line. Returns `true` if the event was
    /// an editing/navigation action this helper handled (so the caller
    /// can skip its own fallthrough), `false` for keys the line doesn't
    /// own (Enter, Esc, Tab, …) which the caller routes elsewhere.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        let (new_cursor, handled) = handle_key_on(&mut self.buffer, self.cursor, key);
        self.cursor = new_cursor;
        handled
    }
}

/// Apply a [`KeyEvent`] to an arbitrary `(buffer, cursor)` pair and
/// return `(new_cursor, handled)`.
///
/// This is the single source of truth for the line-editing semantics
/// shared by [`EditableLine`] (cmdline editor) and the passphrase prompt
/// (which keeps its secret in a [`zeroize::Zeroizing`] `String`). `cursor`
/// is a byte index; the returned cursor is always on a char boundary.
///
/// Char insertion ignores `Char`s carrying CONTROL (so Ctrl+C doesn't
/// type a literal 'c'); the recognised control combos (Ctrl+A/E/D and
/// Alt+B/F, plus Ctrl+Left/Right) are handled explicitly.
pub fn handle_key_on(buffer: &mut String, cursor: usize, key: KeyEvent) -> (usize, bool) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'a' => (0, true),
            'e' => (buffer.len(), true),
            'd' => (delete(buffer, cursor), true),
            _ => (cursor, false),
        },
        KeyCode::Char(c) if alt => match c.to_ascii_lowercase() {
            'b' => (word_left(buffer, cursor), true),
            'f' => (word_right(buffer, cursor), true),
            _ => (cursor, false),
        },
        KeyCode::Char(c) => (insert_char(buffer, cursor, c), true),
        KeyCode::Backspace => (backspace(buffer, cursor), true),
        KeyCode::Delete => (delete(buffer, cursor), true),
        KeyCode::Left if ctrl => (word_left(buffer, cursor), true),
        KeyCode::Right if ctrl => (word_right(buffer, cursor), true),
        KeyCode::Left => (move_left(buffer, cursor), true),
        KeyCode::Right => (move_right(buffer, cursor), true),
        KeyCode::Home => (0, true),
        KeyCode::End => (buffer.len(), true),
        _ => (cursor, false),
    }
}

/// Insert `c` at `cursor`, returning the cursor advanced past it.
fn insert_char(buffer: &mut String, cursor: usize, c: char) -> usize {
    let at = clamp_to_boundary(buffer, cursor);
    buffer.insert(at, c);
    at.saturating_add(c.len_utf8())
}

/// Delete the char immediately before `cursor` (Backspace). No-op at 0.
fn backspace(buffer: &mut String, cursor: usize) -> usize {
    let cur = clamp_to_boundary(buffer, cursor);
    if let Some(prev) = prev_boundary(buffer, cur) {
        buffer.replace_range(prev..cur, "");
        prev
    } else {
        cur
    }
}

/// Delete the char at `cursor` (Delete / Ctrl+D). No-op at end. The
/// cursor stays put; the following text shifts left onto it.
fn delete(buffer: &mut String, cursor: usize) -> usize {
    let cur = clamp_to_boundary(buffer, cursor);
    if let Some(next) = next_boundary(buffer, cur) {
        buffer.replace_range(cur..next, "");
    }
    cur
}

/// Cursor one char left, saturating at 0.
fn move_left(buffer: &str, cursor: usize) -> usize {
    let cur = clamp_to_boundary(buffer, cursor);
    prev_boundary(buffer, cur).unwrap_or(0)
}

/// Cursor one char right, saturating at the end.
fn move_right(buffer: &str, cursor: usize) -> usize {
    let cur = clamp_to_boundary(buffer, cursor);
    next_boundary(buffer, cur).unwrap_or(buffer.len())
}

/// Cursor left to the start of the previous word (Ctrl+Left / Alt+B):
/// skip trailing whitespace, then the word.
fn word_left(buffer: &str, cursor: usize) -> usize {
    let mut cur = clamp_to_boundary(buffer, cursor);
    while let Some(prev) = prev_boundary(buffer, cur) {
        if char_at(buffer, prev).is_some_and(char::is_whitespace) {
            cur = prev;
        } else {
            break;
        }
    }
    while let Some(prev) = prev_boundary(buffer, cur) {
        if char_at(buffer, prev).is_some_and(|c| !c.is_whitespace()) {
            cur = prev;
        } else {
            break;
        }
    }
    cur
}

/// Cursor right to the start of the next word (Ctrl+Right / Alt+F):
/// skip the current word, then whitespace.
fn word_right(buffer: &str, cursor: usize) -> usize {
    let len = buffer.len();
    let mut cur = clamp_to_boundary(buffer, cursor);
    while cur < len {
        if char_at(buffer, cur).is_some_and(|c| !c.is_whitespace()) {
            cur = next_boundary(buffer, cur).unwrap_or(len);
        } else {
            break;
        }
    }
    while cur < len {
        if char_at(buffer, cur).is_some_and(char::is_whitespace) {
            cur = next_boundary(buffer, cur).unwrap_or(len);
        } else {
            break;
        }
    }
    cur
}

/// Round `cursor` down to the nearest char boundary, clamped to the
/// buffer length. Defends against a cursor parked mid-codepoint.
fn clamp_to_boundary(buffer: &str, cursor: usize) -> usize {
    let len = buffer.len();
    if cursor >= len {
        return len;
    }
    let mut idx = cursor;
    while idx > 0 && !buffer.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    idx
}

/// Byte index of the char boundary strictly before `idx`, or `None` at
/// the start.
fn prev_boundary(buffer: &str, idx: usize) -> Option<usize> {
    if idx == 0 {
        return None;
    }
    let mut i = idx.saturating_sub(1);
    while i > 0 && !buffer.is_char_boundary(i) {
        i = i.saturating_sub(1);
    }
    Some(i)
}

/// Byte index of the next char boundary after `idx`, or `None` at the
/// end.
fn next_boundary(buffer: &str, idx: usize) -> Option<usize> {
    let len = buffer.len();
    if idx >= len {
        return None;
    }
    let mut i = idx.saturating_add(1);
    while i < len && !buffer.is_char_boundary(i) {
        i = i.saturating_add(1);
    }
    Some(i)
}

/// The char starting at byte boundary `idx`, or `None` if `idx` is
/// at/after the end.
fn char_at(buffer: &str, idx: usize) -> Option<char> {
    buffer.get(idx..).and_then(|s| s.chars().next())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert with panics on contract failure"
)]
mod tests {
    use super::*;

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
}
