//! Small rendering helpers shared across rescue screens.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};

/// Render a single-line bordered banner. Used by every screen so the
/// header chrome stays consistent.
pub(crate) fn render_banner(frame: &mut Frame<'_>, area: Rect, title: &str, fg: Color) {
    let para = Paragraph::new(Line::styled(
        title.to_owned(),
        Style::default().fg(fg).add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Center)
    .block(Block::bordered());
    frame.render_widget(para, area);
}

/// Group `hex` into space-separated chunks of `chunk` chars per line,
/// wrapping every 16 chars (= 4 chunks of 4) so a 64-char SHA-256
/// digest renders as four rows. Empty input renders as one empty row.
pub(crate) fn group_hex(hex: &str, chunk: usize) -> Vec<String> {
    let chunk = chunk.max(1);
    if hex.is_empty() {
        return vec![String::new()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chunks_on_row = 0usize;
    let mut chars = hex.chars();
    loop {
        let mut group = String::with_capacity(chunk);
        for _ in 0..chunk {
            let Some(c) = chars.next() else { break };
            group.push(c);
        }
        if group.is_empty() {
            break;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&group);
        chunks_on_row = chunks_on_row.saturating_add(1);
        if chunks_on_row >= 4 {
            rows.push(std::mem::take(&mut current));
            chunks_on_row = 0;
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// Same byte→char column conversion as `ui::view::char_column_for_byte_cursor`.
/// Duplicated rather than re-exported because the original is
/// private; flagging it `pub(crate)` would be a wider refactor than
/// E.2 should land.
pub(crate) fn char_column_for_byte_cursor(s: &str, byte_idx: usize) -> usize {
    let clamped = byte_idx.min(s.len());
    let Some(safe) = (0..=clamped).rev().find(|&i| s.is_char_boundary(i)) else {
        return 0;
    };
    s.get(..safe).map_or(0, |prefix| prefix.chars().count())
}

/// Round `byte_idx` down to the nearest char boundary in `s`.
pub(crate) fn clamp_to_char_boundary(s: &str, byte_idx: usize) -> usize {
    let len = s.len();
    if byte_idx >= len {
        return len;
    }
    let mut idx = byte_idx;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    idx
}

/// Byte index of the char boundary strictly before `byte_idx`, or
/// `None` if `byte_idx` is at the start (or before).
pub(crate) fn prev_char_boundary(s: &str, byte_idx: usize) -> Option<usize> {
    if byte_idx == 0 {
        return None;
    }
    let mut idx = byte_idx.saturating_sub(1);
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    Some(idx)
}

/// Byte index of the next char boundary after `byte_idx`, or `None`
/// if `byte_idx` is at or past the end.
pub(crate) fn next_char_boundary(s: &str, byte_idx: usize) -> Option<usize> {
    let len = s.len();
    if byte_idx >= len {
        return None;
    }
    let mut idx = byte_idx.saturating_add(1);
    while idx < len && !s.is_char_boundary(idx) {
        idx = idx.saturating_add(1);
    }
    Some(idx)
}
