//! Reusable kernel-log / NMBL-transcript viewer, decoupled from the
//! selector [`crate::ui::app::App`].
//!
//! The generation selector reaches the log viewer through [`Screen::Log`]
//! and the central `App::on_key` / `render_current_screen` dispatch. That
//! is fine while you already have an `App`, but a future screen (the
//! secure-boot refuse screen) wants to pop up the same scrollable dmesg
//! view WITHOUT constructing a selector App around it. This module lifts
//! the viewer's state + scroll keymap + a self-contained run loop out so a
//! caller can show it over a bare `&mut dyn Console`.
//!
//! The state and the scroll/toggle keymap are byte-for-byte the same as
//! the [`Screen::Log`] arm of `ui/app/handlers.rs` and the field set of
//! the `Screen::Log` variant; the only genuinely-new code is the thin
//! [`LogViewer::run`] event loop and the [`LogViewer::draw`] adapter onto
//! the existing [`crate::ui::view::render_log`] renderer.
//!
//! [`Screen::Log`]: crate::ui::app::Screen::Log

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;

use crate::error::Result;
use crate::ui::POLL_SLICE;
use crate::ui::app::{LOG_PAGE, LogSource};
use crate::ui::console::{Console, ConsoleEvent};
use crate::ui::view::render_log;

/// A scrollable view over one log buffer (NMBL transcript or kernel
/// `dmesg`), with Ctrl+K to toggle between the two.
///
/// Owns exactly the state the [`Screen::Log`] variant carried: the
/// snapshot `lines` (oldest first), the scroll-from-top `offset`, the
/// `follow_bottom` pin, and which `source` is shown. `offset` /
/// `follow_bottom` stay [`Cell`]s because the renderer — the only place
/// that knows the viewport height — writes the resolved, clamped offset
/// back each frame.
///
/// [`Screen::Log`]: crate::ui::app::Screen::Log
pub struct LogViewer {
    lines: Vec<String>,
    offset: Cell<u16>,
    follow_bottom: Cell<bool>,
    source: LogSource,
    /// When `true` the viewer shows a FIXED, caller-supplied line set and
    /// MUST NOT re-read a live buffer: Ctrl+K (toggle source) is suppressed
    /// so the operator cannot flip to the live `Nmbl`/`Kernel` buffer. Used
    /// by the secure-boot refuse screen, where the pre-refuse transcript is
    /// scrubbed and only a post-refuse snapshot may be shown (FIX-41).
    scrubbed: bool,
}

/// What a key did to the viewer, so a host loop knows whether to redraw
/// or to pop the viewer closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogViewerOutcome {
    /// The key scrolled / toggled the view; the host should redraw.
    Redraw,
    /// Esc was pressed; the host should close the viewer.
    Close,
    /// The key was not one the viewer handles; the host may do its own
    /// thing with it.
    Ignored,
}

impl LogViewer {
    /// Open a viewer on `source`, reading a fresh snapshot and pinning to
    /// the bottom (newest lines), exactly like the [`Screen::Log`] open
    /// path (`dmesg` / `less +G`).
    ///
    /// [`Screen::Log`]: crate::ui::app::Screen::Log
    #[must_use]
    pub fn open(source: LogSource) -> Self {
        Self {
            lines: source.read_snapshot(),
            offset: Cell::new(0),
            follow_bottom: Cell::new(true),
            source,
            scrubbed: false,
        }
    }

    /// Open a viewer on a FIXED, caller-supplied line set that is NEVER
    /// re-read from a live buffer (FIX-41). Ctrl+K (toggle source) becomes a
    /// no-op so a scrubbed view cannot be flipped back to the live
    /// `Nmbl`/`Kernel` buffer and leak the pre-refuse transcript. Renders
    /// under the [`LogSource::Nmbl`] chrome (the source label is cosmetic;
    /// the lines are exactly what the caller passed). Used by the secure-boot
    /// refuse screen.
    #[must_use]
    pub fn open_scrubbed(lines: Vec<String>) -> Self {
        Self {
            lines,
            offset: Cell::new(0),
            follow_bottom: Cell::new(true),
            source: LogSource::Nmbl,
            scrubbed: true,
        }
    }

