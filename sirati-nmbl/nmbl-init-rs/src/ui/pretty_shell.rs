//! Pretty-shell screen: host an in-process terminal emulator inside the
//! existing bordered TUI box so the operator can poke around without
//! NMBL `execve(2)`ing the shell as PID 1.
//!
//! The architecture is the same triangle the rest of NMBL uses: state
//! lives in [`crate::ui::app::Screen::PtyShell`], events go through
//! [`PtyShellState::on_key`], and the renderer in
//! [`crate::ui::view::render_pty_shell`] paints the alacritty grid into
//! a ratatui [`ratatui::widgets::Block`] with the rest of the screen
//! chrome (header / footer). The driver loop here glues the three
//! together.
//!
//! ## Why not Console::draw_with?
//!
//! Both shapes would compile. Picking the [`Screen::PtyShell`] variant
//! keeps the "all interactive UI is App+Screen" rule intact and makes
//! the lifecycle (entry, render, key handling, exit) discoverable from
//! [`crate::ui::view::render_current_screen`] alongside every other
//! screen. `draw_with` is reserved for dynamic widgets that don't fit
//! the state machine (download gauges, in-flight editors).
//!
//! ## Scrolling
//!
//! `alacritty_terminal`'s `Grid` carries a `display_offset` for
//! scrollback view. Ctrl+Shift+Up/Down step the offset one row at a
//! time; Ctrl+Shift+PageUp/PageDown jump a screenful; Ctrl+Shift+End
//! snaps back to the live tail. Any keystroke that is not a scroll
//! shortcut implicitly snaps the view to the bottom and is forwarded
//! to the child via the master fd.

use std::os::fd::AsFd;
use std::time::Duration;

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::vte::ansi::Processor;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::sys::pty::{PtyChild, spawn_shell};
use crate::ui::POLL_SLICE;
use crate::ui::console::Console;
use crate::ui::view::{PtyShellScreenData, render_pty_shell};

/// Minimum grid dimensions used by the pretty-shell box. The runtime
/// size is derived from [`Console::size`] minus the chrome the
/// renderer paints (3-row header, 1-row footer, 2-row + 2-col bordered
/// block), so a 1920x1080 splash gets a much larger PTY than the
/// 80x24-derived floor below.
///
/// The floor exists for tiny consoles (degraded VGA, recovery serial
/// shim) so the alacritty grid never collapses to zero cells. On those
/// hosts the renderer still clips to the actual frame; the larger grid
/// just keeps the VT parser happy.
const PRETTY_SHELL_MIN_COLS: u16 = 40;
const PRETTY_SHELL_MIN_ROWS: u16 = 10;

/// Chrome rows the renderer consumes around the pretty-shell grid:
/// 3-row header, 1-row footer, and the bordered block eats 1 row on
/// top + 1 row on bottom (= 2). Total = 6.
const CHROME_ROWS: u16 = 6;
/// Chrome columns the bordered block consumes: 1 col left + 1 col
/// right (= 2). The header/footer don't add side chrome.
const CHROME_COLS: u16 = 2;

/// Single read budget per loop iteration. Bounded so the driver loop
/// always falls back to render-and-poll-key on time. `alacritty`'s VTE
/// parser handles partial sequences across feeds.
const PTY_READ_CHUNK: usize = 4096;

/// `Dimensions` impl for the pretty-shell grid. Local copy of the same
/// trait impl used by `splash::terminal::SplashTerminal` — the upstream
/// crate's `TermSize` is `cfg(test)`-gated and not reusable.
struct GridSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    fn columns(&self) -> usize {
        self.columns
    }
}

/// All mutable state for the pretty-shell screen. The driver loop
/// constructs this once, drives it to exit, and drops it (which kills
/// the child via [`PtyChild::terminate`] on the cleanup path).
pub struct PtyShellState {
    pub term: Term<VoidListener>,
    pub parser: Processor,
    pub child: PtyChild,
    /// True once the master fd reads return EOF or the child has been
    /// reaped via `try_wait`.
    pub child_exited: bool,
    pub cols: u16,
    pub rows: u16,
}

