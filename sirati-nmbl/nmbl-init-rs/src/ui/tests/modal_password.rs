use crossterm::event::KeyCode;

use crate::ui::app::{App, EmergencyChoice, EmergencyItem, ModalKind, Screen};
use crate::ui::modal_confirm::show_modal_confirm_over;
use crate::ui::modal_password::show_wrong_password_modal;
use crate::ui::screen_render::render_current_screen;
use crate::ui::view;
use crate::ui::{ConfirmOutcome, WrongPasswordOutcome};

use super::{ScriptedConsole, block, press};

// --- show_wrong_password_modal -----------------------------------

#[test]
fn show_wrong_password_modal_default_enter_returns_try_again() {
    // Default highlight is [Try again] so a single Enter must
    // commit to TryAgain — protects the most common path
    // (operator mistyped, just wants to retry).
    let keys = vec![press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let out =
        block(show_wrong_password_modal(&mut console, 1)).expect("modal must succeed on Enter");
    assert_eq!(out, WrongPasswordOutcome::TryAgain);
}

#[test]
fn show_wrong_password_modal_right_arrow_then_enter_reboots() {
    // Right toggles to [Reboot]; Enter commits.
    let keys = vec![press(KeyCode::Right), press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_wrong_password_modal(&mut console, 1)).expect("modal must succeed");
    assert_eq!(out, WrongPasswordOutcome::Reboot);
}

#[cfg(feature = "pretty-shell")]
#[test]
fn show_wrong_password_modal_two_rights_then_enter_picks_pretty_shell() {
    // With `pretty-shell` Pretty Shell sits at index 2. Right Right
    // navigates there; Enter commits.
    let keys = vec![
        press(KeyCode::Right),
        press(KeyCode::Right),
        press(KeyCode::Enter),
    ];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_wrong_password_modal(&mut console, 2)).expect("modal must succeed");
    assert_eq!(out, WrongPasswordOutcome::PrettyShell);
}

#[cfg(feature = "pretty-shell")]
#[test]
fn show_wrong_password_modal_three_rights_then_enter_picks_raw_shell() {
    // With `pretty-shell` Raw Shell sits at index 3. Right Right Right
    // navigates there; Enter commits.
    let keys = vec![
        press(KeyCode::Right),
        press(KeyCode::Right),
        press(KeyCode::Right),
        press(KeyCode::Enter),
    ];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_wrong_password_modal(&mut console, 2)).expect("modal must succeed");
    assert_eq!(out, WrongPasswordOutcome::RawShell);
}

#[cfg(not(feature = "pretty-shell"))]
#[test]
fn show_wrong_password_modal_two_rights_then_enter_picks_raw_shell_no_feature() {
    // Without `pretty-shell` Raw Shell sits at index 2 (Pretty
    // Shell row is hidden). Right Right + Enter commits Raw Shell.
    let keys = vec![
        press(KeyCode::Right),
        press(KeyCode::Right),
        press(KeyCode::Enter),
    ];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_wrong_password_modal(&mut console, 2)).expect("modal must succeed");
    assert_eq!(out, WrongPasswordOutcome::RawShell);
}

#[test]
fn show_wrong_password_modal_hotkeys_commit_directly() {
    // 't', 'r', 's' each commit regardless of highlighted button.
    // 'p' is only wired when `pretty-shell` is compiled in.
    for (code, expected) in [
        (KeyCode::Char('t'), WrongPasswordOutcome::TryAgain),
        (KeyCode::Char('r'), WrongPasswordOutcome::Reboot),
        (KeyCode::Char('s'), WrongPasswordOutcome::RawShell),
    ] {
        let mut console = ScriptedConsole::new(vec![press(code)]);
        let out = block(show_wrong_password_modal(&mut console, 1))
            .expect("modal must succeed on hotkey");
        assert_eq!(out, expected, "hotkey {code:?} should yield {expected:?}");
    }
    #[cfg(feature = "pretty-shell")]
    {
        let mut console = ScriptedConsole::new(vec![press(KeyCode::Char('p'))]);
        let out = block(show_wrong_password_modal(&mut console, 1))
            .expect("modal must succeed on 'p' hotkey");
        assert_eq!(out, WrongPasswordOutcome::PrettyShell);
    }
}

#[test]
fn show_wrong_password_modal_esc_maps_to_try_again() {
    // Esc must NOT reboot — defence against a stray Esc keystroke
    // wiping out the boot. Spec: Esc = Try again.
    let keys = vec![press(KeyCode::Esc)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_wrong_password_modal(&mut console, 3)).expect("modal must succeed on Esc");
    assert_eq!(out, WrongPasswordOutcome::TryAgain);
}

#[test]
fn show_wrong_password_modal_left_wraps_from_try_again_to_last_button() {
    // Left arrow from index 0 wraps to the last button (Raw Shell
    // in both feature configurations — it is the rightmost row).
    let keys = vec![press(KeyCode::Left), press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_wrong_password_modal(&mut console, 1)).expect("modal must succeed");
    assert_eq!(out, WrongPasswordOutcome::RawShell);
}

