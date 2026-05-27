//! UI orchestrator. The terminal lifecycle (raw-mode acquisition,
//! ratatui `Terminal` construction, frame loop, serial-console
//! fallback) lives here; pure render functions live in [`view`] and
//! the state machine lives in [`app`].
//!
//! ## Terminal handling
//!
//! In PID-1-as-init we open `/dev/console` ourselves via
//! [`crate::sys::tty::open_console`], wrap it in a
//! [`crate::sys::tty::RawModeGuard`], and then construct a
//! [`ratatui::Terminal`] over [`std::io::stdout`]. We rely on the
//! kernel having pointed stdout (fd 1) at `/dev/console` already —
//! the standard early-userspace contract. We do NOT dup2 the
//! console fd to stdout: that would require `unsafe libc::dup2`
//! with no obvious win, and the project rule is to keep `unsafe`
//! to an absolute minimum.
//!
//! ## Serial-console fallback
//!
//! When `config.general.serial_console` is true we skip raw mode
//! and ratatui altogether and run a line-oriented prompt against
//! stdin/stdout. Many serial environments mangle escape sequences;
//! line mode is reliable and the operator can still drop to the
//! shell or pick a generation.
//!
//! ## Activation passphrase wiring
//!
//! [`TuiPasswordSupplier`] implements
//! [`crate::activation::PasswordSupplier`] so the top-level boot
//! flow can pass it to
//! [`crate::activation::run_all_activations`] as
//! `&mut dyn PasswordSupplier`. When the activation runner reaches
//! a `luks-password` entry it calls `prompt(label)` once; we open
//! the console, render the [`Screen::Passphrase`] modal, and return
//! the entered string in a `Zeroizing<String>` so the buffer is
//! wiped after `cryptsetup` drains it. Esc on the modal returns a
//! [`NmblError::Tui`] which `run_all_activations` wraps as
//! [`NmblError::Activation`] and the top-level driver routes to the
//! emergency shell.

pub mod app;
pub mod console;
pub mod emergency;
pub mod timeout;
pub mod view;

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::Duration;

use crossterm::event::{self, Event};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use zeroize::Zeroizing;

use crate::activation::PasswordSupplier;
use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::sys::tty::{RawModeGuard, open_console};
use crate::ui::view::{
    EditScreenData, EmergencyScreenData, ListScreenData, PassphraseScreenData, render_edit,
    render_emergency, render_list, render_passphrase,
};

/// Default console path used by the activation-phase passphrase modal
/// when the splash isn't owning the framebuffer anymore.
const CONSOLE_PATH: &str = "/dev/console";

#[cfg(feature = "image-splash")]
use std::time::Instant;
#[cfg(feature = "image-splash")]
use alacritty_terminal::term::cell::Flags;
#[cfg(feature = "image-splash")]
use alacritty_terminal::vte::ansi::{Color, NamedColor};
#[cfg(feature = "image-splash")]
use crossterm::event::{KeyCode, KeyModifiers};
#[cfg(feature = "image-splash")]
use ratatui::TerminalOptions;
#[cfg(feature = "image-splash")]
use ratatui::Viewport;
#[cfg(feature = "image-splash")]
use ratatui::layout::Rect;
#[cfg(feature = "image-splash")]
use crate::splash::{compositor, drm, glyph_cache, input, passphrase_demo, png, scale};
#[cfg(feature = "image-splash")]
use crate::splash::terminal::SplashTerminal;
#[cfg(feature = "image-splash")]
use crate::splash::types::CellDims;
#[cfg(feature = "image-splash")]
use crate::ui::timeout::TimeoutOutcome;

pub use app::{App, Decision, EmergencyChoice, EmergencyItem, Screen};
pub use emergency::run_emergency_screen;

/// Slice we wait on input per iteration. Shared by the event loop and
/// the countdown ticker so they have the same responsiveness profile
/// and only one knob to tune.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(100);

/// Tty node opened to acquire raw-mode keyboard input alongside the
/// DRM framebuffer output. `/dev/tty0` is the kernel's "current VT" —
/// write-only per the device docs — so reads return nothing. The
/// kernel routes PS/2 (and VNC) keypresses to `/dev/tty1`, which we
/// open directly so they land in our SplashInput buffer even when
/// `console=` points stdin at a serial line.
#[cfg(feature = "image-splash")]
const INPUT_TTY_PATH: &str = "/dev/tty1";

