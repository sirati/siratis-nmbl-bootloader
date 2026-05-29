use std::path::{Path, PathBuf};

use crossterm::event::KeyCode;

use crate::ui::tty_enum::is_char_device;

use super::super::types::{
    ButtonCursor, CandidateOrigin, CustomValidation, FocusZone, PickerCandidate, PickerState,
};
use super::press;

/// Build a picker state focused on the custom-input row, pre-filled
/// with `input` and the cursor parked at the end.
fn custom_state(input: &str) -> PickerState {
    let cursor = input.len();
    PickerState {
        candidates: vec![PickerCandidate {
            label: "a".into(),
            target: PathBuf::from("/dev/a"),
            origin: CandidateOrigin::KernelConsole,
        }],
        selected: vec![true],
        // candidates(1) → custom-input row is index 1.
        cursor: 1,
        button_cursor: ButtonCursor::Spawn,
        custom_input: input.to_string(),
        custom_cursor: cursor,
        custom_checked: true,
        outcome: None,
    }
}

/// The custom field supports real line editing: Home/End/Left/Right
/// move the caret, and a printable inserted mid-string lands at the
/// caret rather than appending.
#[test]
fn custom_input_mid_string_insert_with_caret_motion() {
    let mut state = custom_state("/dev/tt0");
    assert_eq!(state.focus(), FocusZone::CustomInput);
    // End → cursor at len; Left once → between 't' and '0'.
    state.on_key(press(KeyCode::End));
    assert_eq!(state.custom_cursor, "/dev/tt0".len());
    state.on_key(press(KeyCode::Left));
    assert_eq!(state.custom_cursor, "/dev/tt".len());
    // Insert 'y' → "/dev/tty0".
    state.on_key(press(KeyCode::Char('y')));
    assert_eq!(state.custom_input, "/dev/tty0");
    assert_eq!(state.custom_cursor, "/dev/tty".len());
    // Home jumps to column 0.
    state.on_key(press(KeyCode::Home));
    assert_eq!(state.custom_cursor, 0);
    // Right walks one char forward.
    state.on_key(press(KeyCode::Right));
    assert_eq!(state.custom_cursor, 1);
}

/// Backspace removes the char before the caret; Delete removes the
/// char at the caret. Both operate at the cursor, not the end.
#[test]
fn custom_input_backspace_and_delete_at_cursor() {
    let mut state = custom_state("/dev/ttyX");
    // Park the caret between 'tty' and 'X' (before 'X').
    state.on_key(press(KeyCode::End));
    state.on_key(press(KeyCode::Left)); // before 'X'
    // Backspace removes the 'y' before the caret.
    state.on_key(press(KeyCode::Backspace));
    assert_eq!(state.custom_input, "/dev/ttX");
    // Delete removes the 'X' at the caret.
    state.on_key(press(KeyCode::Delete));
    assert_eq!(state.custom_input, "/dev/tt");
}

/// Live validation reflects the edited buffer after mid-string
/// editing, not just appends. Editing "/dev/nul" → insert 'l' at the
/// end yields the valid chardev "/dev/null".
#[test]
fn custom_input_validation_tracks_edited_buffer() {
    if !is_char_device(Path::new("/dev/null")) {
        return;
    }
    let mut state = custom_state("/dev/nul");
    assert_eq!(state.custom_validation(), CustomValidation::Invalid);
    state.on_key(press(KeyCode::End));
    state.on_key(press(KeyCode::Char('l')));
    assert_eq!(state.custom_input, "/dev/null");
    assert_eq!(state.custom_validation(), CustomValidation::Valid);
    // Backspace mid-edit invalidates again.
    state.on_key(press(KeyCode::Backspace));
    assert_eq!(state.custom_input, "/dev/nul");
    assert_eq!(state.custom_validation(), CustomValidation::Invalid);
}

/// Arrows in the custom field EDIT (move the caret) and never leak
/// into navigation: Left/Right stay on the custom row, while Up/Down
/// still move focus.
#[test]
fn custom_input_arrows_edit_not_navigate() {
    let mut state = custom_state("/dev/x");
    state.on_key(press(KeyCode::Left));
    assert_eq!(
        state.focus(),
        FocusZone::CustomInput,
        "Left must edit the field, not move focus"
    );
    // Up moves focus out (to the list row).
    state.on_key(press(KeyCode::Up));
    assert_eq!(state.focus(), FocusZone::List);
}

/// A literal Space typed in the focused custom field is inserted as
/// text (NOT treated as a checkbox toggle).
#[test]
fn custom_input_space_is_literal() {
    let mut state = custom_state("/dev");
    state.on_key(press(KeyCode::Char(' ')));
    assert_eq!(state.custom_input, "/dev ");
    assert_eq!(state.custom_cursor, "/dev ".len());
}
