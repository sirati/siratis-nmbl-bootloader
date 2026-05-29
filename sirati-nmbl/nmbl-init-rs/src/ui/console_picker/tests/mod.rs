//! Test helpers and submodule declarations for console_picker tests.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]

mod custom_input_tests;
mod render_tests;
mod session_tests;
mod state_tests;

use std::path::Path;
use std::time::Duration;

use crossterm::event::{KeyEvent, KeyModifiers};
use ratatui::Frame;

use crate::error::Result;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::tty_enum::EnumeratedTty;

/// Fake [`Console`] for driving the picker loop in tests.
pub(super) struct FakeConsole {
    pub(super) events: std::collections::VecDeque<Option<KeyEvent>>,
    pub(super) renders: u32,
    pub(super) kind: ConsoleKind,
    pub(super) suspend_calls: u32,
    pub(super) resume_calls: u32,
}

impl FakeConsole {
    pub(super) fn new(events: Vec<Option<KeyEvent>>) -> Self {
        Self {
            events: events.into(),
            renders: 0,
            kind: ConsoleKind::Tty,
            suspend_calls: 0,
            resume_calls: 0,
        }
    }

    pub(super) fn with_kind(mut self, kind: ConsoleKind) -> Self {
        self.kind = kind;
        self
    }
}

impl Console for FakeConsole {
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
        Ok(self.events.pop_front().flatten().map(ConsoleEvent::Key))
    }
    fn size(&self) -> (u16, u16) {
        (80, 24)
    }
    fn kind(&self) -> ConsoleKind {
        self.kind
    }
    fn draw_with(&mut self, _body: &mut dyn FnMut(&mut Frame<'_>)) -> Result<()> {
        self.renders = self.renders.saturating_add(1);
        Ok(())
    }
    fn suspend(&mut self) -> Result<()> {
        self.suspend_calls = self.suspend_calls.saturating_add(1);
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        self.resume_calls = self.resume_calls.saturating_add(1);
        Ok(())
    }
}

/// Drive an async future to completion on a throwaway current-thread
/// runtime so the synchronous picker unit tests can exercise the
/// now-async driver loop and dispatch.
pub(super) fn block<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}

pub(super) fn press(code: crossterm::event::KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(super) fn no_enum(_exclude: &Path) -> Vec<EnumeratedTty> {
    Vec::new()
}
