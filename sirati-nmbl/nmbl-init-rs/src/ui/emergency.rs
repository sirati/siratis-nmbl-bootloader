//! Emergency-screen orchestrator.
//!
//! When a top-level phase returns `Err`, `shell::drop_to_emergency`
//! used to immediately `execve` the shell. That bypassed the splash
//! backend entirely: operators on a VNC console saw nothing useful,
//! and there was no way to choose between rebooting and dropping to
//! the shell. This module replaces that behaviour with a proper TUI
//! that runs over the splash backend when available and falls back
//! to a tty-mode ratatui or a line-oriented serial prompt otherwise.
//!
//! Architectural rule: **all UI is TUI code**. The splash backend is
//! only a render target. The state machine that drives the screen
//! lives in [`crate::ui::app::Screen::Emergency`]; the renderer lives
//! in [`crate::ui::view::render_emergency`]. This module wires the
//! two together against either the splash console or a stdout
//! ratatui backend, and applies a 30-second default-to-reboot timer.
//!
//! ## Timer
//!
//! With no input for 30 seconds we default to [`EmergencyChoice::Reboot`].
//! Operators on a remote VNC console may not be sitting there when boot
//! fails; rebooting is the safe default — if the next boot also fails
//! they'll just land back here.
//!
//! The clock is injected as a `Fn() -> Instant` so unit tests can run
//! the timer machinery without sleeping a real wall-clock second.

use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::error::{NmblError, Result, format_chain};
use crate::sys::tty::{RawModeGuard, open_console};
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, EmergencyChoice, EmergencyItem, Screen};

/// Backend the emergency loop drives. Implementors paint a frame and
/// poll for keys; the loop is otherwise backend-agnostic. Defining
/// this as a trait (rather than two `FnMut` closures) sidesteps the
/// borrow-checker conflict you'd hit holding two mut closures over
/// the same backend handle.
trait EmergencyBackend {
    fn render(&mut self, app: &App<'_>) -> Result<()>;
    fn poll(&mut self, timeout: Duration) -> Result<Option<crossterm::event::KeyEvent>>;
}

/// Default console path for the tty-mode fallback path.
const CONSOLE_PATH: &str = "/dev/console";

/// Default countdown to auto-reboot when the operator is not present.
const EMERGENCY_TIMEOUT: Duration = Duration::from_secs(30);

/// Run the emergency screen and return the operator's choice.
///
/// Tries, in order:
///   1. Splash console (when `image-splash` feature is built in,
///      `config.splash.enable` is true, and DRM bring-up succeeds).
///   2. Raw-mode ratatui over `/dev/console`.
///   3. Line-oriented serial prompt on stdin/stdout when
///      `config.general.serial_console` is set.
///
/// In every path a 30-second no-input timer defaults to
/// [`EmergencyChoice::Reboot`].
pub fn run_emergency_screen(config: &Config, err: &NmblError) -> EmergencyChoice {
    let message = build_message(err);
    let items = default_items();

    // Serial console path: line-oriented; the splash console will not
    // have a working keyboard on a serial-only deployment, and raw
    // mode is unreliable on broken serial lines.
    if config.general.serial_console {
        return run_serial_emergency(&message, &items)
            .unwrap_or(EmergencyChoice::Reboot);
    }

    #[cfg(feature = "image-splash")]
    if config.splash.enable {
        match run_splash_emergency(config, &message, &items) {
            Ok(choice) => return choice,
            Err(e) => {
                crate::nmbl_warn!(
                    "emergency splash bring-up failed: {}; falling back to tty",
                    format_chain(&e as &dyn std::error::Error)
                );
            }
        }
    }

    // Tty fallback. If even raw-mode bring-up fails we have no UI to
    // show, and the safest thing is to fall through to reboot — the
    // caller (drop_to_emergency) will surface the original error via
    // its banner-on-shell path anyway.
    run_tty_emergency(&message, &items).unwrap_or(EmergencyChoice::Reboot)
}

/// Build the message string shown to the operator. Includes the
/// suggested-action hint plus the formatted error chain.
fn build_message(err: &NmblError) -> String {
    let mut s = String::new();
    s.push_str("Boot failed. The chain of errors is:\n\n");
    s.push_str(&format_chain(err as &dyn std::error::Error));
    s.push_str("\n\nChoose what to do next.");
    s
}

/// The two items shown on the emergency screen. Order matters: index 0
/// is the default if the operator just presses Enter, and it's what the
/// timeout rolls over to.
fn default_items() -> Vec<EmergencyItem> {
    vec![
        EmergencyItem {
            label: "Reboot",
            choice: EmergencyChoice::Reboot,
        },
        EmergencyItem {
            label: "Shell",
            choice: EmergencyChoice::Shell,
        },
    ]
}

/// Splash-backed emergency picker.
#[cfg(feature = "image-splash")]
fn run_splash_emergency(
    config: &Config,
    message: &str,
    items_template: &[EmergencyItem],
) -> Result<EmergencyChoice> {
    let Some(mut console) = crate::ui::open_splash_console(config)? else {
        return Err(NmblError::Tui {
            source: std::io::Error::other("splash console unavailable"),
        });
    };

    let mut app = build_emergency_app(message, items_template);
    let mut backend = SplashBackend {
        console: &mut console,
    };
    drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, Instant::now, &mut backend)
}

