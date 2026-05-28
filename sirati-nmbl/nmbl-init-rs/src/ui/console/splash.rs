//! DRM-framebuffer backend for the [`Console`] abstraction.
//!
//! Owns: a [`SplashDrm`] for mode-set + flip, the pre-scaled
//! background, the [`GlyphCache`] for font rasterisation, [`CellDims`]
//! for the cell grid, and a [`SplashInput`] for raw-mode key reads via
//! `/dev/tty1`. Rendering goes through the pre-existing
//! [`crate::ui::render_splash_frame`] pipeline — ratatui-draw → vte
//! parse → cell-walk → blit — so the splash-side `run_splash_selector`
//! and this trait impl stay byte-identical at the framebuffer level.
//!
//! No new `unsafe` is introduced; all syscalls flow through the splash
//! primitives' existing rustix-based wrappers.

use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::log;
use crate::nmbl_warn;
use crate::splash::drm::{SplashDrm, open_card_with_fallback};
use crate::splash::glyph_cache::{self, GlyphCache};
use crate::splash::input::SplashInput;
use crate::splash::png;
use crate::splash::scale;
use crate::splash::types::CellDims;
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::{render_splash_frame, render_splash_frame_with};

/// Tty node opened for raw-mode keyboard input alongside the DRM
/// framebuffer output. See [`crate::ui::INPUT_TTY_PATH`] for the
/// rationale; we mirror that constant here so this module is
/// self-contained.
const INPUT_TTY_PATH: &str = "/dev/tty1";

/// Font size, in pixels, used to rasterise the splash glyph cache.
/// Same value as the existing `crate::ui::SPLASH_FONT_PX`.
const SPLASH_FONT_PX: f32 = 16.0;

/// DRM-backed console. Constructed via [`SplashConsole::open`].
pub struct SplashConsole {
    drm: SplashDrm,
    bg_scaled: Vec<u8>,
    cache: GlyphCache,
    cell_dims: CellDims,
    input: SplashInput,
}

impl SplashConsole {
    /// Bring up the splash backend.
    ///
    /// Returns `Ok(Some(_))` on a clean bring-up, `Ok(None)` when the
    /// backend is unavailable (no DRM device, no font, etc.; the
    /// orchestrator falls back to tty), and `Err(_)` only when a
    /// real, surfaced bring-up error occurred mid-flight.
    pub fn open(config: &Config) -> Result<Option<SplashConsole>> {
        // 1. Open the DRM card. Missing / inaccessible nodes map to
        //    `Ok(None)` inside `open_card_with_fallback`, so this
        //    propagates only real bring-up errors.
        let drm = match open_card_with_fallback(&config.splash.dri_path)? {
            Some(d) => d,
            None => return Ok(None),
        };
        let fb_dims = drm.dims();

        // 2. Load the background PNG and cover-scale it to the framebuffer.
        let bg_image = png::decode_rgba(&config.splash.background_image)?;
        let bg_scaled = scale::cover_scale_nearest(
            &bg_image.rgba,
            bg_image.width,
            bg_image.height,
            fb_dims,
        );

        // 3. Load the font and derive grid dimensions from the cell size.
        let cache = glyph_cache::load(&config.splash.font_path, SPLASH_FONT_PX)?;
        let cell_size = cache.cell_size();
        let cell_w = cell_size.w.max(1);
        let cell_h = cell_size.h.max(1);
        let cols = (fb_dims.w / cell_w).min(u32::from(u16::MAX)) as u16;
        let rows = (fb_dims.h / cell_h).min(u32::from(u16::MAX)) as u16;
        if cols == 0 || rows == 0 {
            return Err(NmblError::Tui {
                source: std::io::Error::other("splash framebuffer too small for one cell"),
            });
        }
        let cell_dims = CellDims {
            cols,
            rows,
            cell_w,
            cell_h,
        };

        // 4. Open /dev/tty1 for raw-mode keyboard input.
        let input = SplashInput::open(Path::new(INPUT_TTY_PATH))?;

        // The splash bring-up sequence already calls KDSETMODE(KD_GRAPHICS)
        // on /dev/tty1, which suppresses kernel printk to that VT. We
        // still flip the macro gate so `nmbl_*!` stops writing to
        // stderr (which would race the ratatui repaint on the splash
        // framebuffer and also leak to any serial line registered as a
        // secondary console).
        log::set_tui_active();

        Ok(Some(SplashConsole {
            drm,
            bg_scaled,
            cache,
            cell_dims,
            input,
        }))
    }

    /// Borrow the cell-grid dimensions. Useful for callers that need
    /// to lay out modals against the grid without re-querying through
    /// the trait.
    pub fn cell_dims(&self) -> CellDims {
        self.cell_dims
    }
}

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

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
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
