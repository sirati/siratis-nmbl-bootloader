//! Tests for countdown latch, modal overlay, and Ctrl-key handling.
use super::tests_list_edit_pass::{fake_gen, press};
use super::*;
use crate::generations::Generation;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

// ---- Error-screen countdown latch -----------------------------

#[test]
fn latch_error_countdown_sets_deadline_on_first_call() {
    // First invocation must transition deadline from None → Some.
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    assert!(app.error_countdown_deadline.is_none());
    app.latch_error_countdown(std::time::Duration::from_secs(30));
    assert!(app.error_countdown_deadline.is_some());
}

#[test]
fn latch_error_countdown_is_idempotent_across_reentries() {
    // Re-entry to the error screen (operator dismissed a modal,
    // navigated back) MUST NOT restart the timer. The deadline
    // captured on the first call must survive every subsequent
    // call regardless of duration.
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.latch_error_countdown(std::time::Duration::from_secs(30));
    let deadline_a = app.error_countdown_deadline;
    // Re-enter twice with different (smaller / larger) durations.
    app.latch_error_countdown(std::time::Duration::from_secs(5));
    let deadline_b = app.error_countdown_deadline;
    app.latch_error_countdown(std::time::Duration::from_secs(99));
    let deadline_c = app.error_countdown_deadline;
    assert_eq!(deadline_a, deadline_b);
    assert_eq!(deadline_a, deadline_c);
}

#[test]
fn latch_error_countdown_preserves_elapsed_deadline() {
    // If the deadline already elapsed during time spent on
    // another screen, the latch must keep the elapsed deadline
    // — the loop driver observes `now >= deadline` and reboots
    // immediately. We test this by pre-setting a deadline in
    // the past and confirming the latch leaves it alone.
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
    app.error_countdown_deadline = Some(past);
    app.latch_error_countdown(std::time::Duration::from_secs(30));
    assert_eq!(
        app.error_countdown_deadline,
        Some(past),
        "latch must not refresh an already-elapsed deadline"
    );
}

// ---- Modal overlay state --------------------------------------

#[test]
fn modal_field_defaults_to_none_on_construction() {
    // App::new() must start with no modal so a fresh boot doesn't
    // accidentally render a stale overlay.
    let gens: Vec<Generation> = vec![];
    let app = App::new(&gens);
    assert!(app.modal.is_none());
}

#[test]
fn modal_scroll_offset_defaults_to_zero_on_construction() {
    let gens: Vec<Generation> = vec![];
    let app = App::new(&gens);
    assert_eq!(app.modal_scroll_offset, 0);
}

#[test]
fn modal_scroll_down_clamps_at_total_minus_visible() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    // 10 total rows, viewport of 4 → max offset is 6.
    app.modal_scroll_down(1, 10, 4);
    assert_eq!(app.modal_scroll_offset, 1);
    app.modal_scroll_down(10, 10, 4);
    assert_eq!(app.modal_scroll_offset, 6, "clamped at total - visible");
    // Down past max stays at max.
    app.modal_scroll_down(99, 10, 4);
    assert_eq!(app.modal_scroll_offset, 6);
}

#[test]
fn modal_scroll_up_saturates_at_zero() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.modal_scroll_offset = 3;
    app.modal_scroll_up(2);
    assert_eq!(app.modal_scroll_offset, 1);
    // Past zero stays at zero.
    app.modal_scroll_up(99);
    assert_eq!(app.modal_scroll_offset, 0);
}

#[test]
fn modal_scroll_reset_clears_offset() {
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.modal_scroll_offset = 5;
    app.modal_scroll_reset();
    assert_eq!(app.modal_scroll_offset, 0);
}

#[test]
fn modal_field_carries_status_overlay_payload() {
    // ModalKind::Status round-trips its payload exactly. The
    // BootReporter writes into this variant when an emergency
    // action wants the menu visible behind a progress dialog.
    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.modal = Some(ModalKind::Status {
        phase: "phase X".into(),
        log_lines: vec!["one".into()],
        spinner_frame: 2,
    });
    match &app.modal {
        Some(ModalKind::Status {
            phase,
            log_lines,
            spinner_frame,
        }) => {
            assert_eq!(phase, "phase X");
            assert_eq!(log_lines, &vec!["one".to_string()]);
            assert_eq!(*spinner_frame, 2);
        }
        other => panic!("expected ModalKind::Status, got {other:?}"),
    }
}

#[test]
fn release_events_are_ignored() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    let release = KeyEvent::new_with_kind(
        KeyCode::Char('q'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    );
    assert!(!app.on_key(release));
    assert!(app.decision.is_none());
}

#[test]
fn ctrl_e_sets_exit_session() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert!(!app.exit_session);
    // Ctrl+E sets the flag, produces no Decision, and does not exit.
    assert!(!app.on_key(ctrl(KeyCode::Char('e'))));
    assert!(app.exit_session);
    assert!(app.decision.is_none());
    // Plain 'e' from the list still opens the editor — proving the
    // global handler only fires with CONTROL held.
    let mut app2 = App::new(&gens);
    app2.on_key(press(KeyCode::Char('e')));
    assert!(matches!(app2.screen, Screen::Editing { .. }));
    assert!(!app2.exit_session);
}

