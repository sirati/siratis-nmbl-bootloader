//! Tests for List, Editing, and Passphrase screen key handling.
use super::*;
use crate::generations::Generation;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;
use zeroize::Zeroizing;

pub(super) fn fake_gen(number: u32, params: &[&str]) -> Generation {
    Generation {
        number,
        profile_link: PathBuf::from(format!("/p/system-{number}-link")),
        kernel: PathBuf::from("/p/kernel"),
        initrd: PathBuf::from("/p/initrd"),
        init_path: PathBuf::from(format!("/p/system-{number}-link/init")),
        kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
        label: String::new(),
    }
}

pub(super) fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn list_arrow_keys_move_selection_within_bounds() {
    let gens = vec![fake_gen(3, &[]), fake_gen(2, &[]), fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert_eq!(app.selected_index, 0);

    // Up at index 0 stays at 0.
    assert!(!app.on_key(press(KeyCode::Up)));
    assert_eq!(app.selected_index, 0);

    // Down moves through the list.
    assert!(!app.on_key(press(KeyCode::Down)));
    assert_eq!(app.selected_index, 1);
    assert!(!app.on_key(press(KeyCode::Down)));
    assert_eq!(app.selected_index, 2);

    // Down at end stays at end.
    assert!(!app.on_key(press(KeyCode::Down)));
    assert_eq!(app.selected_index, 2);

    // vi-keys also work.
    assert!(!app.on_key(press(KeyCode::Char('k'))));
    assert_eq!(app.selected_index, 1);
    assert!(!app.on_key(press(KeyCode::Char('j'))));
    assert_eq!(app.selected_index, 2);
}

#[test]
fn list_e_transitions_to_editing_with_joined_params() {
    let gens = vec![fake_gen(42, &["init=/sbin/init", "quiet", "loglevel=4"])];
    let mut app = App::new(&gens);

    assert!(!app.on_key(press(KeyCode::Char('e'))));
    match &app.screen {
        Screen::Editing {
            generation_index,
            line,
        } => {
            assert_eq!(*generation_index, 0);
            assert_eq!(line.text(), "init=/sbin/init quiet loglevel=4");
            assert_eq!(line.cursor(), line.text().len(), "cursor must land at end");
        }
        _ => panic!("expected Editing screen"),
    }
}

#[test]
fn list_s_sets_shell_decision_and_returns_true() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert!(app.on_key(press(KeyCode::Char('s'))));
    assert!(matches!(app.decision, Some(Decision::Shell)));
}

#[test]
fn list_q_sets_reboot_decision_and_returns_true() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert!(app.on_key(press(KeyCode::Char('q'))));
    assert!(matches!(app.decision, Some(Decision::Reboot)));
}

#[test]
fn list_enter_sets_boot_decision_with_no_override() {
    let gens = vec![fake_gen(7, &[]), fake_gen(6, &[])];
    let mut app = App::new(&gens);
    app.selected_index = 1;
    assert!(app.on_key(press(KeyCode::Enter)));
    match &app.decision {
        Some(Decision::Boot {
            generation_index,
            cmdline_override,
        }) => {
            assert_eq!(*generation_index, 1);
            assert!(cmdline_override.is_none());
        }
        other => panic!("expected Boot decision, got {other:?}"),
    }
}

#[test]
fn list_enter_with_empty_generations_does_not_decide() {
    // Defence-in-depth: if the selector ever ran with zero
    // generations, Enter would otherwise emit Boot{0,..} and
    // main.rs would index out of bounds. Make Enter a no-op.
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    assert!(!app.on_key(press(KeyCode::Enter)));
    assert!(app.decision.is_none(), "decision must stay None");
}

#[test]
fn list_p_toggles_show_kernel_params() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert!(!app.show_kernel_params);
    app.on_key(press(KeyCode::Char('p')));
    assert!(app.show_kernel_params);
    app.on_key(press(KeyCode::Char('p')));
    assert!(!app.show_kernel_params);
}

#[test]
fn any_keypress_in_list_cancels_countdown() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    app.countdown_remaining_secs = Some(4);
    // 'p' is a no-op-ish toggle, but should still clear the countdown.
    app.on_key(press(KeyCode::Char('p')));
    assert!(app.countdown_remaining_secs.is_none());
}

