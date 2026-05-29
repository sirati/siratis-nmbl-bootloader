//! Hash-confirmation screen for the rescue flow.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::error::Result;
use crate::rescue::net::HashConfirmation;
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleEvent};

use super::helpers::{
    char_column_for_byte_cursor, clamp_to_char_boundary, group_hex, next_char_boundary,
    prev_char_boundary, render_banner,
};

/// Mutable state threaded through [`handle_hash_key`]. Split out so
/// the key-handler can be unit-tested without a live terminal.
#[derive(Debug, Clone)]
pub(crate) struct HashConfirmState {
    pub(crate) expected: String,
    pub(crate) cursor: usize,
}

impl HashConfirmState {
    pub(crate) fn new(prefill: &str, cursor_seed: usize) -> Self {
        let expected = prefill.to_string();
        let cursor = cursor_seed.min(expected.len());
        Self { expected, cursor }
    }
}

/// Pure input-handler for the hash-confirm screen. Returns `Some(outcome)`
/// when the operator has committed (Y/N/Enter/Esc/A) and `None` when
/// the loop should re-render and read the next event. Editing keys
/// mutate `state` in place.
///
/// Factored out of [`run_confirm_hash`] so the "Y on mismatch returns
/// Mismatch" contract has a unit test without spinning up a backend.
pub(crate) fn handle_hash_key(
    key: KeyEvent,
    state: &mut HashConfirmState,
    computed_hex: &str,
) -> Option<HashConfirmation> {
    if key.kind != KeyEventKind::Press {
        return None;
    }

    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            if computed_hex.eq_ignore_ascii_case(state.expected.as_str()) {
                Some(HashConfirmation::Confirmed)
            } else {
                // Operator pressed y but the panes disagree: treat as
                // mismatch so the orchestrator restarts the download
                // rather than pivoting into a tampered blob.
                Some(HashConfirmation::Mismatch)
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') => Some(HashConfirmation::Mismatch),
        KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Esc => Some(HashConfirmation::Aborted),
        KeyCode::Enter => {
            let outcome = if computed_hex.eq_ignore_ascii_case(state.expected.as_str()) {
                HashConfirmation::Confirmed
            } else {
                HashConfirmation::Mismatch
            };
            Some(outcome)
        }
        KeyCode::Char(c) => {
            let insert_at = clamp_to_char_boundary(&state.expected, state.cursor);
            state.expected.insert(insert_at, c);
            state.cursor = insert_at.saturating_add(c.len_utf8());
            None
        }
        KeyCode::Backspace => {
            let current = clamp_to_char_boundary(&state.expected, state.cursor);
            if let Some(prev) = prev_char_boundary(&state.expected, current) {
                state.expected.replace_range(prev..current, "");
                state.cursor = prev;
            }
            None
        }
        KeyCode::Left => {
            let current = clamp_to_char_boundary(&state.expected, state.cursor);
            state.cursor = prev_char_boundary(&state.expected, current).unwrap_or(0);
            None
        }
        KeyCode::Right => {
            let current = clamp_to_char_boundary(&state.expected, state.cursor);
            state.cursor =
                next_char_boundary(&state.expected, current).unwrap_or(state.expected.len());
            None
        }
        KeyCode::Home => {
            state.cursor = 0;
            None
        }
        KeyCode::End => {
            state.cursor = state.expected.len();
            None
        }
        _ => None,
    }
}

/// Two-pane hash confirm screen. Returns the chosen outcome and the
/// final cursor offset on the (editable) expected pane. All paint and
/// input goes through the orchestrator-held [`Console`].
pub(super) async fn run_confirm_hash(
    console: &mut dyn Console,
    computed_hex: &str,
    prefill_expected: &str,
    cursor_seed: usize,
) -> Result<(HashConfirmation, usize)> {
    let mut state = HashConfirmState::new(prefill_expected, cursor_seed);

    loop {
        let snapshot_expected = state.expected.clone();
        let snapshot_cursor = state.cursor;
        console.draw_with(&mut |f| {
            render_confirm_hash(f, computed_hex, &snapshot_expected, snapshot_cursor);
        })?;
        let key = match console.poll_event(POLL_SLICE).await? {
            // A `dirty` flag is unnecessary here because every loop
            // iteration already repaints from the latest state — just
            // re-iterate so the new size lands in `console.size()`
            // before the next paint reads it.
            Some(ConsoleEvent::Resize { .. }) | None => continue,
            Some(ConsoleEvent::Key(k)) => k,
        };
        if let Some(outcome) = handle_hash_key(key, &mut state, computed_hex) {
            return Ok((outcome, state.cursor));
        }
    }
}

/// Pure-render side of the hash-confirm screen. Two side-by-side panes
/// (computed = read-only 4-column lowercase hex; expected = editable
/// pre-filled) plus a red MISMATCH banner when the two disagree.
pub(crate) fn render_confirm_hash(
    frame: &mut Frame<'_>,
    computed_hex: &str,
    expected: &str,
    cursor: usize,
) {
    let (banner_text, banner_colour) = if expected.is_empty() {
        // No prefill is its own distinct state — louder than the
        // computed/expected mismatch banner so the operator knows the
        // disagreement is "you haven't typed anything yet" rather
        // than "the upstream hash drifted".
        ("No expected hash pre-filled", Color::Yellow)
    } else if !computed_hex.eq_ignore_ascii_case(expected) {
        ("MISMATCH — refusing to confirm", Color::Red)
    } else {
        ("Hash matches expected", Color::Green)
    };

    let [header, banner, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas::<4>(frame.area());

    render_banner(frame, header, "Hash confirmation", Color::Cyan);
    render_banner(frame, banner, banner_text, banner_colour);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas::<2>(body);

    let computed_lines: Vec<Line<'_>> = group_hex(computed_hex, 4)
        .into_iter()
        .map(Line::raw)
        .collect();
    let computed_para = Paragraph::new(Text::from(computed_lines))
        .block(Block::bordered().title("Computed (SHA-256)"))
        .wrap(Wrap { trim: false });
    frame.render_widget(computed_para, left);

    let column = char_column_for_byte_cursor(expected, cursor);
    let caret = format!("{}{}", " ".repeat(column), "^");
    let expected_lines: Vec<Line<'_>> = {
        let mut v: Vec<Line<'_>> = group_hex(expected, 4).into_iter().map(Line::raw).collect();
        v.push(Line::styled(
            caret,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        v
    };
    let expected_para = Paragraph::new(Text::from(expected_lines))
        .block(Block::bordered().title("Expected (editable)"))
        .wrap(Wrap { trim: false });
    frame.render_widget(expected_para, right);

    let hint = "Y=confirm  N=mismatch  A/Esc=abort  Enter=auto  edit expected to override";
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), footer);
}
