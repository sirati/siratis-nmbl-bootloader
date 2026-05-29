//! Tests for Emergency screen and BootStatus key handling.
use super::tests_list_edit_pass::press;
use super::*;
use crate::generations::Generation;
use crossterm::event::KeyCode;

fn emergency_app() -> App<'static> {
    let mut app = App::new(&[]);
    app.screen = Screen::Emergency {
        message: "boot failed: test".to_string(),
        items: vec![
            EmergencyItem {
                label: "Reboot",
                choice: EmergencyChoice::Reboot,
            },
            EmergencyItem {
                label: "Raw Shell",
                choice: EmergencyChoice::RawShell,
            },
        ],
        selected: 0,
        chosen: None,
    };
    app
}

fn emergency_state(app: &App<'_>) -> (usize, Option<EmergencyChoice>) {
    match &app.screen {
        Screen::Emergency {
            selected, chosen, ..
        } => (*selected, *chosen),
        _ => panic!("expected Emergency screen"),
    }
}

#[test]
fn emergency_arrow_keys_move_selection_within_bounds() {
    let mut app = emergency_app();
    assert_eq!(emergency_state(&app).0, 0);

    // Up at index 0 stays at 0.
    assert!(!app.on_key(press(KeyCode::Up)));
    assert_eq!(emergency_state(&app).0, 0);

    // Down advances.
    assert!(!app.on_key(press(KeyCode::Down)));
    assert_eq!(emergency_state(&app).0, 1);

    // Down at end stays at end.
    assert!(!app.on_key(press(KeyCode::Down)));
    assert_eq!(emergency_state(&app).0, 1);

    // Up walks back.
    assert!(!app.on_key(press(KeyCode::Up)));
    assert_eq!(emergency_state(&app).0, 0);

    // vi-keys also work.
    assert!(!app.on_key(press(KeyCode::Char('j'))));
    assert_eq!(emergency_state(&app).0, 1);
    assert!(!app.on_key(press(KeyCode::Char('k'))));
    assert_eq!(emergency_state(&app).0, 0);
}

#[test]
fn emergency_enter_returns_selected_variant() {
    // selected=0 -> Reboot.
    let mut app = emergency_app();
    assert!(app.on_key(press(KeyCode::Enter)));
    assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Reboot));

    // selected=1 -> RawShell.
    let mut app = emergency_app();
    assert!(!app.on_key(press(KeyCode::Down)));
    assert!(app.on_key(press(KeyCode::Enter)));
    assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::RawShell));
}

#[test]
fn set_emergency_message_replaces_displayed_error() {
    // Regression: the emergency screen used to latch the first
    // error it was built with. set_emergency_message must overwrite
    // the displayed text so the operator always sees the LATEST
    // failure (e.g. a failed Raw Shell), not the original boot one.
    let mut app = emergency_app();
    match &app.screen {
        Screen::Emergency { message, .. } => {
            assert_eq!(message, "boot failed: test");
        }
        _ => panic!("expected Emergency screen"),
    }

    app.set_emergency_message("Latest error (#1): Raw Shell failed\n\nEACCES");
    match &app.screen {
        Screen::Emergency { message, .. } => {
            assert!(
                message.contains("Latest error (#1)"),
                "message not updated: {message}"
            );
            assert!(
                message.contains("Raw Shell failed"),
                "missing title: {message}"
            );
            assert!(
                !message.contains("boot failed: test"),
                "stale first error retained"
            );
        }
        _ => panic!("expected Emergency screen"),
    }

    // A second update wins again (most-recent-wins, no latch).
    app.set_emergency_message("Latest error (#2): Retry failed\n\nENOENT");
    match &app.screen {
        Screen::Emergency { message, .. } => {
            assert!(
                message.contains("Latest error (#2)"),
                "second update lost: {message}"
            );
            assert!(!message.contains("(#1)"), "stale #1 retained: {message}");
        }
        _ => panic!("expected Emergency screen"),
    }

    // Selection / items are untouched by a message-only update.
    assert_eq!(emergency_state(&app).0, 0);
}

#[test]
fn emergency_esc_preserves_selection_without_committing() {
    let mut app = emergency_app();
    // Move to Shell.
    assert!(!app.on_key(press(KeyCode::Down)));
    assert_eq!(emergency_state(&app).0, 1);

    // Esc must not commit and must not move.
    assert!(!app.on_key(press(KeyCode::Esc)));
    let (sel, chosen) = emergency_state(&app);
    assert_eq!(sel, 1, "selection must be preserved across Esc");
    assert!(chosen.is_none(), "Esc must not commit a choice");
}

