#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::vte::ansi::Processor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::keys::key_to_bytes;
use super::state::{EscapeOutcome, EscapeState, GridSize, run_escape};

fn press_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}

#[test]
fn key_to_bytes_plain_chars_round_trip_ascii() {
    let out = key_to_bytes(press_with(KeyCode::Char('a'), KeyModifiers::NONE));
    assert_eq!(out, b"a");
    let out = key_to_bytes(press_with(KeyCode::Char(' '), KeyModifiers::NONE));
    assert_eq!(out, b" ");
}

#[test]
fn key_to_bytes_control_letters_map_to_control_bytes() {
    assert_eq!(
        key_to_bytes(press_with(KeyCode::Char('a'), KeyModifiers::CONTROL)),
        vec![0x01]
    );
    assert_eq!(
        key_to_bytes(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        vec![0x03]
    );
    assert_eq!(
        key_to_bytes(press_with(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        vec![0x04]
    );
}

#[test]
fn key_to_bytes_special_keys_emit_csi() {
    assert_eq!(
        key_to_bytes(press_with(KeyCode::Up, KeyModifiers::NONE)),
        b"\x1b[A"
    );
    assert_eq!(
        key_to_bytes(press_with(KeyCode::Down, KeyModifiers::NONE)),
        b"\x1b[B"
    );
    assert_eq!(
        key_to_bytes(press_with(KeyCode::Enter, KeyModifiers::NONE)),
        b"\r"
    );
    assert_eq!(
        key_to_bytes(press_with(KeyCode::Backspace, KeyModifiers::NONE)),
        b"\x7f"
    );
}

#[test]
fn key_to_bytes_multibyte_utf8_round_trips() {
    // German u-umlaut: U+00FC, UTF-8 0xC3 0xBC.
    let out = key_to_bytes(press_with(KeyCode::Char('ü'), KeyModifiers::NONE));
    assert_eq!(out, vec![0xC3, 0xBC]);
}

// --- SSH-style `<newline>~.` quit escape ------------------------

/// Helper: feed a byte stream through the escape recogniser starting
/// at the start of a line, returning either the forwarded bytes or a
/// `None` sentinel meaning "quit fired".
fn feed(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut st = EscapeState::LineStart;
    match run_escape(&mut st, bytes) {
        EscapeOutcome::Quit => None,
        EscapeOutcome::Forward(v) => Some(v),
    }
}

#[test]
fn escape_tilde_dot_at_line_start_quits() {
    // The canonical sequence: line-leading `~` then `.`.
    assert_eq!(feed(b"~."), None, "~. at line start must quit");
}

#[test]
fn escape_tilde_dot_after_newline_quits() {
    // Type `ls\r`, then `~.`: the `\r` returns us to line start so
    // the `~` arms again.
    assert_eq!(feed(b"ls\r~."), None);
}

#[test]
fn escape_midline_tilde_is_literal() {
    // A `~` that is NOT at the start of a line is an ordinary char,
    // so `a~.` forwards verbatim and never quits.
    assert_eq!(feed(b"a~."), Some(b"a~.".to_vec()));
}

#[test]
fn escape_tilde_then_other_forwards_both() {
    // `~` armed, then `x` (not `.`/`~`): the deferred `~` and the `x`
    // are both forwarded.
    assert_eq!(feed(b"~x"), Some(b"~x".to_vec()));
}

#[test]
fn escape_double_tilde_is_single_literal() {
    // `~~` at line start collapses to one literal `~` (OpenSSH
    // convention) and does not quit on a trailing `.`.
    assert_eq!(feed(b"~~."), Some(b"~.".to_vec()));
}

#[test]
fn escape_lone_tilde_is_held_back() {
    // A line-leading `~` with nothing after it yet is deferred (not
    // forwarded) — exactly like SSH waiting for the escape command.
    assert_eq!(feed(b"~"), Some(Vec::new()));
}

// --- scrollback rendering / resize ------------------------------

/// Build a bare [`Term`] (no PTY) so the grid-snapshot and resize
/// helpers can be exercised without forking a shell.
fn term_with_lines(cols: u16, rows: u16, lines: &[&str]) -> Term<VoidListener> {
    let size = GridSize {
        columns: cols as usize,
        screen_lines: rows as usize,
    };
    let mut term = Term::new(TermConfig::default(), &size, VoidListener);
    let mut parser: Processor = Processor::new();
    for line in lines {
        parser.advance(&mut term, line.as_bytes());
        parser.advance(&mut term, b"\r\n");
    }
    term
}

/// Collect the displayed rows directly off a `Term`, mirroring the
/// production `collect_visible_rows` shift-by-`display_offset` logic.
fn visible(term: &Term<VoidListener>) -> Vec<String> {
    let grid = term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let offset = grid.display_offset() as i32;
    (0..rows)
        .map(|row| {
            let line_idx = row as i32 - offset;
            (0..cols)
                .map(|col| {
                    let p = alacritty_terminal::index::Point::new(
                        alacritty_terminal::index::Line(line_idx),
                        alacritty_terminal::index::Column(col),
                    );
                    let c = grid[p].c;
                    if c == '\0' { ' ' } else { c }
                })
                .collect::<String>()
        })
        .collect()
}

#[test]
fn collect_visible_rows_reflects_display_offset() {
    // Push more lines than fit so there is scrollback history.
    let lines: Vec<String> = (0..30).map(|i| format!("line{i}")).collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let term = term_with_lines(20, 6, &refs);

    // Live tail: the most recent lines are visible, "line0" is gone.
    let tail = visible(&term).join("\n");
    assert!(!tail.contains("line0"), "live tail should not show line0");

    // Scroll up several rows; older content must come into view that
    // was NOT visible at the live tail.
    let mut term = term;
    term.grid_mut()
        .scroll_display(alacritty_terminal::grid::Scroll::Delta(10));
    assert!(term.grid().display_offset() > 0, "offset must be non-zero");
    let scrolled = visible(&term).join("\n");
    assert_ne!(
        scrolled, tail,
        "scrolled view must differ from the live tail"
    );
}

#[test]
fn resize_updates_grid_dimensions() {
    let term = term_with_lines(80, 24, &["hello"]);
    assert_eq!(term.grid().columns(), 80);
    assert_eq!(term.grid().screen_lines(), 24);

    let mut term = term;
    term.resize(GridSize {
        columns: 100,
        screen_lines: 30,
    });
    assert_eq!(term.grid().columns(), 100, "cols must track resize");
    assert_eq!(term.grid().screen_lines(), 30, "rows must track resize");
}