#[test]
fn any_keypress_sets_user_interacted_latch() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert!(!app.interaction.get());
    app.on_key(press(KeyCode::Char('p')));
    assert!(app.interaction.get());
}

#[test]
fn editing_typing_appends_and_backspace_removes() {
    let gens = vec![fake_gen(1, &["foo"])];
    let mut app = App::new(&gens);
    app.on_key(press(KeyCode::Char('e')));

    // Append " bar".
    for c in " bar".chars() {
        app.on_key(press(KeyCode::Char(c)));
    }
    match &app.screen {
        Screen::Editing { line, .. } => {
            assert_eq!(line.text(), "foo bar");
            assert_eq!(line.cursor(), line.text().len());
        }
        _ => panic!("expected Editing"),
    }

    // Backspace once removes 'r'.
    app.on_key(press(KeyCode::Backspace));
    match &app.screen {
        Screen::Editing { line, .. } => assert_eq!(line.text(), "foo ba"),
        _ => panic!("expected Editing"),
    }
}

#[test]
fn editing_enter_sets_boot_with_cmdline_override() {
    let gens = vec![fake_gen(5, &["root=/dev/sda1"])];
    let mut app = App::new(&gens);
    app.on_key(press(KeyCode::Char('e')));
    for c in " quiet".chars() {
        app.on_key(press(KeyCode::Char(c)));
    }
    assert!(app.on_key(press(KeyCode::Enter)));
    match &app.decision {
        Some(Decision::Boot {
            generation_index,
            cmdline_override,
        }) => {
            assert_eq!(*generation_index, 0);
            assert_eq!(cmdline_override.as_deref(), Some("root=/dev/sda1 quiet"));
        }
        other => panic!("expected Boot{{..}}, got {other:?}"),
    }
}

#[test]
fn editing_esc_returns_to_list_without_decision() {
    let gens = vec![fake_gen(5, &["foo"])];
    let mut app = App::new(&gens);
    app.on_key(press(KeyCode::Char('e')));
    assert!(matches!(app.screen, Screen::Editing { .. }));
    assert!(!app.on_key(press(KeyCode::Esc)));
    assert!(matches!(app.screen, Screen::List));
    assert!(app.decision.is_none());
}

#[test]
fn editing_home_end_left_right_navigation() {
    let gens = vec![fake_gen(1, &["abcd"])];
    let mut app = App::new(&gens);
    app.on_key(press(KeyCode::Char('e')));

    // Cursor starts at end. Home jumps to 0.
    app.on_key(press(KeyCode::Home));
    match &app.screen {
        Screen::Editing { line, .. } => assert_eq!(line.cursor(), 0),
        _ => panic!(),
    }
    // Right advances one byte.
    app.on_key(press(KeyCode::Right));
    match &app.screen {
        Screen::Editing { line, .. } => assert_eq!(line.cursor(), 1),
        _ => panic!(),
    }
    // End jumps to the end.
    app.on_key(press(KeyCode::End));
    match &app.screen {
        Screen::Editing { line, .. } => assert_eq!(line.cursor(), line.text().len()),
        _ => panic!(),
    }
    // Left walks back one byte.
    app.on_key(press(KeyCode::Left));
    match &app.screen {
        Screen::Editing { line, .. } => {
            assert_eq!(line.cursor(), line.text().len().saturating_sub(1));
        }
        _ => panic!(),
    }
}

#[test]
fn editing_handles_multibyte_backspace_without_panic() {
    // Backspacing across a multi-byte char boundary must not panic
    // even though clippy's indexing_slicing lint applies to prod code.
    let gens = vec![fake_gen(1, &["héllo"])];
    let mut app = App::new(&gens);
    app.on_key(press(KeyCode::Char('e')));
    app.on_key(press(KeyCode::Backspace));
    match &app.screen {
        Screen::Editing { line, .. } => {
            assert_eq!(line.text(), "héll");
            assert_eq!(line.cursor(), line.text().len());
        }
        _ => panic!("expected Editing"),
    }
}

