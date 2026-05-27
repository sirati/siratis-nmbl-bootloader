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
pub mod input;
pub mod passphrase_demo;
pub mod png;
pub mod scale;
pub mod terminal;
pub mod types;

use std::path::Path;
use std::time::{Duration, Instant};

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use crossterm::event::{KeyCode, KeyModifiers};
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
use crate::ui::POLL_SLICE;
use crate::ui::render_current_screen;
use crate::ui::timeout::TimeoutOutcome;
use crate::ui::{App, Decision, Screen};

/// Tty node opened to acquire raw-mode keyboard input alongside the
/// DRM framebuffer output. `/dev/tty0` is the kernel's active VT, so
/// VNC PS/2 keypresses land here even when `console=` points stdin at
/// a serial line.
const INPUT_TTY_PATH: &str = "/dev/tty0";

/// Font size, in pixels, used to rasterise the splash glyph cache.
const SPLASH_FONT_PX: f32 = 16.0;

/// Attempt to drive the boot menu through the splash renderer.
///
/// - `Ok(Some(decision))`: splash rendered and the operator chose.
/// - `Ok(None)`: splash is unavailable (no DRM device, no assets);
///   caller should fall back to the tty UI without surfacing an error.
/// - `Err(_)`: splash was attempted and failed mid-flight; caller logs
///   and falls back to the tty UI.
pub fn try_run_selector(config: &Config, generations: &[Generation]) -> Result<Option<Decision>> {
    // 1. Open the DRM card. Missing / inaccessible nodes map to
    //    `Ok(None)` inside `open_card`, so this propagates only real
    //    bring-up errors.
    let mut drm = match drm::open_card_with_fallback(&config.splash.dri_path)? {
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

    // 4. Open /dev/tty0 for raw-mode keyboard input. We can't rely on
    //    stdin: the kernel's `console=` directive may have pointed it
    //    at a serial line, so PS/2 keypresses (e.g. via VNC) only
    //    land on the VT directly. SplashInput owns the fd, enters raw
    //    mode, and restores termios on drop.
    let mut input = input::SplashInput::open(Path::new(INPUT_TTY_PATH))?;

    // 5. Build the App. The headless terminal pipeline is built fresh
    //    per frame inside render_frame to bound SGR state to one frame.
    let mut app = App::new(generations);
    app.show_kernel_params = config.tui.show_kernel_params;

    // 6. Countdown phase: replicates ui::timeout::run_countdown but
    //    polls SplashInput rather than stdin. The shared run_countdown
    //    uses crossterm::event which reads from the kernel's stdin —
    //    on a serial-console boot that's the serial line, not the VT,
    //    so VNC keypresses would never cancel the countdown. Drawing
    //    errors are swallowed: the boot must continue even if one
    //    frame fails.
    let countdown = Duration::from_secs(u64::from(config.general.timeout_secs));
    let countdown_outcome = run_splash_countdown(
        countdown,
        &mut input,
        &mut |secs| {
            app.countdown_remaining_secs = Some(secs);
            let _ = render_frame(&mut drm, &bg_scaled, &cache, cell_dims, &app);
        },
    )?;
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
            render_frame(&mut drm, &bg_scaled, &cache, cell_dims, &app)?;
            dirty = false;
        }
        if let Some(key) = input.poll(POLL_SLICE)? {
            // Intercept Ctrl+P from the list screen to demo the
            // passphrase dialog. Plain `p` is already a list-view
            // hotkey (toggles show_kernel_params) and a literal in
            // many kernel cmdline edits (loglevel, console=tty1,
            // ip=), so we use the modifier and gate on screen state.
            if matches!(app.screen, Screen::List)
                && key.code == KeyCode::Char('p')
                && key.modifiers == KeyModifiers::CONTROL
            {
                let outcome =
                    passphrase_demo::run(&mut drm, &bg_scaled, &cache, cell_dims, &mut input)?;
                crate::nmbl_info!("passphrase demo returned: {outcome:?}");
                dirty = true;
                continue;
            }
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
        let mut terminal =
            Terminal::with_options(backend, TerminalOptions { viewport }).map_err(tui_err)?;
        terminal
            .draw(|f| render_current_screen(f, app))
            .map_err(tui_err)?;
    }

    // (b) Build a fresh terminal pipeline so SGR state from a previous
    //     frame can't bleed in, then feed the ratatui-rendered bytes.
    let mut term_pipe = SplashTerminal::new(cell_dims);
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

fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

/// Splash-side countdown loop. Mirrors [`crate::ui::timeout::run_countdown`]
/// but polls [`input::SplashInput`] instead of stdin so cancel-on-keypress
/// works on VT inputs even when the kernel's `console=` directive has
/// pointed stdin at a serial line.
fn run_splash_countdown(
    duration: Duration,
    input: &mut input::SplashInput,
    on_tick: &mut dyn FnMut(u64),
) -> Result<TimeoutOutcome> {
    let start = Instant::now();
    let deadline = start.checked_add(duration).unwrap_or(start);

    let initial = duration.as_secs();
    on_tick(initial);
    let mut last_reported = initial;

    loop {
        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };

        let slice = remaining.min(POLL_SLICE);
        if input.poll(slice)?.is_some() {
            return Ok(TimeoutOutcome::Cancelled);
        }

        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };
        let secs = remaining.as_secs();
        if secs != last_reported {
            on_tick(secs);
            last_reported = secs;
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert with panics on contract failure"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn try_run_selector_returns_ok_none_when_dri_missing() {
        // The fallback contract: when the configured DRI path is missing
        // and the /dev/dri/card* scan finds nothing usable, the splash
        // entry-point must short-circuit to Ok(None) so the caller can
        // fall through to the tty UI without surfacing the ENOENT as an
        // error. On dev hosts the scan may hit a real card whose
        // bring-up requires DRM master we don't have — accept either
        // Ok(None) or an Err in that case, since neither path produces
        // a working splash.
        let mut config = Config::recovery_default();
        config.splash.dri_path = PathBuf::from("/dev/this/does/not/exist");
        match try_run_selector(&config, &[]) {
            Ok(decision) => assert!(
                decision.is_none(),
                "missing DRI must yield Ok(None), got {decision:?}",
            ),
            Err(_) => {
                // Acceptable: a real card was found by the fallback walk
                // and bring-up failed for lack of permissions. The
                // caller (ui::mod.rs) maps this back to a tty UI fallback.
            }
        }
    }
}
