#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::reporter_impl::BootReporter;
use super::types::{ProgressSink, TickOutcome};
use crate::error::Result;
use crate::ui::app::{App, Screen, SessionInteraction};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind, LatchingConsole};

/// Test double for [`Console`]. Records every render call so we can
/// assert the reporter actually drives the backend on each API call.
///
/// `scripted_keys` lets a test feed a sequence of optional
/// `KeyEvent`s in to `poll_key` so [`ProgressSink::tick`]'s abort
/// poll can be exercised without a live tty.
struct MockConsole {
    renders: u32,
    last_phase: Option<String>,
    last_log_len: usize,
    last_spinner: u8,
    scripted_keys: Vec<Option<KeyEvent>>,
    key_cursor: usize,
}

impl MockConsole {
    fn new() -> Self {
        Self {
            renders: 0,
            last_phase: None,
            last_log_len: 0,
            last_spinner: 0,
            scripted_keys: Vec::new(),
            key_cursor: 0,
        }
    }

    fn with_keys(keys: Vec<Option<KeyEvent>>) -> Self {
        let mut c = Self::new();
        c.scripted_keys = keys;
        c
    }
}

impl Console for MockConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.renders = self.renders.saturating_add(1);
        if let Screen::BootStatus(data) = &app.screen {
            self.last_phase = Some(data.phase.clone().into_owned());
            self.last_log_len = data.log_lines.len();
            self.last_spinner = data.spinner_frame;
        }
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
        let v = self.scripted_keys.get(self.key_cursor).copied().flatten();
        self.key_cursor = self.key_cursor.saturating_add(1);
        Ok(v.map(ConsoleEvent::Key))
    }
    fn size(&self) -> (u16, u16) {
        (80, 24)
    }
    fn kind(&self) -> ConsoleKind {
        // No "mock" variant exists; tests don't branch on kind.
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
fn set_phase_renders_with_updated_label() {
    let mut console = MockConsole::new();
    let mut reporter = BootReporter::new(&mut console, "phase 1: init");
    reporter
        .set_phase("phase 2: modules")
        .expect("set_phase must succeed against a mock console");

    match &reporter.app().screen {
        Screen::BootStatus(data) => assert_eq!(&*data.phase, "phase 2: modules"),
        _ => panic!("reporter must keep the app on BootStatus"),
    }
    assert_eq!(console.renders, 1, "set_phase must call render once");
    assert_eq!(
        console.last_phase.as_deref(),
        Some("phase 2: modules"),
        "render must observe the new phase"
    );
}

#[test]
fn tick_advances_spinner_and_renders() {
    let mut console = MockConsole::new();
    let mut reporter = BootReporter::new(&mut console, "waiting");
    reporter.tick().expect("tick must succeed");
    reporter.tick().expect("tick must succeed");
    assert_eq!(console.renders, 2);
    assert_eq!(console.last_spinner, 2, "two ticks land on frame 2");
}

#[test]
fn refresh_log_pulls_snapshot_and_renders() {
    // We intentionally don't touch the global log ring here: the
    // existing `log::tests::*` suite serialises its pushes behind an
    // internal mutex, and parallel test runners pushing through
    // `log::push_ring` from this module would race the
    // `snapshot_caps_at_ring_capacity` test (a flake we observed
    // once). Asserting that refresh_log called render through the
    // backend is enough to pin the contract — the snapshot length
    // depends on whatever else is in the ring at test time, which
    // is fine because we only check that render fires.
    let mut console = MockConsole::new();
    let mut reporter = BootReporter::new(&mut console, "phase X");
    reporter.refresh_log().expect("refresh_log must succeed");
    assert_eq!(console.renders, 1, "refresh_log must call render once");
}

#[test]
fn progress_sink_tick_updates_phase_advances_spinner_and_renders() {
    // ProgressSink::tick is the one-call helper device-wait loops use:
    // it must set the phase string, advance the spinner, push a
    // frame to the backend, and report TickOutcome::Continue when
    // no operator key arrived — all in a single call.
    let mut console = MockConsole::new();
    let mut reporter = BootReporter::new(&mut console, "starting");
    let o1 = ProgressSink::tick(&mut reporter, "phase 3b: waiting for /dev/sda1 (5s / 30s)");
    let o2 = ProgressSink::tick(&mut reporter, "phase 3b: waiting for /dev/sda1 (6s / 30s)");
    assert_eq!(o1, TickOutcome::Continue, "no key → Continue");
    assert_eq!(o2, TickOutcome::Continue, "no key → Continue");
    assert_eq!(
        console.renders, 2,
        "each ProgressSink::tick must call render once"
    );
    assert_eq!(
        console.last_phase.as_deref(),
        Some("phase 3b: waiting for /dev/sda1 (6s / 30s)"),
        "render must observe the most recent phase string"
    );
    assert_eq!(console.last_spinner, 2, "two ticks land on frame 2");
}

#[test]
fn progress_sink_tick_returns_aborted_on_esc_key() {
    // Operator presses Esc on the boot-status screen while a wait
    // is in flight: tick must surface TickOutcome::Aborted so the
    // caller can convert it into an `OperatorAborted` error.
    let mut console =
        MockConsole::with_keys(vec![Some(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))]);
    let mut reporter = BootReporter::new(&mut console, "phase 3b: waiting");
    let outcome = ProgressSink::tick(&mut reporter, "phase 3b: waiting for /dev/sda1");
    assert_eq!(
        outcome,
        TickOutcome::Aborted,
        "Esc on boot-status must produce TickOutcome::Aborted"
    );
}

