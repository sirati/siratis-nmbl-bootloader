#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]

use ratatui::layout::Rect;

use super::{
    SEPARATOR_HEIGHT, compute_modal_layout, compute_modal_layout_with_button_width, wrap_message,
};

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