impl PtyShellState {
    fn new(child: PtyChild, cols: u16, rows: u16) -> Self {
        let size = GridSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(TermConfig::default(), &size, VoidListener);
        Self {
            term,
            parser: Processor::new(),
            child,
            child_exited: false,
            cols,
            rows,
        }
    }

    /// Current scrollback offset (rows above the live tail). Zero means
    /// the live grid is visible.
    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }
}

/// Open a pretty-shell session on the supplied console. Forks
/// `config.paths.shell` onto a fresh PTY, then drives the render-poll-
/// pump loop until the child exits or NMBL detects an I/O failure.
///
/// Returns `Ok(())` on a clean exit (the shell ran to completion) and
/// `Err` only when the supporting plumbing fails (fork, openpty,
/// terminal backend write). The caller in `src/shell.rs` treats both
/// outcomes the same way: re-display the emergency menu.
pub fn run_pretty_shell(console: &mut dyn Console, config: &Config) -> Result<()> {
    // Derive the PTY grid size from the live console dimensions so the
    // alacritty terminal fills the bordered block. The renderer paints
    // a 3-row header + 1-row footer + bordered block (2 rows of border
    // + 2 cols of border); see [`CHROME_ROWS`] / [`CHROME_COLS`].
    let (frame_cols, frame_rows) = console.size();
    let cols = frame_cols
        .saturating_sub(CHROME_COLS)
        .max(PRETTY_SHELL_MIN_COLS);
    let rows = frame_rows
        .saturating_sub(CHROME_ROWS)
        .max(PRETTY_SHELL_MIN_ROWS);

    let child = spawn_shell(&config.paths.shell, cols, rows)?;
    let mut state = PtyShellState::new(child, cols, rows);

    let outcome = drive(&mut state, console);

    // Best-effort kill + reap; safe on a child that has already exited.
    state.child.terminate();

    outcome
}

/// Main loop. Render-then-poll-then-pump. Exits when the child is
/// reaped and the master fd has been drained, or when the operator
/// types the abort shortcut (Ctrl+Shift+Q).
fn drive(state: &mut PtyShellState, console: &mut dyn Console) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            render(state, console)?;
            dirty = false;
        }

        // 1. Poll the keyboard with a short timeout so we get back to
        //    pumping the PTY promptly.
        let key = console.poll_key(POLL_SLICE)?;
        if let Some(k) = key {
            match handle_key(state, k)? {
                KeyOutcome::Quit => return Ok(()),
                KeyOutcome::Redraw => dirty = true,
                KeyOutcome::Noop => {}
            }
        }

        // 2. Drain whatever the child has produced this slice. Multiple
        //    small reads keep memory bounded and let the parser see the
        //    full incremental state.
        match pump_pty(state) {
            Ok(read_any) => {
                if read_any {
                    dirty = true;
                }
            }
            Err(PumpError::Eof) => {
                state.child_exited = true;
            }
            Err(PumpError::Io(e)) => {
                nmbl_warn!("pretty-shell PTY read failed: {e}");
                return Ok(());
            }
        }

        // 3. Reap zombies opportunistically. Don't bail on the FIRST
        //    sight of an exit — the master fd may still hold the
        //    shell's farewell output. Wait for both events.
        if !state.child_exited
            && let Ok(Some(_)) = state.child.try_wait()
        {
            state.child_exited = true;
            dirty = true;
        }

        if state.child_exited {
            // Drain remaining output one last time before bailing.
            let _ = pump_pty(state);
            // One final repaint so the operator sees the shell's last
            // line (typically "exit").
            render(state, console)?;
            return Ok(());
        }
    }
}

/// Render one frame. Backends use `draw_with` because the alacritty
/// grid is dynamic content that doesn't map neatly onto the
/// `App`-typed `Console::render` path.
fn render(state: &PtyShellState, console: &mut dyn Console) -> Result<()> {
    let cols = state.cols;
    let rows = state.rows;
    let scroll = state.scroll_offset();
    let grid_rows = collect_visible_rows(state);
    let data = PtyShellScreenData {
        cols,
        rows,
        rows_text: &grid_rows,
        scroll_offset: scroll,
    };
    console.draw_with(&mut |frame| render_pty_shell(frame, &data))
}

