//! Shared helpers for interactive modal render loops.
//!
//! [`modal_poll`] drives one poll slice and classifies the outcome so
//! every modal loop reacts to host-resize events uniformly.
//! [`handle_modal_scroll_key`] intercepts Ctrl+Shift+scroll combos so
//! every scrollable modal shares one keybinding table.

use std::time::Duration;

use crate::error::Result;
use crate::ui::console::Console;
use crate::ui::modal_layout;

/// Outcome of polling a long-running render loop's input slice. Wraps
/// the trichotomy "key arrived / host terminal reported a new size /
/// nothing this tick" into a single value the caller pattern-matches on
/// so every modal loop redraws on resize without duplicating the
/// `match` boilerplate.
pub(super) enum ModalPollOutcome {
    /// A key event the caller should dispatch.
    Key(crossterm::event::KeyEvent),
    /// Host terminal reported a new grid. Caller should set its
    /// `dirty` flag so the next iteration repaints against the new
    /// layout.
    Resized,
    /// No event this slice. Caller may continue ticking countdowns
    /// or re-poll.
    Idle,
}

/// Poll once via [`Console::poll_event`] and classify the outcome for a
/// modal render loop. Shared by every long-running interactive modal
/// (passphrase, generations picker, rescue menu, console picker,
/// confirm / error / buttons) so they all react to host-reported
/// resize events uniformly.
pub(super) async fn modal_poll(
    console: &mut dyn Console,
    timeout: Duration,
) -> Result<ModalPollOutcome> {
    match console.poll_event(timeout).await? {
        Some(crate::ui::console::ConsoleEvent::Key(k)) => Ok(ModalPollOutcome::Key(k)),
        Some(crate::ui::console::ConsoleEvent::Resize { .. }) => Ok(ModalPollOutcome::Resized),
        None => Ok(ModalPollOutcome::Idle),
    }
}

/// Inspect a [`KeyEvent`] for the modal-scroll bindings
/// (Ctrl+Shift+Up/Down/PgUp/PgDn). Returns `Some(new_offset)` when the
/// key matched and was consumed, `None` when the key should fall
/// through to the caller's regular key dispatch.
///
/// `console.size()` is sampled here so the helper can compute the
/// visible-line count from the same layout the renderer uses. Empty
/// or pathologically small consoles fall back to a 1-line viewport so
/// the offset still advances by one per keypress.
pub(super) fn handle_modal_scroll_key(
    key: crossterm::event::KeyEvent,
    message: &str,
    has_buttons: bool,
    btn_count: u16,
    console: &mut dyn Console,
    scroll_offset: u16,
) -> Option<u16> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if !(ctrl && shift) {
        return None;
    }
    // Re-derive the current layout from the console's reported size.
    // Mirrors `view::split_chrome`: 3-row header + body + 1-row footer.
    let (cols, rows) = console.size();
    let body_h = rows.saturating_sub(4);
    let body = ratatui::layout::Rect::new(0, 3, cols, body_h);
    let layout = modal_layout::compute_modal_layout(message, has_buttons, btn_count, body);
    if !layout.scrollable {
        // Even when not scrollable we still consume the key combo so
        // a stray Ctrl+Shift+arrow doesn't leak through to whatever
        // dispatch would have run otherwise. Returning the unchanged
        // offset keeps the renderer's clamping clean.
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                return Some(scroll_offset);
            }
            _ => return None,
        }
    }
    let visible = layout.inner_text_rect.height.max(1);
    let total = u16::try_from(layout.wrapped_lines.len()).unwrap_or(u16::MAX);
    let max_off = total.saturating_sub(visible);
    let page = visible.saturating_sub(1).max(1);
    let new_off = match key.code {
        KeyCode::Up => scroll_offset.saturating_sub(1),
        KeyCode::Down => scroll_offset.saturating_add(1).min(max_off),
        KeyCode::PageUp => scroll_offset.saturating_sub(page),
        KeyCode::PageDown => scroll_offset.saturating_add(page).min(max_off),
        KeyCode::Home => 0,
        KeyCode::End => max_off,
        _ => return None,
    };
    Some(new_off)
}