/// Font size, in pixels, used to rasterise the splash glyph cache.
#[cfg(feature = "image-splash")]
const SPLASH_FONT_PX: f32 = 16.0;

/// Run the boot-selection TUI and return the operator's decision.
///
/// Falls back to a line-oriented serial prompt when the config opts
/// in via `general.serial_console`.
pub fn run_selector(config: &Config, generations: &[Generation]) -> Result<Decision> {
    if config.general.serial_console {
        return select_generation_serial(config, generations);
    }

    #[cfg(feature = "image-splash")]
    if config.splash.enable {
        match run_splash_selector(config, generations)? {
            Some(d) => return Ok(d),
            None => {
                crate::nmbl_warn!(
                    "splash unavailable; falling back to serial prompt on stdin"
                );
                return select_generation_serial(config, generations);
            }
        }
    }

    // No splash available (either not enabled or feature not built in)
    // and serial mode not requested: drop to the line-oriented prompt
    // anyway. It works on any console the kernel pointed stdin at.
    select_generation_serial(config, generations)
}

/// Live splash-console handles. Holds everything `render_splash_frame`
/// needs plus the input source; lets a caller paint a frame and poll
/// for keys without owning the bring-up details.
///
/// Constructed via [`open_splash_console`]; the rest of the splash
/// orchestration (`run_splash_selector`, `run_emergency_screen`) calls
/// `render` and `poll` against this handle so the bring-up boilerplate
/// is centralised.
#[cfg(feature = "image-splash")]
pub(crate) struct SplashConsole {
    drm: drm::SplashDrm,
    bg_scaled: Vec<u8>,
    cache: glyph_cache::GlyphCache,
    cell_dims: CellDims,
    input: input::SplashInput,
}

#[cfg(feature = "image-splash")]
impl SplashConsole {
    /// Paint one frame of `app` to the splash framebuffer.
    pub(crate) fn render(&mut self, app: &App<'_>) -> Result<()> {
        render_splash_frame(
            &mut self.drm,
            &self.bg_scaled,
            &self.cache,
            self.cell_dims,
            app,
        )
    }

    /// Poll the splash input source for a key event.
    pub(crate) fn poll(&mut self, timeout: Duration) -> Result<Option<crossterm::event::KeyEvent>> {
        self.input.poll(timeout)
    }
}

