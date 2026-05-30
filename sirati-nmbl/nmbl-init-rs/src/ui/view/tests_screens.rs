//! Tests for list, edit, and passphrase render functions.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests"
)]

use std::path::PathBuf;

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Modifier, Style};

use crate::generations::Generation;
use crate::ui::app::SPINNER_FRAMES;
use crate::ui::app::SPINNER_GLYPHS;

use super::{
    EditScreenData, ListScreenData, PassphraseScreenData,
    list_edit::{render_edit, render_list},
    modals::render_passphrase,
    render_footer,
};

fn fake_gen(number: u32, label: &str, params: &[&str]) -> Generation {
    Generation {
        number,
        profile_link: PathBuf::from(format!("/p/system-{number}-link")),
        kernel: PathBuf::from("/p/kernel"),
        initrd: PathBuf::from("/p/initrd"),
        init_path: PathBuf::from(format!("/p/system-{number}-link/init")),
        kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
        label: label.to_string(),
    }
}

pub(super) fn buffer_lines(term: &Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect::<String>()
        })
        .collect()
}

pub(super) fn buffer_text(term: &Terminal<TestBackend>) -> String {
    buffer_lines(term).join("\n")
}

pub(super) fn new_term(w: u16, h: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(w, h)).expect("terminal")
}

/// Walk the rendered buffer and return the cell style under the first
/// occurrence of `needle`'s leading char on the line that contains it.
pub(super) fn style_under_first_match(term: &Terminal<TestBackend>, needle: &str) -> Style {
    let buf = term.backend().buffer();
    let head = needle.chars().next().expect("non-empty needle");
    let head_str = head.to_string();
    for y in 0..buf.area.height {
        let row: String = (0..buf.area.width)
            .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
            .collect();
        if let Some(byte_idx) = row.find(needle) {
            // Byte index in a single-byte ASCII needle equals column.
            let col_chars = row
                .get(..byte_idx)
                .map_or(0, |prefix| prefix.chars().count());
            // Locate cell at (col_chars, y) and assert its symbol matches.
            let cell = buf
                .cell((col_chars as u16, y))
                .expect("cell in rendered area");
            assert_eq!(cell.symbol(), head_str);
            return cell.style();
        }
    }
    panic!("needle {needle:?} not found in rendered buffer");
}

