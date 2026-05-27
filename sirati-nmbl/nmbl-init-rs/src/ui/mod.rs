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
pub mod reporter;
pub mod timeout;
pub mod view;

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use zeroize::Zeroizing;

use crate::activation::PasswordSupplier;
use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::generations::Generation;
use crate::sys::tty::{RawModeGuard, open_console};
use crate::ui::console::Console;
use crate::ui::timeout::TimeoutOutcome;
use crate::ui::view::{
    EditScreenData, EmergencyScreenData, ListScreenData, PassphraseScreenData, render_boot_status,
    render_edit, render_emergency, render_list, render_passphrase,
};

/// Default console path used by the activation-phase passphrase modal
/// when the splash isn't owning the framebuffer anymore.
const CONSOLE_PATH: &str = "/dev/console";

#[cfg(feature = "image-splash")]
use alacritty_terminal::term::cell::Flags;
#[cfg(feature = "image-splash")]
use alacritty_terminal::vte::ansi::{Color, NamedColor};
#[cfg(feature = "image-splash")]
use ratatui::TerminalOptions;
#[cfg(feature = "image-splash")]
use ratatui::Viewport;
#[cfg(feature = "image-splash")]
use ratatui::layout::Rect;
#[cfg(feature = "image-splash")]
use crate::splash::{compositor, drm, glyph_cache};
#[cfg(feature = "image-splash")]
use crate::splash::terminal::SplashTerminal;
#[cfg(feature = "image-splash")]
use crate::splash::types::CellDims;

pub use app::{App, BootStatusData, Decision, EmergencyChoice, EmergencyItem, Screen};
pub use emergency::run_emergency_screen;
pub use reporter::BootReporter;

/// Slice we wait on input per iteration. Shared by the event loop and
/// the countdown ticker so they have the same responsiveness profile
/// and only one knob to tune.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(100);

/// Run the boot-selection TUI on the provided [`Console`] and return
/// the operator's decision.
///
/// The console is brought up once by the orchestrator (main.rs) at the
/// start of phase 1 and held through every phase; this function reuses
/// it instead of opening a parallel splash bring-up, so the same DRM
/// card / raw-mode tty serves the whole boot.
///
/// Falls back to a line-oriented serial prompt when the config opts
/// in via `general.serial_console`.
pub fn run_selector(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
) -> Result<Decision> {
    if config.general.serial_console {
        return select_generation_serial(config, generations);
    }
    run_selector_on_console(config, generations, console)
}

/// TUI event loop. Backend-agnostic: every render and key-poll goes
/// through the [`Console`] trait. Hosts the countdown, the List/Editing
/// state machine, and the timeout-defaults-to-first-generation rule.
fn run_selector_on_console(
    config: &Config,
    generations: &[Generation],
    console: &mut dyn Console,
) -> Result<Decision> {
    let mut app = App::new(generations);
    app.show_kernel_params = config.tui.show_kernel_params;

    // 1. Countdown phase.
    let countdown = Duration::from_secs(u64::from(config.general.timeout_secs));
    let outcome = run_console_countdown(console, &mut app, countdown)?;
    app.countdown_remaining_secs = None;

    if matches!(outcome, TimeoutOutcome::Expired) && app.decision.is_none() {
        return Ok(Decision::Boot {
            generation_index: 0,
            cmdline_override: None,
        });
    }

    // 2. Event loop. Renders on dirty, polls in short slices so future
    //    callers that need to drive an animation can plug in without
    //    rewriting the loop.
    let mut dirty = true;
    loop {
        if dirty {
            console.render(&app)?;
            dirty = false;
        }
        if let Some(key) = console.poll_key(POLL_SLICE)? {
            if app.on_key(key) {
                break;
            }
            dirty = true;
        }
        if app.decision.is_some() {
            break;
        }
    }

    app.decision.ok_or_else(|| NmblError::Tui {
        source: std::io::Error::other("selector exited without decision"),
    })
}

/// Countdown driver that polls the [`Console`] for keys instead of
/// stdin, so cancel-on-keypress works on both the splash framebuffer
/// (input via `/dev/tty1`) and the raw-mode tty.
fn run_console_countdown(
    console: &mut dyn Console,
    app: &mut App<'_>,
    duration: Duration,
) -> Result<TimeoutOutcome> {
    let start = Instant::now();
    let deadline = start.checked_add(duration).unwrap_or(start);

    let initial = duration.as_secs();
    app.countdown_remaining_secs = Some(initial);
    console.render(app)?;
    let mut last_reported = initial;

    loop {
        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };

        let slice = remaining.min(POLL_SLICE);
        if console.poll_key(slice)?.is_some() {
            return Ok(TimeoutOutcome::Cancelled);
        }

        let now = Instant::now();
        let Some(remaining) = deadline.checked_duration_since(now) else {
            return Ok(TimeoutOutcome::Expired);
        };
        let secs = remaining.as_secs();
        if secs != last_reported {
            app.countdown_remaining_secs = Some(secs);
            console.render(app)?;
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
        Screen::BootStatus(data) => render_boot_status(frame, data),
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
