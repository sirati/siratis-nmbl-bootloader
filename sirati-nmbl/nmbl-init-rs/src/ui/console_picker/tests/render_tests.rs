use std::path::{Path, PathBuf};

use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier};

use crate::ui::tty_enum::{TtyKind, is_char_device};

use super::super::render::render_picker_frame;
use super::super::types::{ButtonCursor, CandidateOrigin, PickerCandidate, PickerState};

#[test]
fn renderer_paints_candidate_labels_and_buttons() {
    let state = PickerState {
        candidates: vec![
            PickerCandidate {
                label: "/dev/console (-> /dev/tty0)".into(),
                target: PathBuf::from("/dev/tty0"),
                origin: CandidateOrigin::KernelConsole,
            },
            PickerCandidate {
                label: "/dev/ttyS0".into(),
                target: PathBuf::from("/dev/ttyS0"),
                origin: CandidateOrigin::Enumerated(TtyKind::SerialPort),
            },
        ],
        selected: vec![true, false],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_checked: true,
        outcome: None,
    };
    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    term.draw(|f| render_picker_frame(f, &state)).expect("draw");
    let buf = term.backend().buffer();
    let dump: String = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(dump.contains("Spawn shell on:"), "header missing: \n{dump}");
    assert!(
        dump.contains("/dev/console"),
        "active console label missing: \n{dump}"
    );
    assert!(
        dump.contains("(kernel console)"),
        "origin suffix missing: \n{dump}"
    );
    assert!(
        dump.contains("/dev/ttyS0"),
        "extra-console label missing: \n{dump}"
    );
    assert!(dump.contains("[Spawn"), "Spawn button missing: \n{dump}");
    assert!(dump.contains("[Cancel]"), "Cancel button missing: \n{dump}");
    assert!(
        dump.contains("custom"),
        "custom input title missing: \n{dump}"
    );
}

/// When no candidate is selected the [Spawn] button must render
/// with the DIM modifier so the disabled state is operator-visible.
/// Mirrors the `render_passphrase` precedent from empty-pw-block.
#[test]
fn renderer_dims_spawn_when_no_target_selected() {
    let state = PickerState {
        candidates: vec![PickerCandidate {
            label: "/dev/tty0".into(),
            target: PathBuf::from("/dev/tty0"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![false],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_checked: true,
        outcome: None,
    };
    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    term.draw(|f| render_picker_frame(f, &state)).expect("draw");
    let buf = term.backend().buffer();
    // Find the centre of the Spawn label and inspect its style.
    let mut dim_seen = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y))
                && cell.symbol() == "S"
                && let Some(next) = buf.cell((x.saturating_add(1), y))
                && next.symbol() == "p"
                && cell.style().add_modifier.contains(Modifier::DIM)
            {
                dim_seen = true;
            }
        }
    }
    assert!(dim_seen, "Spawn label must be DIM when no target selected");
}

/// Filled-buffer counterpart: when at least one target is checked
/// the [Spawn] button must NOT be DIM.
#[test]
fn renderer_does_not_dim_spawn_when_target_selected() {
    let state = PickerState {
        candidates: vec![PickerCandidate {
            label: "/dev/tty0".into(),
            target: PathBuf::from("/dev/tty0"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![true],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: String::new(),
        custom_checked: true,
        outcome: None,
    };
    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    term.draw(|f| render_picker_frame(f, &state)).expect("draw");
    let buf = term.backend().buffer();
    let mut any_dim = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y))
                && cell.symbol() == "S"
                && let Some(next) = buf.cell((x.saturating_add(1), y))
                && next.symbol() == "p"
                && cell.style().add_modifier.contains(Modifier::DIM)
            {
                any_dim = true;
            }
        }
    }
    assert!(
        !any_dim,
        "Spawn label must NOT be DIM when a target is selected"
    );
}

/// Renderer must colour the custom-input field GREEN when the
/// path is a valid, non-duplicate chardev, and RED when the path
/// is rejected. The marker glyph also flips ([x] vs [!]).
#[test]
fn renderer_colours_custom_input_by_validation() {
    // Valid case — only runs if /dev/null exists as a chardev (it
    // does on every reasonable target).
    if !is_char_device(Path::new("/dev/null")) {
        return;
    }
    let mut state = PickerState {
        candidates: vec![PickerCandidate {
            label: "/dev/tty0".into(),
            target: PathBuf::from("/dev/tty0"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![true],
        cursor: 0,
        button_cursor: ButtonCursor::Spawn,
        custom_input: "/dev/null".to_string(),
        custom_checked: true,
        outcome: None,
    };
    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    term.draw(|f| render_picker_frame(f, &state)).expect("draw");
    let buf = term.backend().buffer();
    // Locate one of the green cells (the '/' of /dev/null in the
    // custom-input box).
    let mut green_seen = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y))
                && cell.symbol() == "/"
                && cell.style().fg == Some(Color::Green)
            {
                green_seen = true;
            }
        }
    }
    assert!(green_seen, "valid custom path must render green");

    // Invalid case.
    state.custom_input = "/dev/this-does-not-exist-nmbl".to_string();
    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    term.draw(|f| render_picker_frame(f, &state)).expect("draw");
    let buf = term.backend().buffer();
    let mut red_seen = false;
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y))
                && cell.symbol() == "/"
                && cell.style().fg == Some(Color::Red)
            {
                red_seen = true;
            }
        }
    }
    assert!(red_seen, "invalid custom path must render red");
}
