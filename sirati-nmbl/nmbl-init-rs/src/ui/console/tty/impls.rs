//! `impl Console for TtyConsole` and `impl Drop for TtyConsole`.

use std::future::Future;
use std::os::fd::{AsFd, AsRawFd};
use std::pin::Pin;
use std::time::Duration;

use ratatui::backend::Backend;

use crate::error::Result;
use crate::log;
use crate::nmbl_warn;
use crate::sys::printk::PrintkQuiet;
use crate::sys::tty::{enter_raw, restore_termios, save_termios};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::render_current_screen;

use super::TtyConsole;
use super::kd::{enter_kd_graphics, restore_kd_mode};
use super::util::{duration_to_ms, tui_err};

impl Console for TtyConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.terminal
            .draw(|f| render_current_screen(f, app))
            .map(|_| ())
            .map_err(tui_err)
    }

    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move {
            // A key or scroll already classified from a previous cycle
            // is ready now — no need to touch the fd / reactor.
            if self.pending_keys.front().is_some() || self.pending_scrolls.front().is_some() {
                return self.poll_event_blocking(Duration::from_millis(0));
            }
            // Await readability (or the slice deadline) on the console
            // fd through tokio's reactor instead of a blocking poll(2),
            // then run the identical synchronous drain. The blocking
            // path's internal 100ms cap is harmless after we've already
            // waited: a ready fd reads immediately, a timeout drains
            // nothing. No borrow is held across the `.await`.
            let slice = timeout.min(POLL_SLICE);
            crate::ui::console::await_fd_readable(self.fd.as_fd(), slice).await?;
            self.poll_event_blocking(Duration::from_millis(0))
        })
    }

    fn poll_event_blocking(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
        // First: drain any keys / scrolls already classified from a
        // previous poll cycle without going to the fd again.
        if let Some(k) = self.pending_keys.pop_front() {
            return Ok(Some(ConsoleEvent::Key(k)));
        }
        if let Some(s) = self.pending_scrolls.pop_front() {
            return Ok(Some(s));
        }

        // Cap the wait so backends are uniformly responsive to
        // ticking countdowns.
        let slice = timeout.min(POLL_SLICE);
        let timeout_ms = duration_to_ms(slice);
        let resize = self.refill(timeout_ms)?;
        // After refill, prefer surfacing a Resize first (so layout
        // catches up before the next key dispatches against the new
        // size); then surface a key, then a scroll notch, from whatever
        // the parser emitted.
        if let Some(ev) = resize {
            self.apply_resize(&ev);
            return Ok(Some(ev));
        }
        if let Some(k) = self.pending_keys.pop_front() {
            return Ok(Some(ConsoleEvent::Key(k)));
        }
        if let Some(s) = self.pending_scrolls.pop_front() {
            return Ok(Some(s));
        }
        Ok(None)
    }

    fn size(&self) -> (u16, u16) {
        // A host-reported resize wins over the backend's cached size
        // — the backend caches the value it saw at construction time,
        // which on a serial line is the static `stty rows/cols` value
        // the kernel set at boot rather than the operator's live
        // tmux pane geometry.
        if let Some((cols, rows)) = self.last_resize {
            return (cols, rows);
        }
        match self.terminal.backend().size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (0, 0),
        }
    }

    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal.draw(|f| body(f)).map(|_| ()).map_err(tui_err)
    }

    fn suspend(&mut self) -> Result<()> {
        if let Some(mut q) = self.printk_quiet.take() {
            q.restore();
        }
        // Only the primary boot console owns the process-global
        // stderr-suppression refcount; a remote-pty console never set
        // it, so it must not clear it (that would un-suppress the local
        // console's stderr mid-boot).
        if self.owns_global_tui_state {
            log::clear_tui_active();
        }
        if let Some(previous) = self.previous_kd_mode.take() {
            restore_kd_mode(self.fd.as_fd(), previous);
        }
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            nmbl_warn!(
                "TtyConsole::suspend: failed to restore termios on fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        let saved = save_termios(self.fd.as_fd())?;
        let _ = enter_raw(self.fd.as_fd())?;
        self.saved_termios = Some(saved);
        // Re-acquiring the kernel VT / printk / stderr suppression is
        // only correct for the primary console that owned them. A remote
        // pty console renders to its own pty and leaves the globals alone.
        if self.owns_global_tui_state {
            self.previous_kd_mode = enter_kd_graphics(self.fd.as_fd());
            self.printk_quiet = Some(PrintkQuiet::engage());
            log::set_tui_active();
        }
        self.terminal.clear().map_err(tui_err)?;
        Ok(())
    }

    fn caps_lock_active(&self) -> Option<bool> {
        // `/dev/console` is a VT in the framebuffer case and a serial
        // line otherwise. `caps_lock_active` returns `None` on the
        // latter (ENOTTY), so the passphrase warning is shown only when
        // a real VT keyboard reports Caps Lock.
        crate::sys::vt::caps_lock_active(self.fd.as_fd())
    }
}

impl Drop for TtyConsole {
    fn drop(&mut self) {
        if let Some(mut q) = self.printk_quiet.take() {
            q.restore();
        }
        if self.owns_global_tui_state {
            log::clear_tui_active();
        }
        if let Some(previous) = self.previous_kd_mode.take() {
            restore_kd_mode(self.fd.as_fd(), previous);
        }
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            nmbl_warn!(
                "failed to restore termios on tty console fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
    }
}