#[test]
fn passphrase_screen_collects_chars_and_pops() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.screen = Screen::Passphrase {
        prompt_label: "Unlock".to_string(),
        buffer: Zeroizing::new(String::new()),
        cursor: 0,
        verifying: false,
        spinner_frame: 0,
    };
    for c in "hi".chars() {
        assert!(!app.on_key(press(KeyCode::Char(c))));
    }
    match &app.screen {
        Screen::Passphrase { buffer, .. } => assert_eq!(&**buffer, "hi"),
        _ => panic!(),
    }
    app.on_key(press(KeyCode::Backspace));
    match &app.screen {
        Screen::Passphrase { buffer, .. } => assert_eq!(&**buffer, "h"),
        _ => panic!(),
    }
}

#[test]
fn passphrase_esc_drops_to_shell() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.screen = Screen::Passphrase {
        prompt_label: "Unlock".to_string(),
        buffer: Zeroizing::new(String::new()),
        cursor: 0,
        verifying: false,
        spinner_frame: 0,
    };
    assert!(app.on_key(press(KeyCode::Esc)));
    assert!(matches!(app.decision, Some(Decision::Shell)));
}

#[test]
fn passphrase_enter_signals_consumed_without_decision() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.screen = Screen::Passphrase {
        prompt_label: "Unlock".to_string(),
        buffer: Zeroizing::new("secret".to_string()),
        cursor: 0,
        verifying: false,
        spinner_frame: 0,
    };
    assert!(app.on_key(press(KeyCode::Enter)));
    assert!(app.decision.is_none(), "Enter must not set a Decision");
}

#[test]
fn passphrase_set_verifying_toggles_flag_and_resets_spinner_on_clear() {
    // The verifying flag drives the overlay; clearing it must also
    // reset the spinner frame so a re-verify starts from glyph 0.
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.screen = Screen::Passphrase {
        prompt_label: "Unlock".to_string(),
        buffer: Zeroizing::new(String::new()),
        cursor: 0,
        verifying: false,
        spinner_frame: 0,
    };
    app.set_passphrase_verifying(true);
    app.tick_passphrase_spinner();
    app.tick_passphrase_spinner();
    match &app.screen {
        Screen::Passphrase {
            verifying,
            spinner_frame,
            ..
        } => {
            assert!(*verifying, "verifying must be set");
            assert_eq!(*spinner_frame, 2, "two ticks land on frame 2");
        }
        _ => panic!("expected Passphrase"),
    }
    app.set_passphrase_verifying(false);
    match &app.screen {
        Screen::Passphrase {
            verifying,
            spinner_frame,
            ..
        } => {
            assert!(!*verifying, "verifying cleared");
            assert_eq!(*spinner_frame, 0, "spinner reset on clear");
        }
        _ => panic!("expected Passphrase"),
    }
}

#[test]
fn passphrase_tick_spinner_wraps_modulo_frame_count() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.screen = Screen::Passphrase {
        prompt_label: "Unlock".to_string(),
        buffer: Zeroizing::new(String::new()),
        cursor: 0,
        verifying: true,
        spinner_frame: 0,
    };
    for _ in 0..SPINNER_FRAMES {
        app.tick_passphrase_spinner();
    }
    match &app.screen {
        Screen::Passphrase { spinner_frame, .. } => {
            assert_eq!(*spinner_frame, 0, "SPINNER_FRAMES ticks wrap to 0");
        }
        _ => panic!("expected Passphrase"),
    }
}

#[test]
fn passphrase_clear_buffer_resets_state() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.screen = Screen::Passphrase {
        prompt_label: "Unlock".to_string(),
        buffer: Zeroizing::new("typed".to_string()),
        cursor: 0,
        verifying: true,
        spinner_frame: 3,
    };
    app.clear_passphrase_buffer();
    match &app.screen {
        Screen::Passphrase {
            buffer,
            verifying,
            spinner_frame,
            ..
        } => {
            assert!(buffer.is_empty());
            assert!(!*verifying);
            assert_eq!(*spinner_frame, 0);
        }
        _ => panic!("expected Passphrase"),
    }
}