#[test]
fn show_wrong_password_modal_renders_title_with_attempt_counter() {
    // End-to-end visual check: the title must include the literal
    // "attempt N" string so the operator sees the retry counter.
    // Also pins that every button label paints — including the
    // feature-gated Pretty Shell row when present.
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[cfg(feature = "pretty-shell")]
    let labels: &[&str] = &["Try again", "Reboot", "Pretty Shell", "Raw Shell"];
    #[cfg(not(feature = "pretty-shell"))]
    let labels: &[&str] = &["Try again", "Reboot", "Raw Shell"];

    let data = view::ModalButtonsScreenData {
        title: "Wrong password (attempt 3)",
        message: "cryptsetup rejected the passphrase.",
        labels,
        selected: 0,
        hint: "Left/Right select  Enter confirm  Esc = Try again",
        scroll_offset: 0,
    };
    let mut term = Terminal::new(TestBackend::new(80, 16)).expect("test terminal");
    term.draw(|f| view::render_modal_buttons(f, &data))
        .expect("draw");
    let buf = term.backend().buffer();
    let dump: String = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        dump.contains("attempt 3"),
        "title must surface the attempt counter:\n{dump}"
    );
    assert!(
        dump.contains("[Try again]"),
        "Try again button visible:\n{dump}"
    );
    assert!(dump.contains("[Reboot]"), "Reboot button visible:\n{dump}");
    assert!(
        dump.contains("[Raw Shell]"),
        "Raw Shell button visible:\n{dump}"
    );
    #[cfg(feature = "pretty-shell")]
    assert!(
        dump.contains("[Pretty Shell]"),
        "Pretty Shell button visible:\n{dump}"
    );
}

// ---- Overlay variants -------------------------------------------

#[test]
fn show_modal_confirm_over_sets_and_clears_modal_on_app() {
    // The overlay variant must install a `ModalKind::Confirm` on
    // entry and clear it back to `None` on exit so a re-entry into
    // the picker doesn't paint a stale dialog.
    let gens = [];
    let mut app = App::new(&gens);
    // Seed a benign screen state that should survive the modal.
    app.selected_index = 4;
    let keys = vec![press(KeyCode::Char('y'))];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm_over(
        &mut console,
        &mut app,
        "title",
        "body",
        "Yes",
        "No",
        true,
    ))
    .expect("overlay modal must succeed on 'y'");
    assert_eq!(out, ConfirmOutcome::Yes);
    assert!(app.modal.is_none(), "modal must be cleared on exit");
    assert_eq!(
        app.selected_index, 4,
        "underlying selection must survive the modal"
    );
    // No leftover Confirm variant.
    let _: () = match &app.modal {
        None => (),
        Some(ModalKind::Confirm { .. }) => panic!("modal Confirm leaked"),
        Some(_) => panic!("unexpected modal variant"),
    };
}

#[test]
fn show_modal_confirm_over_returns_to_same_screen_on_close() {
    // Close the modal via Esc (Cancelled) and confirm the
    // underlying screen variant is unchanged. Operators expect the
    // menu to be exactly where it was; this pins that behaviour.
    let gens = [];
    let mut app = App::new(&gens);
    // Park on a known emergency-menu screen with selection=2.
    app.screen = Screen::Emergency {
        message: "boot failed".into(),
        items: vec![
            EmergencyItem {
                label: "Reboot",
                choice: EmergencyChoice::Reboot,
            },
            EmergencyItem {
                label: "Raw Shell",
                choice: EmergencyChoice::RawShell,
            },
            EmergencyItem {
                label: "Retry",
                choice: EmergencyChoice::RetryBoot,
            },
        ],
        selected: 2,
        chosen: None,
    };
    let keys = vec![press(KeyCode::Esc)];
    let mut console = ScriptedConsole::new(keys);
    let out = block(show_modal_confirm_over(
        &mut console,
        &mut app,
        "t",
        "b",
        "Yes",
        "Back",
        true,
    ))
    .expect("modal must succeed on Esc");
    assert_eq!(out, ConfirmOutcome::Cancelled);
    assert!(app.modal.is_none());
    match &app.screen {
        Screen::Emergency { selected, .. } => {
            assert_eq!(*selected, 2, "selection must survive the modal");
        }
        _ => panic!("underlying screen must remain Emergency"),
    }
}

#[test]
fn show_modal_confirm_over_renders_modal_atop_underlying_screen() {
    // End-to-end visual check via the splash render path: the
    // dispatcher in `render_current_screen` must paint the menu
    // first and then the modal on top. Both must be visible in
    // the rendered buffer (modal punches a Clear into its rect,
    // but the menu header / footer survive).
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let gens = [];
    let mut app = App::new(&gens);
    app.screen = Screen::Emergency {
        message: "boot failed: synthetic".into(),
        items: vec![EmergencyItem {
            label: "RebootMenuItem",
            choice: EmergencyChoice::Reboot,
        }],
        selected: 0,
        chosen: None,
    };
    app.modal = Some(ModalKind::Confirm {
        title: "ConfirmTitleX".into(),
        message: "modal body".into(),
        yes_label: "Yes".into(),
        no_label: "No".into(),
        yes_selected: true,
        hint: "hint".into(),
    });
    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("test terminal");
    term.draw(|f| render_current_screen(f, &app)).expect("draw");
    let buf = term.backend().buffer();
    let dump: String = (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        dump.contains("ConfirmTitleX"),
        "modal title must paint on top:\n{dump}"
    );
    // The underlying emergency screen must paint BEHIND the modal.
    // The centred modal punches a Clear into its rect (rows ~1..16,
    // cols ~8..72 on an 80x24 backend), but the project header in
    // row 0, the "[Rebo…" menu fragment peeking from below the
    // modal's right edge, the "action" border at the bottom, AND
    // the footer hint must all survive.
    assert!(
        dump.contains("sirati's NMBL"),
        "project header (row 0) must remain visible above the modal:\n{dump}"
    );
    assert!(
        dump.contains("[Rebo"),
        "underlying picker item must peek from behind the modal:\n{dump}"
    );
    assert!(
        dump.contains("up/down select"),
        "underlying footer hint must remain visible:\n{dump}"
    );
}
