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

use std::collections::VecDeque;
use std::io::{stdin, stdout};
use std::os::fd::AsFd;
use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Terminal;
use ratatui::backend::{Backend, TermwizBackend};
use rustix::termios::Termios;
use termwiz::caps::Capabilities;
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::unix::UnixTerminal;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::tty::{enter_raw, restore_termios, save_termios};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::parser::TermwizToCrossterm;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::render_current_screen;
use crate::ui::{
    SessionInteraction, passphrase_prompt_on_console, show_modal_buttons, show_modal_confirm,
    show_modal_error, show_wrong_password_modal,
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
    let res = (|| -> Result<()> {
        let mut console = MockConsole::new()?;
        match args.scenario.as_str() {
            "modal-error" => run_modal_error(&mut console, &args.args),
            "modal-confirm" => run_modal_confirm(&mut console, &args.args),
            "modal-buttons" => run_modal_buttons(&mut console, &args.args),
            "wrong-password" => run_wrong_password(&mut console, &args.args),
            "boot-status" => run_boot_status(&mut console, &args.args),
            "passphrase" => run_passphrase(&mut console, &args.args),
            "resize" => run_resize(&mut console, &args.args),
            "emergency" => run_emergency(&mut console, &args.args),
            other => Err(NmblError::Io {
                source: std::io::Error::other(format!("unknown --debug-tui scenario {other:?}")),
                context: "mocking harness dispatch".to_string(),
            }),
        }
    })();
    // Always restore the terminal, even on error.
    let _ = restore_termios(stdin_fd.as_fd(), &saved);
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
    let attempt: u32 = args.first().map(|s| s.parse().unwrap_or(1)).unwrap_or(1);
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
    match passphrase_prompt_on_console(console, &label, &SessionInteraction::new()) {
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

/// Drive the resize-event plumbing end-to-end on the harness console.
///
/// Scripts two synthetic [`ConsoleEvent::Resize`] events at different
/// sizes and a final key press. Between each event the modal repaints
/// against the new size so a tmux harness can `capture-pane` the
/// before / after dimensions and confirm the layout actually changed.
///
/// The exact sizes can be overridden on the command line:
/// `--debug-tui resize <r1> <c1> <r2> <c2>` — defaults are
/// `40x100`, `20x60`, then any key to dismiss.
fn run_resize(console: &mut MockConsole, args: &[String]) -> Result<()> {
    let rows1: u16 = parse_u16_arg(args, 0).unwrap_or(40);
    let cols1: u16 = parse_u16_arg(args, 1).unwrap_or(100);
    let rows2: u16 = parse_u16_arg(args, 2).unwrap_or(20);
    let cols2: u16 = parse_u16_arg(args, 3).unwrap_or(60);

    let title = "Resize harness";
    let body = format!(
        "Stage 1: waiting for resize to {cols1}x{rows1}.\n\
         Then resize to {cols2}x{rows2}.\n\
         Then press any key to exit."
    );
    let hint = "drives two synthetic ConsoleEvent::Resize events, then a key";
    let labels = ["OK"];

    // Stage 1: paint at the harness's current size.
    paint_resize_stage(console, title, &body, &labels, hint)?;
    eprintln!("[mocking] resize stage=0 size={:?}", Console::size(console));

    // Stage 2: fire the first synthetic resize then re-paint.
    console.script(ConsoleEvent::Resize {
        rows: rows1,
        cols: cols1,
    });
    drain_one_event(console)?;
    paint_resize_stage(console, title, &body, &labels, hint)?;
    eprintln!("[mocking] resize stage=1 size={:?}", Console::size(console));

    // Stage 3: second resize.
    console.script(ConsoleEvent::Resize {
        rows: rows2,
        cols: cols2,
    });
    drain_one_event(console)?;
    paint_resize_stage(console, title, &body, &labels, hint)?;
    eprintln!("[mocking] resize stage=2 size={:?}", Console::size(console));

    // Stage 4: wait for a real key press so tmux captures can land.
    let deadline = std::time::Instant::now() + Duration::from_secs(3600);
    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let slice = remaining.min(POLL_SLICE);
        match console.poll_event(slice)? {
            Some(ConsoleEvent::Key(_)) => break,
            // Any further resize / no event: re-paint and wait again.
            Some(ConsoleEvent::Resize { .. }) | None => continue,
        }
    }
    eprintln!("[mocking] resize dismissed");
    Ok(())
}

fn paint_resize_stage(
    console: &mut MockConsole,
    title: &str,
    body: &str,
    labels: &[&str],
    hint: &str,
) -> Result<()> {
    let (cols, rows) = Console::size(console);
    let resized_body = format!("{body}\n\nObserved size: cols={cols} rows={rows}");
    let data = crate::ui::view::ModalButtonsScreenData {
        title,
        message: &resized_body,
        labels,
        selected: 0,
        hint,
        scroll_offset: 0,
    };
    console.draw_with(&mut |frame| crate::ui::view::render_modal_buttons(frame, &data))
}

/// Drain a single event from the harness queue, ignoring whatever it
/// is. Used after `script()` to ensure the synthetic event has been
/// applied to `last_resize` before the next paint.
fn drain_one_event(console: &mut MockConsole) -> Result<()> {
    let _ = console.poll_event(Duration::from_millis(0))?;
    Ok(())
}

fn parse_u16_arg(args: &[String], idx: usize) -> Option<u16> {
    args.get(idx).and_then(|s| s.parse().ok())
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

/// Drive the real emergency menu (`shell::drop_to_emergency`) on the
/// host terminal so the menu, the console picker, Raw Shell, and the
/// error-display behaviour can be exercised without a VM. A synthetic
/// boot error seeds the screen; the optional first arg overrides the
/// shell path (default `/bin/sh`) so the picker can spawn a real shell
/// on the host's controlling tty.
///
/// This is primarily a manual / tmux-driven smoke test for the
/// latest-error display fix and the Raw Shell spawn: pick `Raw Shell`,
/// keep the current tty checked, hit `Spawn`, and confirm a live shell
/// appears. The picker resolves real `/dev/...` targets, so run it from
/// a real terminal (a tmux pane is ideal).
fn run_emergency(_console: &mut MockConsole, args: &[String]) -> Result<()> {
    let mut config = Config::recovery_default();
    if let Some(shell) = args.first() {
        config.paths.shell = std::path::PathBuf::from(shell);
    }
    // drop_to_emergency owns its console; hand it a fresh boxed
    // MockConsole (same stdin/stdout this process already uses).
    let boxed: Box<dyn Console> = Box::new(MockConsole::new()?);
    let err = NmblError::Io {
        source: std::io::Error::other("synthetic boot failure (mocking harness)"),
        context: "phase-3 generation discovery".to_string(),
    };
    let action = crate::shell::drop_to_emergency(boxed, &config, err);
    eprintln!("[mocking] emergency action={action:?}");
    Ok(())
}

fn arg_or_default(args: &[String], idx: usize, default: &str) -> String {
    args.get(idx)
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

/// Console backend for the mocking harness. termwiz drives both
/// reads (from stdin) and writes (to stdout); no `/dev/console`, no
/// KD ioctls. Raw mode is owned by `run()` so the harness can host
/// multiple scenarios in one process.
struct MockConsole {
    terminal: Terminal<TermwizBackend>,
    /// Termwiz parser → crossterm `KeyEvent` translator.
    parser: TermwizToCrossterm,
    /// Keys produced by the parser, not yet drained.
    pending_keys: VecDeque<KeyEvent>,
    /// Scripted events injected by scenarios (e.g. `resize`).
    /// Drained ahead of stdin reads on the next `poll_event`.
    scripted: VecDeque<ConsoleEvent>,
    /// Latest grid size set by a `ConsoleEvent::Resize`. Overrides
    /// the backend's reported size so the next render lays out
    /// against the simulated geometry, mirroring `TtyConsole`.
    last_resize: Option<(u16, u16)>,
    /// Saved stdin termios so we can revert to blocking mode when
    /// the harness exits.
    saved_stdin_termios: Option<Termios>,
}

impl MockConsole {
    fn new() -> Result<Self> {
        let stdin_fd = stdin();
        let stdout_fd = stdout();
        let saved = save_termios(stdin_fd.as_fd())?;

        let caps = caps_with_fallback()?;
        let unix_term = UnixTerminal::new_with(caps, &stdin_fd, &stdout_fd).map_err(tw_err)?;
        let buf = BufferedTerminal::new(unix_term).map_err(tw_err)?;
        let backend = TermwizBackend::with_buffered_terminal(buf);
        let terminal = Terminal::new(backend).map_err(io_err)?;
        Ok(Self {
            terminal,
            parser: TermwizToCrossterm::new(),
            pending_keys: VecDeque::new(),
            scripted: VecDeque::new(),
            last_resize: None,
            saved_stdin_termios: Some(saved),
        })
    }

    /// Inject a synthetic event into the queue. Drained ahead of any
    /// real input on the next `poll_event`. Used by the `resize`
    /// scenario.
    fn script(&mut self, ev: ConsoleEvent) {
        self.scripted.push_back(ev);
    }

    fn apply_resize(&mut self, ev: &ConsoleEvent) {
        let ConsoleEvent::Resize { rows, cols } = *ev else {
            return;
        };
        self.last_resize = Some((cols, rows));
        let _ = self
            .terminal
            .resize(ratatui::layout::Rect::new(0, 0, cols, rows));
    }

    /// Drain whatever stdin has ready and feed it through the
    /// termwiz parser. Bytes are read non-blockingly via a single
    /// `rustix::io::read` against the stdin fd; partial sequences
    /// stay buffered inside `self.parser` for the next call.
    fn refill_from_stdin(&mut self, timeout: Duration) -> Result<()> {
        use rustix::event::{PollFd, PollFlags, poll};
        let stdin_fd = stdin();
        let timeout_ms = duration_to_ms(timeout);
        let mut pfd = [PollFd::new(&stdin_fd, PollFlags::IN)];
        let ready = poll(&mut pfd, timeout_ms).map_err(rustix_err)?;
        if ready == 0 {
            // No new bytes — flush termwiz so a dangling ESC commits.
            let mut out = Vec::new();
            self.parser.feed(&[], false, &mut out);
            for k in out {
                self.pending_keys.push_back(k);
            }
            return Ok(());
        }
        let mut chunk = [0u8; 256];
        match rustix::io::read(&stdin_fd, &mut chunk) {
            Ok(0) => Ok(()),
            Ok(n) => {
                let mut out = Vec::new();
                self.parser
                    .feed(chunk.get(..n).unwrap_or(&[]), false, &mut out);
                for k in out {
                    self.pending_keys.push_back(k);
                }
                Ok(())
            }
            Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::WOULDBLOCK => Ok(()),
            Err(e) => Err(rustix_err(e)),
        }
    }
}

impl Console for MockConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.terminal
            .draw(|f| render_current_screen(f, app))
            .map(|_| ())
            .map_err(io_err)
    }

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
        if let Some(ev) = self.scripted.pop_front() {
            self.apply_resize(&ev);
            return Ok(Some(ev));
        }
        if let Some(k) = self.pending_keys.pop_front() {
            return Ok(Some(ConsoleEvent::Key(k)));
        }
        let slice = timeout.min(POLL_SLICE);
        self.refill_from_stdin(slice)?;
        Ok(self.pending_keys.pop_front().map(ConsoleEvent::Key))
    }

    fn size(&self) -> (u16, u16) {
        if let Some((cols, rows)) = self.last_resize {
            return (cols, rows);
        }
        match self.terminal.backend().size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (0, 0),
        }
    }

    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal.draw(|f| body(f)).map(|_| ()).map_err(io_err)
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

