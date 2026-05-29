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

use crate::config::{Config, SplashBackgroundLocation};
use crate::error::{NmblError, Result};
use crate::log;
use crate::nmbl_warn;
use crate::splash::drm::{SplashDrm, open_card_with_fallback};
use crate::splash::glyph_cache::{self, GlyphCache};
use crate::splash::input::SplashInput;
use crate::splash::png;
use crate::splash::scale;
use crate::splash::types::CellDims;

use background::load_sidecar_background_or_fallback;

pub(crate) mod background;
mod console_impl;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests;

/// Re-export for external callers that need the sidecar basename.
pub use background::SIDECAR_SPLASH_BG_BASENAME;

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
    pub(super) drm: SplashDrm,
    pub(super) bg_scaled: Vec<u8>,
    pub(super) cache: GlyphCache,
    pub(super) cell_dims: CellDims,
    pub(super) input: SplashInput,
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
        //
        //    Two sources, selected by `splash.background_location`:
        //    * `Initrd` (default): decode the embedded PNG at
        //      `splash.background_image`. A decode failure here is a
        //      real bring-up error (the asset is baked into the
        //      initramfs and must be present) and propagates as today.
        //    * `BootPartition`: decode the sidecar PNG staged next to
        //      the initrd on the boot partition, resolved against the
        //      Phase-0.5 mountpoint. Phase ordering: in bootstrap mode
        //      `run_bootstrap_phase` mounts the boot partition and sets
        //      `runtime_boot_mountpoint` BEFORE `open_console` runs, so
        //      the file is reachable here. If the mountpoint is unknown
        //      (legacy embedded-config mode) or the PNG is
        //      missing/unreadable/corrupt, we WARN and fall back to a
        //      solid background — never panic, never block boot. This
        //      mirrors how `rescue::disk` treats a missing
        //      `nmbl-rescue.sfs` on the boot partition.
        let bg_scaled = match config.splash.background_location {
            SplashBackgroundLocation::Initrd => {
                let bg_image = png::decode_rgba(&config.splash.background_image)?;
                scale::cover_scale_nearest(&bg_image.rgba, bg_image.width, bg_image.height, fb_dims)
            }
            SplashBackgroundLocation::BootPartition => {
                load_sidecar_background_or_fallback(config, fb_dims)
            }
        };

        // 3. Load the font and derive grid dimensions from the cell size.
        //
        //    Try the configured on-disk font first. On ANY load error
        //    (missing file, unreadable, corrupt/unsupported face) WARN
        //    and fall back to the DejaVu Sans Mono baked into the binary
        //    so a bad operator font degrades gracefully instead of
        //    dropping splash entirely. Mirrors how the sidecar
        //    background falls back to a solid fill.
        let cache = match glyph_cache::load(&config.splash.font_path, SPLASH_FONT_PX) {
            Ok(cache) => cache,
            Err(e) => {
                nmbl_warn!(
                    "splash: failed to load font {} ({e}); using embedded fallback",
                    config.splash.font_path.display()
                );
                glyph_cache::load_embedded_fallback(SPLASH_FONT_PX)?
            }
        };
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