#[test]
fn ctrl_l_opens_log_and_esc_returns() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert!(matches!(app.screen, Screen::List));

    // Ctrl+L opens the log viewer.
    assert!(!app.on_key(ctrl(KeyCode::Char('l'))));
    assert!(matches!(app.screen, Screen::Log { .. }));

    // Esc pops back to the List.
    assert!(!app.on_key(press(KeyCode::Esc)));
    assert!(matches!(app.screen, Screen::List));

    // Re-open then close via a second Ctrl+L.
    app.on_key(ctrl(KeyCode::Char('l')));
    assert!(matches!(app.screen, Screen::Log { .. }));
    app.on_key(ctrl(KeyCode::Char('l')));
    assert!(matches!(app.screen, Screen::List));
}

#[test]
fn ctrl_k_toggles_log_source_and_resets_scroll() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);

    // Open the viewer: defaults to NMBL's own log.
    app.on_key(ctrl(KeyCode::Char('l')));
    assert!(matches!(
        app.screen,
        Screen::Log {
            source: LogSource::Nmbl,
            ..
        }
    ));

    // Scroll down so we can prove Ctrl+K resets the offset.
    app.on_key(press(KeyCode::Down));
    assert!(matches!(app.screen, Screen::Log { offset: 1, .. }));

    // Ctrl+K flips to the kernel ring buffer and resets scroll to 0.
    app.on_key(ctrl(KeyCode::Char('k')));
    assert!(matches!(
        app.screen,
        Screen::Log {
            source: LogSource::Kernel,
            offset: 0,
            ..
        }
    ));

    // Ctrl+K again flips back to NMBL's log.
    app.on_key(ctrl(KeyCode::Char('k')));
    assert!(matches!(
        app.screen,
        Screen::Log {
            source: LogSource::Nmbl,
            ..
        }
    ));
}

#[test]
fn ctrl_k_is_a_noop_outside_the_log_viewer() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    assert!(matches!(app.screen, Screen::List));
    // Ctrl+K on the list screen must not open or change anything.
    assert!(!app.on_key(ctrl(KeyCode::Char('k'))));
    assert!(matches!(app.screen, Screen::List));
}

#[test]
fn log_source_toggle_hint_text_per_mode() {
    // The bottom-left footer hint advertises the OTHER source.
    assert_eq!(LogSource::Nmbl.toggle_hint(), "Ctrl+K: kernel logs");
    assert_eq!(LogSource::Kernel.toggle_hint(), "Ctrl+K: NMBL logs");
    // And `toggled` flips between the two.
    assert_eq!(LogSource::Nmbl.toggled(), LogSource::Kernel);
    assert_eq!(LogSource::Kernel.toggled(), LogSource::Nmbl);
}

#[test]
fn log_scroll_offset_moves_and_saturates() {
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    app.screen = Screen::Log {
        lines: vec!["a".into(), "b".into(), "c".into()],
        offset: 0,
        source: LogSource::Nmbl,
    };

    // Up at 0 saturates at 0.
    app.on_key(press(KeyCode::Up));
    assert!(matches!(app.screen, Screen::Log { offset: 0, .. }));
    // Down advances by 1.
    app.on_key(press(KeyCode::Down));
    assert!(matches!(app.screen, Screen::Log { offset: 1, .. }));
    // PageDown advances by a page.
    app.on_key(press(KeyCode::PageDown));
    assert!(matches!(app.screen, Screen::Log { offset, .. } if offset == 1 + LOG_PAGE));
    // End jumps to u16::MAX (renderer clamps for display).
    app.on_key(press(KeyCode::End));
    assert!(matches!(
        app.screen,
        Screen::Log {
            offset: u16::MAX,
            ..
        }
    ));
    // Home returns to 0.
    app.on_key(press(KeyCode::Home));
    assert!(matches!(app.screen, Screen::Log { offset: 0, .. }));
}

#[test]
fn shared_session_latch_spans_two_apps() {
    // Once the central `LatchingConsole` layer sets the session latch
    // (on the operator's first input), that presence must be visible to
    // every App built from the same session via new_in_session — proving
    // the emergency screen sees interaction from the selector / passphrase
    // prompt. We set the latch directly here (the wrapper is what sets it
    // in production; `on_key` no longer does).
    let session = SessionInteraction::new();
    let gens = vec![fake_gen(1, &[])];
    let first = App::new_in_session(&gens, &session);
    assert!(!session.get());
    assert!(!first.interaction.get());
    session.set();
    assert!(
        first.interaction.get(),
        "first App observes the shared latch"
    );

    let second = App::new_in_session(&[], &session);
    assert!(
        second.interaction.get(),
        "second App must observe the shared latch"
    );
}