/// Snapshot the visible portion of the alacritty grid as a vector of
/// per-row strings. The row count equals `state.rows` so the renderer
/// can paint each row at a fixed position without scanning bounds.
fn collect_visible_rows(state: &PtyShellState) -> Vec<String> {
    let grid = state.term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    let mut out: Vec<String> = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut line = String::with_capacity(cols);
        for col in 0..cols {
            let point = alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(row as i32),
                alacritty_terminal::index::Column(col),
            );
            let cell = &grid[point];
            // Treat NUL as space so the line is renderable.
            let c = if cell.c == '\0' { ' ' } else { cell.c };
            line.push(c);
        }
        out.push(line);
    }
    out
}

/// Internal pump-error type so the driver loop can distinguish "child
/// closed the PTY" (graceful) from "kernel returned an error" (log and
/// bail).
enum PumpError {
    Eof,
    Io(std::io::Error),
}

/// Drain at most a few non-blocking reads from the master fd into the
/// VT parser. Returns `Ok(true)` if any bytes were fed (the grid may
/// have changed); `Ok(false)` if the fd was empty this slice.
fn pump_pty(state: &mut PtyShellState) -> std::result::Result<bool, PumpError> {
    let mut buf = [0u8; PTY_READ_CHUNK];
    let mut any = false;
    // Bound the per-iteration drain so a runaway `yes` doesn't starve
    // the input poll. Multiple loop iterations will catch up over time.
    for _ in 0..8 {
        let fd = state.child.master.as_fd();
        match rustix::io::read(fd, &mut buf) {
            Ok(0) => return Err(PumpError::Eof),
            Ok(n) => {
                let bytes = buf.get(..n).unwrap_or(&[]);
                state.parser.advance(&mut state.term, bytes);
                any = true;
            }
            Err(rustix::io::Errno::AGAIN) => break,
            Err(rustix::io::Errno::IO) => {
                // EIO on a PTY master typically means the slave hung up
                // (shell exited). Treat as orderly EOF.
                return Err(PumpError::Eof);
            }
            Err(e) => return Err(PumpError::Io(std::io::Error::from(e))),
        }
    }
    Ok(any)
}

/// Outcome of a single keystroke. The driver loop reads this to decide
/// whether to repaint, exit, or proceed silently.
enum KeyOutcome {
    Quit,
    Redraw,
    Noop,
}

