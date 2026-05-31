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

    // Scroll down so we can prove Ctrl+K resets the offset. Down also
    // leaves follow-bottom mode.
    app.on_key(press(KeyCode::Down));
    assert!(
        matches!(&app.screen, Screen::Log { offset, follow_bottom, .. }
        if offset.get() == 1 && !follow_bottom.get())
    );

    // Ctrl+K flips to the kernel ring buffer and re-pins to the bottom
    // (offset reset to 0, follow_bottom re-armed).
    app.on_key(ctrl(KeyCode::Char('k')));
    assert!(matches!(&app.screen,
        Screen::Log {
            source: LogSource::Kernel,
            offset,
            follow_bottom,
            ..
        } if offset.get() == 0 && follow_bottom.get()));

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
fn kernel_snapshot_taken_once_not_per_scroll() {
    // Regression: scrolling the kernel-log view must operate purely on
    // the cached snapshot. Draining + parsing /dev/kmsg per keystroke is
    // O(buffer) and made the kernel view laggy. Install a counting fake
    // raw-reader, toggle to kernel mode (one read), fire a burst of
    // scroll events, and assert the reader was invoked exactly once.
    use std::cell::Cell;
    use std::rc::Rc;

    let calls = Rc::new(Cell::new(0usize));
    let calls_in_closure = Rc::clone(&calls);
    let _guard = crate::log::set_raw_reader_for_test(move || {
        calls_in_closure.set(calls_in_closure.get() + 1);
        Ok("6,1,0,-;kernel line one\n6,2,1000000,-;kernel line two\n".to_owned())
    });

    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);

    // Open the viewer (NMBL log — does not touch the kernel reader).
    app.on_key(ctrl(KeyCode::Char('l')));
    assert_eq!(calls.get(), 0, "opening the NMBL log must not read kmsg");

    // Ctrl+K snapshots the kernel ring buffer exactly once.
    app.on_key(ctrl(KeyCode::Char('k')));
    assert!(matches!(
        app.screen,
        Screen::Log {
            source: LogSource::Kernel,
            ..
        }
    ));
    assert_eq!(calls.get(), 1, "Ctrl+K snapshots the kernel log once");

    // A burst of scroll events must NOT re-invoke the reader.
    for key in [
        KeyCode::Down,
        KeyCode::Down,
        KeyCode::PageDown,
        KeyCode::Up,
        KeyCode::PageUp,
        KeyCode::End,
        KeyCode::Home,
    ] {
        app.on_key(press(key));
    }
    assert_eq!(
        calls.get(),
        1,
        "scrolling the kernel log must reuse the cached snapshot, not re-read kmsg"
    );

    // The cached lines are the parsed snapshot, proving scroll reads the
    // cache rather than an empty/placeholder buffer.
    match &app.screen {
        Screen::Log { lines, .. } => assert_eq!(
            lines,
            &vec![
                "[    0.000000] kernel line one".to_owned(),
                "[    1.000000] kernel line two".to_owned(),
            ]
        ),
        _ => panic!("expected Screen::Log after Ctrl+K"),
    }
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
    use std::cell::Cell;
    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);
    app.screen = Screen::Log {
        lines: vec!["a".into(), "b".into(), "c".into()],
        offset: Cell::new(0),
        follow_bottom: Cell::new(false),
        source: LogSource::Nmbl,
    };

    let off = |app: &App<'_>| match &app.screen {
        Screen::Log { offset, .. } => offset.get(),
        _ => panic!("expected Screen::Log"),
    };
    let following = |app: &App<'_>| match &app.screen {
        Screen::Log { follow_bottom, .. } => follow_bottom.get(),
        _ => panic!("expected Screen::Log"),
    };

    // Up at 0 saturates at 0.
    app.on_key(press(KeyCode::Up));
    assert_eq!(off(&app), 0);
    // Down advances by 1.
    app.on_key(press(KeyCode::Down));
    assert_eq!(off(&app), 1);
    // PageDown advances by a page.
    app.on_key(press(KeyCode::PageDown));
    assert_eq!(off(&app), 1 + LOG_PAGE);
    // End re-arms follow-bottom (renderer resolves the concrete offset).
    app.on_key(press(KeyCode::End));
    assert!(following(&app), "End re-pins to the bottom");
    // Home jumps to 0 and leaves follow-bottom mode.
    app.on_key(press(KeyCode::Home));
    assert_eq!(off(&app), 0);
    assert!(!following(&app), "Home is an explicit scroll, not follow");
}

#[test]
fn log_opens_at_bottom_and_first_up_is_immediate() {
    // Open-at-bottom: a >viewport buffer must show its LAST page after the
    // first render, and the very first Up must move up by exactly one line
    // (no dead presses). We drive the real renderer through a test backend
    // so `render_log` resolves `follow_bottom` against a concrete viewport
    // height and writes the clamped offset back.
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let gens = vec![fake_gen(1, &[])];
    let mut app = App::new(&gens);

    // 200 lines on a 24-row terminal: the body box (minus borders + footer)
    // is well under 200 rows, so the buffer is scrollable.
    let lines: Vec<String> = (0..200).map(|i| format!("line {i}")).collect();
    app.on_key(ctrl(KeyCode::Char('l')));
    if let Screen::Log { lines: l, .. } = &mut app.screen {
        *l = lines;
    } else {
        panic!("Ctrl+L must open the log viewer");
    }

    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");

    // First render resolves the bottom-most offset. With 200 lines and a
    // ~21-row inner viewport the bottom offset is 200 - visible, i.e. large.
    term.draw(|f| crate::ui::render_current_screen(f, &app))
        .expect("first frame");
    let bottom = match &app.screen {
        Screen::Log {
            offset,
            follow_bottom,
            ..
        } => {
            assert!(follow_bottom.get(), "still following the bottom on open");
            offset.get()
        }
        _ => panic!("expected Screen::Log"),
    };
    assert!(
        bottom > 0,
        "a >viewport buffer must open scrolled to the bottom"
    );

    // First Up: moves up by exactly one line from the resolved bottom and
    // leaves follow-bottom mode — no dead presses.
    app.on_key(press(KeyCode::Up));
    match &app.screen {
        Screen::Log {
            offset,
            follow_bottom,
            ..
        } => {
            assert!(!follow_bottom.get(), "first Up stops following the bottom");
            assert_eq!(
                offset.get(),
                bottom - 1,
                "first Up moves up exactly one line from the bottom"
            );
        }
        _ => panic!("expected Screen::Log"),
    }

    // End re-pins to the bottom on the next frame.
    app.on_key(press(KeyCode::End));
    term.draw(|f| crate::ui::render_current_screen(f, &app))
        .expect("second frame");
    match &app.screen {
        Screen::Log { offset, .. } => {
            assert_eq!(offset.get(), bottom, "End jumps back to the bottom");
        }
        _ => panic!("expected Screen::Log"),
    }

    // Home jumps to the top.
    app.on_key(press(KeyCode::Home));
    term.draw(|f| crate::ui::render_current_screen(f, &app))
        .expect("third frame");
    match &app.screen {
        Screen::Log { offset, .. } => assert_eq!(offset.get(), 0, "Home jumps to the top"),
        _ => panic!("expected Screen::Log"),
    }
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
