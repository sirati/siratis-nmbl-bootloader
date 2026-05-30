use super::*;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;

use crate::error::Result;
use crate::rescue::RescueMode;
use crate::terminal::TerminalAction;
use crate::ui::app::App;
use crate::ui::console::{ConsoleEvent, ConsoleKind};

fn io_err(ctx: &str) -> NmblError {
    NmblError::Io {
        source: std::io::Error::other("test"),
        context: ctx.to_string(),
    }
}

/// Scripted in-process [`Console`] for unit-testing
/// `drop_to_emergency`. Drives a queued sequence of key events on
/// `poll_key()` and stays in lockstep with the emergency-screen
/// loop's render/poll cadence.
///
/// Mirrors the `TestConsole` in `ui::emergency::tests`; lives
/// here because the cross-module visibility rules make the
/// emergency-module one unreachable from `shell::tests`.
struct ScriptedConsole {
    events: Vec<Option<KeyEvent>>,
    cursor: usize,
}

impl ScriptedConsole {
    fn new(events: Vec<Option<KeyEvent>>) -> Self {
        Self { events, cursor: 0 }
    }
}

impl Console for ScriptedConsole {
    fn render(&mut self, _app: &App<'_>) -> Result<()> {
        Ok(())
    }
    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>>
    {
        Box::pin(async move { self.poll_event_blocking(timeout) })
    }
    fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
        let v = self.events.get(self.cursor).copied().flatten();
        self.cursor = self.cursor.saturating_add(1);
        Ok(v.map(ConsoleEvent::Key))
    }
    fn size(&self) -> (u16, u16) {
        (80, 24)
    }
    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }
    fn draw_with(&mut self, _body: &mut dyn FnMut(&mut Frame<'_>)) -> Result<()> {
        Ok(())
    }
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Drive an async future to completion on a throwaway current-thread
/// runtime so the synchronous shell tests can exercise the now-async
/// `drop_to_emergency`.
fn block<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}

/// A poller `LocalSender` for tests that don't reach the activation
/// reap path (none of these scripts pick Retry boot), so the op is
/// never enqueued and the undriven poller is harmless.
fn test_sender() -> crate::sys::poller::LocalSender {
    crate::sys::poller::build().1
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn drop_to_emergency_shell_choice_cancels_picker_then_reboots() {
    // The `[Raw Shell]` choice now opens the in-process picker
    // dialog (in-process flow, NOT TerminalAction::Execve). The
    // script presses 's' to commit Raw Shell directly (feature-
    // independent — the row index for Raw Shell drifts between the
    // feature configurations, the hotkey does not), then Esc to
    // cancel the picker, then 'r' on the re-displayed emergency
    // menu to commit a reboot. Verifying the produced
    // TerminalAction is Reboot — not Execve — pins the
    // architectural change.
    let mut config = Config::recovery_default();
    config.rescue.mode = RescueMode::Embedded;
    config.paths.shell = PathBuf::from("/bin/test-emergency-shell");

    let console: Box<dyn Console> = Box::new(ScriptedConsole::new(vec![
        // Emergency menu: 's' hotkey commits Raw Shell.
        Some(press(KeyCode::Char('s'))),
        // Picker dialog: Esc to cancel back to the emergency menu.
        Some(press(KeyCode::Esc)),
        // Emergency menu (second iteration): 'r' commits Reboot.
        Some(press(KeyCode::Char('r'))),
    ]));

    let action = block(drop_to_emergency(
        console,
        &config,
        io_err("synthetic boot failure"),
        &SessionInteraction::new(),
        &test_sender(),
    ));
    assert!(
        matches!(action, TerminalAction::Reboot),
        "Raw Shell choice must NOT produce a TerminalAction::Execve any more; \
         got {action:?}"
    );
}

#[test]
fn drop_to_emergency_returns_reboot_on_r_hotkey() {
    // The emergency screen surfaces 'r' as a one-shot reboot
    // hotkey (matches the operator muscle-memory call-out in
    // ui::app::handle_emergency_key). drop_to_emergency must
    // surface that as TerminalAction::Reboot.
    let config = Config::recovery_default();

    let console: Box<dyn Console> =
        Box::new(ScriptedConsole::new(vec![Some(press(KeyCode::Char('r')))]));

    let action = block(drop_to_emergency(
        console,
        &config,
        io_err("synthetic"),
        &SessionInteraction::new(),
        &test_sender(),
    ));

    match action {
        TerminalAction::Reboot => {}
        other => panic!("expected Reboot, got {other:?}"),
    }
}
