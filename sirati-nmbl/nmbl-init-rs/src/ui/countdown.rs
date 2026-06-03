//! Reusable console-driven countdown widget with a CALLER-OWNED input
//! policy.
//!
//! [`CountdownScreen`] owns the wall-clock deadline, the one-second tick
//! cadence, and the per-tick render callback; it does **not** decide what
//! any given key means. On every input event the widget calls a
//! caller-supplied classifier ([`FnMut(ConsoleEvent) -> CountdownAction`])
//! and acts on the returned [`CountdownAction`]. This is the one behaviour
//! the older `ui/selector.rs` countdown could not express: it hard-coded
//! "cancel on the central `UserHasInteracted` notice", so a future screen
//! (the secure-boot refuse screen) that needs "Enter reboots, Ctrl+L opens
//! logs, every other key is ignored and the timer keeps running" had no way
//! to plug in. Here the widget owns rendering + the tick; the CALLER owns
//! the input contract.
//!
//! Backend-agnostic: every render and poll goes through `&mut dyn Console`,
//! so the same code drives the splash framebuffer, the raw-mode tty, and a
//! remote session. It is also `App`-free — the render callback is an opaque
//! `FnMut(u64)` over the remaining whole seconds — so callers can compose it
//! without dragging in the selector [`crate::ui::app::App`].

use std::time::{Duration, Instant};

use crate::error::Result;
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleEvent};

/// What the caller's classifier wants the countdown to do with one input
/// event.
///
/// Open-coded (rather than a `bool`) so a future caller can add a third
/// disposition — e.g. "consume this event AND keep ticking but re-render"
/// — without rippling through every classifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountdownAction {
    /// Stop the countdown immediately and return
    /// [`CountdownOutcome::Cancelled`]. The caller inspects whatever side
    /// state it set (e.g. a recorded key) to decide what to do next.
    Cancel,
    /// Ignore this event; keep ticking toward the deadline.
    Continue,
}

/// Why the countdown loop returned.
///
/// Identical in spirit to the old `ui/timeout.rs::TimeoutOutcome`; the
/// shared name lives here now so both the selector and the refuse screen
/// speak one vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountdownOutcome {
    /// The deadline passed with no event the classifier chose to cancel on.
    Expired,
    /// The classifier returned [`CountdownAction::Cancel`] for an event.
    Cancelled,
}

/// A countdown that renders for at most `duration`, ticking a caller
/// render callback once per displayed second and routing every input
/// event through a caller classifier.
pub struct CountdownScreen {
    duration: Duration,
}

impl CountdownScreen {
    /// Build a countdown that runs for `duration`.
    #[must_use]
    pub fn new(duration: Duration) -> Self {
        Self { duration }
    }

    /// Drive the countdown on `console`.
    ///
    /// * `on_tick(console, remaining_secs)` is invoked once immediately
    ///   with the initial second (rounded UP, so a sub-second budget reads
    ///   "1s" rather than a misleading "0s") and again whenever the
    ///   displayed second changes. The widget hands its OWN `console` to
    ///   the callback so the render and the poll share one backend without
    ///   the caller having to keep a second mutable borrow alive — the
    ///   callback typically stores the value somewhere the backend renders
    ///   and calls `console.render(...)`. It returns a [`Result`] so a
    ///   render error propagates.
    /// * `classify(event)` is called for every [`ConsoleEvent`] the console
    ///   surfaces. Returning [`CountdownAction::Cancel`] ends the loop with
    ///   [`CountdownOutcome::Cancelled`]; [`CountdownAction::Continue`]
    ///   keeps ticking. The classifier may record side state (which key,
    ///   which action) for the caller to read after `run` returns.
    ///
    /// Returns [`CountdownOutcome::Expired`] when the deadline passes with
    /// no cancelling event.
    pub async fn run<C, T>(
        &self,
        console: &mut dyn Console,
        mut classify: C,
        mut on_tick: T,
    ) -> Result<CountdownOutcome>
    where
        C: FnMut(ConsoleEvent) -> CountdownAction,
        T: FnMut(&mut dyn Console, u64) -> Result<()>,
    {
        let start = Instant::now();
        let deadline = start.checked_add(self.duration).unwrap_or(start);

        // Emit the initial frame before we ever poll so the caller paints
        // "… in N seconds" up front. A zero-duration countdown ticks once
        // here and then expires on the first deadline check below.
        let initial = ceil_secs(self.duration);
        on_tick(console, initial)?;
        let mut last_reported = initial;

        loop {
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return Ok(CountdownOutcome::Expired);
            };

            let slice = remaining.min(POLL_SLICE);
            // Route every event through the caller's classifier. The widget
            // itself has NO opinion on what a key means — that is the whole
            // point of the extraction.
            if let Some(event) = console.poll_event(slice).await?
                && classify(event) == CountdownAction::Cancel
            {
                return Ok(CountdownOutcome::Cancelled);
            }

            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return Ok(CountdownOutcome::Expired);
            };
            let secs = ceil_secs(remaining);
            if secs != last_reported {
                on_tick(console, secs)?;
                last_reported = secs;
            }
        }
    }
}

