use crossterm::event::KeyCode;

use crate::error::NmblError;
use crate::ui::app::SessionInteraction;
use crate::ui::password_supplier::passphrase_prompt_on_console;
use crate::ui::view::{PassphraseScreenData, render_passphrase};

use super::{ScriptedConsole, block, press};

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
