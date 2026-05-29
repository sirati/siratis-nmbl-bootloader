use ratatui::layout::Rect;

use super::wrap::wrap_message;
use super::{
    INNER_BOT_PAD, INNER_H_PAD, INNER_TOP_PAD, MAX_BOX_WIDTH_DEN, MAX_BOX_WIDTH_NUM,
    MIN_TEXT_WIDTH, ModalLayout, OUTER_BOT_PAD, OUTER_H_BUDGET, OUTER_TOP_PAD, SEPARATOR_HEIGHT,
};

/// Char-width of the longest line in `msg`, treating embedded `\n` as
/// line separators. Used by the width algorithm to decide whether a
/// wrap is even needed.
pub(super) fn max_line_length(msg: &str) -> u16 {
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

/// Width-pass result: final `inner_h_pad`, `inner_width`.
#[derive(Debug, Clone, Copy)]
struct WidthChoice {
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
            inner_h_pad: pad,
            inner_width: inner,
        }
    }
}

/// Mutable sizing state threaded through the height-degradation stages.
struct HeightState<'a> {
    msg: &'a str,
    screen_w: u16,
    screen_h: u16,
    inner_width: &'a mut u16,
    inner_h_pad: &'a mut u16,
    inner_top_pad: &'a mut u16,
    inner_bot_pad: &'a mut u16,
    outer_top_pad: &'a mut u16,
    outer_bot_pad: &'a mut u16,
    wrapped: &'a mut Vec<String>,
    text_lines: &'a mut u16,
    scrollable: &'a mut bool,
    outer_top_pad_default: u16,
    outer_bot_pad_default: u16,
    separator_h: u16,
    btn_h: u16,
}

