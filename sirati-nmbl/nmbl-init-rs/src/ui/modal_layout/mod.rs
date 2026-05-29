//! Structured sizing for modal dialogs.
//!
//! Every modal call site (confirm / error / N-button / status overlay)
//! pipes its message through [`compute_modal_layout`] so the operator
//! sees a consistent shape regardless of which call site rendered.
//!
//! ## Constants
//!
//! - `INNER_H_PAD = 3`        spaces inside the box on each side of text
//! - `INNER_TOP_PAD = 2`      blank lines above the text inside the box
//! - `INNER_BOT_PAD = 1`      blank lines below the text, before separator
//! - `SEPARATOR_HEIGHT = 1`   the `- - -` separator before the buttons
//! - `OUTER_TOP_PAD = 3`      minimum empty lines above the box on screen
//! - `OUTER_BOT_PAD = 4`      minimum empty lines below the box on screen
//! - `OUTER_H_BUDGET = 6`     minimum total empty cols outside the box
//! - `MAX_BOX_WIDTH_RATIO_*`  default cap on box width is 2/3 of screen
//! - `MIN_TEXT_WIDTH = 40`    or `max_line_length(msg)` if shorter
//!
//! Width degrades W0 → W1 → W2 → W3; height degrades H1 → H2 → H3 → H4
//! (where H4 enables a scrollable text region with a hint outside the
//! box). See the README of the calling change for the full algorithm.

use ratatui::layout::Rect;

mod compute;
mod wrap;

#[cfg(test)]
mod tests;

pub use compute::{compute_modal_layout, compute_modal_layout_with_button_width};
pub use wrap::wrap_message;

/// Spaces inside the box on each side of the text region.
pub const INNER_H_PAD: u16 = 3;
/// Blank lines above the text inside the box.
pub const INNER_TOP_PAD: u16 = 2;
/// Blank lines below the text, before the separator.
pub const INNER_BOT_PAD: u16 = 1;
/// Height of the `- - -` separator row.
pub const SEPARATOR_HEIGHT: u16 = 1;
/// Minimum empty lines above the box on screen.
pub const OUTER_TOP_PAD: u16 = 3;
/// Minimum empty lines below the box on screen.
pub const OUTER_BOT_PAD: u16 = 4;
/// Minimum total horizontal empty cols outside the box (3 each side).
pub const OUTER_H_BUDGET: u16 = 6;
/// Default cap numerator on box width ratio (`MAX_BOX_WIDTH_RATIO`).
pub const MAX_BOX_WIDTH_NUM: u32 = 2;
/// Default cap denominator on box width ratio (`MAX_BOX_WIDTH_RATIO`).
pub const MAX_BOX_WIDTH_DEN: u32 = 3;
/// Floor for inner text width unless the message itself is shorter.
pub const MIN_TEXT_WIDTH: u16 = 40;
/// Hint painted on the row below the box when the text region is
/// scrollable (stage H4 triggered). Matches the precedent in
/// `view::render_pty_shell`.
pub const SCROLL_HINT: &str = "Ctrl+Shift+Up/Dn scroll";