/// Translate one [`KeyEvent`] into either a state mutation (scroll
/// shortcut, quit) or a stream of bytes written to the master fd.
fn handle_key(state: &mut PtyShellState, key: KeyEvent) -> Result<KeyOutcome> {
    use crossterm::event::KeyEventKind;
    if key.kind != KeyEventKind::Press {
        // crossterm reports key releases on some backends; ignore them.
        return Ok(KeyOutcome::Noop);
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Ctrl+Shift+* — scroll bindings + emergency quit.
    if ctrl && shift {
        match key.code {
            KeyCode::Up => {
                state.term.grid_mut().scroll_display(Scroll::Delta(1));
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::Down => {
                state.term.grid_mut().scroll_display(Scroll::Delta(-1));
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::PageUp => {
                state.term.grid_mut().scroll_display(Scroll::PageUp);
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::PageDown => {
                state.term.grid_mut().scroll_display(Scroll::PageDown);
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::End => {
                state.term.grid_mut().scroll_display(Scroll::Bottom);
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::Home => {
                state.term.grid_mut().scroll_display(Scroll::Top);
                return Ok(KeyOutcome::Redraw);
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                return Ok(KeyOutcome::Quit);
            }
            _ => {}
        }
    }

    // Any non-scroll keystroke snaps the view to the bottom so the
    // operator sees their own input land in the live grid.
    if state.scroll_offset() != 0 {
        state.term.grid_mut().scroll_display(Scroll::Bottom);
    }

    let bytes = key_to_bytes(key);
    if bytes.is_empty() {
        return Ok(KeyOutcome::Noop);
    }
    write_to_pty(state, &bytes)?;
    // The terminal grid won't change until the shell echoes the byte
    // back; let the read pump trigger the next repaint.
    Ok(KeyOutcome::Noop)
}

/// Write `bytes` to the master fd, retrying on partial writes. EAGAIN
/// is treated as a hard error here because we just polled-then-wrote on
/// a fd that should always accept a keystroke's worth of data; if it
/// refuses, the shell is wedged and the operator wants to know.
fn write_to_pty(state: &mut PtyShellState, bytes: &[u8]) -> Result<()> {
    let fd = state.child.master.as_fd();
    let mut written = 0usize;
    while written < bytes.len() {
        match rustix::io::write(fd, bytes.get(written..).unwrap_or(&[])) {
            Ok(0) => break,
            Ok(n) => written = written.saturating_add(n),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                return Err(NmblError::Tui {
                    source: std::io::Error::from(e),
                });
            }
        }
    }
    Ok(())
}

/// Convert one [`KeyEvent`] into the byte sequence a typical terminal
/// emulator would send to the slave. The mapping intentionally targets
/// busybox / xterm conventions and ignores OS-specific keymap features
/// (Meta-as-Esc, application-mode arrows). Programs that need
/// application mode work by emitting their own escapes via DECSET.
pub fn key_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Map Ctrl+letter to control bytes (^A=0x01, …). Pass
                // through punctuation unchanged so e.g. Ctrl+] still
                // does the right thing for shells that bind it.
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    return vec![(upper as u8) & 0x1F];
                }
                match upper {
                    '@' => return vec![0x00],
                    '[' => return vec![0x1B],
                    '\\' => return vec![0x1C],
                    ']' => return vec![0x1D],
                    '^' => return vec![0x1E],
                    '_' => return vec![0x1F],
                    _ => {}
                }
            }
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => b"\x7f".to_vec(),
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        // Function keys — VT100 / xterm sequences. Rarely used in a
        // recovery shell but cheap to include.
        KeyCode::F(1) => b"\x1bOP".to_vec(),
        KeyCode::F(2) => b"\x1bOQ".to_vec(),
        KeyCode::F(3) => b"\x1bOR".to_vec(),
        KeyCode::F(4) => b"\x1bOS".to_vec(),
        KeyCode::F(n @ 5..=12) => {
            // CSI sequences for F5-F12.
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return Vec::new(),
            };
            format!("\x1b[{code}~").into_bytes()
        }
        _ => Vec::new(),
    }
}

// Silence the warning about Duration being imported but unused on
// builds where the driver loop doesn't expand to a Duration call site.
#[allow(dead_code)]
fn _duration_marker(_d: Duration) {}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    fn press_with(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn key_to_bytes_plain_chars_round_trip_ascii() {
        let out = key_to_bytes(press_with(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(out, b"a");
        let out = key_to_bytes(press_with(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(out, b" ");
    }

    #[test]
    fn key_to_bytes_control_letters_map_to_control_bytes() {
        assert_eq!(
            key_to_bytes(press_with(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            vec![0x01]
        );
        assert_eq!(
            key_to_bytes(press_with(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            vec![0x03]
        );
        assert_eq!(
            key_to_bytes(press_with(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            vec![0x04]
        );
    }

    #[test]
    fn key_to_bytes_special_keys_emit_csi() {
        assert_eq!(
            key_to_bytes(press_with(KeyCode::Up, KeyModifiers::NONE)),
            b"\x1b[A"
        );
        assert_eq!(
            key_to_bytes(press_with(KeyCode::Down, KeyModifiers::NONE)),
            b"\x1b[B"
        );
        assert_eq!(
            key_to_bytes(press_with(KeyCode::Enter, KeyModifiers::NONE)),
            b"\r"
        );
        assert_eq!(
            key_to_bytes(press_with(KeyCode::Backspace, KeyModifiers::NONE)),
            b"\x7f"
        );
    }

    #[test]
    fn key_to_bytes_multibyte_utf8_round_trips() {
        // German u-umlaut: U+00FC, UTF-8 0xC3 0xBC.
        let out = key_to_bytes(press_with(KeyCode::Char('ü'), KeyModifiers::NONE));
        assert_eq!(out, vec![0xC3, 0xBC]);
    }
}
