//! Tests for modal, boot-status and footer render functions.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests"
)]

use crate::ui::app::BootStatusData;

use super::{
    ModalConfirmScreenData, ModalErrorScreenData,
    list_edit::render_boot_status,
    modals::{render_modal_confirm, render_modal_error},
    tests_screens::{buffer_lines, buffer_text, new_term},
};

fn boot_status_data<'a>(phase: &'a str, lines: &[&str], spinner_frame: u8) -> BootStatusData<'a> {
    BootStatusData {
        phase: std::borrow::Cow::Borrowed(phase),
        log_lines: lines.iter().map(|s| (*s).to_string()).collect(),
        spinner_frame,
    }
}

#[test]
fn test_render_modal_confirm_shows_title_buttons_and_hint() {
    let data = ModalConfirmScreenData {
        title: "Boot one?",
        message: "Found 3 generations.",
        yes_label: "Yes",
        no_label: "Back",
        yes_selected: true,
        hint: "Left/Right select  Enter confirm  Esc cancel",
        scroll_offset: 0,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("Boot one?"), "title missing in:\n{text}");
    assert!(
        text.contains("Found 3 generations"),
        "message missing in:\n{text}"
    );
    assert!(text.contains("[Yes]"), "yes button missing in:\n{text}");
    assert!(text.contains("[Back]"), "no button missing in:\n{text}");
    assert!(text.contains("Enter confirm"), "hint missing in:\n{text}");
}

#[test]
fn test_render_modal_confirm_renders_with_no_selected() {
    // Pin the other branch of yes_selected: when false, the "No"
    // button gets the highlight. The plain-text scan can't see
    // colour but it can confirm both labels paint.
    let data = ModalConfirmScreenData {
        title: "Confirm",
        message: "Proceed?",
        yes_label: "Boot",
        no_label: "Back",
        yes_selected: false,
        hint: "h",
        scroll_offset: 0,
    };
    let mut term = new_term(60, 16);
    term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("[Boot]"), "yes label missing in:\n{text}");
    assert!(text.contains("[Back]"), "no label missing in:\n{text}");
}

