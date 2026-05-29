//! Unit tests for the rescue UI screens and helpers.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use crate::rescue::net::{DownloadStatus, HashConfirmation};

use super::confirm_hash::{HashConfirmState, handle_hash_key, render_confirm_hash};
use super::helpers::{char_column_for_byte_cursor, clamp_to_char_boundary, group_hex};
use super::pick_source::render_pick_source;
use super::progress::render_progress;
use super::prompt_url::render_prompt_url;

fn buffer_lines(term: &Terminal<TestBackend>) -> Vec<String> {
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect::<String>()
        })
        .collect()
}

fn buffer_text(term: &Terminal<TestBackend>) -> String {
    buffer_lines(term).join("\n")
}

fn new_term(w: u16, h: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(w, h)).expect("terminal")
}

#[test]
fn render_pick_source_shows_header_and_options() {
    let mut term = new_term(80, 24);
    term.draw(|f| render_pick_source(f, "loop_dev: ENOSPC", 0))
        .expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("Disk rescue unavailable"),
        "missing header in:\n{text}"
    );
    assert!(text.contains("loop_dev: ENOSPC"), "missing disk reason");
    assert!(text.contains("[N]"), "missing N hotkey");
    assert!(text.contains("[R]"), "missing R hotkey");
    assert!(text.contains("[H]"), "missing H hotkey");
    assert!(text.contains("Network rescue"), "missing network label");
}

#[test]
fn render_prompt_url_shows_prefill_and_caret() {
    let mut term = new_term(80, 12);
    let url = "https://example.invalid/rescue.sfs";
    term.draw(|f| render_prompt_url(f, url, url.len()))
        .expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("Rescue URL"), "missing header");
    assert!(text.contains(url), "missing prefill in:\n{text}");
    assert!(text.contains('^'), "missing caret indicator");
    assert!(text.contains("Enter=confirm"), "missing footer hint");
}

#[test]
fn render_progress_gauge_shows_percentage_when_total_known() {
    let mut term = new_term(80, 12);
    term.draw(|f| {
        render_progress(
            f,
            DownloadStatus {
                bytes: 50,
                total: Some(200),
            },
            0,
        );
    })
    .expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("Downloading rescue blob"), "missing banner");
    assert!(
        text.contains("50 / 200 bytes"),
        "missing byte counter in:\n{text}"
    );
    assert!(text.contains("25%"), "missing percentage label in:\n{text}");
}

#[test]
fn render_progress_spinner_shows_byte_count_when_total_unknown() {
    let mut term = new_term(80, 12);
    term.draw(|f| {
        render_progress(
            f,
            DownloadStatus {
                bytes: 1234,
                total: None,
            },
            2,
        );
    })
    .expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("1234 bytes"),
        "missing byte count in:\n{text}"
    );
    assert!(
        text.contains("Content-Length unknown"),
        "missing fallback label"
    );
}

#[test]
fn render_confirm_hash_shows_both_panes_and_match_banner() {
    let mut term = new_term(120, 24);
    let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    term.draw(|f| render_confirm_hash(f, h, h, h.len()))
        .expect("draw");
    let text = buffer_text(&term);
    assert!(text.contains("Computed (SHA-256)"), "missing computed pane");
    assert!(
        text.contains("Expected (editable)"),
        "missing expected pane"
    );
    assert!(
        text.contains("Hash matches expected"),
        "missing match banner in:\n{text}"
    );
    // First 4-char chunk of the canonical empty digest.
    assert!(
        text.contains("e3b0"),
        "missing grouped hex chunk in:\n{text}"
    );
}

#[test]
fn render_confirm_hash_shows_mismatch_banner_when_panes_disagree() {
    let mut term = new_term(120, 24);
    let computed = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let expected = "deadbeef";
    term.draw(|f| render_confirm_hash(f, computed, expected, expected.len()))
        .expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("MISMATCH"),
        "missing MISMATCH banner in:\n{text}"
    );
}

#[test]
fn render_confirm_hash_shows_no_prefill_banner_when_expected_empty() {
    let mut term = new_term(120, 24);
    let computed = "abcd1234";
    term.draw(|f| render_confirm_hash(f, computed, "", 0))
        .expect("draw");
    let text = buffer_text(&term);
    assert!(
        text.contains("No expected hash pre-filled"),
        "missing no-prefill banner in:\n{text}",
    );
}

/// Pressing Y when the panes disagree must return `Mismatch`, not
/// `Confirmed`. Pinning this so a future refactor can't regress
/// the orchestrator into pivoting onto a tampered blob.
#[test]
fn handle_hash_key_y_on_mismatch_returns_mismatch() {
    let computed = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let expected = "deadbeef"; // Intentionally disagrees with `computed`.
    let mut state = HashConfirmState::new(expected, expected.len());
    let key = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE);
    let outcome = handle_hash_key(key, &mut state, computed);
    assert_eq!(outcome, Some(HashConfirmation::Mismatch));
    // The expected buffer must be untouched so the operator can
    // edit-and-retry rather than re-typing from scratch.
    assert_eq!(state.expected, expected);
}

#[test]
fn handle_hash_key_y_on_match_returns_confirmed() {
    let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let mut state = HashConfirmState::new(h, h.len());
    let key = KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::NONE);
    let outcome = handle_hash_key(key, &mut state, h);
    assert_eq!(outcome, Some(HashConfirmation::Confirmed));
}

#[test]
fn handle_hash_key_release_event_is_ignored() {
    let h = "abcd";
    let mut state = HashConfirmState::new(h, h.len());
    let release = KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    );
    assert!(handle_hash_key(release, &mut state, h).is_none());
}

#[test]
fn group_hex_breaks_into_four_chunks_of_four() {
    let h = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let rows = group_hex(h, 4);
    // 64 chars / (4 chars * 4 chunks) = 4 rows.
    assert_eq!(rows.len(), 4, "expected 4 rows, got {rows:?}");
    for row in &rows {
        // 4 chunks * 4 chars = 16 chars + 3 spaces = 19 chars.
        assert_eq!(row.len(), 19, "wrong row width: {row:?}");
    }
}

#[test]
fn group_hex_handles_empty_input() {
    assert_eq!(group_hex("", 4), vec![String::new()]);
}

#[test]
fn char_column_clamps_inside_multibyte_string() {
    // "héllo" — 'é' is 2 bytes; byte index 4 = inside/after 'l'.
    // Char column should be 3 (h, é, l = three chars before).
    assert_eq!(char_column_for_byte_cursor("héllo", 4), 3);
    // Past the end clamps to the last column.
    assert_eq!(char_column_for_byte_cursor("héllo", 99), 5);
}

#[test]
fn clamp_to_char_boundary_walks_back_inside_multibyte() {
    // "héllo": valid boundaries are 0,1,3,4,5,6. byte 2 is mid-char.
    assert_eq!(clamp_to_char_boundary("héllo", 2), 1);
    assert_eq!(clamp_to_char_boundary("héllo", 5), 5);
    assert_eq!(clamp_to_char_boundary("héllo", 99), 6);
}

#[test]
fn make_rescue_ui_returns_default_state() {
    let mut console = crate::ui::console::NoopConsole::new();
    let ui = super::make_rescue_ui(&mut console);
    assert_eq!(ui.url_cursor, 0);
    assert_eq!(ui.expected_cursor, 0);
    assert_eq!(ui.spinner_phase, 0);
    assert!(ui.last_redraw.is_none());
}
