//! Low-level editing primitives for the single-line text buffer.
//!
//! All functions operate on a raw `(&mut String, usize)` pair (buffer +
//! byte-index cursor) and return a new cursor. Both [`super::EditableLine`]
//! and the LUKS passphrase prompt (which stores its secret in a
//! [`zeroize::Zeroizing`] `String`) delegate here so the byte-boundary
//! bookkeeping lives in one place.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Apply a [`KeyEvent`] to an arbitrary `(buffer, cursor)` pair and
/// return `(new_cursor, handled)`.
///
/// This is the single source of truth for the line-editing semantics
/// shared by [`super::EditableLine`] (cmdline editor) and the passphrase prompt
/// (which keeps its secret in a [`zeroize::Zeroizing`] `String`). `cursor`
/// is a byte index; the returned cursor is always on a char boundary.
///
/// Char insertion ignores `Char`s carrying CONTROL (so Ctrl+C doesn't
/// type a literal 'c'); the recognised control combos (Ctrl+A/E/D and
/// Alt+B/F, plus Ctrl+Left/Right) are handled explicitly.
///
/// `allow_word_motion` gates the word-wise jumps (Alt+B/F and
/// Ctrl+Left/Right). The masked passphrase prompt passes `false`: a
/// word jump there would reveal where the spaces sit in the secret, so
/// those keys degrade to a single-char move instead. Absolute Home/End
/// and Ctrl+A/E stay available either way — they don't expose word
/// boundaries.
pub fn handle_key_on(
    buffer: &mut String,
    cursor: usize,
    key: KeyEvent,
    allow_word_motion: bool,
) -> (usize, bool) {
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
            'b' if allow_word_motion => (word_left(buffer, cursor), true),
            'f' if allow_word_motion => (word_right(buffer, cursor), true),
            // Masked secret: degrade word motion to a single-char move so
            // no word boundary (and no space position) is revealed.
            'b' => (move_left(buffer, cursor), true),
            'f' => (move_right(buffer, cursor), true),
            _ => (cursor, false),
        },
        KeyCode::Char(c) => (insert_char(buffer, cursor, c), true),
        KeyCode::Backspace => (backspace(buffer, cursor), true),
        KeyCode::Delete => (delete(buffer, cursor), true),
        KeyCode::Left if ctrl && allow_word_motion => (word_left(buffer, cursor), true),
        KeyCode::Right if ctrl && allow_word_motion => (word_right(buffer, cursor), true),
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
