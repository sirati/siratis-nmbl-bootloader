//! Boot-status reporter.
//!
//! Thin wrapper around `&mut dyn Console` plus the active [`App`]. Phase
//! code calls `reporter.set_phase(...)` and `reporter.tick()` without
//! needing to know about [`Console::render`] directly.
//!
//! ## Spinner plumbing for blocking waits
//!
//! Long-running device-wait and activation-wait loops should not look
//! frozen on the splash. The [`ProgressSink`] trait gives those loops a
//! one-method handle they can call every poll iteration to:
//!
//! * advance the spinner one frame,
//! * replace the phase label with a "waiting for X (Ns / Ms)" string,
//! * pull the latest log snapshot from the global ring,
//! * push a fresh frame to the underlying [`Console`].
//!
//! [`BootReporter`] implements [`ProgressSink`] so the same handle that
//! drives phase transitions also drives the spinner. Tests use a counting
//! mock that doesn't open a console.
//!
//! ## Sibling-subagent contract
//!
//! The screen the reporter mutates is [`crate::ui::app::Screen::BootStatus`],
//! which the renderer in [`crate::ui::view::render_boot_status`] already
//! handles for both backends.

use std::borrow::Cow;
use std::time::Duration;

use crossterm::event::KeyCode;

use crate::error::Result;
use crate::log;
use crate::ui::app::App;
use crate::ui::console::Console;

/// Slice we wait on input per tick. Matches the 100 ms poll cadence of
/// the device-wait loop so the tick stays cheap and the operator's
/// Esc keypress aborts within one iteration.
const TICK_POLL_SLICE: Duration = Duration::from_millis(100);

/// Outcome of a single [`ProgressSink::tick`] call.
///
/// `Aborted` lets a blocking wait loop bail out cleanly when the
/// operator presses Esc on the boot-status screen; the caller is
/// expected to surface this as [`crate::error::NmblError::OperatorAborted`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickOutcome {
    /// No operator input demanded an abort; keep polling.
    Continue,
    /// Operator pressed Esc; the wait loop should stop and propagate
    /// an [`crate::error::NmblError::OperatorAborted`] up to its caller.
    Aborted,
}

/// Animated progress sink for blocking wait loops.
///
/// Implementors advance whatever spinner / status the operator sees and
/// re-render the underlying surface. Phase code passes
/// `Option<&mut dyn ProgressSink>` down through the wait helpers so tests
/// and headless contexts can skip the UI cost; production wires a
/// [`BootReporter`] through.
///
/// Implementations should be cheap enough to call every ~100 ms (the
/// existing poll cadence in `devices::wait_for`); skipping a render when
/// the phase string is unchanged is allowed but not required.
pub trait ProgressSink {
    /// Update the visible phase label, advance the spinner one frame,
    /// refresh the log snapshot, push a frame to the backend, and poll
    /// the backend for an abort key (Esc).
    ///
    /// The implementation is expected to swallow non-fatal render errors
    /// (e.g. transient DRM hiccups) rather than abort the wait — the
    /// boot must not fail because the spinner couldn't repaint. The
    /// only way `tick` should return [`TickOutcome::Aborted`] is when
    /// the operator pressed Esc on the boot-status screen.
    fn tick(&mut self, phase: &str) -> TickOutcome;
}

/// Number of log lines pulled from the ring on every refresh.
///
/// Larger than any plausible visible panel height — the renderer clips
/// to what fits, so we err on the side of "have enough" rather than
/// peeking at the backend's grid size every frame.
const LOG_SNAPSHOT_LINES: usize = 64;

/// Boot status reporter — a thin wrapper around `&mut dyn Console` plus
/// the active [`App`] so phase code can report status without needing
/// to know the underlying render plumbing.
///
/// The inner [`App`] is parameterised over `'static`: every production
/// caller passes a `&'static str` literal as the initial phase, and
/// [`Self::set_phase`] / [`ProgressSink::tick`] always promote into an
/// owned `Cow`, so a second lifetime would be dead weight.
pub struct BootReporter<'c> {
    pub console: &'c mut dyn Console,
    pub app: App<'static>,
}