#[test]
fn test_render_modal_confirm_separator_row_dash_space_filling_inner_width() {
    // Pin the spec's separator row: `- ` repeated across the inner
    // text width. Cell scan finds the dash pattern on the
    // separator row.
    let data = ModalConfirmScreenData {
        title: "T",
        message: "hello world",
        yes_label: "Yes",
        no_label: "No",
        yes_selected: true,
        hint: "h",
        scroll_offset: 0,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
    let text = buffer_text(&term);
    // The dash-space pattern must appear at least once.
    assert!(text.contains("- - -"), "separator missing: \n{text}");
}

#[test]
fn test_render_modal_confirm_short_msg_box_hugs_message_not_padded_to_min() {
    // 11-char "hello world" message: the box must not pad out to 40
    // — `min(MIN_TEXT_WIDTH, max_line_length(msg)) = 11`, so the
    // floor is 11 and the box hugs the message + buttons.
    let data = ModalConfirmScreenData {
        title: "T",
        message: "hello world",
        yes_label: "Yes",
        no_label: "No",
        yes_selected: true,
        hint: "h",
        scroll_offset: 0,
    };
    let mut term = new_term(120, 30);
    term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
    let lines = buffer_lines(&term);
    // Find the title row and measure the box width.
    let row = lines.iter().find(|l| l.contains("┌T")).expect("title row");
    // Box width = sum of chars between the corners; quick proxy:
    // the row's leading spaces + box span + trailing spaces.
    let lead = row.chars().take_while(|c| *c == ' ').count();
    let trail = row.chars().rev().take_while(|c| *c == ' ').count();
    // Symmetry check: roughly equal left/right margins.
    assert!(
        lead.abs_diff(trail) <= 1,
        "box must be centred: lead={lead} trail={trail}, row={row:?}"
    );
}

#[test]
fn test_render_modal_error_shows_title_message_and_hint() {
    // Modal must surface the title, the error chain body, and the
    // any-key hint in the footer. Cell-by-cell text scan is enough
    // — the actual layout/colour is incidental detail tested in
    // ratatui's own suite.
    let data = ModalErrorScreenData {
        title: "Pretty Shell failed to start",
        message: "openpty failed: ENOENT: No such file or directory",
        hint: "press any key to continue",
        scroll_offset: 0,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_modal_error(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("Pretty Shell failed to start"),
        "title missing in:\n{text}"
    );
    assert!(
        text.contains("openpty failed"),
        "message missing in:\n{text}"
    );
    assert!(text.contains("press any key"), "hint missing in:\n{text}");
}

#[test]
fn test_render_modal_error_tall_message_shows_scroll_hint_outside_box() {
    // 50 short lines on a 24-row screen: the layout MUST enter
    // scroll mode (stage H4) and paint "Ctrl+Shift+Up/Dn scroll"
    // on the row just BELOW the box.
    let msg = (0..50)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let data = ModalErrorScreenData {
        title: "Tall",
        message: &msg,
        hint: "press any key to continue",
        scroll_offset: 0,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_modal_error(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("Ctrl+Shift+Up/Dn scroll"),
        "scroll hint missing:\n{text}"
    );
}

#[test]
fn test_render_modal_error_tall_message_respects_scroll_offset() {
    // With offset 5 the first visible line must be "line 05",
    // not "line 00".
    let msg = (0..50)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let data = ModalErrorScreenData {
        title: "Tall",
        message: &msg,
        hint: "press any key to continue",
        scroll_offset: 5,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_modal_error(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("line 05"),
        "offset 5 should show line 05:\n{text}"
    );
    assert!(
        !text.contains("line 00"),
        "offset 5 should hide line 00:\n{text}"
    );
}

#[test]
fn test_render_boot_status_shows_header_lines_phase_and_spinner_frame0() {
    // Layout assumes a 24-row terminal: 3-row header, body in the
    // middle, 1-row status. Three log lines fit comfortably in the
    // body so all three should be visible.
    let data = boot_status_data(
        "phase 1: udev coldplug",
        &["mount /proc", "mount /sys", "starting udev"],
        0,
    );
    let mut term = new_term(80, 24);
    term.draw(|f| render_boot_status(f, &data)).expect("draw");
    let text = buffer_text(&term);

    assert!(text.contains("sirati's NMBL"), "missing header in:\n{text}");
    assert!(
        text.contains("phase 1: udev coldplug"),
        "missing phase in:\n{text}"
    );
    for line in ["mount /proc", "mount /sys", "starting udev"] {
        assert!(text.contains(line), "missing log line {line:?} in:\n{text}");
    }
    // Frame 0 -> '|'.
    assert!(text.contains('|'), "missing spinner glyph '|' in:\n{text}");
}

#[test]
fn test_render_boot_status_spinner_advances_with_frame() {
    // Render at frame 0 and frame 1 and assert the status row
    // differs. Frame 1 must contain '/'; frame 0 must not (the
    // header / log content is fixed in this fixture, so no other
    // '/' appears in the buffer apart from the spinner cell).
    let data0 = boot_status_data("waiting", &["a"], 0);
    let data1 = boot_status_data("waiting", &["a"], 1);

    let mut t0 = new_term(40, 10);
    t0.draw(|f| render_boot_status(f, &data0)).expect("draw");
    let mut t1 = new_term(40, 10);
    t1.draw(|f| render_boot_status(f, &data1)).expect("draw");

    let txt0 = buffer_text(&t0);
    let txt1 = buffer_text(&t1);

    assert!(txt0.contains('|'), "frame0 must contain |");
    assert!(txt1.contains('/'), "frame1 must contain /");
    assert_ne!(txt0, txt1, "spinner advance must change buffer");
}

#[test]
fn test_render_boot_status_shows_esc_to_abort_hint() {
    // The wait hint must always be present on the BootStatus screen so
    // the operator knows the wait is interruptible (Esc) AND that the
    // boot log is reachable (Ctrl+L) without having to read the docs.
    let data = boot_status_data("phase 3b: waiting", &["mount /proc"], 0);
    let mut term = new_term(80, 24);
    term.draw(|f| render_boot_status(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("Esc abort"),
        "missing 'Esc abort' hint in:\n{text}"
    );
    assert!(
        text.contains("Ctrl+L logs"),
        "missing 'Ctrl+L logs' hint in:\n{text}"
    );
}

#[test]
fn test_render_boot_status_esc_hint_is_bottom_right_of_log_panel() {
    // Stronger assertion: the hint must sit on the bottom-right of
    // the log box, mirroring pretty-shell's scroll-hint placement.
    // We pin the row (last interior row of the log panel) and the
    // alignment (right edge) so a future refactor can't silently
    // move the hint to a place the operator won't notice.
    let data = boot_status_data("p", &["line"], 0);
    let mut term = new_term(80, 24);
    term.draw(|f| render_boot_status(f, &data)).expect("draw");
    let lines = buffer_lines(&term);

    // Layout: 3-row header, body fills the middle, 1-row status.
    // The log box borders the body, so its bottom border row is at
    // `header_h + body_h - 1 = 3 + 20 - 1 = 22`. The hint paints on
    // the LAST INNER row, which is one above the bottom border:
    // row 21.
    let hint_row_idx = 21;
    let hint_row = lines
        .get(hint_row_idx)
        .expect("hint row must exist in 24-row term");
    assert!(
        hint_row.contains("Esc abort"),
        "hint missing on expected row {hint_row_idx}: {hint_row:?}"
    );
    // Right alignment: the hint should sit near the right border,
    // not the left. The hint string now leads with "Ctrl+L logs", so
    // pin that the right-aligned block reaches the right edge — the
    // trailing "Esc abort" must start past mid-width (col 40).
    let hint_col = hint_row.find("Esc abort").expect("hint substring");
    assert!(
        hint_col > 40,
        "hint must be right-aligned (col {hint_col} should exceed 40): {hint_row:?}"
    );
}

#[test]
fn test_render_boot_status_clips_to_panel_height() {
    // 50 lines into a 10-row terminal. The header eats 3 rows and
    // the status line eats 1, leaving 6 rows for the bordered log
    // panel; the panel borders eat 2 more, so only ~4 lines of
    // content are visible. The exact panel height isn't load-
    // bearing for this test — what matters is that *only* the
    // most recent lines appear and *none* of the earliest ones
    // do, regardless of clipping math.
    //
    // Zero-padded width-2 indices make each label uniquely
    // identifiable as a substring (e.g. "log-00" is not a prefix
    // of "log-49"), so `str::contains` is a safe substring check.
    let lines: Vec<String> = (0..50).map(|i| format!("log-{i:02}")).collect();
    let lines_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let data = boot_status_data("phase X", &lines_refs, 0);

    let mut term = new_term(40, 10);
    term.draw(|f| render_boot_status(f, &data)).expect("draw");
    let text = buffer_text(&term);

    // The last line must appear; the first and one mid-range
    // sample must not.
    assert!(
        text.contains("log-49"),
        "most-recent line missing in:\n{text}"
    );
    assert!(
        !text.contains("log-00"),
        "earliest line leaked through clipping in:\n{text}"
    );
    assert!(
        !text.contains("log-10"),
        "mid-range line leaked through clipping in:\n{text}"
    );
}
