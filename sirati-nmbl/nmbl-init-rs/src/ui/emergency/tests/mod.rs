//! Tests for the emergency-screen module.
//!
//! Loop-behaviour tests live in [`loop_tests`]; this file holds shared
//! utilities and the builder-focused tests.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]

mod loop_tests;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::error::Result;
use crate::ui::app::{App, EmergencyChoice, SessionInteraction};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};

use super::{build_emergency_app, build_message, default_items};

pub(super) fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Drive an async future to completion on a throwaway current-thread
/// runtime so the synchronous unit tests can exercise the now-async
/// emergency loop. The scripted console resolves `poll_event`
/// instantly and the loop's `select!` is `biased` (input arm first),
/// so the timer-sleep arm never wins and no real wall-clock time
/// elapses — the tests stay fast and deterministic.
pub(super) fn block<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}

/// Build an emergency `App` on a fresh (un-interacted) session — the
/// unattended-boot case where the countdown is expected to arm.
pub(super) fn fresh_emergency_app(message: &str) -> App<'static> {
    let session = SessionInteraction::new();
    build_emergency_app(message, &default_items(), &session)
}

/// In-process [`Console`] for unit-testing the emergency loop.
/// Drives a scripted sequence of key events on `poll_event()` and
/// counts renders.
pub(super) struct TestConsole {
    pub(super) events: Vec<Option<KeyEvent>>,
    pub(super) cursor: usize,
    pub(super) renders: u32,
}

impl TestConsole {
    pub(super) fn new(events: Vec<Option<KeyEvent>>) -> Self {
        Self {
            events,
            cursor: 0,
            renders: 0,
        }
    }
}

impl Console for TestConsole {
    fn render(&mut self, _app: &App<'_>) -> Result<()> {
        self.renders = self.renders.saturating_add(1);
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

#[test]
fn build_message_includes_error_chain_lines() {
    use crate::error::NmblError;
    let err = NmblError::Io {
        source: std::io::Error::other("disk on fire"),
        context: "mounting /tmp".to_string(),
    };
    let msg = build_message(&err);
    assert!(msg.contains("mounting /tmp"), "expected context: {msg}");
    assert!(msg.contains("disk on fire"), "expected source: {msg}");
}

#[test]
fn resolve_emergency_timeout_uses_default_when_absent() {
    let config = crate::config::Config::recovery_default();
    assert_eq!(
        super::resolve_emergency_timeout(&config),
        super::EMERGENCY_TIMEOUT
    );
}

#[test]
fn resolve_emergency_timeout_honours_override() {
    let mut config = crate::config::Config::recovery_default();
    config.general.emergency_timeout_secs = Some(1);
    assert_eq!(
        super::resolve_emergency_timeout(&config),
        Duration::from_secs(1)
    );
}

#[test]
fn default_items_first_is_reboot() {
    // The whole "timeout defaults to Reboot" contract hangs on
    // Reboot being the first item; if a future refactor flips the
    // order, the timeout test still passes but production gets
    // surprising behaviour. Pin the contract here.
    let items = default_items();
    assert_eq!(items[0].choice, EmergencyChoice::Reboot);
    // With `pretty-shell` the Pretty Shell entry sits at index 1
    // and the Raw Shell entry at index 2; without the feature the
    // Raw Shell entry falls back to index 1.
    #[cfg(feature = "pretty-shell")]
    {
        assert_eq!(items[1].choice, EmergencyChoice::PrettyShell);
        assert_eq!(items[2].choice, EmergencyChoice::RawShell);
    }
    #[cfg(not(feature = "pretty-shell"))]
    {
        assert_eq!(items[1].choice, EmergencyChoice::RawShell);
    }
}

#[test]
fn default_items_includes_retry_and_verify_in_order() {
    // The dispatcher in `shell.rs` matches on these variants by
    // name; the order pinned here is what the operator actually
    // sees on the picker. Reboot comes first (muscle-memory + the
    // 30s timeout default), then Pretty Shell (feature-gated, the
    // preferred recovery shell when available), then Raw Shell,
    // then RetryBoot, then VerifyKexecReadiness — most-destructive
    // to least-destructive, so a stray Enter on the default
    // doesn't kick off an in-process retry the operator didn't
    // want.
    let items = default_items();
    let choices: Vec<EmergencyChoice> = items.iter().map(|it| it.choice).collect();

    let mut expected: Vec<EmergencyChoice> = vec![EmergencyChoice::Reboot];
    #[cfg(feature = "pretty-shell")]
    expected.push(EmergencyChoice::PrettyShell);
    expected.push(EmergencyChoice::RawShell);
    expected.push(EmergencyChoice::RetryBoot);
    expected.push(EmergencyChoice::VerifyKexecReadiness);

    assert_eq!(choices, expected, "default_items order has drifted");
}

#[test]
fn default_items_labels_match_spec() {
    // The labels appear verbatim in the emergency picker; pin
    // them so a relabel doesn't slip past review (the empirical
    // verification step greps for these strings).
    let items = default_items();
    let labels: Vec<&str> = items.iter().map(|it| it.label).collect();
    assert!(
        labels.contains(&"Retry boot from config"),
        "missing 'Retry boot from config' in {labels:?}"
    );
    assert!(
        labels.contains(&"Verify kexec readiness"),
        "missing 'Verify kexec readiness' in {labels:?}"
    );
}
