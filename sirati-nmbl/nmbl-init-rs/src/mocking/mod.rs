//! TUI mocking harness (feature `mocking`).
//!
//! When compiled with the `mocking` feature the binary accepts an extra
//! `--debug-tui -- <scenario> [args...]` invocation that runs a single
//! modal/screen flow on the current terminal (stdin/stdout via
//! crossterm) instead of executing the PID-1 boot pipeline. The
//! harness is designed for tmux-driven smoke testing — a test spawns
//! the binary in a tmux pane, drives keystrokes through `tmux
//! send-keys`, and captures the rendered cells with `tmux capture-pane`.
//! No VM, no DRM, no /dev/console: the running shell already has a tty.
//!
//! ## Scenarios
//!
//! - `modal-error <title> <body>`
//! - `modal-confirm <title> <body> [yes_label=Yes] [no_label=No]`
//! - `modal-buttons <title> <body> <label1> [label2 …]`
//! - `wrong-password <attempt>`
//! - `boot-status <phase> [log_line …]`
//! - `passphrase [label]` — drives the ratatui passphrase modal end-to-end
//!   (the same code path the LUKS activation flow uses). Stderr surfaces
//!   the entered string with single quotes so a test harness can scrape
//!   it; Esc-cancel surfaces "cancelled".
//!
//! Each scenario blocks until the operator closes the modal (Enter /
//! Esc / hotkey) at which point the harness prints the outcome on
//! stderr (so test harnesses can scrape it) and exits.
//!
//! ## What this MUST NOT do
//!
//! - Open `/dev/console` (we're not PID 1).
//! - Install the panic hook (we want panics to crash the harness so
//!   the test runner notices).
//! - Touch `KDSETMODE` / `KDGETMODE` (we're on an emulator pane).
//! - Run any of the boot phases — only the requested screen flow.

use std::io::stdout;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};

use crate::error::{NmblError, Result};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleKind};
use crate::ui::render_current_screen;
use crate::ui::{
    passphrase_prompt_on_console, show_modal_buttons, show_modal_confirm, show_modal_error,
    show_wrong_password_modal,
};

/// Parsed `--debug-tui -- <scenario> [args...]` invocation.
///
/// `scenario` plus the trailing positional args are kept as plain
/// `String`s so the dispatcher can interpret them per-scenario without
/// pre-classifying types in the parser.
pub struct DebugTuiArgs {
    pub scenario: String,
    pub args: Vec<String>,
}

/// Strip `--debug-tui -- <scenario> [args...]` from a raw argv. Returns
/// `Some(parsed)` when the marker is present, `None` otherwise. The
/// caller can fall through to normal boot args on `None`.
///
/// We accept both `--debug-tui -- <s>` and `--debug-tui <s>` — the
/// `--` is conventional but not load-bearing. Anything after the
/// scenario keyword goes into `args` unchanged.
pub fn parse_debug_tui_args<I, S>(argv: I) -> Option<DebugTuiArgs>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
    let pos = argv.iter().position(|a| a == "--debug-tui")?;
    let mut rest = argv.into_iter().skip(pos.saturating_add(1));
    let first = rest.next()?;
    // Allow `--debug-tui -- <scenario>` and `--debug-tui <scenario>`.
    let scenario = if first == "--" {
        rest.next()?
    } else {
        first
    };
    let args: Vec<String> = rest.collect();
    Some(DebugTuiArgs { scenario, args })
}

