#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]

mod modal_confirm;
mod modal_password;
mod passphrase;

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::error::Result;
use crate::ui::app::{App, Screen, SessionInteraction};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};

#[test]
fn tui_password_supplier_satisfies_password_supplier_trait() {
    // The integration contract: activation::run_all_activations
    // accepts `Option<&mut dyn PasswordSupplier>`. This test pins
    // that coercion so a future signature drift on either side
    // breaks the build instead of breaking at boot.
    use crate::activation::PasswordSupplier;
    use crate::config::Config;
    use crate::ui::TuiPasswordSupplier;
    let cfg: Config = toml::from_str("").expect("default cfg");
    let session = SessionInteraction::new();
    let mut sup = TuiPasswordSupplier::new(&cfg, &session);
    let _coerced: &mut dyn PasswordSupplier = &mut sup;
}

/// Console test double that returns canned key events and records
/// every render call. `poll_key` first drains the queue (returning
/// one event per call) and then yields `None` so the supplier's
/// loop wraps tightly without us having to manage real timeouts.
pub(super) struct ScriptedConsole {
    pub(super) keys: std::collections::VecDeque<KeyEvent>,
    pub(super) renders: u32,
    pub(super) last_buffer_len: usize,
    pub(super) last_label: Option<String>,
}

impl ScriptedConsole {
    pub(super) fn new(keys: Vec<KeyEvent>) -> Self {
        Self {
            keys: keys.into(),
            renders: 0,
            last_buffer_len: 0,
            last_label: None,
        }
    }
}

impl Console for ScriptedConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.renders = self.renders.saturating_add(1);
        if let Screen::Passphrase {
            buffer,
            prompt_label,
            ..
        } = &app.screen
        {
            self.last_buffer_len = buffer.len();
            self.last_label = Some(prompt_label.clone());
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
        Ok(self.keys.pop_front().map(ConsoleEvent::Key))
    }
    fn size(&self) -> (u16, u16) {
        (80, 24)
    }
    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }
    fn draw_with(&mut self, _body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.renders = self.renders.saturating_add(1);
        Ok(())
    }
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}

pub(super) fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// Drive an async modal future to completion on a throwaway
/// current-thread runtime so the existing synchronous tests can call
/// the now-async modal helpers unchanged in spirit.
pub(super) fn block<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}