impl<'c> BootReporter<'c> {
    /// Build a reporter parked on the boot-status screen with the given
    /// initial phase label. Does NOT render — the caller decides when
    /// the first frame is meaningful (typically right after construction
    /// via [`Self::set_phase`] or [`Self::refresh_log`]).
    pub fn new(console: &'c mut dyn Console, phase: impl Into<Cow<'static, str>>) -> Self {
        let app = App::boot_status(phase);
        Self { console, app }
    }

    /// Replace the phase label, refresh the log snapshot, and render.
    ///
    /// This is the canonical "phase transition" call: in one go we
    /// update everything the operator sees so a slow phase doesn't
    /// leave a stale label on screen.
    pub fn set_phase(&mut self, phase: impl Into<Cow<'static, str>>) -> Result<()> {
        self.app.set_boot_phase(phase);
        self.app.set_boot_log_lines(log::snapshot(LOG_SNAPSHOT_LINES));
        self.console.render(&self.app)
    }

    /// Refresh the log panel from the global ring and re-render.
    ///
    /// Cheap enough to call on every `tick()`; the ring is a small
    /// `VecDeque<String>` clone of the most recent lines.
    pub fn refresh_log(&mut self) -> Result<()> {
        self.app.set_boot_log_lines(log::snapshot(LOG_SNAPSHOT_LINES));
        self.console.render(&self.app)
    }

    /// Advance the spinner one frame and render.
    ///
    /// Designed to be called inside device-wait spin loops by sibling
    /// subagent work so the operator sees the boot is alive even when
    /// no phase transition is firing.
    pub fn tick(&mut self) -> Result<()> {
        self.app.tick_boot_spinner();
        self.console.render(&self.app)
    }
}

impl ProgressSink for BootReporter<'_> {
    /// Update the phase label, refresh the log snapshot, advance the
    /// spinner, render, and poll the backend for an abort key.
    ///
    /// Errors from the backend are deliberately dropped: a flaky DRM
    /// ioctl shouldn't abort a 30 s device wait — the next iteration
    /// will retry. Phase code still sees a fatal error if the
    /// underlying wait itself fails.
    ///
    /// Returns [`TickOutcome::Aborted`] when the operator presses Esc
    /// on the boot-status screen — the caller (`devices::wait_for`,
    /// `activation` waits, …) surfaces this as
    /// [`crate::error::NmblError::OperatorAborted`] so the emergency
    /// menu can re-appear with the operator's explicit "abort"
    /// context.
    fn tick(&mut self, phase: &str) -> TickOutcome {
        // Promote to an owned Cow so the borrow on `phase` doesn't
        // escape this call (BootStatusData::phase is `Cow<'static, str>`).
        self.app
            .set_boot_phase(Cow::<'static, str>::Owned(phase.to_string()));
        self.app.set_boot_log_lines(log::snapshot(LOG_SNAPSHOT_LINES));
        self.app.tick_boot_spinner();
        let _ = self.console.render(&self.app);

        // Poll for a single key with a short timeout so the wait stays
        // responsive without adding latency beyond the existing 100 ms
        // POLL_INTERVAL in `devices::wait_for`. A failed poll (transient
        // DRM / tty error) is treated as "no key" — same swallowing
        // policy as the render above.
        match self.console.poll_key(TICK_POLL_SLICE) {
            Ok(Some(key)) if key.code == KeyCode::Esc => TickOutcome::Aborted,
            _ => TickOutcome::Continue,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use std::time::Duration;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::ui::app::Screen;
    use crate::ui::console::ConsoleKind;

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
        fn poll_key(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
            let v = self.scripted_keys.get(self.key_cursor).copied().flatten();
            self.key_cursor = self.key_cursor.saturating_add(1);
            Ok(v)
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            // No "mock" variant exists; tests don't branch on kind.
            ConsoleKind::Tty
        }
        fn draw_with(
            &mut self,
            _body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
        ) -> Result<()> {
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

        match &reporter.app.screen {
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
        let mut console = MockConsole::with_keys(vec![Some(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        ))]);
        let mut reporter = BootReporter::new(&mut console, "phase 3b: waiting");
        let outcome = ProgressSink::tick(&mut reporter, "phase 3b: waiting for /dev/sda1");
        assert_eq!(
            outcome,
            TickOutcome::Aborted,
            "Esc on boot-status must produce TickOutcome::Aborted"
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
        assert!(matches!(reporter.app.screen, Screen::BootStatus(_)));
    }
}