/// Entry point: dispatch the requested scenario on a stdin/stdout
/// console. The harness wires up raw mode itself; on return the raw
/// mode is restored regardless of outcome.
pub fn run(args: DebugTuiArgs) -> Result<()> {
    enable_raw_mode().map_err(io_err)?;
    let res = (|| -> Result<()> {
        let mut console = MockConsole::new()?;
        match args.scenario.as_str() {
            "modal-error" => run_modal_error(&mut console, &args.args),
            "modal-confirm" => run_modal_confirm(&mut console, &args.args),
            "modal-buttons" => run_modal_buttons(&mut console, &args.args),
            "wrong-password" => run_wrong_password(&mut console, &args.args),
            "boot-status" => run_boot_status(&mut console, &args.args),
            "passphrase" => run_passphrase(&mut console, &args.args),
            other => Err(NmblError::Io {
                source: std::io::Error::other(format!("unknown --debug-tui scenario {other:?}")),
                context: "mocking harness dispatch".to_string(),
            }),
        }
    })();
    // Always restore the terminal, even on error.
    let _ = disable_raw_mode();
    res
}

fn run_modal_error(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let title = arg_or_default(args, 0, "Error");
    let body = arg_or_default(args, 1, "");
    // Long timeout so the test runner has time to capture the pane.
    show_modal_error(console, &title, &body, Duration::from_secs(3600))?;
    eprintln!("[mocking] modal-error dismissed");
    Ok(())
}

fn run_modal_confirm(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let title = arg_or_default(args, 0, "Confirm");
    let body = arg_or_default(args, 1, "");
    let yes = arg_or_default(args, 2, "Yes");
    let no = arg_or_default(args, 3, "No");
    let outcome = show_modal_confirm(console, &title, &body, &yes, &no, true)?;
    eprintln!("[mocking] modal-confirm outcome={outcome:?}");
    Ok(())
}

fn run_modal_buttons(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let Some(title) = args.first().cloned() else {
        return Err(NmblError::Io {
            source: std::io::Error::other("modal-buttons requires <title> <body> <label…>"),
            context: "mocking harness".to_string(),
        });
    };
    let body = arg_or_default(args, 1, "");
    let labels: Vec<String> = args.iter().skip(2).cloned().collect();
    if labels.is_empty() {
        return Err(NmblError::Io {
            source: std::io::Error::other("modal-buttons requires at least one button label"),
            context: "mocking harness".to_string(),
        });
    }
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let outcome = show_modal_buttons(
        console,
        &title,
        &body,
        &label_refs,
        "Left/Right select  Enter confirm  Esc cancel",
    )?;
    eprintln!("[mocking] modal-buttons outcome_idx={outcome:?}");
    Ok(())
}

fn run_wrong_password(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let attempt: u32 = args
        .first()
        .map(|s| s.parse().unwrap_or(1))
        .unwrap_or(1);
    let outcome = show_wrong_password_modal(console, attempt)?;
    eprintln!("[mocking] wrong-password outcome={outcome:?}");
    Ok(())
}

/// Drive the production passphrase modal on the harness console. Same
/// `passphrase_prompt_on_console` entry point the LUKS activation path
/// calls, so a tmux-driven smoke test exercises the exact code that
/// runs at boot. On Enter the entered string is reported on stderr (in
/// quotes so leading/trailing whitespace is visible); on Esc-cancel the
/// supplier returns `NmblError::Tui`, which we surface as
/// `[mocking] passphrase cancelled` on stderr and exit cleanly so the
/// test harness can distinguish the two outcomes from the exit code.
fn run_passphrase(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let label = arg_or_default(args, 0, "Unlock root");
    match passphrase_prompt_on_console(console, &label) {
        Ok(secret) => {
            eprintln!("[mocking] passphrase entered='{}'", &**secret);
            Ok(())
        }
        Err(_) => {
            eprintln!("[mocking] passphrase cancelled");
            Ok(())
        }
    }
}

fn run_boot_status(console: &mut MockConsole, args: &[String]) -> Result<()> {
    use crate::ui::app::Screen;
    let phase = arg_or_default(args, 0, "phase X");
    let log_lines: Vec<String> = args.iter().skip(1).cloned().collect();
    let mut app = App::boot_status(phase.clone());
    if let Screen::BootStatus(data) = &mut app.screen {
        data.log_lines = log_lines;
    }
    // One paint then wait for any key (or 1h timeout) so tmux can
    // capture the rendered cells.
    console.render(&app)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let slice = remaining.min(POLL_SLICE);
        if console.poll_key(slice)?.is_some() {
            break;
        }
    }
    eprintln!("[mocking] boot-status dismissed");
    Ok(())
}

