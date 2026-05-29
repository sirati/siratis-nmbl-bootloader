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
//!
//! ## Quitting
//!
//! `Ctrl+Shift+<letter>` is not encodable over a legacy serial/xterm
//! line, so quit uses the OpenSSH-style escape instead: at the start of
//! a line, type `~.` to return to the emergency menu. The `~` is only
//! honoured immediately after a newline; a mid-line `~` is an ordinary
//! character, and `~~` sends a literal tilde.

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
use crate::ui::console::{Console, ConsoleEvent};
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

/// Scrollback rows moved per mouse-wheel notch, matching a typical
/// terminal-emulator wheel step. Ctrl+Shift+Up/Down still step one row
/// at a time; the wheel scrolls a few rows per detent so a flick covers
/// ground.
const WHEEL_SCROLL_STEP: i32 = 3;

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
    /// SSH-style `<newline>~.` quit-escape recogniser. Tracks where in
    /// the input stream we are so a bare `~` only triggers when typed at
    /// the start of a line, exactly like OpenSSH's `~.` escape.
    escape: EscapeState,
}

/// State of the SSH-style `~.` escape recogniser. The escape char is
/// only honoured immediately after a line break (or at session start),
/// mirroring OpenSSH client behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    /// At the start of a line — a `~` here arms the escape.
    LineStart,
    /// Mid-line — `~` is an ordinary character.
    MidLine,
    /// A line-leading `~` was seen; the next byte selects the command
    /// (`.` quits, anything else is passed through).
    Armed,
}

