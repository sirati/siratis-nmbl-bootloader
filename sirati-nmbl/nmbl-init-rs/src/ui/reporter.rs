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
use crate::ui::app::{App, ModalKind};
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

/// Where the reporter writes its phase / log / spinner state.
///
/// Two modes:
/// - `Owned` carries its own `App<'static>` parked on
///   [`crate::ui::app::Screen::BootStatus`]. Used in early boot before
///   any selector / menu screen exists — there is nothing to overlay,
///   so the full-screen boot-status view is appropriate.
/// - `Overlay` borrows an externally-supplied `&mut App` and writes to
///   its [`crate::ui::app::App::modal`] field as a
///   [`ModalKind::Status`] overlay. Used by the emergency-action
///   subflows (Retry boot, Verify kexec readiness) so the underlying
///   emergency menu stays visible behind the progress dialog.
enum ReporterApp<'a> {
    // App<'static> is ~296 bytes (Screen, modal, Vecs, options); the
    // Overlay variant is an 8-byte reference. Boxing the owned App
    // keeps both variants the same size — silences clippy's
    // `large_enum_variant` lint without adding indirection in the
    // hot path (the reporter is constructed once per phase, not per
    // tick).
    Owned(Box<App<'static>>),
    // The inner `'static` bound on `App<'static>` matches the fact
    // that the only App ever used in overlay mode is the emergency-
    // menu App built by `build_emergency_app(&[])` — its `generations`
    // slice is empty (`&[]`, `'static`). Two separate lifetimes would
    // require a HRTB on every BootReporter signature, which we avoid
    // by pinning the inner App parameter here.
    Overlay(&'a mut App<'static>),
}

impl ReporterApp<'_> {
    fn as_ref(&self) -> &App<'_> {
        match self {
            ReporterApp::Owned(a) => a,
            ReporterApp::Overlay(a) => a,
        }
    }
}

/// Boot status reporter — a thin wrapper around `&mut dyn Console` plus
/// the active [`App`] so phase code can report status without needing
/// to know the underlying render plumbing.
///
/// In `Owned` mode the reporter parks its own [`App`] on
/// [`crate::ui::app::Screen::BootStatus`]; in `Overlay` mode it pumps
/// the supplied App's [`crate::ui::app::App::modal`] field with a
/// [`ModalKind::Status`] so the underlying menu stays visible behind.
pub struct BootReporter<'c, 'a> {
    pub console: &'c mut dyn Console,
    app: ReporterApp<'a>,
}

impl<'c> BootReporter<'c, 'static> {
    /// Build an owned-mode reporter parked on the boot-status screen
    /// with the given initial phase label. Does NOT render — the caller
    /// decides when the first frame is meaningful (typically right after
    /// construction via [`Self::set_phase`] or [`Self::refresh_log`]).
    ///
    /// Used by the early-boot phases (phase 1, 2a, 2b, 4) where no
    /// underlying menu exists yet.
    pub fn new(console: &'c mut dyn Console, phase: impl Into<Cow<'static, str>>) -> Self {
        let app = App::boot_status(phase);
        Self {
            console,
            app: ReporterApp::Owned(Box::new(app)),
        }
    }
}

impl<'c, 'a> BootReporter<'c, 'a> {
    /// Borrow the App the reporter is currently driving. Lets test
    /// code inspect the latest phase / screen state without taking a
    /// dependency on the internal `ReporterApp` enum.
    pub fn app(&self) -> &App<'_> {
        self.app.as_ref()
    }

    /// Build an overlay-mode reporter that writes its status to the
    /// supplied App's `modal` field. The underlying screen (typically
    /// the emergency menu) keeps rendering behind so the operator can
    /// see "where they were"; closing the reporter (drop) clears the
    /// modal automatically.
    ///
    /// Used by emergency-action subflows so the menu stays visible.
    pub fn overlay(
        console: &'c mut dyn Console,
        app: &'a mut App<'static>,
        phase: impl Into<Cow<'static, str>>,
    ) -> Self {
        app.modal = Some(ModalKind::Status {
            phase: phase.into().into_owned(),
            log_lines: Vec::new(),
            spinner_frame: 0,
        });
        Self {
            console,
            app: ReporterApp::Overlay(app),
        }
    }

    /// Replace the phase label, refresh the log snapshot, and render.
    ///
    /// This is the canonical "phase transition" call: in one go we
    /// update everything the operator sees so a slow phase doesn't
    /// leave a stale label on screen.
    pub fn set_phase(&mut self, phase: impl Into<Cow<'static, str>>) -> Result<()> {
        let snap = log::snapshot(LOG_SNAPSHOT_LINES);
        write_phase(&mut self.app, phase, Some(snap), false);
        self.console.render(self.app.as_ref())
    }

    /// Refresh the log panel from the global ring and re-render.
    ///
    /// Cheap enough to call on every `tick()`; the ring is a small
    /// `VecDeque<String>` clone of the most recent lines. Does NOT
    /// change the phase string — both modes pull the prior phase
    /// through unmodified.
    pub fn refresh_log(&mut self) -> Result<()> {
        let snap = log::snapshot(LOG_SNAPSHOT_LINES);
        match &mut self.app {
            ReporterApp::Owned(a) => a.set_boot_log_lines(snap),
            ReporterApp::Overlay(a) => {
                if let Some(ModalKind::Status { log_lines, .. }) = &mut a.modal {
                    *log_lines = snap;
                }
            }
        }
        self.console.render(self.app.as_ref())
    }

    /// Advance the spinner one frame and render.
    ///
    /// Designed to be called inside device-wait spin loops by sibling
    /// subagent work so the operator sees the boot is alive even when
    /// no phase transition is firing.
    pub fn tick(&mut self) -> Result<()> {
        tick_spinner(&mut self.app);
        self.console.render(self.app.as_ref())
    }
}