/// Remaining whole seconds, rounded UP, for the countdown header. A
/// non-zero sub-second remainder still reads as at least "1s" so a
/// sub-second budget never displays a misleading "0s".
//
// (Moved verbatim from `ui/selector.rs::ceil_secs` so the one rounding
// rule lives with the one countdown.)
fn ceil_secs(d: Duration) -> u64 {
    let secs = d.as_secs();
    if d.subsec_nanos() > 0 {
        secs.saturating_add(1)
    } else {
        secs
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::pin::Pin;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{CountdownAction, CountdownOutcome, CountdownScreen, ceil_secs};
    use crate::error::Result;
    use crate::ui::app::App;
    use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};

    /// Console double that replays canned events then yields `None`, so
    /// the loop relies on its own deadline once drained.
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

    fn key(code: KeyCode) -> ConsoleEvent {
        ConsoleEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn block<F: std::future::Future>(fut: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("test runtime");
        rt.block_on(fut)
    }

    #[test]
    fn ceil_secs_rounds_subsecond_up() {
        assert_eq!(ceil_secs(std::time::Duration::ZERO), 0);
        assert_eq!(ceil_secs(std::time::Duration::from_millis(1)), 1);
        assert_eq!(ceil_secs(std::time::Duration::from_millis(500)), 1);
        assert_eq!(ceil_secs(std::time::Duration::from_secs(3)), 3);
        assert_eq!(ceil_secs(std::time::Duration::from_millis(3_001)), 4);
    }

    #[test]
    fn classifier_cancel_stops_the_countdown() {
        // A classifier that cancels on a specific key proves the widget
        // routes input through the caller and that "cancel on ANY key" is
        // NOT baked in: the 'x' here cancels only because the classifier
        // says so.
        let mut console = ScriptedConsole::new(vec![key(KeyCode::Char('x'))]);
        let saw = Cell::new(None);
        let outcome = block(
            CountdownScreen::new(std::time::Duration::from_secs(60)).run(
                &mut console,
                |event| {
                    if let ConsoleEvent::Key(k) = event {
                        saw.set(Some(k.code));
                        CountdownAction::Cancel
                    } else {
                        CountdownAction::Continue
                    }
                },
                |_console, _secs| Ok(()),
            ),
        )
        .expect("countdown runs");
        assert_eq!(outcome, CountdownOutcome::Cancelled);
        assert_eq!(saw.get(), Some(KeyCode::Char('x')));
    }

    #[test]
    fn classifier_continue_keeps_ticking_to_expiry() {
        // A classifier that ignores everything must let the (sub-second)
        // budget expire even though an event was delivered — i.e. an
        // ignored key does NOT cancel.
        let mut console = ScriptedConsole::new(vec![key(KeyCode::Char('z'))]);
        let outcome = block(
            CountdownScreen::new(std::time::Duration::from_millis(1)).run(
                &mut console,
                |_event| CountdownAction::Continue,
                |_console, _secs| Ok(()),
            ),
        )
        .expect("countdown runs");
        assert_eq!(outcome, CountdownOutcome::Expired);
    }

    #[test]
    fn initial_tick_fires_once_with_rounded_seconds() {
        // The first frame is painted before any poll, with the budget
        // rounded UP so a 1500 ms countdown opens reading "2s".
        let mut console = ScriptedConsole::new(vec![key(KeyCode::Char('q'))]);
        let first = Cell::new(None);
        let _ = block(
            CountdownScreen::new(std::time::Duration::from_millis(1_500)).run(
                &mut console,
                |_event| CountdownAction::Cancel,
                |_console, secs| {
                    if first.get().is_none() {
                        first.set(Some(secs));
                    }
                    Ok(())
                },
            ),
        )
        .expect("countdown runs");
        assert_eq!(first.get(), Some(2));
    }
}
