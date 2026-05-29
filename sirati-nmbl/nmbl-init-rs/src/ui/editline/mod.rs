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

mod ops;
pub use ops::handle_key_on;

use crossterm::event::KeyEvent;

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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert with panics on contract failure"
)]
mod tests;