/// What the escape recogniser decided for a chunk of outgoing bytes.
enum EscapeOutcome {
    /// Forward these bytes to the child (may differ from the input when
    /// an escape was partially consumed), then continue.
    Forward(Vec<u8>),
    /// `~.` was completed — quit the pretty shell.
    Quit,
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
            // The shell prompt starts a fresh line, so a `~` typed as
            // the very first keystroke arms the escape.
            escape: EscapeState::LineStart,
        }
    }

    /// Run a chunk of outgoing bytes (the encoding of one keystroke)
    /// through the SSH-style escape recogniser, updating the line-start
    /// tracking and detecting the `~.` quit sequence.
    fn process_escape(&mut self, bytes: &[u8]) -> EscapeOutcome {
        run_escape(&mut self.escape, bytes)
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
/// types the SSH-style `<newline>~.` quit escape.
fn drive(state: &mut PtyShellState, console: &mut dyn Console) -> Result<()> {
    let mut dirty = true;
    loop {
        if dirty {
            render(state, console)?;
            dirty = false;
        }

        // 1. Poll for one input event with a short timeout so we get
        //    back to pumping the PTY promptly. We use `poll_event` (not
        //    `poll_key`) so host-terminal resizes reach us: the default
        //    `poll_key` adapter silently drops `ConsoleEvent::Resize`,
        //    which would leave the shell box stuck at its old geometry.
        match console.poll_event(POLL_SLICE)? {
            Some(ConsoleEvent::Key(k)) => match handle_key(state, k)? {
                KeyOutcome::Quit => return Ok(()),
                KeyOutcome::Redraw => dirty = true,
                KeyOutcome::Noop => {}
            },
            // The backend has already cached the new size; re-derive the
            // grid geometry and push it to the emulator + child. The guard
            // applies the resize and only marks dirty when geometry changed.
            Some(ConsoleEvent::Resize { .. }) if apply_resize(state, console) => {
                dirty = true;
            }
            // Mouse wheel drives NMBL's scrollback exactly like
            // Ctrl+Shift+Up/Down — a few rows per notch. A wheel notch is
            // a scroll, not a keystroke, so it must NOT snap the view to
            // the bottom and is never forwarded to the child PTY.
            Some(ConsoleEvent::Scroll { up }) => {
                handle_scroll(state, up);
                dirty = true;
            }
            _ => {}
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
    // `Grid`'s `Index<Point>` ignores the scrollback `display_offset`:
    // `Line(0)` is always the top of the *live* viewport regardless of
    // how far the operator has scrolled back. To render the DISPLAYED
    // region we shift every line up by the offset, so `display_offset =
    // N` shows the screenful that starts `N` rows above the live tail.
    // The shifted lines stay within `[topmost_line, bottommost_line]`
    // because `scroll_display` clamps the offset to the history size.
    let offset = grid.display_offset() as i32;
    let mut out: Vec<String> = Vec::with_capacity(rows);
    for row in 0..rows {
        let line_idx = row as i32 - offset;
        let mut line = String::with_capacity(cols);
        for col in 0..cols {
            let point = alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(line_idx),
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

/// Derive the pretty-shell grid geometry from the current console
/// frame size, the same way [`run_pretty_shell`] does at startup.
fn grid_size_from_console(console: &dyn Console) -> (u16, u16) {
    let (frame_cols, frame_rows) = console.size();
    let cols = frame_cols
        .saturating_sub(CHROME_COLS)
        .max(PRETTY_SHELL_MIN_COLS);
    let rows = frame_rows
        .saturating_sub(CHROME_ROWS)
        .max(PRETTY_SHELL_MIN_ROWS);
    (cols, rows)
}

/// React to a host-terminal resize: re-derive the grid geometry from
/// the (already-updated) console size, resize the alacritty emulator
/// grid, update the cached `state.cols`/`state.rows`, and push the new
/// winsize down to the PTY so the child shell and any full-screen
/// program running on it get `SIGWINCH`. Returns `true` when the grid
/// actually changed (so the caller should repaint).
fn apply_resize(state: &mut PtyShellState, console: &dyn Console) -> bool {
    let (cols, rows) = grid_size_from_console(console);
    if cols == state.cols && rows == state.rows {
        return false;
    }
    let size = GridSize {
        columns: cols as usize,
        screen_lines: rows as usize,
    };
    state.term.resize(size);
    state.cols = cols;
    state.rows = rows;
    // Best-effort: the in-process grid has already reflowed; a failure
    // here only means the child keeps stale `$LINES`/`$COLUMNS`.
    if let Err(e) = state.child.resize(cols, rows) {
        nmbl_warn!("pretty-shell PTY winsize update to {cols}x{rows} failed: {e}");
    }
    true
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
    // Run the keystroke's bytes through the SSH-style `<newline>~.`
    // quit recogniser before forwarding. A line-leading `~` is held
    // back until the next byte decides whether it begins the `~.` quit
    // command or is just a literal tilde.
    match state.process_escape(&bytes) {
        EscapeOutcome::Quit => return Ok(KeyOutcome::Quit),
        EscapeOutcome::Forward(forward) => {
            if !forward.is_empty() {
                write_to_pty(state, &forward)?;
            }
        }
    }
    // The terminal grid won't change until the shell echoes the byte
    // back; let the read pump trigger the next repaint.
    Ok(KeyOutcome::Noop)
}

/// The signed `Scroll::Delta` for one mouse-wheel notch. Wheel-up is a
/// positive delta (toward older scrollback); wheel-down is negative
/// (toward the live tail). Pure so the sign mapping is unit-testable
/// without a live PTY.
fn wheel_scroll_delta(up: bool) -> i32 {
    if up {
        WHEEL_SCROLL_STEP
    } else {
        -WHEEL_SCROLL_STEP
    }
}

/// Scroll the scrollback in response to one mouse-wheel notch. Uses the
/// same `scroll_display` path as the Ctrl+Shift+Up/Down key bindings;
/// `scroll_display` clamps the offset to the history size, so
/// over-scrolling at either end is a no-op.
fn handle_scroll(state: &mut PtyShellState, up: bool) {
    state
        .term
        .grid_mut()
        .scroll_display(Scroll::Delta(wheel_scroll_delta(up)));
}

/// Compute the escape line-state implied by having just sent byte `b`:
/// a carriage return or newline puts us at the start of a fresh line
/// (where a `~` arms the escape); any other byte is mid-line.
fn next_line_state(b: u8) -> EscapeState {
    if b == b'\r' || b == b'\n' {
        EscapeState::LineStart
    } else {
        EscapeState::MidLine
    }
}

/// Pure SSH-style `<newline>~.` recogniser over a byte chunk. Mutates
/// `escape` in place and returns the bytes to forward (or `Quit`). Split
/// out as a free function so it can be unit-tested without a live PTY.
fn run_escape(escape: &mut EscapeState, bytes: &[u8]) -> EscapeOutcome {
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len().saturating_add(1));
    for &b in bytes {
        match *escape {
            EscapeState::Armed => {
                if b == b'.' {
                    return EscapeOutcome::Quit;
                }
                if b == b'~' {
                    // `~~` is the escape for a single literal tilde
                    // (OpenSSH convention): emit one `~` and stay
                    // mid-line so a following `.` is not a quit.
                    out.push(b'~');
                    *escape = EscapeState::MidLine;
                } else {
                    // Not an escape command: emit the deferred `~` then
                    // the current byte, recomputing line state.
                    out.push(b'~');
                    out.push(b);
                    *escape = next_line_state(b);
                }
            }
            EscapeState::LineStart if b == b'~' => {
                // Defer the `~`: don't forward it yet — it may be the
                // start of an escape.
                *escape = EscapeState::Armed;
            }
            _ => {
                out.push(b);
                *escape = next_line_state(b);
            }
        }
    }
    EscapeOutcome::Forward(out)
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

    // --- SSH-style `<newline>~.` quit escape ------------------------

    /// Helper: feed a byte stream through the escape recogniser starting
    /// at the start of a line, returning either the forwarded bytes or a
    /// `None` sentinel meaning "quit fired".
    fn feed(bytes: &[u8]) -> Option<Vec<u8>> {
        let mut st = EscapeState::LineStart;
        match run_escape(&mut st, bytes) {
            EscapeOutcome::Quit => None,
            EscapeOutcome::Forward(v) => Some(v),
        }
    }

    #[test]
    fn escape_tilde_dot_at_line_start_quits() {
        // The canonical sequence: line-leading `~` then `.`.
        assert_eq!(feed(b"~."), None, "~. at line start must quit");
    }

    #[test]
    fn escape_tilde_dot_after_newline_quits() {
        // Type `ls\r`, then `~.`: the `\r` returns us to line start so
        // the `~` arms again.
        assert_eq!(feed(b"ls\r~."), None);
    }

    #[test]
    fn escape_midline_tilde_is_literal() {
        // A `~` that is NOT at the start of a line is an ordinary char,
        // so `a~.` forwards verbatim and never quits.
        assert_eq!(feed(b"a~."), Some(b"a~.".to_vec()));
    }

    #[test]
    fn escape_tilde_then_other_forwards_both() {
        // `~` armed, then `x` (not `.`/`~`): the deferred `~` and the `x`
        // are both forwarded.
        assert_eq!(feed(b"~x"), Some(b"~x".to_vec()));
    }

    #[test]
    fn escape_double_tilde_is_single_literal() {
        // `~~` at line start collapses to one literal `~` (OpenSSH
        // convention) and does not quit on a trailing `.`.
        assert_eq!(feed(b"~~."), Some(b"~.".to_vec()));
    }

    #[test]
    fn escape_lone_tilde_is_held_back() {
        // A line-leading `~` with nothing after it yet is deferred (not
        // forwarded) — exactly like SSH waiting for the escape command.
        assert_eq!(feed(b"~"), Some(Vec::new()));
    }

    // --- scrollback rendering / resize ------------------------------

    /// Build a bare [`Term`] (no PTY) so the grid-snapshot and resize
    /// helpers can be exercised without forking a shell.
    fn term_with_lines(cols: u16, rows: u16, lines: &[&str]) -> Term<VoidListener> {
        let size = GridSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let mut term = Term::new(TermConfig::default(), &size, VoidListener);
        let mut parser: Processor = Processor::new();
        for line in lines {
            parser.advance(&mut term, line.as_bytes());
            parser.advance(&mut term, b"\r\n");
        }
        term
    }

    /// Collect the displayed rows directly off a `Term`, mirroring the
    /// production `collect_visible_rows` shift-by-`display_offset` logic.
    fn visible(term: &Term<VoidListener>) -> Vec<String> {
        let grid = term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        let offset = grid.display_offset() as i32;
        (0..rows)
            .map(|row| {
                let line_idx = row as i32 - offset;
                (0..cols)
                    .map(|col| {
                        let p = alacritty_terminal::index::Point::new(
                            alacritty_terminal::index::Line(line_idx),
                            alacritty_terminal::index::Column(col),
                        );
                        let c = grid[p].c;
                        if c == '\0' { ' ' } else { c }
                    })
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn collect_visible_rows_reflects_display_offset() {
        // Push more lines than fit so there is scrollback history.
        let lines: Vec<String> = (0..30).map(|i| format!("line{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let term = term_with_lines(20, 6, &refs);

        // Live tail: the most recent lines are visible, "line0" is gone.
        let tail = visible(&term).join("\n");
        assert!(!tail.contains("line0"), "live tail should not show line0");

        // Scroll up several rows; older content must come into view that
        // was NOT visible at the live tail.
        let mut term = term;
        term.grid_mut()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(10));
        assert!(term.grid().display_offset() > 0, "offset must be non-zero");
        let scrolled = visible(&term).join("\n");
        assert_ne!(
            scrolled, tail,
            "scrolled view must differ from the live tail"
        );
    }

    #[test]
    fn wheel_scroll_delta_sign_matches_direction() {
        assert!(
            wheel_scroll_delta(true) > 0,
            "wheel-up must scroll toward older scrollback (positive delta)"
        );
        assert!(
            wheel_scroll_delta(false) < 0,
            "wheel-down must scroll toward the live tail (negative delta)"
        );
    }

    /// One wheel-up notch moves the scrollback offset off the live tail
    /// (by `WHEEL_SCROLL_STEP` rows), and a wheel-down notch brings it
    /// back. Exercises the exact `scroll_display` path `handle_scroll`
    /// drives, without a live PTY.
    #[test]
    fn wheel_notches_move_scroll_offset() {
        let lines: Vec<String> = (0..30).map(|i| format!("line{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let mut term = term_with_lines(20, 6, &refs);
        assert_eq!(term.grid().display_offset(), 0, "starts at live tail");

        // Wheel up one notch.
        term.grid_mut()
            .scroll_display(Scroll::Delta(wheel_scroll_delta(true)));
        assert_eq!(
            term.grid().display_offset(),
            WHEEL_SCROLL_STEP as usize,
            "wheel-up moves the offset up by one step"
        );

        // Wheel down one notch returns to the live tail.
        term.grid_mut()
            .scroll_display(Scroll::Delta(wheel_scroll_delta(false)));
        assert_eq!(
            term.grid().display_offset(),
            0,
            "wheel-down snaps back to the live tail"
        );
    }

    #[test]
    fn resize_updates_grid_dimensions() {
        let term = term_with_lines(80, 24, &["hello"]);
        assert_eq!(term.grid().columns(), 80);
        assert_eq!(term.grid().screen_lines(), 24);

        let mut term = term;
        term.resize(GridSize {
            columns: 100,
            screen_lines: 30,
        });
        assert_eq!(term.grid().columns(), 100, "cols must track resize");
        assert_eq!(term.grid().screen_lines(), 30, "rows must track resize");
    }
}
