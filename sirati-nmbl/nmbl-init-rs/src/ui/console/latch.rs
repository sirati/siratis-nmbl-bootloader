//! Central interaction-latch layer.
//!
//! [`LatchingConsole`] is a transparent [`Console`] decorator that wraps
//! a session's real backend (splash framebuffer, raw-mode tty, or remote
//! pty) and is the SINGLE place in the whole codebase that records
//! "the operator is present". On the FIRST real input event of the
//! session — Key, Resize, or Scroll — it:
//!
//!   1. sets the session's [`SessionInteraction`] latch, and
//!   2. emits a one-shot [`ConsoleEvent::UserHasInteracted`] *before*
//!      surfacing that first real event (the real event follows on the
//!      next poll and does its normal job).
//!
//! Every subsequent input passes straight through. No other layer sets
//! the latch: the selector and emergency screens read `interaction.get()`
//! on entry (to decide whether to arm their auto-action countdown) and
//! cancel that countdown on [`ConsoleEvent::UserHasInteracted`] in their
//! loop, instead of re-deriving presence from raw keys.
//!
//! ## Why this is the convergence point for BOTH local and remote
//!
//! Every interactive consumer drives input exclusively through the
//! [`Console`] trait: the early-boot [`crate::ui::reporter::BootReporter`]
//! via [`Console::poll_key`], the generation selector and the emergency
//! loop via [`Console::poll_event`]. The local boot wraps its single
//! session console in a `LatchingConsole` in `run_tui_session`; each
//! remote recovery session wraps its own pty console in a
//! `LatchingConsole` in `serve_session`. So wherever a real or remote
//! operator's keypress enters the program — including during the early
//! boot-log window the reporter polls — it flows through this one layer,
//! which latches it and emits the notice. Because each session owns its
//! own backend and its own [`SessionInteraction`], one wrapper per
//! session keeps the per-session independence the remote model requires.
//!
//! ## Threading
//!
//! NMBL runs on a single-thread `LocalRuntime`; every session (local and
//! remote) lives on that one thread. The latch stays
//! `Rc<Cell<bool>>` ([`SessionInteraction`]) and this wrapper holds its
//! one-shot state in plain `Cell`s — no atomics, no `Send`/`Sync` bound,
//! matching the rest of the fork-safe single-thread runtime.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::error::Result;
use crate::ui::app::{App, SessionInteraction};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};

/// Transparent [`Console`] decorator that owns the session's real
/// backend and latches operator presence on first input. See the module
/// docs for the full contract.
pub struct LatchingConsole<'c> {
    /// The wrapped backend. Owned (not borrowed) so a `LatchingConsole`
    /// can stand in for a `Box<dyn Console>` everywhere the session
    /// threads its console — including the by-value hand-off into the
    /// emergency menu — without a separate borrow lifetime to juggle.
    inner: Box<dyn Console + 'c>,
    /// The session latch this layer owns the right to set. A clone of
    /// the same `Rc<Cell<bool>>` the screens read via `app.interaction`.
    interaction: SessionInteraction,
    /// Whether the one-shot [`ConsoleEvent::UserHasInteracted`] has
    /// already been emitted for this session. Once `true` the layer is
    /// fully transparent.
    emitted: Cell<bool>,
    /// The first real input event, stashed while we surface the
    /// `UserHasInteracted` notice ahead of it. The very next poll drains
    /// this before touching the inner backend, so the operator's first
    /// keypress is never lost — it merely arrives one poll later.
    pending: Cell<Option<ConsoleEvent>>,
}

impl<'c> LatchingConsole<'c> {
    /// Wrap `inner` so the first real input event of the session sets
    /// `interaction` and emits a one-shot
    /// [`ConsoleEvent::UserHasInteracted`]. `interaction` must be a clone
    /// of the same latch the session's screens consult via
    /// `app.interaction`.
    #[must_use]
    pub fn new(inner: Box<dyn Console + 'c>, interaction: SessionInteraction) -> Self {
        Self {
            inner,
            interaction,
            emitted: Cell::new(false),
            pending: Cell::new(None),
        }
    }