#[test]
fn test_render_list_shows_generation_numbers() {
    let gens = vec![
        fake_gen(42, "nixos-25.05", &["root=/dev/sda1", "quiet"]),
        fake_gen(10, "nixos-24.11", &["console=ttyS0"]),
    ];
    // selected_index past the end exercises clamping; show_kernel_params
    // exercises the dim second-line branch.
    let data = ListScreenData {
        generations: &gens,
        selected_index: 99,
        countdown_remaining_secs: Some(3),
        show_kernel_params: true,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_list(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("#42"), "missing #42 in:\n{text}");
    assert!(text.contains("#10"), "missing #10 in:\n{text}");
    assert!(text.contains("auto-boot"), "missing countdown");
    assert!(text.contains("console=ttyS0"), "missing params line");
}

#[test]
fn test_render_edit_shows_generation_number_and_cmdline() {
    let g = fake_gen(99, "", &[]);
    let data = EditScreenData {
        generation: &g,
        edited_cmdline: "init=/sbin/init quiet",
        cursor_position: 5,
    };
    let mut term = new_term(80, 10);
    term.draw(|f| render_edit(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("#99"), "missing gen number");
    assert!(text.contains("init=/sbin/init"), "missing cmdline");
    assert!(text.contains('^'), "missing caret indicator");
}

#[test]
fn test_render_edit_caret_column_for_multibyte_cmdline() {
    // "héllo" is 6 bytes ('h' 'é'×2 'l' 'l' 'o'); cursor at byte
    // index 4 sits right after the 'l' that follows 'é', which
    // is char column 3. A naive `chars().take(cursor).count()`
    // would render the caret at column 4 — this test pins the
    // byte→char conversion.
    let g = fake_gen(7, "", &[]);
    let data = EditScreenData {
        generation: &g,
        edited_cmdline: "héllo",
        cursor_position: 4,
    };
    let mut term = new_term(40, 10);
    term.draw(|f| render_edit(f, &data)).expect("draw");
    let lines = buffer_lines(&term);

    // Find the caret row (the line that contains '^') and the
    // text row that precedes it.
    let caret_row = lines
        .iter()
        .position(|l| l.contains('^'))
        .expect("caret '^' must appear in rendered buffer");
    let caret_line = &lines[caret_row];
    // Count chars (not bytes) so multi-byte border glyphs like '│'
    // don't throw the column off.
    let caret_col = caret_line
        .chars()
        .position(|c| c == '^')
        .expect("'^' present");

    // The border draws on column 0, so the body starts at col 1.
    // Column-3 of the edited string is column-4 of the row.
    assert_eq!(
        caret_col,
        4,
        "caret rendered at row col {caret_col}, expected 4 (string col 3) \
         in rendered lines:\n{}",
        lines.join("\n")
    );
}

#[test]
fn test_render_passphrase_dots_and_label() {
    let data = PassphraseScreenData {
        prompt_label: "Unlock /dev/sda2",
        buffer_len: 5,
        cursor_column: 5,
        caps_lock_on: false,
        select_generation: false,
        verifying: false,
        spinner_frame: 0,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_passphrase(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("*****|"), "wrong mask count in:\n{text}");
    assert!(text.contains("Unlock /dev/sda2"));
    assert!(
        !text.contains("verifying"),
        "non-verifying render must not show the spinner label"
    );
    assert!(
        text.contains("Enter=submit"),
        "default footer hint must appear: {text}"
    );
}

#[test]
fn test_render_passphrase_verifying_shows_spinner_and_label() {
    // When verifying=true the modal must paint a spinner glyph and
    // the "verifying passphrase..." label so the operator doesn't
    // think the UI hung while cryptsetup runs.
    for frame_idx in 0..SPINNER_FRAMES {
        let data = PassphraseScreenData {
            prompt_label: "Unlock /dev/sda2",
            buffer_len: 8,
            cursor_column: 8,
            caps_lock_on: false,
            select_generation: false,
            verifying: true,
            spinner_frame: frame_idx,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_passphrase(f, &data)).expect("draw");
        let text = buffer_text(&term);
        // The dotted input row is still present.
        assert!(text.contains("********|"), "input row missing: {text}");
        // The verifying overlay row.
        assert!(
            text.contains("verifying passphrase"),
            "missing verifying label at frame {frame_idx}:\n{text}"
        );
        // The expected spinner glyph for this frame.
        let expected = SPINNER_GLYPHS[frame_idx as usize];
        assert!(
            text.contains(expected),
            "expected spinner glyph '{expected}' at frame {frame_idx}:\n{text}"
        );
        // Footer hint switches.
        assert!(
            text.contains("verifying..."),
            "verifying footer hint missing at frame {frame_idx}:\n{text}"
        );
    }
}

#[test]
fn test_render_passphrase_verifying_out_of_range_frame_does_not_panic() {
    // Defence-in-depth: a caller that didn't wrap modulo
    // SPINNER_FRAMES must not crash the renderer. We pin that the
    // out-of-range frame falls back to a sensible default glyph.
    let data = PassphraseScreenData {
        prompt_label: "Unlock",
        buffer_len: 2,
        cursor_column: 2,
        caps_lock_on: false,
        select_generation: false,
        verifying: true,
        spinner_frame: 99,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_passphrase(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("verifying passphrase"));
}

#[test]
fn test_render_passphrase_submit_hint_dim_when_buffer_empty() {
    // Empty buffer → "Enter=submit" rendered DIM so the disabled
    // state is visible to the operator. Non-empty buffer → default.
    let empty = PassphraseScreenData {
        prompt_label: "Unlock root",
        buffer_len: 0,
        cursor_column: 0,
        caps_lock_on: false,
        select_generation: false,
        verifying: false,
        spinner_frame: 0,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_passphrase(f, &empty)).expect("draw");
    let style_empty = style_under_first_match(&term, "Enter=submit");
    assert!(
        style_empty.add_modifier.contains(Modifier::DIM),
        "Enter=submit must be DIM when buffer is empty; got {style_empty:?}",
    );

    let filled = PassphraseScreenData {
        prompt_label: "Unlock root",
        buffer_len: 3,
        cursor_column: 3,
        caps_lock_on: false,
        select_generation: false,
        verifying: false,
        spinner_frame: 0,
    };
    let mut term2 = new_term(80, 24);
    term2.draw(|f| render_passphrase(f, &filled)).expect("draw");
    let style_filled = style_under_first_match(&term2, "Enter=submit");
    assert!(
        !style_filled.add_modifier.contains(Modifier::DIM),
        "Enter=submit must NOT be DIM with non-empty buffer; got {style_filled:?}",
    );
}

/// The masked caret must sit at `cursor_column`, not always at the
/// end: a 5-char secret with the cursor at column 2 renders
/// `**|***` so the operator sees where a mid-string edit lands.
#[test]
fn test_render_passphrase_caret_tracks_cursor_column() {
    let data = PassphraseScreenData {
        prompt_label: "Unlock",
        buffer_len: 5,
        cursor_column: 2,
        verifying: false,
        spinner_frame: 0,
        caps_lock_on: false,
        select_generation: false,
    };
    let mut term = new_term(80, 24);
    term.draw(|f| render_passphrase(f, &data)).expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("**|***"),
        "caret must render after 2 dots with 3 trailing dots:\n{text}"
    );
}

/// Caps-Lock warning must appear when on and the box geometry must be
/// byte-for-byte identical whether the warning shows or not — the
/// reserved row guarantees zero reflow.
#[test]
fn test_render_passphrase_caps_lock_warning_does_not_resize_box() {
    let base = |caps: bool| PassphraseScreenData {
        prompt_label: "Unlock root",
        buffer_len: 4,
        cursor_column: 4,
        verifying: false,
        spinner_frame: 0,
        caps_lock_on: caps,
        select_generation: false,
    };

    let mut term_off = new_term(80, 24);
    term_off
        .draw(|f| render_passphrase(f, &base(false)))
        .expect("draw");
    let mut term_on = new_term(80, 24);
    term_on
        .draw(|f| render_passphrase(f, &base(true)))
        .expect("draw");

    let off = buffer_lines(&term_off);
    let on = buffer_lines(&term_on);

    // Warning present only when on.
    assert!(
        on.join("\n").contains("Caps Lock is ON"),
        "warning must show when caps on:\n{}",
        on.join("\n")
    );
    assert!(
        !off.join("\n").contains("Caps Lock is ON"),
        "warning must be absent when caps off:\n{}",
        off.join("\n")
    );

    // The box border rows must be at the SAME positions in both
    // renders — i.e. the modal occupies identical rows. Compare the
    // set of rows that contain a vertical border glyph.
    let border_rows = |lines: &[String]| -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains('│') || l.contains('┌') || l.contains('└'))
            .map(|(i, _)| i)
            .collect()
    };
    assert_eq!(
        border_rows(&off),
        border_rows(&on),
        "modal box must occupy identical rows regardless of caps warning"
    );
}

#[test]
fn test_render_footer_text_present() {
    use ratatui::layout::Rect;
    let mut term = new_term(40, 5);
    term.draw(|f| {
        let area = f.area();
        render_footer(
            f,
            Rect::new(0, area.height - 1, area.width, 1),
            "Enter=boot",
        );
    })
    .expect("draw");
    let last = buffer_lines(&term).pop().expect("row");
    assert!(last.contains("Enter=boot"), "footer missing in: {last:?}");
}