fn arg_or_default(args: &[String], idx: usize, default: &str) -> String {
    args.get(idx).cloned().unwrap_or_else(|| default.to_string())
}

/// Console backend for the mocking harness: a crossterm terminal over
/// `stdout()` paired with crossterm's stdin event reader. No
/// `/dev/console`, no KD ioctls, no termios snapshot — the raw-mode
/// state lives at the `run()` boundary so this struct can be used
/// from multiple scenarios within one run without re-entering raw mode.
struct MockConsole {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
}

impl MockConsole {
    fn new() -> Result<Self> {
        let backend = CrosstermBackend::new(stdout());
        let terminal = Terminal::new(backend).map_err(io_err)?;
        Ok(Self { terminal })
    }
}

impl Console for MockConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.terminal
            .draw(|f| render_current_screen(f, app))
            .map(|_| ())
            .map_err(io_err)
    }

    fn poll_key(&mut self, timeout: Duration) -> Result<Option<KeyEvent>> {
        let slice = timeout.min(POLL_SLICE);
        if !event::poll(slice).map_err(io_err)? {
            return Ok(None);
        }
        match event::read().map_err(io_err)? {
            Event::Key(k) => Ok(Some(k)),
            _ => Ok(None),
        }
    }

    fn size(&self) -> (u16, u16) {
        match self.terminal.backend().size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (0, 0),
        }
    }

    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal
            .draw(|f| body(f))
            .map(|_| ())
            .map_err(io_err)
    }

    fn suspend(&mut self) -> Result<()> {
        // No-op for the mocking harness; we don't host external shells.
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        // Force a full repaint so the next render produces a clean frame.
        self.terminal.clear().map_err(io_err)
    }
}

fn io_err(source: std::io::Error) -> NmblError {
    NmblError::Io {
        source,
        context: "mocking harness".to_string(),
    }
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
    fn parse_debug_tui_extracts_scenario_with_double_dash() {
        let argv = vec![
            "nmbl-init".to_string(),
            "--debug-tui".to_string(),
            "--".to_string(),
            "modal-error".to_string(),
            "Title".to_string(),
            "Body".to_string(),
        ];
        let parsed = parse_debug_tui_args(argv).expect("scenario present");
        assert_eq!(parsed.scenario, "modal-error");
        assert_eq!(parsed.args, vec!["Title", "Body"]);
    }

    #[test]
    fn parse_debug_tui_extracts_scenario_without_double_dash() {
        let argv = vec![
            "nmbl-init".to_string(),
            "--debug-tui".to_string(),
            "modal-confirm".to_string(),
            "T".to_string(),
        ];
        let parsed = parse_debug_tui_args(argv).expect("scenario present");
        assert_eq!(parsed.scenario, "modal-confirm");
        assert_eq!(parsed.args, vec!["T"]);
    }

    #[test]
    fn parse_debug_tui_returns_none_without_marker() {
        let argv = vec!["nmbl-init".to_string(), "--config=/etc/nmbl/c.toml".to_string()];
        assert!(parse_debug_tui_args(argv).is_none());
    }

    #[test]
    fn parse_debug_tui_returns_none_with_marker_but_no_scenario() {
        let argv = vec!["nmbl-init".to_string(), "--debug-tui".to_string()];
        assert!(parse_debug_tui_args(argv).is_none());

        let argv = vec![
            "nmbl-init".to_string(),
            "--debug-tui".to_string(),
            "--".to_string(),
        ];
        assert!(parse_debug_tui_args(argv).is_none());
    }
}