/// Bring up the splash backend so callers can render the TUI over it.
///
/// Returns `Ok(Some(console))` on a clean bring-up, `Ok(None)` when
/// the splash backend is unavailable (no DRM device, no font, etc.),
/// and `Err(_)` when bring-up failed mid-flight.
#[cfg(feature = "image-splash")]
pub(crate) fn open_splash_console(config: &Config) -> Result<Option<SplashConsole>> {
    // 1. Open the DRM card. Missing / inaccessible nodes map to
    //    `Ok(None)` inside `open_card_with_fallback`, so this
    //    propagates only real bring-up errors.
    let drm = match drm::open_card_with_fallback(&config.splash.dri_path)? {
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

    // 4. Open /dev/tty1 for raw-mode keyboard input.
    let input = input::SplashInput::open(Path::new(INPUT_TTY_PATH))?;

    Ok(Some(SplashConsole {
        drm,
        bg_scaled,
        cache,
        cell_dims,
        input,
    }))
}

/// Run the graphical boot selector backed by DRM + alacritty + SplashInput.
///
/// Returns `Ok(Some(decision))` on a clean operator choice, `Ok(None)`
/// when the splash backend is unavailable (no DRM device, no font, etc.),
/// and `Err(_)` when bring-up failed mid-flight.
#[cfg(feature = "image-splash")]
fn run_splash_selector(
    config: &Config,
    generations: &[Generation],
) -> Result<Option<Decision>> {
    let Some(mut console) = open_splash_console(config)? else {
        return Ok(None);
    };

    // 5. Build the App. The headless terminal pipeline is built fresh
    //    per frame inside render_frame to bound SGR state to one frame.
    let mut app = App::new(generations);
    app.show_kernel_params = config.tui.show_kernel_params;

    // 6. Countdown phase.
    let countdown = Duration::from_secs(u64::from(config.general.timeout_secs));
    let countdown_outcome = run_splash_countdown(
        countdown,
        &mut console.input,
        &mut |secs| {
            app.countdown_remaining_secs = Some(secs);
            let _ = render_splash_frame(
                &mut console.drm,
                &console.bg_scaled,
                &console.cache,
                console.cell_dims,
                &app,
            );
        },
    )?;
    app.countdown_remaining_secs = None;

    if matches!(countdown_outcome, TimeoutOutcome::Expired) && app.decision.is_none() {
        return Ok(Some(Decision::Boot {
            generation_index: 0,
            cmdline_override: None,
        }));
    }

    // 7. Event loop.
    let mut dirty = true;
    loop {
        if dirty {
            console.render(&app)?;
            dirty = false;
        }
        if let Some(key) = console.poll(POLL_SLICE)? {
            // Intercept Ctrl+P from the list screen to demo the
            // passphrase dialog. Plain `p` is already a list-view
            // hotkey (toggles show_kernel_params) and a literal in
            // many kernel cmdline edits (loglevel, console=tty1,
            // ip=), so we use the modifier and gate on screen state.
            if matches!(app.screen, Screen::List)
                && key.code == KeyCode::Char('p')
                && key.modifiers == KeyModifiers::CONTROL
            {
                let outcome = passphrase_demo::run(
                    &mut console.drm,
                    &console.bg_scaled,
                    &console.cache,
                    console.cell_dims,
                    &mut console.input,
                )?;
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

/// Splash-side countdown loop. Mirrors [`crate::ui::timeout::run_countdown`]
/// but polls [`input::SplashInput`] instead of stdin so cancel-on-keypress
/// works on VT inputs even when the kernel's `console=` directive has
/// pointed stdin at a serial line.
#[cfg(feature = "image-splash")]
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

    let mut term_pipe = SplashTerminal::new(cell_dims);
    term_pipe.feed(&buf);

    drm.render(|fb, fb_dims| {
        compositor::blit_background(fb, fb_dims, bg_scaled);
        term_pipe.for_each_cell(|col, row, cell| {
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

/// Dispatch render based on which screen the App is currently on.
///
/// `pub(crate)` so the splash orchestrator can reuse the same dispatch
/// without forking the per-screen branching.
pub(crate) fn render_current_screen(frame: &mut ratatui::Frame<'_>, app: &App<'_>) {
    match &app.screen {
        Screen::List => render_list(frame, &list_data(app)),
        Screen::Editing {
            generation_index,
            buffer,
            cursor,
        } => {
            if let Some(g) = app.generations.get(*generation_index) {
                let data = EditScreenData {
                    generation: g,
                    edited_cmdline: buffer,
                    cursor_position: *cursor,
                };
                render_edit(frame, &data);
            }
        }
        Screen::Passphrase {
            prompt_label,
            buffer,
        } => {
            let data = PassphraseScreenData {
                prompt_label,
                buffer_len: buffer.len(),
            };
            render_passphrase(frame, &data);
        }
        Screen::Emergency {
            message,
            items,
            selected,
            ..
        } => {
            let data = EmergencyScreenData {
                message,
                items,
                selected_index: *selected,
                countdown_remaining_secs: app.countdown_remaining_secs,
            };
            render_emergency(frame, &data);
        }
    }
}

fn list_data<'a>(app: &'a App<'a>) -> ListScreenData<'a> {
    ListScreenData {
        generations: app.generations,
        selected_index: app.selected_index,
        countdown_remaining_secs: app.countdown_remaining_secs,
        show_kernel_params: app.show_kernel_params,
    }
}

/// Line-oriented fallback for serial consoles. The protocol is
/// intentionally trivial — operators on broken serial lines can drive
/// it by hand. Commands:
///   - empty line or "boot"          → boot default (index 0)
///   - integer N (1-based)           → boot the Nth generation
///   - "edit N" or "edit"            → boot Nth with edited cmdline
///   - "shell" or "s"                → drop to emergency shell
///   - "reboot" or "q"               → reboot
fn select_generation_serial(_config: &Config, generations: &[Generation]) -> Result<Decision> {
    let stdout = std::io::stdout();
    let stdin = std::io::stdin();

    {
        let mut out = stdout.lock();
        writeln!(out, "[nmbl] Serial console selector").map_err(tui_err)?;
        for (i, g) in generations.iter().enumerate() {
            let label = if g.label.is_empty() {
                String::new()
            } else {
                format!(" {}", g.label)
            };
            writeln!(out, "  {}) #{}{}", i.saturating_add(1), g.number, label).map_err(tui_err)?;
        }
        writeln!(
            out,
            "Enter number to boot, 'edit N' to edit cmdline, 'shell', or 'reboot':"
        )
        .map_err(tui_err)?;
        out.flush().map_err(tui_err)?;
    }

    let mut line = String::new();
    stdin.lock().read_line(&mut line).map_err(tui_err)?;
    let trimmed = line.trim();
    let last_idx = generations.len().saturating_sub(1);

    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("boot") {
        return Ok(Decision::Boot {
            generation_index: 0,
            cmdline_override: None,
        });
    }
    if trimmed.eq_ignore_ascii_case("shell") || trimmed == "s" {
        return Ok(Decision::Shell);
    }
    if trimmed.eq_ignore_ascii_case("reboot") || trimmed == "q" {
        return Ok(Decision::Reboot);
    }
    if let Some(rest) = trimmed.strip_prefix("edit") {
        let idx = parse_serial_index(rest.trim(), last_idx)?;
        let original = generations
            .get(idx)
            .map(|g| g.kernel_params.join(" "))
            .unwrap_or_default();
        let edited = prompt_serial_line(&format!("cmdline [{original}]: "))?;
        let override_str = if edited.trim().is_empty() {
            original
        } else {
            edited
        };
        return Ok(Decision::Boot {
            generation_index: idx,
            cmdline_override: Some(override_str),
        });
    }
    let idx = parse_serial_index(trimmed, last_idx)?;
    Ok(Decision::Boot {
        generation_index: idx,
        cmdline_override: None,
    })
}

/// Parse a 1-based generation number from a serial response and clamp
/// it into the legal range. Empty input is treated as "first entry".
fn parse_serial_index(input: &str, last_idx: usize) -> Result<usize> {
    if input.is_empty() {
        return Ok(0);
    }
    let n: usize = input.parse().map_err(|_| NmblError::Tui {
        source: std::io::Error::other(format!("serial input {input:?} is not a number")),
    })?;
    let zero_based = n.saturating_sub(1);
    Ok(zero_based.min(last_idx))
}

/// Prompt for and read a single trimmed line of serial input.
fn prompt_serial_line(prompt: &str) -> Result<String> {
    let stdout = std::io::stdout();
    {
        let mut out = stdout.lock();
        write!(out, "{prompt}").map_err(tui_err)?;
        out.flush().map_err(tui_err)?;
    }
    let mut buf = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buf)
        .map_err(tui_err)?;
    // Strip exactly one trailing newline; don't trim user-meaningful
    // spaces from inside the cmdline.
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(buf)
}

/// PasswordSupplier impl that pops a passphrase modal in the same
/// kind of terminal the rest of the TUI uses, or — when serial — does
/// a line-mode `getpass`-style read.
pub struct TuiPasswordSupplier {
    pub config_serial: bool,
}

impl TuiPasswordSupplier {
    pub fn new(config: &Config) -> Self {
        Self {
            config_serial: config.general.serial_console,
        }
    }
}

impl PasswordSupplier for TuiPasswordSupplier {
    fn prompt(&mut self, label: &str) -> Result<Zeroizing<String>> {
        if self.config_serial {
            return serial_passphrase_prompt(label);
        }
        tui_passphrase_prompt(label)
    }
}

/// Best-effort serial passphrase prompt. We can't reliably disable
/// echo on every serial line discipline; we print the masking-disabled
/// notice so the operator knows.
fn serial_passphrase_prompt(label: &str) -> Result<Zeroizing<String>> {
    let stdout = std::io::stdout();
    {
        let mut out = stdout.lock();
        writeln!(out, "[nmbl] {label}").map_err(tui_err)?;
        write!(out, "Enter passphrase (input may be visible): ").map_err(tui_err)?;
        out.flush().map_err(tui_err)?;
    }
    let mut buf = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut buf)
        .map_err(tui_err)?;
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(Zeroizing::new(buf))
}

/// Render the passphrase modal under a raw-mode guard and poll keys
/// until the operator submits or cancels. Esc returns an error so the
/// caller can drop to the emergency shell.
///
/// The splash passphrase modal is deferred: activation runs after the
/// boot menu, so the DRM card may have been handed off back to the
/// kernel console already. Routing this through the tty path keeps the
/// passphrase prompt reliable across both splash and non-splash boots.
fn tui_passphrase_prompt(label: &str) -> Result<Zeroizing<String>> {
    let console = open_console(Path::new(CONSOLE_PATH))?;
    let _raw = RawModeGuard::new(console.as_fd())?;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).map_err(tui_err)?;

    // No generations to render — pass an empty slice. The App is
    // only used here for its Passphrase screen state.
    let empty: [Generation; 0] = [];
    let mut app = App::new(&empty);
    app.screen = Screen::Passphrase {
        prompt_label: label.to_string(),
        buffer: Zeroizing::new(String::new()),
    };

    let mut dirty = true;
    loop {
        if dirty {
            terminal
                .draw(|f| render_current_screen(f, &app))
                .map_err(tui_err)?;
            dirty = false;
        }

        if event::poll(POLL_SLICE).map_err(tui_err)? {
            let evt = event::read().map_err(tui_err)?;
            if let Event::Key(key) = evt {
                let exited = app.on_key(key);
                // Esc on the passphrase screen sets a Shell decision.
                if matches!(app.decision, Some(Decision::Shell)) {
                    return Err(NmblError::Tui {
                        source: std::io::Error::other("operator cancelled passphrase entry"),
                    });
                }
                if exited {
                    // Enter was pressed — extract the buffer and return.
                    if let Screen::Passphrase { buffer, .. } = app.screen {
                        return Ok(buffer);
                    }
                    return Err(NmblError::Tui {
                        source: std::io::Error::other("passphrase screen exited without a buffer"),
                    });
                }
                dirty = true;
            }
        }
    }
}

fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_serial_index_clamps_and_rejects_garbage() {
        assert_eq!(parse_serial_index("", 4).expect("empty -> 0"), 0);
        assert_eq!(parse_serial_index("1", 4).expect("1-based -> 0"), 0);
        assert_eq!(parse_serial_index("3", 4).expect("3 -> 2"), 2);
        // Clamps to last_idx.
        assert_eq!(parse_serial_index("99", 4).expect("clamp"), 4);
        assert!(parse_serial_index("not-a-number", 4).is_err());
    }

    #[test]
    fn tui_password_supplier_carries_serial_flag() {
        let sup = TuiPasswordSupplier {
            config_serial: true,
        };
        assert!(sup.config_serial);
    }

    #[test]
    fn tui_password_supplier_reads_serial_from_config() {
        // serial_console = true → supplier picks the line-mode path.
        let cfg: Config = toml::from_str("[general]\nserial_console = true\n").expect("parse cfg");
        let sup = TuiPasswordSupplier::new(&cfg);
        assert!(sup.config_serial);

        // default config (no serial) leaves the raw-mode TUI path active.
        let cfg_default: Config = toml::from_str("").expect("empty cfg parses");
        let sup_default = TuiPasswordSupplier::new(&cfg_default);
        assert!(!sup_default.config_serial);
    }

    #[test]
    fn tui_password_supplier_satisfies_password_supplier_trait() {
        // The integration contract: activation::run_all_activations
        // accepts `Option<&mut dyn PasswordSupplier>`. This test pins
        // that coercion so a future signature drift on either side
        // breaks the build instead of breaking at boot.
        let cfg: Config = toml::from_str("").expect("default cfg");
        let mut sup = TuiPasswordSupplier::new(&cfg);
        let _coerced: &mut dyn PasswordSupplier = &mut sup;
    }
}