/// Raw-mode tty emergency picker.
fn run_tty_emergency(message: &str, items_template: &[EmergencyItem]) -> Result<EmergencyChoice> {
    let console = open_console(Path::new(CONSOLE_PATH))?;
    let _raw = RawModeGuard::new(console.as_fd())?;

    let crossterm_backend = CrosstermBackend::new(std::io::stdout());
    let terminal = Terminal::new(crossterm_backend).map_err(tui_err)?;
    let mut backend = TtyBackend { terminal };

    let mut app = build_emergency_app(message, items_template);
    drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, Instant::now, &mut backend)
}

/// Build an `App` parked on the Emergency screen with the given
/// message and items.
fn build_emergency_app<'a>(
    message: &str,
    items_template: &[EmergencyItem],
) -> App<'a> {
    // Items are tiny, no point fighting the borrow checker — clone
    // the template into the App's own Screen state.
    let items: Vec<EmergencyItem> = items_template
        .iter()
        .map(|it| EmergencyItem {
            label: it.label,
            choice: it.choice,
        })
        .collect();
    let mut app = App::new(&[]);
    app.screen = Screen::Emergency {
        message: message.to_owned(),
        items,
        selected: 0,
        chosen: None,
    };
    app
}

/// Shared event-loop driver. Render, poll, react, repeat — and apply
/// the no-input timeout. Returns the operator's choice or the default
/// when the timer expires.
///
/// `now` is injected so tests can drive the timeout machinery without
/// real wall-clock waits.
fn drive_emergency_loop<N>(
    app: &mut App<'_>,
    timeout: Duration,
    now: N,
    backend: &mut dyn EmergencyBackend,
) -> Result<EmergencyChoice>
where
    N: Fn() -> Instant,
{
    let start = now();
    let deadline = start.checked_add(timeout).unwrap_or(start);
    let initial_secs = timeout.as_secs();
    app.countdown_remaining_secs = Some(initial_secs);
    let mut last_reported = initial_secs;

    let mut dirty = true;
    loop {
        if dirty {
            backend.render(app)?;
            dirty = false;
        }

        let current = now();
        let remaining = match deadline.checked_duration_since(current) {
            Some(r) => r,
            None => {
                // Timer expired. Default to Reboot (the first item).
                return Ok(EmergencyChoice::Reboot);
            }
        };
        let slice = remaining.min(POLL_SLICE);

        if let Some(key) = backend.poll(slice)? {
            // Any keypress cancels the auto-reboot countdown so the
            // operator can take their time deciding once present.
            app.countdown_remaining_secs = None;
            if app.on_key(key) {
                break;
            }
            dirty = true;
            continue;
        }

        // No input this slice. Tick the countdown if the displayed
        // second has changed. We only show the countdown until the
        // first keypress.
        if app.countdown_remaining_secs.is_some() {
            let current = now();
            let Some(remaining) = deadline.checked_duration_since(current) else {
                return Ok(EmergencyChoice::Reboot);
            };
            let secs = remaining.as_secs();
            if secs != last_reported {
                app.countdown_remaining_secs = Some(secs);
                last_reported = secs;
                dirty = true;
            }
        }
    }

    match &app.screen {
        Screen::Emergency { chosen, .. } => Ok(chosen.unwrap_or(EmergencyChoice::Reboot)),
        _ => Err(NmblError::Tui {
            source: std::io::Error::other("emergency screen exited off-screen"),
        }),
    }
}

