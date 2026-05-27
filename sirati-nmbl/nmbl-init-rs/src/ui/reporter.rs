//! Boot-status reporter.
//!
//! Thin wrapper around `&mut dyn Console` plus the active [`App`]. Phase
//! code calls `reporter.set_phase(...)` and `reporter.tick()` without
//! needing to know about [`Console::render`] directly.
//!
//! ## Sibling-subagent contract
//!
//! The shape of [`BootReporter`] is the API that the devices/activation
//! spinner work plugs into. Phase code only ever sees this struct; the
//! underlying [`Console`] trait is an implementation detail.
//!
//! ```ignore
//! pub struct BootReporter<'c, 'a> {
//!     pub console: &'c mut dyn Console,
//!     pub app: App<'a>,
//! }
//!
//! impl<'c, 'a> BootReporter<'c, 'a> {
//!     pub fn set_phase(&mut self, phase: impl Into<Cow<'a, str>>) -> Result<()>;
//!     pub fn refresh_log(&mut self) -> Result<()>;
//!     pub fn tick(&mut self) -> Result<()>;
//! }
//! ```
//!
//! The screen the reporter mutates is [`crate::ui::app::Screen::BootStatus`],
//! which the renderer in [`crate::ui::view::render_boot_status`] already
//! handles for both backends.

use std::borrow::Cow;

use crate::error::Result;
use crate::log;
use crate::ui::app::App;
use crate::ui::console::Console;

/// Number of log lines pulled from the ring on every refresh.
///
/// Larger than any plausible visible panel height — the renderer clips
/// to what fits, so we err on the side of "have enough" rather than
/// peeking at the backend's grid size every frame.
const LOG_SNAPSHOT_LINES: usize = 64;

/// Boot status reporter — a thin wrapper around `&mut dyn Console` plus
/// the active [`App`] so phase code can report status without needing
/// to know the underlying render plumbing.
pub struct BootReporter<'c, 'a> {
    pub console: &'c mut dyn Console,
    pub app: App<'a>,
}

impl<'c, 'a> BootReporter<'c, 'a> {
    /// Build a reporter parked on the boot-status screen with the given
    /// initial phase label. Does NOT render — the caller decides when
    /// the first frame is meaningful (typically right after construction
    /// via [`Self::set_phase`] or [`Self::refresh_log`]).
    pub fn new(console: &'c mut dyn Console, phase: impl Into<Cow<'a, str>>) -> Self {
        let app = App::boot_status(phase);
        Self { console, app }
    }

    /// Replace the phase label, refresh the log snapshot, and render.
    ///
    /// This is the canonical "phase transition" call: in one go we
    /// update everything the operator sees so a slow phase doesn't
    /// leave a stale label on screen.
    pub fn set_phase(&mut self, phase: impl Into<Cow<'a, str>>) -> Result<()> {
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use std::time::Duration;

    use crossterm::event::KeyEvent;

    use super::*;
    use crate::ui::app::Screen;
    use crate::ui::console::ConsoleKind;

    /// Test double for [`Console`]. Records every render call so we can
    /// assert the reporter actually drives the backend on each API call.
    struct MockConsole {
        renders: u32,
        last_phase: Option<String>,
        last_log_len: usize,
        last_spinner: u8,
    }

    impl MockConsole {
        fn new() -> Self {
            Self {
                renders: 0,
                last_phase: None,
                last_log_len: 0,
                last_spinner: 0,
            }
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
            Ok(None)
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            // No "mock" variant exists; tests don't branch on kind.
            ConsoleKind::Tty
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