#[test]
fn reporter_keypress_during_log_window_latches_presence() {
    // Structural bug fix: a key pressed during the early boot-log window
    // (the reporter polls via the blocking `poll_key`) must mark the
    // operator present. The reporter holds no latch itself; it polls
    // through the central `LatchingConsole`, which sets the shared
    // SessionInteraction on the first input. A non-Esc key is dropped by
    // `poll_key` (so the wait continues) but the latch is still set, so
    // the selector/emergency screens later this session skip auto-boot.
    let latch = SessionInteraction::new();
    let inner = MockConsole::with_keys(vec![Some(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    ))]);
    let mut console = LatchingConsole::new(Box::new(inner), latch.clone());
    assert!(!latch.get(), "latch starts clear before any input");

    let mut reporter = BootReporter::new(&mut console, "phase 1: waiting");
    let outcome = ProgressSink::tick(&mut reporter, "phase 1: waiting for /dev");

    assert_eq!(
        outcome,
        TickOutcome::Continue,
        "a non-Esc key must not abort the wait"
    );
    assert!(
        latch.get(),
        "a key during the boot-log window must latch operator presence"
    );
}

#[test]
fn progress_sink_tick_ignores_non_esc_keys() {
    // Stray keypresses (Enter, letters, …) should not abort the
    // wait — only Esc carries the operator-abort semantics.
    let mut console = MockConsole::with_keys(vec![
        Some(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Some(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
    ]);
    let mut reporter = BootReporter::new(&mut console, "phase 3b: waiting");
    let o1 = ProgressSink::tick(&mut reporter, "p");
    let o2 = ProgressSink::tick(&mut reporter, "p");
    assert_eq!(o1, TickOutcome::Continue, "Enter must not abort");
    assert_eq!(o2, TickOutcome::Continue, "'q' must not abort");
}

#[test]
fn reporter_app_stays_on_boot_status_across_calls() {
    // Defence-in-depth: a future refactor that flips a setter to
    // mutate `screen` would break the BootStatus contract. Make
    // sure the reporter never leaves BootStatus on the happy path.
    let mut console = MockConsole::new();
    let mut reporter = BootReporter::new(&mut console, "p1");
    reporter.set_phase("p2").expect("set_phase");
    reporter.refresh_log().expect("refresh_log");
    reporter.tick().expect("tick");
    assert!(matches!(reporter.app().screen, Screen::BootStatus(_)));
}

// ---- Overlay-mode reporter --------------------------------------

#[test]
fn overlay_reporter_installs_status_modal_on_construction() {
    // BootReporter::overlay must write a ModalKind::Status into the
    // supplied App's `modal` field so the renderer paints the
    // overlay on top of the underlying screen. The screen variant
    // itself must not be changed.
    use crate::ui::app::ModalKind;
    let mut console = MockConsole::new();
    let mut app: App<'static> = App::new(&[]);
    // Park the app on a non-BootStatus screen.
    app.screen = Screen::List;
    let _reporter = BootReporter::overlay(&mut console, &mut app, "phase X");
    match &app.modal {
        Some(ModalKind::Status { phase, .. }) => {
            assert_eq!(phase, "phase X", "phase string must round-trip");
        }
        other => panic!("expected ModalKind::Status, got {other:?}"),
    }
    assert!(
        matches!(app.screen, Screen::List),
        "screen must be untouched"
    );
}

#[test]
fn overlay_reporter_set_phase_updates_modal_status() {
    // set_phase in overlay mode must update the modal's phase
    // string AND render through the console — same contract as
    // owned mode but on a different storage location.
    use crate::ui::app::ModalKind;
    let mut console = MockConsole::new();
    let mut app: App<'static> = App::new(&[]);
    {
        let mut reporter = BootReporter::overlay(&mut console, &mut app, "initial");
        reporter
            .set_phase("phase 2")
            .expect("set_phase must succeed");
    }
    match &app.modal {
        Some(ModalKind::Status { phase, .. }) => {
            assert_eq!(phase, "phase 2");
        }
        _ => panic!("modal Status must persist with updated phase"),
    }
    assert!(console.renders >= 1, "set_phase must render");
}

#[test]
fn overlay_reporter_progress_sink_tick_advances_modal_spinner() {
    // ProgressSink::tick in overlay mode must advance the modal's
    // spinner frame so the operator sees the status alive.
    use crate::ui::app::ModalKind;
    let mut console = MockConsole::new();
    let mut app: App<'static> = App::new(&[]);
    {
        let mut reporter = BootReporter::overlay(&mut console, &mut app, "waiting");
        let _ = ProgressSink::tick(&mut reporter, "still waiting");
        let _ = ProgressSink::tick(&mut reporter, "still waiting");
    }
    match &app.modal {
        Some(ModalKind::Status {
            spinner_frame,
            phase,
            ..
        }) => {
            assert_eq!(*spinner_frame, 2, "two ticks land on frame 2");
            assert_eq!(phase, "still waiting", "phase reflects last tick");
        }
        _ => panic!("modal Status must persist across ticks"),
    }
}
