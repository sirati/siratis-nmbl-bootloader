//! A production [`Console`] that replays a fixed key script, used by the
//! `--validate-initrm` `ErrorToErrorScreen` scenario to drive the REAL
//! emergency menu to an immediate, side-effect-free exit.
//!
//! The emergency menu loop only returns on a keypress or its auto-reboot
//! countdown (30 s by default). A `NoopConsole` would therefore make the
//! dry-run block ~30 s of wall-clock per scenario. Instead we feed the menu
//! a single scripted `Enter`, which selects the default item (index 0 =
//! `Reboot` — a pure choice that touches no ops, forks nothing) and exits at
//! once.
//!
//! It is deliberately NOT `#[cfg(test)]`: the `--validate-initrm` mode is
//! shipped production code, so the console it feeds the menu must compile in
//! a normal build. `render`/`draw_with` are no-ops (nothing to paint in a
//! dry run); `poll_event` hands out the next scripted event, then `None`
//! forever so any loop that outruns the script falls back to its own idle
//! path.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;

use nmbl_init::error::Result;
use nmbl_init::ui::app::App;
use nmbl_init::ui::console::{Console, ConsoleEvent, ConsoleKind};

/// A [`Console`] that returns a pre-recorded sequence of key events and
/// then idles. See the module docs for why it is production code.
pub(super) struct ScriptedConsole {
    /// Remaining scripted events, popped front-to-back per poll.
    events: VecDeque<ConsoleEvent>,
    /// Reported grid size; 80x24 is the lowest common denominator, the
    /// same default [`nmbl_init::ui::console::NoopConsole`] uses.
    size: (u16, u16),
}

impl ScriptedConsole {
    /// Build a scripted console that replays `keys` (each becomes a
    /// [`ConsoleEvent::Key`]) in order, one per `poll_event` /
    /// `poll_event_blocking` call, then yields `None` indefinitely.
    pub(super) fn from_keys(keys: impl IntoIterator<Item = KeyCode>) -> Self {
        let events = keys
            .into_iter()
            .map(|code| ConsoleEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .collect();
        Self {
            events,
            size: (80, 24),
        }
    }
}

impl Console for ScriptedConsole {
    fn render(&mut self, _app: &App<'_>) -> Result<()> {
        Ok(())
    }

    fn poll_event<'a>(
        &'a mut self,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        let next = self.events.pop_front();
        Box::pin(async move { Ok(next) })
    }

    fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
        Ok(self.events.pop_front())
    }

    fn size(&self) -> (u16, u16) {
        self.size
    }

    fn kind(&self) -> ConsoleKind {
        // The emergency path branches on Tty vs Splash only for real-device
        // handling; Tty is the inert, device-free choice.
        ConsoleKind::Tty
    }

    fn draw_with(&mut self, _body: &mut dyn FnMut(&mut Frame<'_>)) -> Result<()> {
        Ok(())
    }

    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}
