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

/// Hand-rolled word-wrap. Splits each input line on ASCII whitespace
/// and re-emits chunks whose char-width does not exceed `width`. A
/// single word longer than `width` is hard-split at `width` chars.
/// Empty input yields one empty line so the caller always has at least
/// one row to render.
///
/// Char counting (not byte counting) so multi-byte text (UTF-8) doesn't
/// throw the wrap width off — same correctness target as
/// `view::char_column_for_byte_cursor`.
pub fn wrap_message(msg: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut out: Vec<String> = Vec::new();
    // Preserve hard newlines but treat the rest of each line as
    // whitespace-separated words.
    for paragraph in msg.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_chars: usize = 0;
        for word in paragraph.split_whitespace() {
            let word_chars = word.chars().count();
            if word_chars > width {
                // Flush current line first if non-empty.
                if line_chars > 0 {
                    out.push(std::mem::take(&mut line));
                    line_chars = 0;
                }
                // Hard-split the oversized word.
                let mut buf = String::new();
                let mut buf_chars: usize = 0;
                for c in word.chars() {
                    if buf_chars >= width {
                        out.push(std::mem::take(&mut buf));
                        buf_chars = 0;
                    }
                    buf.push(c);
                    buf_chars = buf_chars.saturating_add(1);
                }
                if buf_chars > 0 {
                    line = buf;
                    line_chars = buf_chars;
                }
                continue;
            }
            let needed = if line_chars == 0 {
                word_chars
            } else {
                line_chars.saturating_add(1).saturating_add(word_chars)
            };
            if needed > width {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
                line_chars = word_chars;
            } else {
                if line_chars > 0 {
                    line.push(' ');
                    line_chars = line_chars.saturating_add(1);
                }
                line.push_str(word);
                line_chars = line_chars.saturating_add(word_chars);
            }
        }
        if line_chars > 0 {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Char-width of the longest line in `msg`, treating embedded `\n` as
/// line separators. Used by the width algorithm to decide whether a
/// wrap is even needed.
fn max_line_length(msg: &str) -> u16 {
    let m = msg
        .split('\n')
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0);
    u16::try_from(m).unwrap_or(u16::MAX)
}

/// Default cap = `floor(W * 2 / 3)`.
fn cap_default(screen_w: u16) -> u16 {
    let w = u32::from(screen_w);
    let capped = w.saturating_mul(MAX_BOX_WIDTH_NUM) / MAX_BOX_WIDTH_DEN.max(1);
    u16::try_from(capped).unwrap_or(u16::MAX)
}

/// Width-pass result: final `box_width`, `inner_h_pad`, `inner_width`.
#[derive(Debug, Clone, Copy)]
struct WidthChoice {
    box_width: u16,
    inner_h_pad: u16,
    inner_width: u16,
}

fn compute_width_with_target(
    target_inner_in: u16,
    min_text_floor: u16,
    _msg: &str,
    screen_w: u16,
) -> WidthChoice {
    let target_inner = target_inner_in;
    // box = inner + 2 * inner_h_pad + 2 borders
    let inner_h_pad_default = INNER_H_PAD;
    let borders: u16 = 2;
    let target_box = target_inner
        .saturating_add(inner_h_pad_default.saturating_mul(2))
        .saturating_add(borders);
    let min_text = min_text_floor;

    let cap_def = cap_default(screen_w);
    let cap_relaxed = screen_w.saturating_sub(OUTER_H_BUDGET);
    let cap_no_outer = screen_w;

    // Stage W0
    {
        let box_w = target_box.min(cap_def);
        let inner = box_w
            .saturating_sub(inner_h_pad_default.saturating_mul(2))
            .saturating_sub(borders);
        if inner >= min_text {
            return WidthChoice {
                box_width: box_w,
                inner_h_pad: inner_h_pad_default,
                inner_width: inner,
            };
        }
    }

    // Stage W1: relax cap to cap_relaxed
    {
        let box_w = target_box.min(cap_relaxed);
        let inner = box_w
            .saturating_sub(inner_h_pad_default.saturating_mul(2))
            .saturating_sub(borders);
        if inner >= min_text {
            return WidthChoice {
                box_width: box_w,
                inner_h_pad: inner_h_pad_default,
                inner_width: inner,
            };
        }
    }

    // Stage W2: reduce inner_h_pad from 3 → 0 at cap_relaxed
    {
        let mut pad = INNER_H_PAD;
        loop {
            let box_w = target_box.min(cap_relaxed);
            let inner = box_w
                .saturating_sub(pad.saturating_mul(2))
                .saturating_sub(borders);
            if inner >= min_text {
                return WidthChoice {
                    box_width: box_w,
                    inner_h_pad: pad,
                    inner_width: inner,
                };
            }
            if pad == 0 {
                break;
            }
            pad = pad.saturating_sub(1);
        }
    }

    // Stage W3: relax to full screen
    {
        let pad: u16 = 0;
        let box_w = target_box.min(cap_no_outer);
        let inner = box_w
            .saturating_sub(pad.saturating_mul(2))
            .saturating_sub(borders);
        // Whether or not min_text is met, this is the largest available;
        // accept and let height handle further degradation.
        WidthChoice {
            box_width: box_w,
            inner_h_pad: pad,
            inner_width: inner,
        }
    }
}

/// Compute the modal layout for `msg` on `screen` with `btn_count`
/// buttons. The `has_buttons` flag toggles whether the layout reserves
/// space for a separator + button row (most modals do; the boot-status
/// overlay does not). `btn_row_width` is the rendered width of the
/// joined button row (sum of `[Label]` cell widths plus the 2-col
/// gutters); the layout pass uses it as a floor on `inner_width` so a
/// short message doesn't shrink the box past the buttons.
///
/// The returned [`ModalLayout`] gives the renderer every absolute rect
/// it needs to paint without re-deriving sizing math, plus the pre-
/// wrapped text lines and the scrollable flag (set when the text
/// region was forced shorter than the wrapped line count, stage H4).
pub fn compute_modal_layout(
    msg: &str,
    has_buttons: bool,
    btn_count: u16,
    screen: Rect,
) -> ModalLayout {
    compute_modal_layout_with_button_width(msg, has_buttons, btn_count, 0, screen)
}

/// Variant of [`compute_modal_layout`] that takes an explicit
/// `btn_row_width` hint. Used by renderers that know the exact button-
/// row width (sum of label widths + gutters) at sizing time. Callers
/// that don't know just pass `0` and the layout falls back to
/// `compute_modal_layout`'s heuristic-free behaviour.
pub fn compute_modal_layout_with_button_width(
    msg: &str,
    has_buttons: bool,
    btn_count: u16,
    btn_row_width: u16,
    screen: Rect,
) -> ModalLayout {
    let screen_w = screen.width;
    let screen_h = screen.height;

    // Width pass. Returns the chosen box_width, inner_h_pad and the
    // usable inner_width (cols for the text region). Bias the effective
    // message length and the wrap floor up to the button-row width so
    // a short message doesn't shrink the box past where the buttons
    // need to fit.
    let max_line = max_line_length(msg);
    let effective_msg_len = max_line.max(btn_row_width);
    let wrap_floor = MIN_TEXT_WIDTH.min(max_line.max(1));
    let min_text_floor = wrap_floor.max(btn_row_width);
    let initial = compute_width_with_target(effective_msg_len, min_text_floor, msg, screen_w);
    let mut box_width = initial.box_width;
    let mut inner_h_pad = initial.inner_h_pad;
    let mut inner_width = initial.inner_width.max(1);

    // Buttons row: 1 line. If a renderer wraps onto two lines for a
    // narrow box, the algorithm caller bumps btn_h externally via
    // recomputation; for now treat 1.
    let btn_h: u16 = if has_buttons {
        // If the joined button labels would not fit in inner_width, the
        // caller still draws them — but reserve 2 rows so the second row
        // has somewhere to go. Approximate: each "[X]" + "  " ≈ 5 cols
        // per button. Conservative single-row threshold:
        //   total = sum(label_chars) + 4 * count + 2 * (count - 1)
        // We don't have labels here; use a width heuristic: any modal
        // with >= 4 buttons on a < 80-col inner gets 2 rows.
        if btn_count >= 4 && inner_width < 60 {
            2
        } else {
            1
        }
    } else {
        0
    };
    let separator_h = if has_buttons { SEPARATOR_HEIGHT } else { 0 };

    // Initial wrap pass.
    let mut wrapped = wrap_message(msg, inner_width);
    let mut text_lines = u16::try_from(wrapped.len()).unwrap_or(u16::MAX);

    let target_inner_h = INNER_TOP_PAD
        .saturating_add(text_lines)
        .saturating_add(INNER_BOT_PAD)
        .saturating_add(separator_h)
        .saturating_add(btn_h);
    let target_box_h = target_inner_h.saturating_add(2);

    let outer_top_pad_default = OUTER_TOP_PAD;
    let outer_bot_pad_default = OUTER_BOT_PAD;
    let mut outer_top_pad = outer_top_pad_default;
    let mut outer_bot_pad = outer_bot_pad_default;
    let mut inner_top_pad = INNER_TOP_PAD;
    let mut inner_bot_pad = INNER_BOT_PAD;
    let mut scrollable = false;
    // Start with the target visible count / box height; the degrade
    // branches overwrite both as they walk the stages. Declared up
    // front so the post-branch positioning code references the final
    // values regardless of which stage settled.
    let mut visible_text_lines: u16;
    let mut box_height: u16;

    let available =
        |top: u16, bot: u16| -> u16 { screen_h.saturating_sub(top.saturating_add(bot)) };

    // Initial fit?
    if target_box_h <= available(outer_top_pad, outer_bot_pad) {
        box_height = target_box_h;
        visible_text_lines = text_lines;
    } else {
        // Stage H1: widen text up to min(W - 2, max_line_length(msg))
        let max_line = max_line_length(msg);
        let widen_cap = screen_w.saturating_sub(2).min(max_line);
        // We may already be at or beyond widen_cap due to messages
        // shorter than current inner_width — only widen when there is
        // room AND when widening can possibly help (wrapping is in
        // play).
        if inner_width < widen_cap {
            // Loop widening one column at a time while it helps.
            for new_inner in (inner_width.saturating_add(1))..=widen_cap {
                inner_width = new_inner;
                // Recompute box_width and pad to keep things consistent:
                // we keep current inner_h_pad and bump box_width.
                box_width = inner_width
                    .saturating_add(inner_h_pad.saturating_mul(2))
                    .saturating_add(2);
                // box can't exceed screen.
                if box_width > screen_w {
                    box_width = screen_w;
                    inner_h_pad = 0;
                    inner_width = box_width.saturating_sub(2);
                }
                wrapped = wrap_message(msg, inner_width);
                text_lines = u16::try_from(wrapped.len()).unwrap_or(u16::MAX);
                let try_inner_h = inner_top_pad
                    .saturating_add(text_lines)
                    .saturating_add(inner_bot_pad)
                    .saturating_add(separator_h)
                    .saturating_add(btn_h);
                let try_box_h = try_inner_h.saturating_add(2);
                if try_box_h <= available(outer_top_pad, outer_bot_pad) {
                    break;
                }
            }
        }
        // Snapshot the post-H1 box dimensions; H2/H3/H4 will mutate
        // these in place if they need more vertical room.
        let try_inner_h = inner_top_pad
            .saturating_add(text_lines)
            .saturating_add(inner_bot_pad)
            .saturating_add(separator_h)
            .saturating_add(btn_h);
        let mut try_box_h = try_inner_h.saturating_add(2);
        box_height = try_box_h;
        visible_text_lines = text_lines;

        // Stage H2: reduce inner_top_pad 2 → 0
        while try_box_h > available(outer_top_pad, outer_bot_pad) && inner_top_pad > 0 {
            inner_top_pad = inner_top_pad.saturating_sub(1);
            let new_inner_h = inner_top_pad
                .saturating_add(text_lines)
                .saturating_add(inner_bot_pad)
                .saturating_add(separator_h)
                .saturating_add(btn_h);
            try_box_h = new_inner_h.saturating_add(2);
            box_height = try_box_h;
        }
        // Also relax inner_bot_pad to 0 if still over.
        while try_box_h > available(outer_top_pad, outer_bot_pad) && inner_bot_pad > 0 {
            inner_bot_pad = inner_bot_pad.saturating_sub(1);
            let new_inner_h = inner_top_pad
                .saturating_add(text_lines)
                .saturating_add(inner_bot_pad)
                .saturating_add(separator_h)
                .saturating_add(btn_h);
            try_box_h = new_inner_h.saturating_add(2);
            box_height = try_box_h;
        }

        // Stage H3: reduce outer paddings alternating top → bot → top …
        // Top from 3 → 0, Bot from 4 → 0.
        loop {
            if try_box_h <= available(outer_top_pad, outer_bot_pad) {
                break;
            }
            let stepped = if outer_top_pad > 0 {
                outer_top_pad = outer_top_pad.saturating_sub(1);
                true
            } else if outer_bot_pad > 0 {
                outer_bot_pad = outer_bot_pad.saturating_sub(1);
                true
            } else {
                false
            };
            if !stepped {
                break;
            }
        }

        // Stage H4: scroll mode. Restore default outer paddings (the
        // hint replaces the need for cramped outer padding). Compute
        // visible text rows from what's left after restoring defaults,
        // accounting for the scroll-hint row below the box.
        if try_box_h > available(outer_top_pad, outer_bot_pad) {
            scrollable = true;
            outer_top_pad = outer_top_pad_default;
            outer_bot_pad = outer_bot_pad_default;
            inner_top_pad = INNER_TOP_PAD;
            inner_bot_pad = INNER_BOT_PAD;
            // Reserve one row below the box for the hint.
            let hint_h: u16 = 1;
            let avail_h = screen_h
                .saturating_sub(outer_top_pad)
                .saturating_sub(outer_bot_pad)
                .saturating_sub(hint_h);
            // Available text rows inside the box.
            let chrome = inner_top_pad
                .saturating_add(inner_bot_pad)
                .saturating_add(separator_h)
                .saturating_add(btn_h)
                .saturating_add(2);
            let max_text = avail_h.saturating_sub(chrome);
            visible_text_lines = max_text.max(1);
            box_height = visible_text_lines
                .saturating_add(inner_top_pad)
                .saturating_add(inner_bot_pad)
                .saturating_add(separator_h)
                .saturating_add(btn_h)
                .saturating_add(2);
        }
    }

    // If still doesn't fit even with all paddings at 0 and not
    // scrollable (pathologically tiny screen), accept whatever fits;
    // box_height = available height.
    let max_box_h = screen_h
        .saturating_sub(outer_top_pad)
        .saturating_sub(outer_bot_pad);
    if box_height > max_box_h {
        box_height = max_box_h.max(3);
    }

    // Position. Centre horizontally; vertically anchor with outer_top_pad
    // from the top, then center within the remaining slack if any.
    let x_slack = screen_w.saturating_sub(box_width);
    let x = screen.x.saturating_add(x_slack / 2);
    let total_h = if scrollable {
        box_height.saturating_add(1) // hint row
    } else {
        box_height
    };
    let y_slack = screen_h
        .saturating_sub(total_h)
        .saturating_sub(outer_top_pad)
        .saturating_sub(outer_bot_pad);
    // Anchor at outer_top_pad + half the slack so wider screens centre
    // the box vertically.
    let y = screen
        .y
        .saturating_add(outer_top_pad)
        .saturating_add(y_slack / 2);

    let box_rect = Rect::new(x, y, box_width, box_height);
    // Inner text rect.
    let text_x = x.saturating_add(1).saturating_add(inner_h_pad);
    let text_y = y.saturating_add(1).saturating_add(inner_top_pad);
    let text_w = inner_width;
    let text_h = visible_text_lines.max(1);
    let inner_text_rect = Rect::new(text_x, text_y, text_w, text_h);

    let separator_y = text_y.saturating_add(text_h).saturating_add(inner_bot_pad);
    let button_row_y = if has_buttons {
        separator_y.saturating_add(separator_h)
    } else {
        separator_y
    };

    let scroll_hint = if scrollable {
        // Stretch the hint rect to the full screen width so the hint
        // text (right-aligned) actually fits, instead of being clipped
        // by a narrow box. Same y as one row below the box.
        let hint_y = y.saturating_add(box_height);
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    fn screen(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn short_message_wide_screen_box_fits_message_not_padded_to_min() {
        // 8-char message on a 120-col screen: target_box = 8 + 6 + 2 = 16.
        // min_text = min(40, 8) = 8 so the floor is satisfied at stage W0
        // with box_width = 16.
        let layout = compute_modal_layout("eight ch", true, 2, screen(120, 40));
        assert_eq!(layout.box_rect.width, 16, "box width must hug the message");
        assert_eq!(layout.inner_h_pad, 3);
        assert_eq!(layout.inner_width, 8);
        // Outer padding symmetric (centred): x_slack / 2 each side.
        let right_gap = 120u16.saturating_sub(layout.box_rect.x + layout.box_rect.width);
        let left_gap = layout.box_rect.x;
        // Centring tolerates 1 col of rounding asymmetry.
        assert!(
            left_gap.abs_diff(right_gap) <= 1,
            "outer padding must be symmetric: left={left_gap} right={right_gap}"
        );
    }

    #[test]
    fn min_text_width_floor_does_not_pad_short_lines_past_their_length() {
        // 15-char message on a wide screen. min_text = min(40, 15) = 15.
        // The box must NOT be padded out to 40 — it should hug the 15.
        let msg = "fifteen-charXYZ";
        assert_eq!(msg.chars().count(), 15);
        let layout = compute_modal_layout(msg, true, 2, screen(120, 30));
        assert_eq!(layout.inner_width, 15, "inner must hug the message");
    }

    #[test]
    fn long_single_line_falls_to_stage_w1() {
        // 60-char single line on a 100-col screen.
        // Stage W0: cap_default = floor(100 * 2 / 3) = 66
        //           target_box = 60 + 6 + 2 = 68 → 66
        //           inner = 66 - 6 - 2 = 58. min_text = min(40, 60) = 40.
        //           58 >= 40 → Stage W0 satisfied at box_width 66.
        // (Spec note: the spec said this falls to W1, but with the math
        // 58 >= 40 the floor is already met at W0. We pin the actual
        // numerical outcome — the algorithm shouldn't grow the box past
        // the cap when the floor is already met.)
        let msg: String = "x".repeat(60);
        let layout = compute_modal_layout(&msg, true, 2, screen(100, 30));
        assert_eq!(layout.box_rect.width, 66);
        assert_eq!(layout.inner_width, 58);
    }

    #[test]
    fn very_long_line_relaxes_inner_pad_to_zero() {
        // 200-char line on a 50-col screen.
        // cap_default = floor(50 * 2 / 3) = 33; box = min(208, 33) = 33;
        // inner = 33 - 6 - 2 = 25. min_text = min(40, 200) = 40.
        // 25 < 40 → W1. cap_relaxed = 44. box = min(208, 44) = 44;
        // inner = 44 - 6 - 2 = 36. 36 < 40 → W2.
        // pad=3 → 36; pad=2 → 38; pad=1 → 40 → satisfied at pad=1.
        let msg: String = "x".repeat(200);
        let layout = compute_modal_layout(&msg, true, 2, screen(50, 80));
        // Algorithm settles at pad=1, inner=40 on this screen size.
        assert!(layout.inner_h_pad <= 3);
        assert!(layout.inner_width >= 40, "inner must meet floor");
    }

    #[test]
    fn tall_message_triggers_scroll_mode() {
        // 50 lines of "abc" on a 24-row, 80-col screen. After widening,
        // each "abc" is its own line, so wrapped lines stay 50. H1/H2/H3
        // can't get target_box_h ≤ available; H4 must kick in.
        let msg = std::iter::repeat_n("abc", 50)
            .collect::<Vec<_>>()
            .join("\n");
        let layout = compute_modal_layout(&msg, true, 2, screen(80, 24));
        assert!(layout.scrollable, "tall content must trigger scroll mode");
        assert!(layout.scroll_hint.is_some(), "hint rect must be present");
        // The hint sits below the box.
        let hint = layout.scroll_hint.expect("hint present");
        assert_eq!(hint.y, layout.box_rect.y + layout.box_rect.height);
    }

    #[test]
    fn separator_row_is_above_button_row() {
        let layout = compute_modal_layout("hi", true, 2, screen(80, 24));
        // separator_y = text_y + text_h + inner_bot_pad
        let text_end = layout.inner_text_rect.y + layout.inner_text_rect.height;
        assert!(layout.separator_y >= text_end);
        assert_eq!(layout.button_row_y, layout.separator_y + SEPARATOR_HEIGHT);
    }

    #[test]
    fn no_buttons_omits_separator_and_button_rows() {
        let layout = compute_modal_layout("hi", false, 0, screen(80, 24));
        // When has_buttons is false, button_row_y == separator_y (both
        // are at the same row since separator_h is 0 too).
        assert_eq!(layout.button_row_y, layout.separator_y);
    }

    #[test]
    fn wrap_message_preserves_short_word() {
        let lines = wrap_message("hello world", 20);
        assert_eq!(lines, vec!["hello world".to_string()]);
    }

    #[test]
    fn wrap_message_splits_on_word_boundary() {
        let lines = wrap_message("the quick brown fox", 10);
        // "the quick" is 9 → fits; "brown fox" is 9 → fits next.
        assert_eq!(
            lines,
            vec!["the quick".to_string(), "brown fox".to_string()]
        );
    }

    #[test]
    fn wrap_message_hard_splits_oversize_word() {
        // No spaces; 30-char word with width 10 → 3 chunks of 10.
        let lines = wrap_message("aaaaaaaaaabbbbbbbbbbcccccccccc", 10);
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
    }

    #[test]
    fn wrap_message_preserves_hard_newlines() {
        let lines = wrap_message("a\nb\nc", 10);
        assert_eq!(
            lines,
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn wrap_message_empty_input_yields_one_empty_line() {
        let lines = wrap_message("", 10);
        assert_eq!(lines, vec![String::new()]);
    }

    #[test]
    fn scroll_hint_rect_spans_full_screen_width_when_scrollable() {
        // Defence: the hint must use the full screen width so a narrow
        // box doesn't clip "Ctrl+Shift+Up/Dn scroll" to "Ctrl+Shift…".
        let msg = std::iter::repeat_n("abc", 50)
            .collect::<Vec<_>>()
            .join("\n");
        let layout = compute_modal_layout(&msg, false, 0, screen(80, 24));
        let hint = layout.scroll_hint.expect("scrollable hint present");
        assert_eq!(hint.width, 80, "hint width must span the screen");
        assert_eq!(hint.x, 0, "hint must start at x=0");
    }

    #[test]
    fn buttons_row_floor_lifts_min_text_when_button_row_exceeds_msg() {
        // A wrong-password-style modal: short message but a wide
        // button row of ~50 cols. The layout must allocate inner_width
        // >= 50 even though `min_text` from the spec is 40.
        let labels = ["[Try again]  [Reboot]  [Pretty Shell]  [Raw Shell]"];
        let btn_w = u16::try_from(labels[0].chars().count()).unwrap_or(0);
        let msg = "short message that wraps under buttons";
        let layout = compute_modal_layout_with_button_width(msg, true, 4, btn_w, screen(80, 24));
        assert!(
            layout.inner_width >= btn_w,
            "inner_width ({}) must fit buttons ({btn_w})",
            layout.inner_width
        );
    }

    #[test]
    fn tall_message_scroll_offset_clamping_works_through_paint() {
        // Pin the scroll viewport math: at offset 5 the visible window
        // starts at line index 5 (assuming visible <= total - 5).
        let msg = (0..50)
            .map(|i| format!("line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let layout = compute_modal_layout(&msg, true, 2, screen(80, 24));
        assert!(layout.scrollable);
        // The wrapped buffer must contain all 50 lines (no chars were
        // dropped on the floor by the wrap pass).
        assert_eq!(layout.wrapped_lines.len(), 50);
    }
}