// Intentionally no `Drop` impl: a non-trivial Drop holds the
// `&mut Console` borrow until end-of-scope (NLL can't release it
// early), which breaks every existing test of the form
// `let reporter = …; … ; assert_eq!(console.field, …)`. Overlay
// callers (`crate::ui::emergency_actions`) instead drop the reporter
// in a `{}` block; the next `BootReporter::overlay` overwrites
// `app.modal`, and `drop_to_emergency` clears `app.modal = None`
// at the top of every loop iteration so a stale overlay never
// reaches the picker.

/// Apply a phase / log-snapshot / spinner update to whichever App the
/// reporter is driving. In owned mode this mutates `Screen::BootStatus`;
/// in overlay mode it mutates the `ModalKind::Status` in `app.modal`.
fn write_phase(
    app: &mut ReporterApp<'_>,
    phase: impl Into<Cow<'static, str>>,
    log_lines: Option<Vec<String>>,
    spinner_advance: bool,
) {
    let phase = phase.into();
    match app {
        ReporterApp::Owned(a) => {
            a.set_boot_phase(phase);
            if let Some(lines) = log_lines {
                a.set_boot_log_lines(lines);
            }
            if spinner_advance {
                a.tick_boot_spinner();
            }
        }
        ReporterApp::Overlay(a) => {
            if let Some(ModalKind::Status {
                phase: p,
                log_lines: l,
                spinner_frame,
            }) = &mut a.modal
            {
                *p = phase.into_owned();
                if let Some(lines) = log_lines {
                    *l = lines;
                }
                if spinner_advance {
                    *spinner_frame =
                        spinner_frame.wrapping_add(1) % crate::ui::app::SPINNER_FRAMES;
                }
            } else {
                // Defence-in-depth: a caller that swapped the modal out
                // from under us re-installs a fresh Status so the next
                // tick still paints.
                a.modal = Some(ModalKind::Status {
                    phase: phase.into_owned(),
                    log_lines: log_lines.unwrap_or_default(),
                    spinner_frame: 0,
                });
            }
        }
    }
}

fn tick_spinner(app: &mut ReporterApp<'_>) {
    match app {
        ReporterApp::Owned(a) => a.tick_boot_spinner(),
        ReporterApp::Overlay(a) => {
            if let Some(ModalKind::Status { spinner_frame, .. }) = &mut a.modal {
                *spinner_frame =
                    spinner_frame.wrapping_add(1) % crate::ui::app::SPINNER_FRAMES;
            }
        }
    }
}

impl ProgressSink for BootReporter<'_, '_> {
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
        let snap = log::snapshot(LOG_SNAPSHOT_LINES);
        write_phase(
            &mut self.app,
            Cow::<'static, str>::Owned(phase.to_string()),
            Some(snap),
            true,
        );
        let _ = self.console.render(self.app.as_ref());

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
        assert!(matches!(app.screen, Screen::List), "screen must be untouched");
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
            reporter.set_phase("phase 2").expect("set_phase must succeed");
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
            Some(ModalKind::Status { spinner_frame, phase, .. }) => {
                assert_eq!(*spinner_frame, 2, "two ticks land on frame 2");
                assert_eq!(phase, "still waiting", "phase reflects last tick");
            }
            _ => panic!("modal Status must persist across ticks"),
        }
    }
}
