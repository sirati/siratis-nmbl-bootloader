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
//! - `resize [r1 c1 r2 c2]` — fires two synthetic
//!   [`ConsoleEvent::Resize`] events on the mock console at the
//!   supplied sizes (defaults 40x100, 20x60), repainting between each,
//!   then blocks on a real key press for tmux capture. Exercises the
//!   end-to-end resize-redraw plumbing without needing a parent
//!   terminal that actually emits CSI 8;rows;cols t.
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

mod console;
mod scenarios;

use std::io::stdin;
use std::os::fd::AsFd;

use crate::error::{NmblError, Result};
use crate::sys::tty::{enter_raw, restore_termios, save_termios};

use self::console::MockConsole;
use self::scenarios::{
    run_boot_status, run_emergency, run_modal_buttons, run_modal_confirm, run_modal_error,
    run_passphrase, run_resize, run_wrong_password,
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
    let scenario = if first == "--" { rest.next()? } else { first };
    let args: Vec<String> = rest.collect();
    Some(DebugTuiArgs { scenario, args })
}

/// Entry point: dispatch the requested scenario on a stdin/stdout
/// console. The harness wires up raw mode itself; on return the raw
/// mode is restored regardless of outcome.
pub fn run(args: DebugTuiArgs) -> Result<()> {
    // Snapshot the stdin termios so we can restore it ourselves on
    // return; termwiz's UnixTerminal does its own snapshot too, but
    // its drop runs after `MockConsole` drops, which is after the
    // scenario returns — so we need an outer guard to restore raw
    // mode on the panic-unwind / early-return paths.
    let stdin_fd = stdin();
    let saved = save_termios(stdin_fd.as_fd())?;
    let _ = enter_raw(stdin_fd.as_fd())?;
    // The modal flows are async (they `.await` `Console::poll_event`);
    // drive them on a throwaway single-thread `LocalRuntime` with the
    // reserve poller spawned, exactly like the production interactive
    // phase. `block_on_tui` returns `Err` only if the runtime fails to
    // build.
    let res = crate::ui::block_on_tui(async {
        let mut console = MockConsole::new()?;
        match args.scenario.as_str() {
            "modal-error" => run_modal_error(&mut console, &args.args).await,
            "modal-confirm" => run_modal_confirm(&mut console, &args.args).await,
            "modal-buttons" => run_modal_buttons(&mut console, &args.args).await,
            "wrong-password" => run_wrong_password(&mut console, &args.args).await,
            "boot-status" => run_boot_status(&mut console, &args.args).await,
            "passphrase" => run_passphrase(&mut console, &args.args).await,
            "resize" => run_resize(&mut console, &args.args).await,
            "emergency" => run_emergency(&mut console, &args.args).await,
            other => Err(NmblError::Io {
                source: std::io::Error::other(format!("unknown --debug-tui scenario {other:?}")),
                context: "mocking harness dispatch".to_string(),
            }),
        }
    })
    .and_then(|inner| inner);
    // Always restore the terminal, even on error.
    let _ = restore_termios(stdin_fd.as_fd(), &saved);
    res
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
        let argv = vec![
            "nmbl-init".to_string(),
            "--config=/etc/nmbl/c.toml".to_string(),
        ];
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
