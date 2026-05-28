//! Sentinel [`Console`] backend that produces no visible output.
//!
//! Used by the orchestrator (`main.rs`) for the pre-console phases
//! (phase 1 mount-pseudo-filesystems and phase 2a load-early-modules).
//! At that point neither the splash framebuffer nor the raw-mode tty
//! is up yet: phase 2a is what brings up the DRM card the splash needs,
//! so we can't open the real console before it runs. But the rest of
//! the boot pipeline is shaped around `BootReporter<&mut dyn Console>`,
//! so phase code expects *some* `Console` to push frames into.
//!
//! `NoopConsole` is that sentinel. `render` is a no-op (so any phase
//! that calls `reporter.set_phase` or `reporter.tick` just discards the
//! frame) and `poll_key` returns `Ok(None)` immediately so any caller
//! that tries to drive an interactive flow against it terminates
//! gracefully rather than blocking forever.
//!
//! Log lines still reach the kernel ring via `nmbl_info!` /
//! `nmbl_warn!` — those go through `log::push_ring` independently of
//! the Console plumbing, so a downstream operator viewing kmsg sees the
//! early-phase narration even though the screen is dark.
//!
//! After phase 2a completes, the orchestrator opens the real console
//! and constructs a fresh `BootReporter` around it; the `NoopConsole`
//! is dropped at that point.

use std::time::Duration;

use crossterm::event::KeyEvent;

use crate::error::Result;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleKind};

/// Drop-in [`Console`] that discards everything. See the module docs
/// for the precise role.
pub struct NoopConsole {
    /// Reported terminal size. 80x24 is the lowest common denominator
    /// for both serial consoles and bare-VGA framebuffers; phase code
    /// that asks for `console.size()` will get a sane number rather
    /// than (0, 0) which could trip layout code.
    size: (u16, u16),
}

impl NoopConsole {
    /// Build a sentinel console with the default 80x24 grid. The size
    /// is reported back via [`Console::size`] for any caller that
    /// branches on it; phase 1 and phase 2a do not exercise that path
    /// but a future phase relocated to the pre-console window would.
    #[must_use]
    pub fn new() -> Self {
        Self { size: (80, 24) }
    }
}

impl Default for NoopConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl Console for NoopConsole {
    /// Discard the frame. The boot still progresses; the operator
    /// just sees nothing until phase 2a finishes and the real console
    /// is opened.
    fn render(&mut self, _app: &App<'_>) -> Result<()> {
        Ok(())
    }

    /// No input source is wired up before the real console comes up.
    /// Any caller that tries to poll us gets an immediate `None` so
    /// blocking flows do not deadlock against the sentinel.
    fn poll_key(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
        Ok(None)
    }

    fn size(&self) -> (u16, u16) {
        self.size
    }

    fn kind(&self) -> ConsoleKind {
        // `NoopConsole` is a sentinel, not a real backend; phase code
        // that branches on `kind()` should treat the pre-console window
        // as "tty-equivalent" (line-oriented, no DRM). We surface
        // `ConsoleKind::Tty` so existing branches behave as if the
        // splash hadn't kicked in yet.
        ConsoleKind::Tty
    }

    /// Discard the ad-hoc closure-driven frame, just like [`render`].
    fn draw_with(&mut self, _body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        Ok(())
    }

    /// The sentinel owns no display, so suspending is a no-op. We
    /// implement the trait method to keep the call sites unconditional;
    /// the suspend/resume contract is "release the display if you have
    /// one", and we don't.
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }

    /// Counterpart to [`suspend`]; nothing to re-acquire.
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::ui::app::App;

    #[test]
    fn noop_render_is_a_no_op_and_returns_ok() {
        let mut console = NoopConsole::new();
        let empty: [crate::generations::Generation; 0] = [];
        let app = App::new(&empty);
        // The point of NoopConsole is that calling render against it
        // must never fail and must produce no visible side-effect.
        console
            .render(&app)
            .expect("NoopConsole::render must always succeed");
        console
            .render(&app)
            .expect("NoopConsole::render must be re-callable");
    }

    #[test]
    fn noop_poll_key_returns_none_immediately() {
        let mut console = NoopConsole::new();
        // Any poll, with any timeout, must return Ok(None) without
        // blocking — phase code that drives a render-poll loop against
        // the sentinel will otherwise deadlock.
        assert!(
            console
                .poll_key(Duration::from_millis(0))
                .expect("poll_key must succeed")
                .is_none(),
            "NoopConsole::poll_key must yield no key",
        );
        assert!(
            console
                .poll_key(Duration::from_secs(60))
                .expect("poll_key must succeed even with a long timeout")
                .is_none(),
            "NoopConsole::poll_key must not honour the timeout",
        );
    }

    #[test]
    fn noop_size_is_default_80x24() {
        let console = NoopConsole::new();
        assert_eq!(console.size(), (80, 24));
    }

    #[test]
    fn noop_kind_is_tty_equivalent() {
        // Pre-console phases that branch on `kind()` should see a
        // tty-equivalent value — DRM bring-up hasn't happened yet.
        let console = NoopConsole::new();
        assert_eq!(console.kind(), ConsoleKind::Tty);
    }

    #[test]
    fn noop_default_matches_new() {
        let from_default = NoopConsole::default();
        let from_new = NoopConsole::new();
        assert_eq!(from_default.size(), from_new.size());
        assert_eq!(from_default.kind(), from_new.kind());
    }

    #[test]
    fn noop_draw_with_discards_closure_and_returns_ok() {
        // `draw_with` is the closure-driven counterpart to `render`;
        // on the sentinel both must succeed without observable effect.
        let mut console = NoopConsole::new();
        let mut called = 0u32;
        console
            .draw_with(&mut |_f| {
                called = called.saturating_add(1);
            })
            .expect("draw_with must always succeed on NoopConsole");
        // The contract is "discard the frame" — the closure may run
        // zero or more times, but if it does run the side-effect must
        // not panic. We mainly assert the Result is Ok above.
        let _ = called;
    }

    #[test]
    fn noop_satisfies_console_trait_for_dyn_dispatch() {
        // Pin the dyn-dispatch shape — `BootReporter::new` takes
        // `&mut dyn Console`, so a coercion failure here breaks the
        // orchestrator's phase 1 / 2a construction.
        let mut console = NoopConsole::new();
        let _coerced: &mut dyn Console = &mut console;
    }
}
