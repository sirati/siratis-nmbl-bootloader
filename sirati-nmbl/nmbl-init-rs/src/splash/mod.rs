//! Optional graphical boot splash.
//!
//! Gated behind the `image-splash` Cargo feature. When the feature is
//! enabled and `config.splash.enable == true`, [`try_run_selector`]
//! drives the boot menu through a DRM framebuffer instead of through
//! `/dev/console`. Every failure path returns `Ok(None)` or `Err(_)`
//! so the caller can fall back to today's tty UI.
//!
//! Submodule layout: [`drm`] owns the framebuffer, [`png`] decodes the
//! background image, [`scale`] cover-scales it to the framebuffer,
//! [`glyph_cache`] pre-rasterises the font, [`terminal`] runs an
//! `alacritty_terminal::Term` over the ratatui-rendered ANSI bytes,
//! and [`compositor`] paints cells into the framebuffer. The orchestrator
//! lives here.

pub mod compositor;
pub mod drm;
pub mod glyph_cache;
pub mod png;
pub mod scale;
pub mod terminal;
pub mod types;

use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use crossterm::event::{self, Event};
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::splash::terminal::SplashTerminal;
use crate::splash::types::CellDims;
use crate::sys::tty::open_console;
use crate::ui::POLL_SLICE;
use crate::ui::render_current_screen;
use crate::ui::timeout::{TimeoutOutcome, run_countdown};
use crate::ui::{App, Decision};

/// Console node opened to acquire raw-mode keyboard input alongside
/// the DRM framebuffer output.
const CONSOLE_PATH: &str = "/dev/console";

/// Font size, in pixels, used to rasterise the splash glyph cache.
const SPLASH_FONT_PX: f32 = 16.0;

/// Attempt to drive the boot menu through the splash renderer.
///
/// - `Ok(Some(decision))`: splash rendered and the operator chose.
/// - `Ok(None)`: splash is unavailable (no DRM device, no assets);
///   caller should fall back to the tty UI without surfacing an error.
/// - `Err(_)`: splash was attempted and failed mid-flight; caller logs
///   and falls back to the tty UI.
pub fn try_run_selector(
    config: &Config,
    generations: &[Generation],
) -> Result<Option<Decision>> {
    // 1. Open the DRM card. Missing / inaccessible nodes map to
    //    `Ok(None)` inside `open_card`, so this propagates only real
    //    bring-up errors.
    let mut drm = match drm::open_card(&config.splash.dri_path)? {
        Some(d) => d,
        None => return Ok(None),
    };
    let fb_dims = drm.dims();

    // 2. Load the background PNG and cover-scale it to the framebuffer.
    let bg_image = png::decode_rgba(&config.splash.background_image)?;
    let bg_scaled =
        scale::cover_scale_nearest(&bg_image.rgba, bg_image.width, bg_image.height, fb_dims);

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

    // 4. Open /dev/console for raw-mode keyboard input. crossterm's
    //    `event::poll` will read from stdin which the kernel pointed
    //    at /dev/console in early userspace.
    let console = open_console(Path::new(CONSOLE_PATH))?;
    let _raw = crate::sys::tty::RawModeGuard::new(console.as_fd())?;

    // 5. Build the App and the headless terminal pipeline.
    let mut app = App::new(generations);
    app.show_kernel_params = config.tui.show_kernel_params;
    let mut term_pipe = SplashTerminal::new(cell_dims);

    // 6. Countdown phase: drive run_countdown; redraw on each tick.
    //    Drawing errors during the countdown shouldn't tear down the
    //    boot — log via stderr (via the `_ = ` discard) and continue.
    let countdown = Duration::from_secs(u64::from(config.general.timeout_secs));
    let countdown_outcome = {
        let mut on_tick = |secs: u64| {
            app.countdown_remaining_secs = Some(secs);
            let _ = render_frame(&mut drm, &bg_scaled, &cache, cell_dims, &mut term_pipe, &app);
        };
        run_countdown(countdown, &mut on_tick)?
    };
    app.countdown_remaining_secs = None;

    if matches!(countdown_outcome, TimeoutOutcome::Expired) && app.decision.is_none() {
        return Ok(Some(Decision::Boot {
            generation_index: 0,
            cmdline_override: None,
        }));
    }

    // 7. Event loop: dirty/poll/redraw until App::on_key returns true
    //    or App::decision is filled by a side-effecting key.
    let mut dirty = true;
    loop {
        if dirty {
            render_frame(&mut drm, &bg_scaled, &cache, cell_dims, &mut term_pipe, &app)?;
            dirty = false;
        }
        if event::poll(POLL_SLICE).map_err(tui_err)?
            && let Event::Key(key) = event::read().map_err(tui_err)?
        {
            if app.on_key(key) {
                break;
            }
            dirty = true;
        }
        if app.decision.is_some() {
            break;
        }
    }

    match app.decision {
        Some(d) => Ok(Some(d)),
        None => Err(NmblError::Tui {
            source: std::io::Error::other("splash exited without decision"),
        }),
    }
}

/// Render one frame: ratatui-draw → vte parse → cell-walk → blit.
///
/// The ratatui side emits absolute cursor positions every frame, but
/// `alacritty_terminal::Term` accumulates SGR state across feeds. A
/// fresh `SplashTerminal` per frame is the simplest way to guarantee
/// the grid reflects only the current frame's bytes.
fn render_frame(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    term_pipe: &mut SplashTerminal,
    app: &App<'_>,
) -> Result<()> {
    // (a) Render the current App screen into a Vec<u8> by way of a
    //     ratatui CrosstermBackend over the vec. `Viewport::Fixed`
    //     bypasses crossterm's terminal::size() call — we use the
    //     dimensions we derived from the framebuffer and font.
    let mut buf: Vec<u8> = Vec::new();
    {
        let backend = CrosstermBackend::new(&mut buf);
        let viewport = Viewport::Fixed(Rect::new(0, 0, cell_dims.cols, cell_dims.rows));
        let mut terminal = Terminal::with_options(backend, TerminalOptions { viewport })
            .map_err(tui_err)?;
        terminal
            .draw(|f| render_current_screen(f, app))
            .map_err(tui_err)?;
    }

    // (b) Reset the alacritty Term so SGR state from the previous frame
    //     can't bleed into this one, then feed the ratatui bytes.
    *term_pipe = SplashTerminal::new(cell_dims);
    term_pipe.feed(&buf);

    // (c) Blit the background then walk every cell and stamp glyphs.
    drm.render(|fb, fb_dims| {
        compositor::blit_background(fb, fb_dims, bg_scaled);
        term_pipe.for_each_cell(|col, row, cell| {
            // Skip cells that are default-bg spaces: the PNG already
            // shows through and there's no glyph contribution.
            if cell.c == ' ' && cell.bg == Color::Named(NamedColor::Background) {
                return;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let Some(glyph) = cache.get(cell.c, bold) else {
                return;
            };
            let fg = compositor::resolve_color(cell.fg);
            let bg = compositor::resolve_color(cell.bg);
            let x = u32::from(col).saturating_mul(cell_dims.cell_w);
            let y = u32::from(row).saturating_mul(cell_dims.cell_h);
            compositor::blit_cell(fb, fb_dims, glyph, x, y, fg, bg);
        });
        Ok(())
    })
}

fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}
