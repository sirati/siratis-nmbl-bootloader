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

use crossterm::event::KeyEvent;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::splash::drm::{SplashDrm, open_card_with_fallback};
use crate::splash::glyph_cache::{self, GlyphCache};
use crate::splash::input::SplashInput;
use crate::splash::png;
use crate::splash::scale;
use crate::splash::types::CellDims;
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleKind};
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

    fn poll_key(&mut self, timeout: Duration) -> Result<Option<KeyEvent>> {
        // Cap the effective wait the same way [`TtyConsole`] does so
        // backends are uniformly responsive to ticking countdowns and
        // spinner animations. The caller-supplied timeout is honoured
        // but never longer than POLL_SLICE per call; the trait doc
        // pins this contract for both backends.
        let slice = timeout.min(POLL_SLICE);
        self.input.poll(slice)
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
}
