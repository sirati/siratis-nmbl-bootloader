use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::error::NmblError;
use crate::ui::app::{SessionInteraction, SkipSelector};
use crate::ui::password_supplier::passphrase_prompt_on_console;
use crate::ui::view::{PassphraseScreenData, render_passphrase};

use super::{ScriptedConsole, block, press};

/// Ctrl+G as a KeyEvent for driving the checkbox-toggle tests.
fn ctrl_g() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)
}

#[test]
fn passphrase_prompt_collects_typed_chars_and_returns_on_enter() {
    // Type "ok" + Enter — supplier must return "ok" and the
    // console must have observed the per-char buffer growth.
    let keys = vec![
        press(KeyCode::Char('o')),
        press(KeyCode::Char('k')),
        press(KeyCode::Enter),
    ];
    let mut console = ScriptedConsole::new(keys);
    let secret = block(passphrase_prompt_on_console(
        &mut console,
        "Unlock root",
        &SessionInteraction::new(),
        &SkipSelector::new(),
    ))
    .expect("Enter submits the buffer");
    assert_eq!(&**secret, "ok");
    // Initial render + 2 char-keys + 1 Enter = 4 dirty repaints.
    assert!(
        console.renders >= 3,
        "expected at least 3 renders, got {}",
        console.renders
    );
    assert_eq!(
        console.last_label.as_deref(),
        Some("Unlock root"),
        "render path must observe the supplied prompt label"
    );
}

#[test]
fn passphrase_prompt_ignores_enter_on_empty_buffer() {
    // Enter on an empty buffer must be silently ignored (matches
    // login-screen convention; an empty string would surface as a
    // cryptsetup IO error otherwise). Once a char arrives, Enter
    // submits as usual.
    let keys = vec![
        press(KeyCode::Enter),
        press(KeyCode::Char('p')),
        press(KeyCode::Enter),
    ];
    let mut console = ScriptedConsole::new(keys);
    let secret = block(passphrase_prompt_on_console(
        &mut console,
        "Unlock",
        &SessionInteraction::new(),
        &SkipSelector::new(),
    ))
    .expect("Enter after a char submits the buffer");
    assert_eq!(&**secret, "p");
}

#[test]
fn passphrase_prompt_backspace_shrinks_buffer() {
    let keys = vec![
        press(KeyCode::Char('a')),
        press(KeyCode::Char('b')),
        press(KeyCode::Backspace),
        press(KeyCode::Enter),
    ];
    let mut console = ScriptedConsole::new(keys);
    let secret = block(passphrase_prompt_on_console(
        &mut console,
        "Unlock",
        &SessionInteraction::new(),
        &SkipSelector::new(),
    ))
    .expect("Enter submits the buffer");
    assert_eq!(&**secret, "a", "backspace must drop the last char");
}

#[test]
fn passphrase_prompt_esc_returns_tui_error() {
    let keys = vec![press(KeyCode::Char('x')), press(KeyCode::Esc)];
    let mut console = ScriptedConsole::new(keys);
    let err = block(passphrase_prompt_on_console(
        &mut console,
        "Unlock",
        &SessionInteraction::new(),
        &SkipSelector::new(),
    ))
    .expect_err("Esc must propagate as a Tui error");
    assert!(matches!(err, NmblError::Tui { .. }));
}

#[test]
fn passphrase_prompt_renders_dotted_mask_via_view() {
    // End-to-end visual check: drive the supplier under a TestBackend
    // until just before Enter, then synthesise one final render to
    // capture the masked view. Sanity-checks both that the supplier
    // reuses render_passphrase and that the mask grows with the buffer.
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let data = PassphraseScreenData {
        prompt_label: "Unlock root",
        buffer_len: 4,
        cursor_column: 4,
        verifying: false,
        spinner_frame: 0,
        caps_lock_on: false,
        select_generation: false,
    };
    let mut term = Terminal::new(TestBackend::new(60, 14)).expect("test terminal");
    term.draw(|f| render_passphrase(f, &data)).expect("draw");
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
        dump.contains("****"),
        "masked dots must be visible for buffer_len=4: \n{dump}"
    );
    assert!(
        dump.contains("Unlock root"),
        "prompt label must be visible: \n{dump}"
    );
    assert!(
        dump.contains("Enter=submit"),
        "footer hint must be visible: \n{dump}"
    );
}