    /// Apply the latch policy to one event freshly drained from the inner
    /// backend. Returns the event the caller should surface this poll.
    ///
    /// * Before first input: a real `Some(event)` latches presence, is
    ///   stashed in `pending`, and we return `UserHasInteracted` instead.
    /// * `None` (idle slice) passes through untouched — an empty poll is
    ///   not input and must not latch.
    /// * After first input: everything passes straight through.
    fn intercept(&self, fresh: Option<ConsoleEvent>) -> Option<ConsoleEvent> {
        if self.emitted.get() {
            return fresh;
        }
        match fresh {
            Some(event) => {
                // First real input of the session. Latch presence, stash
                // the real event behind the one-shot notice, and surface
                // the notice now.
                self.interaction.set();
                self.emitted.set(true);
                self.pending.set(Some(event));
                Some(ConsoleEvent::UserHasInteracted)
            }
            None => None,
        }
    }

    /// Drain the stashed first real event if one is waiting. Returned
    /// before the inner backend is polled so the first keypress is never
    /// dropped.
    fn take_pending(&self) -> Option<ConsoleEvent> {
        self.pending.take()
    }
}

impl Console for LatchingConsole<'_> {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.inner.render(app)
    }

    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move {
            // One input-poll cycle of the event loop. Bump the global
            // diagnostic tick here — the single convergence point every
            // interactive loop polls through — so the top-right spinner
            // advances iff the loop keeps iterating, and freezes the
            // instant a synchronous op blocks the loop.
            crate::ui::event_tick::tick();
            if let Some(ev) = self.take_pending() {
                return Ok(Some(ev));
            }
            let fresh = self.inner.poll_event(timeout).await?;
            Ok(self.intercept(fresh))
        })
    }

    fn poll_event_blocking(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
        // One input-poll cycle (early-boot reporter / blocking path):
        // bump the same diagnostic tick the async path drives.
        crate::ui::event_tick::tick();
        if let Some(ev) = self.take_pending() {
            return Ok(Some(ev));
        }
        let fresh = self.inner.poll_event_blocking(timeout)?;
        Ok(self.intercept(fresh))
    }

    fn size(&self) -> (u16, u16) {
        self.inner.size()
    }

    fn kind(&self) -> ConsoleKind {
        self.inner.kind()
    }

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.inner.draw_with(body)
    }

    fn suspend(&mut self) -> Result<()> {
        self.inner.suspend()
    }

    fn resume(&mut self) -> Result<()> {
        self.inner.resume()
    }

    fn caps_lock_active(&self) -> Option<bool> {
        self.inner.caps_lock_active()
    }
}