    /// The buffer currently shown.
    #[must_use]
    pub fn source(&self) -> LogSource {
        self.source
    }

    /// Flip between the NMBL transcript and the kernel ring buffer,
    /// re-reading the freshly-selected buffer and re-pinning to its
    /// bottom. Byte-identical to the Ctrl+K arm of `App::on_key`.
    pub fn toggle_source(&mut self) {
        self.source = self.source.toggled();
        self.lines = self.source.read_snapshot();
        self.offset.set(0);
        self.follow_bottom.set(true);
    }

    /// Apply one scroll/close/toggle key. Returns how the host should
    /// react. The scroll keymap (Up/Down/PageUp/PageDown/Home/End and the
    /// follow-bottom semantics) is moved verbatim from the
    /// [`Screen::Log`] arm of `ui/app/handlers.rs`; Ctrl+K (toggle source)
    /// and Ctrl+L / Esc (close) are folded in here so a host that is NOT
    /// the selector App still gets the full keymap.
    ///
    /// [`Screen::Log`]: crate::ui::app::Screen::Log
    pub fn handle_key(&mut self, key: KeyEvent) -> LogViewerOutcome {
        // Ignore Release/Repeat so a held key doesn't fire repeatedly.
        if key.kind != KeyEventKind::Press {
            return LogViewerOutcome::Ignored;
        }

        if key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            match key.code {
                // Ctrl+L closes the viewer (it is the open/close toggle on
                // the selector); a standalone host treats it as close too.
                KeyCode::Char('l') => return LogViewerOutcome::Close,
                KeyCode::Char('k') => {
                    // In scrubbed mode there is no live buffer to toggle to:
                    // suppress Ctrl+K so the operator cannot flip back to the
                    // pre-refuse transcript (FIX-41).
                    if self.scrubbed {
                        return LogViewerOutcome::Ignored;
                    }
                    self.toggle_source();
                    return LogViewerOutcome::Redraw;
                }
                _ => return LogViewerOutcome::Ignored,
            }
        }