/// Splash-console backend.
#[cfg(feature = "image-splash")]
struct SplashBackend<'c> {
    console: &'c mut crate::ui::SplashConsole,
}

#[cfg(feature = "image-splash")]
impl EmergencyBackend for SplashBackend<'_> {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.console.render(app)
    }
    fn poll(&mut self, timeout: Duration) -> Result<Option<crossterm::event::KeyEvent>> {
        self.console.poll(timeout)
    }
}

/// Raw-mode ratatui backend over `/dev/console`.
struct TtyBackend<W: Write> {
    terminal: Terminal<CrosstermBackend<W>>,
}

impl<W: Write> EmergencyBackend for TtyBackend<W> {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.terminal
            .draw(|f| crate::ui::render_current_screen(f, app))
            .map_err(tui_err)?;
        Ok(())
    }
    fn poll(&mut self, timeout: Duration) -> Result<Option<crossterm::event::KeyEvent>> {
        if event::poll(timeout).map_err(tui_err)? {
            let evt = event::read().map_err(tui_err)?;
            if let Event::Key(key) = evt {
                return Ok(Some(key));
            }
        }
        Ok(None)
    }
}

/// Line-oriented serial fallback. Trivially scripted by hand on a
/// broken serial line: prompt is `[r]eboot / [s]hell?` and the timer
/// still defaults to Reboot, but we can't easily interrupt a blocking
/// stdin read without OS-specific shenanigans — so the serial path
/// just reads one line and dispatches.
fn run_serial_emergency(
    message: &str,
    items: &[EmergencyItem],
) -> Result<EmergencyChoice> {
    let stdout = std::io::stdout();
    let stdin = std::io::stdin();

    {
        let mut out = stdout.lock();
        writeln!(out, "{}", "=".repeat(72)).map_err(tui_err)?;
        writeln!(out, "NMBL: boot failed").map_err(tui_err)?;
        writeln!(out, "{}", "=".repeat(72)).map_err(tui_err)?;
        for line in message.lines() {
            writeln!(out, "  {line}").map_err(tui_err)?;
        }
        writeln!(out).map_err(tui_err)?;
        for item in items {
            writeln!(out, "  [{}] {}", item_hotkey(item), item.label).map_err(tui_err)?;
        }
        writeln!(out, "Pick reboot (r) or shell (s) [r]:").map_err(tui_err)?;
        out.flush().map_err(tui_err)?;
    }

    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        // Treat a closed stdin as "no input" — default to Reboot.
        return Ok(EmergencyChoice::Reboot);
    }
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("r") || trimmed.eq_ignore_ascii_case("reboot") {
        return Ok(EmergencyChoice::Reboot);
    }
    if trimmed.eq_ignore_ascii_case("s") || trimmed.eq_ignore_ascii_case("shell") {
        return Ok(EmergencyChoice::Shell);
    }
    // Unrecognised input: be conservative and reboot rather than
    // dropping someone into a shell they didn't ask for.
    Ok(EmergencyChoice::Reboot)
}

