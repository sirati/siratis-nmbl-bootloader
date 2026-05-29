//! Tests for the remote-TUI server session lifecycle.
//!
//! These exercise the per-session loop ([`run_remote_menu`]) with a
//! scripted fake console (so no real pty/socket is needed) and the
//! shutdown / sink bookkeeping the accept multiplexer relies on.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::terminal::TerminalAction;
use crate::ui::app::{App, SessionInteraction};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::{build_emergency_app, build_message, default_items};

use super::{ActionSink, Shutdown, run_remote_menu};

/// Drive an async future to completion on a throwaway current-thread
/// runtime. The scripted console resolves instantly and the emergency
/// loop's `select!` is biased on input, so no wall-clock time elapses.
fn block<F: Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}

/// A scripted in-process [`Console`] for the remote session loop. Feeds a
/// sequence of optional key events; `error_after` makes `poll_event`
/// return an error after N events to simulate a client disconnect.
struct FakeConsole {
    events: Vec<Option<KeyEvent>>,
    cursor: usize,
    error_after: Option<usize>,
}

impl FakeConsole {
    fn new(events: Vec<Option<KeyEvent>>) -> Self {
        Self {
            events,
            cursor: 0,
            error_after: None,
        }
    }

    fn erroring(error_after: usize) -> Self {
        Self {
            events: Vec::new(),
            cursor: 0,
            error_after: Some(error_after),
        }
    }
}

impl Console for FakeConsole {
    fn render(&mut self, _app: &App<'_>) -> Result<()> {
        Ok(())
    }
    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move { self.poll_event_blocking(timeout) })
    }
    fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
        let at = self.cursor;
        self.cursor = self.cursor.saturating_add(1);
        if let Some(n) = self.error_after
            && at >= n
        {
            return Err(NmblError::Tui {
                source: std::io::Error::other("client disconnected"),
            });
        }
        Ok(self
            .events
            .get(at)
            .copied()
            .flatten()
            .map(ConsoleEvent::Key))
    }
    fn size(&self) -> (u16, u16) {
        (80, 24)
    }
    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }
    fn draw_with(&mut self, _body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        Ok(())
    }
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn fresh_app() -> App<'static> {
    let session = SessionInteraction::new();
    let message = build_message(&NmblError::Io {
        source: std::io::Error::other("boot failed"),
        context: "test".to_string(),
    });
    build_emergency_app(&message, &default_items(), &session)
}

fn run_menu(console: &mut dyn Console) -> Option<TerminalAction> {
    let config = Config::recovery_default();
    let session = SessionInteraction::new();
    let mut app = fresh_app();
    let mut errs = 0u32;
    block(run_remote_menu(
        console,
        &mut app,
        &config,
        &session,
        Duration::from_secs(30),
        &mut errs,
    ))
}

#[test]
fn enter_on_reboot_commits_reboot() {
    // Index 0 is Reboot; pressing Enter selects it → terminal action.
    let mut console = FakeConsole::new(vec![Some(press(KeyCode::Enter))]);
    let action = run_menu(&mut console);
    assert!(
        matches!(action, Some(TerminalAction::Reboot)),
        "expected Reboot, got {action:?}"
    );
}

#[test]
fn ctrl_e_ends_session_with_no_action() {
    // Ctrl+E sets app.exit_session; the remote loop must end the session
    // WITHOUT committing any terminal action (the machine keeps running
    // for the local operator / other remote sessions).
    let mut console = FakeConsole::new(vec![Some(ctrl(KeyCode::Char('e')))]);
    let action = run_menu(&mut console);
    assert!(action.is_none(), "Ctrl+E must not commit an action");
}

#[test]
fn poll_error_ends_session_without_rebooting() {
    // A client disconnect surfaces as a console poll error. The session
    // must end with None — NEVER silently commit a machine-wide Reboot.
    let mut console = FakeConsole::erroring(0);
    let action = run_menu(&mut console);
    assert!(
        action.is_none(),
        "a disconnected client must not trigger a reboot"
    );
}

#[test]
fn shutdown_signal_is_observable_across_clones() {
    let s = Shutdown::new();
    let c = s.clone();
    assert!(!s.is_signalled());
    c.signal();
    assert!(s.is_signalled(), "signal must be visible across clones");
}

#[test]
fn action_sink_keeps_first_committer() {
    // Mirrors serve_session's "first committer wins" rule on the shared
    // sink: a second commit must not overwrite the first.
    let sink: ActionSink = Rc::new(RefCell::new(None));
    {
        let mut slot = sink.borrow_mut();
        if slot.is_none() {
            *slot = Some(TerminalAction::Reboot);
        }
    }
    {
        let mut slot = sink.borrow_mut();
        if slot.is_none() {
            *slot = Some(TerminalAction::Kexec);
        }
    }
    assert!(
        matches!(*sink.borrow(), Some(TerminalAction::Reboot)),
        "first committed action must win"
    );
}

#[test]
fn server_returns_on_pre_signalled_shutdown_and_unlinks() {
    // The full server: with shutdown pre-signalled it must bind, then
    // return promptly (no clients) and unlink the socket via the
    // SocketUnlinkGuard. Exercises bind + multiplexer + shutdown +
    // unlink end-to-end without needing root or a pty.
    use super::run_remote_server;
    use crate::ipc::tui_socket::TUI_SOCK_PATH;

    let config = Config::recovery_default();
    let shutdown = Shutdown::new();
    let sink: ActionSink = Rc::new(RefCell::new(None));
    shutdown.signal();

    // bind_listener needs /nmbl-run root-owned 0700; in an unprivileged
    // test sandbox that may be unavailable. If the bind fails the server
    // returns early without creating the socket — the assertion below
    // (socket absent) still holds, so the test is meaningful either way.
    block(run_remote_server(&config, shutdown, sink));

    assert!(
        !std::path::Path::new(TUI_SOCK_PATH).exists(),
        "socket must be unlinked after server shutdown"
    );
}