impl Drop for MockConsole {
    fn drop(&mut self) {
        // Best-effort: restore the stdin termios we snapshotted at
        // construction. The outer `run()` guard does this too — both
        // paths are idempotent because `enter_raw` accepts being
        // applied to an already-cooked tty.
        if let Some(saved) = self.saved_stdin_termios.take() {
            let _ = restore_termios(stdin().as_fd(), &saved);
        }
    }
}

fn caps_with_fallback() -> Result<Capabilities> {
    if let Ok(c) = Capabilities::new_from_env() {
        return Ok(c);
    }
    let hints = termwiz::caps::ProbeHints::new_from_env().term(Some("xterm-256color".to_owned()));
    if let Ok(c) = Capabilities::new_with_hints(hints) {
        return Ok(c);
    }
    Capabilities::new_with_hints(termwiz::caps::ProbeHints::new_from_env()).map_err(tw_err)
}

fn tw_err(e: termwiz::Error) -> NmblError {
    NmblError::Io {
        source: std::io::Error::other(format!("termwiz: {e}")),
        context: "mocking harness".to_string(),
    }
}

fn rustix_err(e: rustix::io::Errno) -> NmblError {
    NmblError::Io {
        source: std::io::Error::from(e),
        context: "mocking harness".to_string(),
    }
}

fn duration_to_ms(d: Duration) -> i32 {
    let ms = d.as_millis();
    if ms > i32::MAX as u128 {
        i32::MAX
    } else {
        i32::try_from(ms).unwrap_or(i32::MAX)
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
