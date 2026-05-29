use crossterm::event::KeyCode;

use crate::ui::ConfirmOutcome;
use crate::ui::modal_confirm::show_modal_confirm;

use super::{ScriptedConsole, block, press};

#[test]
fn show_modal_confirm_returns_yes_on_enter_with_default_true() {
    // yes_default = true highlights Yes; Enter immediately commits
    // to Yes without needing arrow keys.
    let keys = vec![press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm(
        &mut console,
        "Boot one?",
        "Found 3 generations.",
        "Yes",
        "Back",
        true,
    ))
    .expect("modal must succeed on Enter");
    assert_eq!(out, ConfirmOutcome::Yes);
}

#[test]
fn show_modal_confirm_returns_no_on_enter_with_default_false() {
    // yes_default = false highlights Back; Enter commits to No.
    let keys = vec![press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm(
        &mut console,
        "Are you sure?",
        "This may destroy data.",
        "Yes",
        "No",
        false,
    ))
    .expect("modal must succeed");
    assert_eq!(out, ConfirmOutcome::No);
}

#[test]
fn show_modal_confirm_arrow_keys_toggle_selection_then_enter_commits() {
    // Default Yes, then Right toggles to No, Enter commits to No.
    let keys = vec![press(KeyCode::Right), press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm(
        &mut console,
        "t",
        "b",
        "Yes",
        "No",
        true,
    ))
    .expect("modal must succeed");
    assert_eq!(out, ConfirmOutcome::No);

    // Default No, then Left toggles to Yes, Enter commits to Yes.
    let keys = vec![press(KeyCode::Left), press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm(
        &mut console,
        "t",
        "b",
        "Yes",
        "No",
        false,
    ))
    .expect("modal must succeed");
    assert_eq!(out, ConfirmOutcome::Yes);
}

#[test]
fn show_modal_confirm_hotkey_y_returns_yes() {
    // 'y' hotkey commits to Yes regardless of which button is
    // highlighted — matches the muscle-memory pattern of every
    // other confirmation prompt in the binary.
    let keys = vec![press(KeyCode::Char('y'))];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm(
        &mut console,
        "t",
        "b",
        "Yes",
        "No",
        false,
    ))
    .expect("modal must succeed on 'y'");
    assert_eq!(out, ConfirmOutcome::Yes);
}

#[test]
fn show_modal_confirm_hotkey_n_returns_no() {
    let keys = vec![press(KeyCode::Char('n'))];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm(
        &mut console,
        "t",
        "b",
        "Yes",
        "No",
        true,
    ))
    .expect("modal must succeed on 'n'");
    assert_eq!(out, ConfirmOutcome::No);
}

#[test]
fn show_modal_confirm_esc_returns_cancelled() {
    let keys = vec![press(KeyCode::Esc)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm(
        &mut console,
        "t",
        "b",
        "Yes",
        "Back",
        true,
    ))
    .expect("modal must succeed on Esc");
    assert_eq!(out, ConfirmOutcome::Cancelled);
}

#[test]
fn show_modal_confirm_renders_at_least_once_before_polling() {
    // Defence-in-depth: the operator must see the modal BEFORE we
    // start blocking on input. If a future refactor reorders the
    // draw and poll, the picker would block on a stale screen.
    let keys = vec![press(KeyCode::Char('y'))];
    let mut console = ScriptedConsole::new(keys);
    let _ = block(show_modal_confirm(
        &mut console,
        "t",
        "b",
        "Yes",
        "No",
        true,
    ))
    .expect("modal must succeed");
    assert!(
        console.renders >= 1,
        "expected at least one render, got {}",
        console.renders
    );
}