// `poll_key` is the default trait impl: it calls `poll_event_blocking`
// (so it runs the latch policy above) and then drops every non-`Key`
// event — including the synthetic `UserHasInteracted`. That is exactly
// right for the early-boot reporter: a key pressed during the boot-log
// window latches presence here, the reporter only needs the key for its
// Esc→Abort check, and the synthetic notice carries no key so reporting
// "no key" for it is correct.

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use std::collections::VecDeque;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    /// Replays a canned sequence of optional events; once drained yields
    /// `None`. `None` entries model idle poll slices.
    struct ScriptedConsole {
        events: VecDeque<Option<ConsoleEvent>>,
    }

    impl ScriptedConsole {
        fn new(events: Vec<Option<ConsoleEvent>>) -> Self {
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
            timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
            Box::pin(async move { self.poll_event_blocking(timeout) })
        }
        fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
            Ok(self.events.pop_front().flatten())
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

    fn key(c: char) -> ConsoleEvent {
        ConsoleEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
    }

    fn block<F: std::future::Future>(fut: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("test runtime");
        rt.block_on(fut)
    }

    #[test]
    fn first_input_latches_and_emits_oneshot_then_passes_through() {
        let latch = SessionInteraction::new();
        let inner = ScriptedConsole::new(vec![Some(key('x')), Some(key('y'))]);
        let mut wrapped = LatchingConsole::new(Box::new(inner), latch.clone());
        assert!(!latch.get(), "latch starts clear");

        // First poll: the synthetic notice arrives BEFORE the key, and
        // the latch is now set.
        let first = block(wrapped.poll_event(Duration::from_millis(0)))
            .expect("poll ok")
            .expect("event present");
        assert_eq!(first, ConsoleEvent::UserHasInteracted);
        assert!(latch.get(), "first input must latch presence");

        // Second poll: the stashed real key, unmodified.
        let second = block(wrapped.poll_event(Duration::from_millis(0)))
            .expect("poll ok")
            .expect("event present");
        assert_eq!(second, key('x'), "the first real key follows the notice");

        // Third poll: subsequent input passes straight through, no
        // further notice.
        let third = block(wrapped.poll_event(Duration::from_millis(0)))
            .expect("poll ok")
            .expect("event present");
        assert_eq!(third, key('y'), "subsequent input is transparent");
    }

    #[test]
    fn oneshot_fires_only_once() {
        let latch = SessionInteraction::new();
        let inner = ScriptedConsole::new(vec![Some(key('a')), Some(key('b')), Some(key('c'))]);
        let mut wrapped = LatchingConsole::new(Box::new(inner), latch);

        let mut seen = Vec::new();
        for _ in 0..6 {
            if let Some(ev) = block(wrapped.poll_event(Duration::from_millis(0))).expect("poll ok")
            {
                seen.push(ev);
            }
        }
        let notices = seen
            .iter()
            .filter(|e| matches!(e, ConsoleEvent::UserHasInteracted))
            .count();
        assert_eq!(notices, 1, "UserHasInteracted must be emitted exactly once");
        // The three real keys must all survive, in order, after the notice.
        let keys: Vec<_> = seen
            .iter()
            .filter(|e| matches!(e, ConsoleEvent::Key(_)))
            .copied()
            .collect();
        assert_eq!(keys, vec![key('a'), key('b'), key('c')]);
    }

    #[test]
    fn idle_polls_do_not_latch() {
        let latch = SessionInteraction::new();
        let inner = ScriptedConsole::new(vec![None, None]);
        let mut wrapped = LatchingConsole::new(Box::new(inner), latch.clone());

        for _ in 0..2 {
            assert!(
                block(wrapped.poll_event(Duration::from_millis(0)))
                    .expect("poll ok")
                    .is_none(),
                "an idle slice must surface no event"
            );
        }
        assert!(!latch.get(), "idle polls must not latch presence");
    }

    #[test]
    fn poll_key_latches_but_drops_the_oneshot() {
        // The reporter polls via the blocking `poll_key`, which keeps
        // only Key events. A key pressed during the early boot-log window
        // must still latch presence even though the synthetic notice is
        // dropped by `poll_key`.
        let latch = SessionInteraction::new();
        let inner = ScriptedConsole::new(vec![Some(key('z'))]);
        let mut wrapped = LatchingConsole::new(Box::new(inner), latch.clone());

        // First poll_key: the inner key is consumed by `intercept`, which
        // returns the synthetic notice; `poll_key` drops it → Ok(None).
        // The latch is set as a side effect.
        let first: Option<KeyEvent> = wrapped.poll_key(Duration::from_millis(0)).expect("poll ok");
        assert!(first.is_none(), "the one-shot notice is not a key");
        assert!(latch.get(), "a reporter-window key must latch presence");

        // Second poll_key: the stashed real key surfaces.
        let second = wrapped
            .poll_key(Duration::from_millis(0))
            .expect("poll ok")
            .expect("real key surfaces");
        assert_eq!(second.code, KeyCode::Char('z'));
    }

    #[test]
    fn resize_counts_as_first_input() {
        // A Resize is real input from a present operator's terminal, so
        // it latches and emits the notice just like a key.
        let latch = SessionInteraction::new();
        let inner = ScriptedConsole::new(vec![Some(ConsoleEvent::Resize {
            rows: 40,
            cols: 100,
        })]);
        let mut wrapped = LatchingConsole::new(Box::new(inner), latch.clone());

        let first = block(wrapped.poll_event(Duration::from_millis(0)))
            .expect("poll ok")
            .expect("event present");
        assert_eq!(first, ConsoleEvent::UserHasInteracted);
        assert!(latch.get());
        let second = block(wrapped.poll_event(Duration::from_millis(0)))
            .expect("poll ok")
            .expect("event present");
        assert_eq!(
            second,
            ConsoleEvent::Resize {
                rows: 40,
                cols: 100
            }
        );
    }
}
