//! Splash-framebuffer render helpers (image-splash feature only).
//!
//! Ratatui emits vt100 bytes into a buffer; `alacritty_terminal::Term`
//! parses those bytes into a cell grid; the compositor blits the grid
//! onto the DRM framebuffer. A fresh `SplashTerminal` per frame is the
//! simplest way to guarantee the grid reflects only the current frame's
//! bytes.

#[cfg(feature = "image-splash")]
use alacritty_terminal::term::cell::Flags;
#[cfg(feature = "image-splash")]
use alacritty_terminal::vte::ansi::{Color, NamedColor};
#[cfg(feature = "image-splash")]
use ratatui::Terminal;
#[cfg(feature = "image-splash")]
use ratatui::TerminalOptions;
#[cfg(feature = "image-splash")]
use ratatui::Viewport;
#[cfg(feature = "image-splash")]
use ratatui::backend::CrosstermBackend;
#[cfg(feature = "image-splash")]
use ratatui::layout::Rect;

#[cfg(feature = "image-splash")]
use crate::splash::terminal::SplashTerminal;
#[cfg(feature = "image-splash")]
use crate::splash::types::CellDims;
#[cfg(feature = "image-splash")]
use crate::splash::{compositor, drm, glyph_cache};

#[cfg(feature = "image-splash")]
use crate::error::{NmblError, Result};
#[cfg(feature = "image-splash")]
use crate::ui::app::App;
#[cfg(feature = "image-splash")]
use crate::ui::screen_render::render_current_screen;

/// Render one frame: ratatui-draw → vte parse → cell-walk → blit.
///
/// The ratatui side emits absolute cursor positions every frame, but
/// `alacritty_terminal::Term` accumulates SGR state across feeds. A
/// fresh `SplashTerminal` per frame is the simplest way to guarantee
/// the grid reflects only the current frame's bytes.
#[cfg(feature = "image-splash")]
pub(crate) fn render_splash_frame(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    app: &App<'_>,
) -> Result<()> {
    render_splash_frame_with(drm, bg_scaled, cache, cell_dims, &mut |f| {
        render_current_screen(f, app);
    })
}

/// Generic counterpart to [`render_splash_frame`] that takes a render
/// closure instead of an `App`. Used by [`Console::draw_with`] on the
/// splash backend to paint dynamic widgets (network-rescue gauges,
/// cursor-tracking editors) that don't fit the App+Screen state
/// machine — the same compositor / cell-walk / blit pipeline is reused
/// so no parallel splash bring-up is performed.
#[cfg(feature = "image-splash")]
pub(crate) fn render_splash_frame_with(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let backend = CrosstermBackend::new(&mut buf);
        let viewport = Viewport::Fixed(Rect::new(0, 0, cell_dims.cols, cell_dims.rows));
        let mut terminal =
            Terminal::with_options(backend, TerminalOptions { viewport }).map_err(tui_err)?;
        terminal.draw(|f| body(f)).map_err(tui_err)?;
    }

    let mut term_pipe = SplashTerminal::new(cell_dims);
    term_pipe.feed(&buf);

    drm.render(|fb, fb_dims| {
        compositor::blit_background(fb, fb_dims, bg_scaled);
        // Pass 1: dark contrast halo behind glyphs on the transparent
        // default background, painted first so it only darkens the
        // background photo and never bleeds onto adjacent drawn text.
        term_pipe.for_each_cell(|col, row, cell| {
            if !compositor::wants_halo(cell.bg) {
                return;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let Some(glyph) = cache.get(cell.c, bold) else {
                return;
            };
            let x = u32::from(col).saturating_mul(cell_dims.cell_w);
            let y = u32::from(row).saturating_mul(cell_dims.cell_h);
            let rect = compositor::CellRect {
                x,
                y,
                w: cell_dims.cell_w,
                h: cell_dims.cell_h,
            };
            compositor::blit_halo(fb, fb_dims, glyph, rect);
        });
        // Pass 2: cell backgrounds + glyphs.
        term_pipe.for_each_cell(|col, row, cell| {
            if cell.c == ' ' && cell.bg == Color::Named(NamedColor::Background) {
                return;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let Some(glyph) = cache.get(cell.c, bold) else {
                return;
            };
            let fg = compositor::resolve_color(cell.fg);
            let bg = compositor::resolve_bg_color(cell.bg);
            let x = u32::from(col).saturating_mul(cell_dims.cell_w);
            let y = u32::from(row).saturating_mul(cell_dims.cell_h);
            let rect = compositor::CellRect {
                x,
                y,
                w: cell_dims.cell_w,
                h: cell_dims.cell_h,
            };
            compositor::blit_cell(fb, fb_dims, glyph, rect, fg, bg);
        });
        Ok(())
    })
}

#[cfg(feature = "image-splash")]
pub(crate) fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}