#[test]
fn emergency_hotkeys_r_and_s_commit_directly() {
    let mut app = emergency_app();
    assert!(app.on_key(press(KeyCode::Char('r'))));
    assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Reboot));

    let mut app = emergency_app();
    assert!(app.on_key(press(KeyCode::Char('s'))));
    assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::RawShell));
}

#[cfg(feature = "pretty-shell")]
#[test]
fn emergency_hotkey_p_commits_pretty_shell() {
    let mut app = emergency_app();
    assert!(app.on_key(press(KeyCode::Char('p'))));
    assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::PrettyShell));
}

#[test]
fn emergency_hotkeys_t_and_v_commit_retry_and_verify() {
    let mut app = emergency_app();
    assert!(app.on_key(press(KeyCode::Char('t'))));
    assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::RetryBoot));

    let mut app = emergency_app();
    assert!(app.on_key(press(KeyCode::Char('v'))));
    assert_eq!(
        emergency_state(&app).1,
        Some(EmergencyChoice::VerifyKexecReadiness)
    );
}

#[test]
fn boot_status_constructor_parks_app_on_boot_screen() {
    let app = App::boot_status("phase 0: kernel handoff");
    assert!(app.decision.is_none());
    match &app.screen {
        Screen::BootStatus(data) => {
            assert_eq!(&*data.phase, "phase 0: kernel handoff");
            assert!(data.log_lines.is_empty());
            assert_eq!(data.spinner_frame, 0);
        }
        _ => panic!("expected BootStatus screen"),
    }
}

#[test]
fn boot_status_setters_mutate_in_place() {
    let mut app = App::boot_status("initial");
    app.set_boot_phase("phase 2");
    app.set_boot_log_lines(vec!["one".into(), "two".into()]);
    match &app.screen {
        Screen::BootStatus(data) => {
            assert_eq!(&*data.phase, "phase 2");
            assert_eq!(data.log_lines, vec!["one", "two"]);
        }
        _ => panic!("expected BootStatus screen"),
    }
}

#[test]
fn boot_status_spinner_tick_wraps_modulo_frame_count() {
    let mut app = App::boot_status("waiting");
    for _ in 0..SPINNER_FRAMES {
        app.tick_boot_spinner();
    }
    // SPINNER_FRAMES ticks must wrap back to 0.
    match &app.screen {
        Screen::BootStatus(data) => assert_eq!(data.spinner_frame, 0),
        _ => panic!("expected BootStatus screen"),
    }
    // One more tick lands on frame 1.
    app.tick_boot_spinner();
    match &app.screen {
        Screen::BootStatus(data) => assert_eq!(data.spinner_frame, 1),
        _ => panic!("expected BootStatus screen"),
    }
}

#[test]
fn boot_status_on_key_does_not_produce_decision() {
    let mut app = App::boot_status("phase X");
    // Any keypress is absorbed; no decision is emitted.
    for code in [
        KeyCode::Enter,
        KeyCode::Esc,
        KeyCode::Char('s'),
        KeyCode::Char('q'),
    ] {
        assert!(!app.on_key(press(code)), "{code:?} must not exit");
        assert!(app.decision.is_none(), "{code:?} must not set decision");
    }
}

// The boot-status setters use `debug_assert!(false, ...)` on the
// wrong-screen branch, so behaviour differs between profiles:
//   - debug builds: each setter panics with the assertion text.
//   - release builds: each setter is a silent no-op.
// We pin both profiles so a future edit that breaks either path
// (e.g. flipping `debug_assert!` to `assert!`, or swapping the
// branch to a state mutation) is caught by `cargo test`.

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "set_boot_phase called on non-BootStatus screen")]
fn boot_status_set_phase_panics_on_wrong_screen_in_debug() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens); // Screen::List
    app.set_boot_phase("ignored");
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "set_boot_log_lines called on non-BootStatus screen")]
fn boot_status_set_log_lines_panics_on_wrong_screen_in_debug() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens); // Screen::List
    app.set_boot_log_lines(vec!["ignored".into()]);
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "tick_boot_spinner called on non-BootStatus screen")]
fn boot_status_tick_spinner_panics_on_wrong_screen_in_debug() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens); // Screen::List
    app.tick_boot_spinner();
}

#[cfg(not(debug_assertions))]
#[test]
fn boot_status_setters_are_noop_on_wrong_screen_in_release() {
    // debug_assert is stripped, so each setter must leave the App
    // unchanged when invoked on a non-BootStatus screen.
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens); // Screen::List
    app.set_boot_phase("ignored");
    app.set_boot_log_lines(vec!["ignored".into()]);
    app.tick_boot_spinner();
    assert!(matches!(app.screen, Screen::List));
}
