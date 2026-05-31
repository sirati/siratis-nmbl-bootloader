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
/// Drives a scripted sequence of events on `poll_event()` and counts
/// renders. Most tests script keys via [`TestConsole::new`]; tests that
/// exercise the central layer's `UserHasInteracted` notice (which the
/// loop now keys its countdown-cancel off of) script full events via
/// [`TestConsole::with_events`].
pub(super) struct TestConsole {
    pub(super) events: Vec<Option<ConsoleEvent>>,
    pub(super) cursor: usize,
    pub(super) renders: u32,
}

impl TestConsole {
    /// Script a sequence of optional KEY events. `None` models an idle
    /// poll slice.
    pub(super) fn new(keys: Vec<Option<KeyEvent>>) -> Self {
        let events = keys.into_iter().map(|k| k.map(ConsoleEvent::Key)).collect();
        Self {
            events,
            cursor: 0,
            renders: 0,
        }
    }

    /// Script a sequence of optional full console events — used to inject
    /// the central layer's one-shot [`ConsoleEvent::UserHasInteracted`]
    /// ahead of the key it precedes, exactly as `LatchingConsole` would.
    pub(super) fn with_events(events: Vec<Option<ConsoleEvent>>) -> Self {
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
        Ok(v)
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
fn build_message_prepends_likely_cause_for_device_timeout() {
    // The reported failure mode: yanked boot device → DeviceTimeout.
    // The error screen must lead with a plain-language "Likely cause"
    // line naming the unplug/seating problem, then still carry the raw
    // chain (device path + the timeout text) below it.
    use crate::error::NmblError;
    use std::path::PathBuf;
    let err = NmblError::DeviceTimeout {
        device: PathBuf::from("/dev/disk/by-uuid/abc-123"),
        timeout_ms: 30_000,
    };
    let msg = build_message(&err);
    assert!(
        msg.contains("Likely cause:"),
        "DeviceTimeout must produce a likely-cause hint: {msg}"
    );
    assert!(
        msg.contains("unplugged") || msg.contains("not seated"),
        "hint must name the unplug/seating cause: {msg}"
    );
    // The raw chain (device path) must still be present below the hint
    // so the operator keeps the actionable detail.
    assert!(
        msg.contains("/dev/disk/by-uuid/abc-123"),
        "the threaded-through device path must remain in the message: {msg}"
    );
    // Hint comes first, raw chain after.
    let hint_pos = msg.find("Likely cause:").expect("hint present");
    let chain_pos = msg.find("Boot failed").expect("chain present");
    assert!(
        hint_pos < chain_pos,
        "hint must precede the raw chain: {msg}"
    );
}

#[test]
fn likely_cause_unwraps_wrapped_device_timeout() {
    // DeviceTimeout nested under an Activation wrapper (the real shape
    // when a LUKS-produced dm node never appears) must still be matched
    // by walking the source() chain.
    use crate::error::NmblError;
    use std::path::PathBuf;
    let wrapped = NmblError::Activation {
        kind: "luks-password".to_string(),
        source: Box::new(NmblError::DeviceTimeout {
            device: PathBuf::from("/dev/mapper/cryptroot"),
            timeout_ms: 15_000,
        }),
    };
    assert!(
        super::likely_cause(&wrapped).is_some(),
        "a DeviceTimeout wrapped in Activation must still yield a hint"
    );
}

#[test]
fn emergency_screen_footer_advertises_ctrl_l_logs() {
    // The error screen must tell the operator the log viewer exists —
    // a "Ctrl+L" hint in the footer is the recoverability affordance
    // the report asked for. Render to a TestBackend and grep the frame.
    use crate::ui::app::{EmergencyChoice, EmergencyItem};
    use crate::ui::view::{EmergencyScreenData, render_emergency};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let items = vec![EmergencyItem {
        label: "Reboot",
        choice: EmergencyChoice::Reboot,
    }];
    let data = EmergencyScreenData {
        message: "Likely cause: the boot device may have been unplugged.\n\nBoot failed.",
        items: &items,
        selected_index: 0,
        countdown_remaining_secs: None,
    };
    let mut term = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
    term.draw(|f| render_emergency(f, &data)).expect("draw");
    let buf = term.backend().buffer();
    let text: String = (0..buf.area.height)
        .flat_map(|y| {
            (0..buf.area.width).filter_map(move |x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
        })
        .collect();
    assert!(
        text.contains("Ctrl+L"),
        "emergency footer must advertise the Ctrl+L log hotkey:\n{text}"
    );
    assert!(
        text.contains("Likely cause"),
        "the threaded-through likely-cause line must render:\n{text}"
    );
}

#[test]
fn likely_cause_none_for_unrecognised_error() {
    // An error with no specific operator-actionable pattern must NOT
    // fabricate a hint — the raw chain stands alone.
    use crate::error::NmblError;
    let err = NmblError::Tui {
        source: std::io::Error::other("some render glitch"),
    };
    assert!(
        super::likely_cause(&err).is_none(),
        "unrecognised errors must not produce a misleading hint"
    );
    let msg = build_message(&err);
    assert!(
        !msg.contains("Likely cause:"),
        "no hint line when the cause is unknown: {msg}"
    );
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