        let cur = self.offset.get();
        match key.code {
            KeyCode::Esc => LogViewerOutcome::Close,
            // Any explicit up-scroll leaves "follow the bottom" mode so the
            // view stops auto-pinning to the newest line.
            KeyCode::Up => {
                self.follow_bottom.set(false);
                self.offset.set(cur.saturating_sub(1));
                LogViewerOutcome::Redraw
            }
            KeyCode::PageUp => {
                self.follow_bottom.set(false);
                self.offset.set(cur.saturating_sub(LOG_PAGE));
                LogViewerOutcome::Redraw
            }
            KeyCode::Home => {
                self.follow_bottom.set(false);
                self.offset.set(0);
                LogViewerOutcome::Redraw
            }
            KeyCode::Down => {
                self.follow_bottom.set(false);
                self.offset.set(cur.saturating_add(1));
                LogViewerOutcome::Redraw
            }
            KeyCode::PageDown => {
                self.follow_bottom.set(false);
                self.offset.set(cur.saturating_add(LOG_PAGE));
                LogViewerOutcome::Redraw
            }
            // End re-pins to the bottom; the renderer resolves the concrete
            // offset against the live viewport height.
            KeyCode::End => {
                self.follow_bottom.set(true);
                LogViewerOutcome::Redraw
            }
            _ => LogViewerOutcome::Ignored,
        }
    }

    /// Paint the viewer into `frame`'s full area via the shared
    /// [`render_log`] renderer (the one [`Screen::Log`] uses), so a
    /// standalone host produces a pixel-identical viewer.
    ///
    /// [`Screen::Log`]: crate::ui::app::Screen::Log
    pub fn draw(&self, frame: &mut Frame<'_>) {
        render_log(
            frame,
            frame.area(),
            &self.lines,
            &self.offset,
            &self.follow_bottom,
            self.source,
        );
    }

    /// Run a self-contained viewer loop on `console` until the operator
    /// closes it (Esc / Ctrl+L). Renders through [`Console::draw_with`] so
    /// no selector `App` is needed. Returns once the viewer is closed.
    ///
    /// This is the genuinely-new glue: the existing selector drives the
    /// viewer through its own coalescing event loop, but a standalone host
    /// (the refuse screen) wants a one-call "show the logs until closed".
    pub async fn run(&mut self, console: &mut dyn Console) -> Result<()> {
        // Paint once before the first poll so the viewer is visible
        // immediately.
        console.draw_with(&mut |frame| self.draw(frame))?;
        loop {
            let Some(event) = console.poll_event(POLL_SLICE).await? else {
                continue;
            };
            match event {
                ConsoleEvent::Key(key) => match self.handle_key(key) {
                    LogViewerOutcome::Close => return Ok(()),
                    LogViewerOutcome::Redraw => {
                        console.draw_with(&mut |frame| self.draw(frame))?;
                    }
                    LogViewerOutcome::Ignored => {}
                },
                // A resize just repaints at the new geometry.
                ConsoleEvent::Resize { .. } => {
                    console.draw_with(&mut |frame| self.draw(frame))?;
                }
                // No scrollback wheel / presence notice handling here.
                ConsoleEvent::Scroll { .. } | ConsoleEvent::UserHasInteracted => {}
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{LogViewer, LogViewerOutcome};
    use crate::ui::app::LogSource;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn esc_and_ctrl_l_close_the_viewer() {
        let mut v = LogViewer::open(LogSource::Nmbl);
        assert_eq!(v.handle_key(press(KeyCode::Esc)), LogViewerOutcome::Close);
        assert_eq!(
            v.handle_key(ctrl(KeyCode::Char('l'))),
            LogViewerOutcome::Close
        );
    }

    #[test]
    fn up_scroll_clears_follow_bottom_and_moves_offset() {
        let mut v = LogViewer::open(LogSource::Nmbl);
        // The viewer opens pinned to the bottom; the renderer normally
        // resolves a concrete offset, but for a pure-logic test we seed a
        // known offset and confirm Up steps it by one and drops follow.
        v.offset.set(10);
        v.follow_bottom.set(true);
        assert_eq!(v.handle_key(press(KeyCode::Up)), LogViewerOutcome::Redraw);
        assert_eq!(v.offset.get(), 9);
        assert!(!v.follow_bottom.get(), "an up-scroll must drop follow mode");
    }

    #[test]
    fn end_repins_to_bottom() {
        let mut v = LogViewer::open(LogSource::Nmbl);
        v.follow_bottom.set(false);
        assert_eq!(v.handle_key(press(KeyCode::End)), LogViewerOutcome::Redraw);
        assert!(v.follow_bottom.get(), "End must re-pin to the bottom");
    }

    #[test]
    fn ctrl_k_toggles_source_and_repins() {
        let mut v = LogViewer::open(LogSource::Nmbl);
        v.offset.set(5);
        v.follow_bottom.set(false);
        assert_eq!(
            v.handle_key(ctrl(KeyCode::Char('k'))),
            LogViewerOutcome::Redraw
        );
        assert_eq!(v.source(), LogSource::Kernel);
        assert_eq!(v.offset.get(), 0, "toggling re-pins to the new bottom");
        assert!(v.follow_bottom.get());
    }

    #[test]
    fn unhandled_key_is_ignored() {
        let mut v = LogViewer::open(LogSource::Nmbl);
        assert_eq!(
            v.handle_key(press(KeyCode::Char('z'))),
            LogViewerOutcome::Ignored
        );
    }
}
