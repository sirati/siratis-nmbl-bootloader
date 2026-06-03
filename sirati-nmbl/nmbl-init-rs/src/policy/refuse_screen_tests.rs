//! Tests for the non-interactive refuse countdown: the CLOSED default-DENY
//! input policy (FIX-18), the scrubbed Ctrl+L log (FIX-41), and that
//! Enter/timeout yield the RebootIntoRescue terminus.

use std::collections::VecDeque;
use std::pin::Pin;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{RefuseKey, classify, run_refuse_screen};
use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::policy::{guard, registry};
use crate::terminal::TerminalAction;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};

fn key(code: KeyCode) -> ConsoleEvent {
    ConsoleEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ctrl(code: KeyCode) -> ConsoleEvent {
    ConsoleEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL))
}

#[test]
fn classify_allows_only_enter_and_ctrl_l() {
    assert_eq!(classify(key(KeyCode::Enter)), Some(RefuseKey::RebootNow));
    assert_eq!(
        classify(ctrl(KeyCode::Char('l'))),
        Some(RefuseKey::OpenLogs)
    );
}

#[test]
fn classify_default_denies_every_other_key() {
    // Default-DENY: none of these act, so none can shorten the countdown
    // (the caller maps `None` to CountdownAction::Continue) — FIX-18.
    for ev in [
        key(KeyCode::Char('x')),
        key(KeyCode::Char('q')),
        key(KeyCode::Esc),
        key(KeyCode::Char(' ')),
        key(KeyCode::Char('r')),
        ctrl(KeyCode::Char('c')),
        ctrl(KeyCode::Char('k')),
        key(KeyCode::Up),
        ConsoleEvent::Resize {
            rows: 40,
            cols: 100,
        },
        ConsoleEvent::Scroll { up: true },
        ConsoleEvent::UserHasInteracted,
    ] {
        assert_eq!(classify(ev), None, "{ev:?} must be default-denied");
    }
}

/// A scripted console that replays canned events then yields `None`. Its
/// `draw_with` is a no-op so the refuse render never touches a real
/// backend.
struct ScriptedConsole {
    events: VecDeque<ConsoleEvent>,
}

impl ScriptedConsole {
    fn new(events: Vec<ConsoleEvent>) -> Self {
        Self {
            events: events.into(),
        }
    }
}

impl Console for ScriptedConsole {
    fn render(&mut self, _app: &App<'_>) -> Result<()> {
        Ok(())
    }
    fn poll_event<'a>(
        &'a mut self,
        timeout: std::time::Duration,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move { self.poll_event_blocking(timeout) })
    }
    fn poll_event_blocking(
        &mut self,
        _timeout: std::time::Duration,
    ) -> Result<Option<ConsoleEvent>> {
        Ok(self.events.pop_front())
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

fn dummy_sender() -> crate::sys::poller::LocalSender {
    crate::sys::poller::build().1
}

/// Reset the always-compiled seal/registry thread-locals + redirect the
/// on-disk mapper registry to a temp file so the refuse seal is hermetic.
fn fresh() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    registry::set_persist_path(dir.path().join("mappers"));
    guard::reset_latch();
    registry::reset();
    guard::test_seam::reset();
    dir
}

fn synthetic_cause() -> NmblError {
    NmblError::Signature {
        stage: "gen-kernel",
        detail: "synthetic refuse".to_string(),
    }
}

#[tokio::test]
async fn enter_yields_reboot_into_rescue() {
    let dir = fresh();
    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());
    let sender = dummy_sender();
    // Enter on the first poll ⇒ reboot now.
    let mut console = ScriptedConsole::new(vec![key(KeyCode::Enter)]);

    let action = run_refuse_screen(&cfg, &mut console, synthetic_cause(), &sender).await;
    assert!(
        matches!(action, TerminalAction::RebootIntoRescue { .. }),
        "Enter must yield the RebootIntoRescue terminus"
    );
    // The security teardown ran before the UI: the sentinel is on disk.
    assert!(crate::policy::sentinel_present(&cfg));
}

// Gated on `secure-boot` because only that build exposes the
// `refuse_countdown_seconds` knob this test zeroes to make the countdown
// expire instantly; without it a non-secure-boot build would wait the full
// 30 s default. The validation command is `cargo test --all-features`, so
// this always runs there.
#[cfg(feature = "secure-boot")]
#[tokio::test]
async fn timeout_yields_reboot_into_rescue() {
    let dir = fresh();
    let mut cfg = Config::recovery_default();
    // A 0-second countdown expires immediately with no input.
    cfg.secure_boot.refuse_countdown_seconds = 0;
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());
    let sender = dummy_sender();
    // No events: the empty queue yields None forever, so only the deadline
    // can end the loop.
    let mut console = ScriptedConsole::new(vec![]);

    let action = run_refuse_screen(&cfg, &mut console, synthetic_cause(), &sender).await;
    assert!(
        matches!(action, TerminalAction::RebootIntoRescue { .. }),
        "a timeout must yield the RebootIntoRescue terminus"
    );
}

#[test]
fn scrubbed_log_viewer_shows_only_the_banner_and_suppresses_ctrl_k() {
    use crate::ui::log_viewer::{LogViewer, LogViewerOutcome};
    let v = LogViewer::open_scrubbed(vec![super::SCRUBBED_BANNER.to_string()]);
    // Ctrl+K (toggle to live buffer) is suppressed so the pre-refuse
    // transcript can never be surfaced (FIX-41).
    let mut v = v;
    assert_eq!(
        v.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        LogViewerOutcome::Ignored,
        "Ctrl+K must be a no-op on a scrubbed viewer"
    );
    // Ctrl+L still closes it.
    assert_eq!(
        v.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
        LogViewerOutcome::Close
    );
}