/// Render dump helper for the checkbox tests.
fn render_dump(data: &PassphraseScreenData<'_>) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let mut term = Terminal::new(TestBackend::new(70, 16)).expect("test terminal");
    term.draw(|f| render_passphrase(f, data)).expect("draw");
    let buf = term.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn passphrase_render_shows_unchecked_checkbox_and_ctrl_g_hint() {
    // Default (unchecked) render must show `[ ] Select NixOS Generation`
    // plus the `(Ctrl+G)` hint so the operator discovers the hotkey.
    let data = PassphraseScreenData {
        prompt_label: "Unlock root",
        buffer_len: 0,
        cursor_column: 0,
        verifying: false,
        spinner_frame: 0,
        caps_lock_on: false,
        select_generation: false,
    };
    let dump = render_dump(&data);
    assert!(
        dump.contains("[ ] Select NixOS Generation"),
        "unchecked checkbox must render:\n{dump}"
    );
    assert!(
        dump.contains("(Ctrl+G)"),
        "Ctrl+G hint must render:\n{dump}"
    );
}

#[test]
fn passphrase_render_shows_checked_checkbox_when_selected() {
    let data = PassphraseScreenData {
        prompt_label: "Unlock root",
        buffer_len: 0,
        cursor_column: 0,
        verifying: false,
        spinner_frame: 0,
        caps_lock_on: false,
        select_generation: true,
    };
    let dump = render_dump(&data);
    assert!(
        dump.contains("[x] Select NixOS Generation"),
        "checked checkbox must render:\n{dump}"
    );
}

#[test]
fn ctrl_g_toggles_select_generation_only_on_passphrase_screen() {
    use crate::generations::Generation;
    use crate::ui::app::{App, Screen};

    let gens: Vec<Generation> = vec![];
    let mut app = App::new(&gens);
    app.screen = Screen::Passphrase {
        prompt_label: "Unlock".to_string(),
        buffer: zeroize::Zeroizing::new(String::new()),
        cursor: 0,
        verifying: false,
        spinner_frame: 0,
        select_generation: false,
    };
    // Ctrl+G flips false -> true -> false; never produces a Decision.
    assert!(!app.on_key(ctrl_g()));
    match &app.screen {
        Screen::Passphrase {
            select_generation, ..
        } => assert!(*select_generation),
        _ => panic!("expected Passphrase"),
    }
    assert!(!app.on_key(ctrl_g()));
    match &app.screen {
        Screen::Passphrase {
            select_generation, ..
        } => assert!(!*select_generation),
        _ => panic!("expected Passphrase"),
    }
    // Ctrl+G is inert on other screens (no panic, no state change).
    app.screen = Screen::List;
    assert!(!app.on_key(ctrl_g()));
    assert!(matches!(app.screen, Screen::List));
}

#[test]
fn submit_unchecked_sets_skip_selector() {
    // Default (checkbox off): a plain unlock must latch skip_selector so
    // the dispatch boots the default generation without the selector.
    let keys = vec![press(KeyCode::Char('z')), press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let skip = SkipSelector::new();
    let secret = block(passphrase_prompt_on_console(
        &mut console,
        "Unlock",
        &SessionInteraction::new(),
        &skip,
    ))
    .expect("Enter submits the buffer");
    assert_eq!(&**secret, "z");
    assert!(
        skip.get(),
        "unchecked checkbox at submit must set skip_selector=true"
    );
}

#[test]
fn submit_checked_clears_skip_selector() {
    // Ctrl+G checks the box, then Enter submits: skip_selector must be
    // false so the selector is shown as today.
    let keys = vec![press(KeyCode::Char('z')), ctrl_g(), press(KeyCode::Enter)];
    let mut console = ScriptedConsole::new(keys);
    let skip = SkipSelector::new();
    let secret = block(passphrase_prompt_on_console(
        &mut console,
        "Unlock",
        &SessionInteraction::new(),
        &skip,
    ))
    .expect("Enter submits the buffer");
    assert_eq!(&**secret, "z");
    assert!(
        !skip.get(),
        "checked checkbox at submit must leave skip_selector=false (show selector)"
    );
}
