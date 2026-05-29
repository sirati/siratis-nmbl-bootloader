//! [`Console`] trait implementation for [`SplashConsole`].

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::error::Result;
use crate::log;
use crate::nmbl_warn;
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::{render_splash_frame, render_splash_frame_with};

use super::SplashConsole;

impl Console for SplashConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        render_splash_frame(
            &mut self.drm,
            &self.bg_scaled,
            &self.cache,
            self.cell_dims,
            app,
        )
    }

    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move {
            // A key buffered by a prior poll is ready now; skip the
            // reactor and drain it.
            if self.input.has_pending() {
                return Ok(self
                    .input
                    .poll(Duration::from_millis(0))?
                    .map(ConsoleEvent::Key));
            }
            // Await readability on /dev/tty1 through tokio's reactor,
            // then run the identical synchronous drain (which keeps the
            // bare-Esc 10ms follow-up disambiguation). No borrow held
            // across the await.
            let slice = timeout.min(POLL_SLICE);
            super::super::await_fd_readable(self.input.input_fd(), slice).await?;
            Ok(self
                .input
                .poll(Duration::from_millis(0))?
                .map(ConsoleEvent::Key))
        })
    }

    fn poll_event_blocking(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
        // Cap the effective wait the same way [`TtyConsole`] does so
        // backends are uniformly responsive to ticking countdowns and
        // spinner animations. The caller-supplied timeout is honoured
        // but never longer than POLL_SLICE per call; the trait doc
        // pins this contract for both backends.
        //
        // The splash framebuffer has a fixed cell grid derived at
        // bring-up from the DRM mode, so this backend never emits
        // resize events — only keys.
        let slice = timeout.min(POLL_SLICE);
        Ok(self.input.poll(slice)?.map(ConsoleEvent::Key))
    }

    fn size(&self) -> (u16, u16) {
        (self.cell_dims.cols, self.cell_dims.rows)
    }

    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Splash
    }

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        render_splash_frame_with(
            &mut self.drm,
            &self.bg_scaled,
            &self.cache,
            self.cell_dims,
            body,
        )
    }

    /// Hand the framebuffer back to the kernel-elected VT so the
    /// kernel resumes painting printk + the active VT renders the
    /// multiplexed shell output. We release DRM master and restore
    /// the input tty's termios so the foreign writer can pass bytes
    /// through `/dev/tty1` without our raw-mode flags eating them.
    ///
    /// The mode-set state is preserved — `resume` re-acquires master
    /// and re-renders the splash composite without re-running the
    /// font load / cover-scale pipeline.
    fn suspend(&mut self) -> Result<()> {
        // Re-enable eprintln in the `nmbl_*!` macros so any warning
        // emitted by the rest of the suspend / relay path reaches the
        // operator's pre-shell screen. Re-armed on `resume`.
        log::clear_tui_active();
        // DRM master FIRST: doing it before termios restore minimises
        // the window where the kernel could paint printk while
        // userspace still has raw-mode termios.
        self.drm.drop_master();
        if let Err(e) = self.input.suspend() {
            nmbl_warn!("SplashConsole::suspend: input suspend failed: {e}");
        }
        Ok(())
    }

    /// Re-acquire the framebuffer + raw-mode input tty. The render
    /// pipeline is unchanged; the next [`render`] call will flush a
    /// full frame because each splash frame redoes the composite +
    /// page-flip from scratch (no incremental updates).
    fn resume(&mut self) -> Result<()> {
        if let Err(e) = self.input.resume() {
            nmbl_warn!("SplashConsole::resume: input resume failed: {e}");
        }
        self.drm.acquire_master();
        // Re-arm the macro gate so the post-shell render path doesn't
        // leak eprintln smear over the splash framebuffer.
        log::set_tui_active();
        Ok(())
    }

    fn caps_lock_active(&self) -> Option<bool> {
        // `/dev/tty1` is always a kernel VT, so KDGKBLED works here and
        // reports the live Caps-Lock state of the framebuffer keyboard.
        self.input.caps_lock_active()
    }
}

impl Drop for SplashConsole {
    fn drop(&mut self) {
        // Final handover (kexec / emergency execve): re-enable
        // eprintln in `nmbl_*!`. The splash backend's other Drop
        // chains (SplashDrm, SplashInput) handle KD mode and termios
        // restoration on their own.
        log::clear_tui_active();
    }
}