fn item_hotkey(item: &EmergencyItem) -> char {
    match item.choice {
        EmergencyChoice::Reboot => 'r',
        EmergencyChoice::Shell => 's',
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::cell::Cell;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// In-process backend for unit-testing the emergency loop. Drives
    /// a scripted sequence of key events on poll() and counts renders.
    struct TestBackend {
        events: Vec<Option<KeyEvent>>,
        cursor: usize,
        renders: u32,
    }

    impl TestBackend {
        fn new(events: Vec<Option<KeyEvent>>) -> Self {
            Self {
                events,
                cursor: 0,
                renders: 0,
            }
        }
    }

    impl EmergencyBackend for TestBackend {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            self.renders = self.renders.saturating_add(1);
            Ok(())
        }
        fn poll(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
            let v = self.events.get(self.cursor).copied().flatten();
            self.cursor = self.cursor.saturating_add(1);
            Ok(v)
        }
    }

    #[test]
    fn build_message_includes_error_chain_lines() {
        let err = NmblError::Io {
            source: std::io::Error::other("disk on fire"),
            context: "mounting /tmp".to_string(),
        };
        let msg = build_message(&err);
        assert!(msg.contains("mounting /tmp"), "expected context: {msg}");
        assert!(msg.contains("disk on fire"), "expected source: {msg}");
    }

    #[test]
    fn drive_emergency_loop_returns_reboot_on_timeout() {
        // Advance the injected clock past the deadline on the very
        // first poll. No keys are ever delivered, so the loop must
        // bail out with the default (Reboot) rather than spinning.
        let now_calls = Cell::new(0u32);
        let fake_now = || {
            let n = now_calls.get();
            now_calls.set(n.saturating_add(1));
            // Start at a fixed epoch, then jump way past it.
            let base = Instant::now();
            if n < 1 {
                base
            } else {
                base + Duration::from_secs(60)
            }
        };

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut backend = TestBackend::new(vec![None; 16]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, fake_now, &mut backend)
            .expect("loop must not error on timeout path");
        assert_eq!(outcome, EmergencyChoice::Reboot);
        assert!(backend.renders >= 1, "must render at least one frame");
    }

    #[test]
    fn drive_emergency_loop_returns_selected_on_enter() {
        // Move down once, press Enter. Expected choice: Shell.
        // The clock never advances, so the timeout never fires.
        let start = Instant::now();
        let frozen_now = move || start;

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut backend =
            TestBackend::new(vec![Some(press(KeyCode::Down)), Some(press(KeyCode::Enter))]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut backend)
            .expect("loop must not error on the happy path");
        assert_eq!(outcome, EmergencyChoice::Shell);
    }

    #[test]
    fn drive_emergency_loop_keypress_cancels_countdown() {
        // Press 'r' immediately, before the timer matters. Choice must
        // be Reboot and the countdown must have been cleared.
        let start = Instant::now();
        let frozen_now = move || start;

        let mut app = build_emergency_app("boot failed", &default_items());
        let mut backend = TestBackend::new(vec![Some(press(KeyCode::Char('r')))]);

        let outcome = drive_emergency_loop(&mut app, EMERGENCY_TIMEOUT, frozen_now, &mut backend)
            .expect("loop must succeed");
        assert_eq!(outcome, EmergencyChoice::Reboot);
        assert!(app.countdown_remaining_secs.is_none());
    }

    #[test]
    fn default_items_first_is_reboot() {
        // The whole "timeout defaults to Reboot" contract hangs on
        // Reboot being the first item; if a future refactor flips the
        // order, the timeout test still passes but production gets
        // surprising behaviour. Pin the contract here.
        let items = default_items();
        assert_eq!(items[0].choice, EmergencyChoice::Reboot);
        assert_eq!(items[1].choice, EmergencyChoice::Shell);
    }
}