impl HeightState<'_> {
    fn available(&self, top: u16, bot: u16) -> u16 {
        self.screen_h.saturating_sub(top.saturating_add(bot))
    }

    fn inner_h(&self) -> u16 {
        self.inner_top_pad
            .saturating_add(*self.text_lines)
            .saturating_add(*self.inner_bot_pad)
            .saturating_add(self.separator_h)
            .saturating_add(self.btn_h)
    }

    /// Stage H1: widen text to reduce wrapped-line count.
    fn stage_h1(&mut self) {
        let max_line = max_line_length(self.msg);
        let widen_cap = self.screen_w.saturating_sub(2).min(max_line);
        if *self.inner_width >= widen_cap {
            return;
        }
        for new_inner in (self.inner_width.saturating_add(1))..=widen_cap {
            *self.inner_width = new_inner;
            let mut bw = self
                .inner_width
                .saturating_add(self.inner_h_pad.saturating_mul(2))
                .saturating_add(2);
            if bw > self.screen_w {
                bw = self.screen_w;
                *self.inner_h_pad = 0;
                *self.inner_width = bw.saturating_sub(2);
            }
            *self.wrapped = wrap_message(self.msg, *self.inner_width);
            *self.text_lines = u16::try_from(self.wrapped.len()).unwrap_or(u16::MAX);
            if self.inner_h().saturating_add(2)
                <= self.available(*self.outer_top_pad, *self.outer_bot_pad)
            {
                break;
            }
        }
    }

    /// Stage H4: enter scroll mode, restore default paddings.
    /// Returns `(box_height, visible_text_lines)` for scroll layout.
    fn stage_h4(&mut self) -> (u16, u16) {
        *self.scrollable = true;
        *self.outer_top_pad = self.outer_top_pad_default;
        *self.outer_bot_pad = self.outer_bot_pad_default;
        *self.inner_top_pad = INNER_TOP_PAD;
        *self.inner_bot_pad = INNER_BOT_PAD;
        // Reserve one row below the box for the hint.
        let hint_h: u16 = 1;
        let avail_h = self
            .screen_h
            .saturating_sub(*self.outer_top_pad)
            .saturating_sub(*self.outer_bot_pad)
            .saturating_sub(hint_h);
        // Available text rows inside the box.
        let chrome = self
            .inner_top_pad
            .saturating_add(*self.inner_bot_pad)
            .saturating_add(self.separator_h)
            .saturating_add(self.btn_h)
            .saturating_add(2);
        let visible = avail_h.saturating_sub(chrome).max(1);
        let bh = visible
            .saturating_add(*self.inner_top_pad)
            .saturating_add(*self.inner_bot_pad)
            .saturating_add(self.separator_h)
            .saturating_add(self.btn_h)
            .saturating_add(2);
        (bh, visible)
    }

    /// Stages H1–H4. Returns `(box_height, visible_text_lines)`.
    fn degrade(&mut self) -> (u16, u16) {
        self.stage_h1();

        let mut try_box_h = self.inner_h().saturating_add(2);
        let mut box_height = try_box_h;
        let mut visible_text_lines = *self.text_lines;

        // Stage H2: reduce inner_top_pad 2 → 0
        while try_box_h > self.available(*self.outer_top_pad, *self.outer_bot_pad)
            && *self.inner_top_pad > 0
        {
            *self.inner_top_pad = self.inner_top_pad.saturating_sub(1);
            try_box_h = self.inner_h().saturating_add(2);
            box_height = try_box_h;
        }
        // Also relax inner_bot_pad to 0 if still over.
        while try_box_h > self.available(*self.outer_top_pad, *self.outer_bot_pad)
            && *self.inner_bot_pad > 0
        {
            *self.inner_bot_pad = self.inner_bot_pad.saturating_sub(1);
            try_box_h = self.inner_h().saturating_add(2);
            box_height = try_box_h;
        }

        // Stage H3: reduce outer paddings alternating top → bot → top …
        // Top from 3 → 0, Bot from 4 → 0.
        loop {
            if try_box_h <= self.available(*self.outer_top_pad, *self.outer_bot_pad) {
                break;
            }
            let stepped = if *self.outer_top_pad > 0 {
                *self.outer_top_pad = self.outer_top_pad.saturating_sub(1);
                true
            } else if *self.outer_bot_pad > 0 {
                *self.outer_bot_pad = self.outer_bot_pad.saturating_sub(1);
                true
            } else {
                false
            };
            if !stepped {
                break;
            }
        }

        // Stage H4: scroll mode.
        if try_box_h > self.available(*self.outer_top_pad, *self.outer_bot_pad) {
            (box_height, visible_text_lines) = self.stage_h4();
        }

        (box_height, visible_text_lines)
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

    let max_line = max_line_length(msg);
    let effective_msg_len = max_line.max(btn_row_width);
    let wrap_floor = MIN_TEXT_WIDTH.min(max_line.max(1));
    let min_text_floor = wrap_floor.max(btn_row_width);
    let initial = compute_width_with_target(effective_msg_len, min_text_floor, msg, screen_w);
    let mut inner_h_pad = initial.inner_h_pad;
    let mut inner_width = initial.inner_width.max(1);

    // Buttons row: 1 line. Reserve 2 rows for narrow boxes with >= 4 buttons.
    let btn_h: u16 = if has_buttons {
        if btn_count >= 4 && inner_width < 60 {
            2
        } else {
            1
        }
    } else {
        0
    };
    let separator_h = if has_buttons { SEPARATOR_HEIGHT } else { 0 };

    let mut wrapped = wrap_message(msg, inner_width);
    let mut text_lines = u16::try_from(wrapped.len()).unwrap_or(u16::MAX);

    let target_box_h = INNER_TOP_PAD
        .saturating_add(text_lines)
        .saturating_add(INNER_BOT_PAD)
        .saturating_add(separator_h)
        .saturating_add(btn_h)
        .saturating_add(2);

    let outer_top_pad_default = OUTER_TOP_PAD;
    let outer_bot_pad_default = OUTER_BOT_PAD;
    let mut outer_top_pad = outer_top_pad_default;
    let mut outer_bot_pad = outer_bot_pad_default;
    let mut inner_top_pad = INNER_TOP_PAD;
    let mut inner_bot_pad = INNER_BOT_PAD;
    let mut scrollable = false;

    let avail = |top: u16, bot: u16| -> u16 { screen_h.saturating_sub(top.saturating_add(bot)) };

    let (box_height, visible_text_lines) = if target_box_h <= avail(outer_top_pad, outer_bot_pad) {
        (target_box_h, text_lines)
    } else {
        let mut hs = HeightState {
            msg,
            screen_w,
            screen_h,
            inner_width: &mut inner_width,
            inner_h_pad: &mut inner_h_pad,
            inner_top_pad: &mut inner_top_pad,
            inner_bot_pad: &mut inner_bot_pad,
            outer_top_pad: &mut outer_top_pad,
            outer_bot_pad: &mut outer_bot_pad,
            wrapped: &mut wrapped,
            text_lines: &mut text_lines,
            scrollable: &mut scrollable,
            outer_top_pad_default,
            outer_bot_pad_default,
            separator_h,
            btn_h,
        };
        hs.degrade()
    };

    // Compute box_width from final inner_width (H1 may have widened it).
    let box_width = inner_width
        .saturating_add(inner_h_pad.saturating_mul(2))
        .saturating_add(2)
        .min(screen_w);

    super::build_layout(
        screen,
        box_width,
        box_height,
        inner_h_pad,
        inner_top_pad,
        inner_bot_pad,
        visible_text_lines,
        has_buttons,
        separator_h,
        scrollable,
        outer_top_pad,
        outer_bot_pad,
        wrapped,
        inner_width,
    )
}