/// Sizing result fed to every modal renderer. Coordinates are absolute
/// so the renderer can paint without re-deriving the layout. `box_rect`
/// is the bordered box; `inner_text_rect` is the area where the wrapped
/// text lines (or a scroll viewport) goes. `separator_y` and
/// `button_row_y` are absolute rows. `scroll_hint` is `Some` only on
/// stage H4 (scroll mode). `wrapped_lines` is pre-wrapped at
/// `inner_text_rect.width` so the renderer can slice into it.
#[derive(Debug, Clone)]
pub struct ModalLayout {
    /// Absolute screen rect of the bordered box.
    pub box_rect: Rect,
    /// Absolute rect inside the box where wrapped text (or scroll
    /// viewport) is painted. Width is the wrap width; height is the
    /// number of text rows actually visible (may be less than
    /// `wrapped_lines.len()` in scroll mode).
    pub inner_text_rect: Rect,
    /// Absolute screen row of the `- - -` separator.
    pub separator_y: u16,
    /// Absolute screen row of the buttons row (just below the separator).
    pub button_row_y: u16,
    /// When `Some`, the rect of the right-aligned scroll hint below
    /// the box. Painted only when `scrollable == true`.
    pub scroll_hint: Option<Rect>,
    /// `true` when the text region is shorter than the wrapped line
    /// count and Ctrl+Shift+Up/Down should scroll. The caller clamps
    /// the offset to `total_lines - visible_lines`.
    pub scrollable: bool,
    /// Message pre-wrapped at `inner_text_rect.width` cols. The
    /// renderer slices into this based on the current scroll offset.
    pub wrapped_lines: Vec<String>,
    /// Inner box width MINUS the two-side `inner_h_pad` — usable text
    /// columns. Mirrors `inner_text_rect.width`; kept on the layout so
    /// callers can compute scroll viewport math without rederiving.
    pub inner_width: u16,
    /// Per-side horizontal padding applied inside the box. May be < 3
    /// when stage W2 reduced it to fit.
    pub inner_h_pad: u16,
    /// Top inner padding actually applied. May be < 2 when stage H2
    /// reduced it to fit.
    pub inner_top_pad: u16,
}

/// Build absolute rects and construct a `ModalLayout`. Called after
/// all sizing passes have settled.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_layout(
    screen: Rect,
    box_width: u16,
    box_height: u16,
    inner_h_pad: u16,
    inner_top_pad: u16,
    inner_bot_pad: u16,
    visible_text_lines: u16,
    has_buttons: bool,
    separator_h: u16,
    scrollable: bool,
    outer_top_pad: u16,
    outer_bot_pad: u16,
    wrapped: Vec<String>,
    inner_width: u16,
) -> ModalLayout {
    let screen_w = screen.width;
    let screen_h = screen.height;

    // Clamp box_height to available screen.
    let max_box_h = screen_h
        .saturating_sub(outer_top_pad)
        .saturating_sub(outer_bot_pad);
    let box_height = if box_height > max_box_h {
        max_box_h.max(3)
    } else {
        box_height
    };

    // Position. Centre horizontally; anchor with outer_top_pad, then
    // center within the remaining slack if any.
    let x_slack = screen_w.saturating_sub(box_width);
    let x = screen.x.saturating_add(x_slack / 2);
    let total_h = if scrollable {
        box_height.saturating_add(1)
    } else {
        box_height
    };
    let y_slack = screen_h
        .saturating_sub(total_h)
        .saturating_sub(outer_top_pad)
        .saturating_sub(outer_bot_pad);
    let y = screen
        .y
        .saturating_add(outer_top_pad)
        .saturating_add(y_slack / 2);

    let box_rect = Rect::new(x, y, box_width, box_height);
    let text_x = x.saturating_add(1).saturating_add(inner_h_pad);
    let text_y = y.saturating_add(1).saturating_add(inner_top_pad);
    let text_h = visible_text_lines.max(1);
    let inner_text_rect = Rect::new(text_x, text_y, inner_width, text_h);

    let separator_y = text_y.saturating_add(text_h).saturating_add(inner_bot_pad);
    let button_row_y = if has_buttons {
        separator_y.saturating_add(separator_h)
    } else {
        separator_y
    };
    let scroll_hint = if scrollable {
        let hint_y = y.saturating_add(box_height);
        // Stretch to full screen width so right-aligned hint text fits.
        Some(Rect::new(screen.x, hint_y, screen_w, 1))
    } else {
        None
    };

    ModalLayout {
        box_rect,
        inner_text_rect,
        separator_y,
        button_row_y,
        scroll_hint,
        scrollable,
        wrapped_lines: wrapped,
        inner_width,
        inner_h_pad,
        inner_top_pad,
    }
}
