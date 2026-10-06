//! Raw-mode tty backend for the [`Console`] abstraction.
//!
//! Opens `/dev/console` (or adopts a remote operator's pty), enters raw
//! mode, and drives a [`ratatui::Terminal`] over a [`QueuedBackend`]: a
//! termwiz `Surface` + terminfo renderer that renders into an in-memory
//! queue. The fd is non-blocking because NMBL is a single-threaded PID 1;
//! the queue is written with non-blocking writes and drained when the fd
//! becomes writable, so a slow terminal is back-pressure (frames
//! coalesce) rather than an error, and one stalled remote peer can never
//! park the whole init. Crossterm's `OnceLock`-backed stdin reader is
//! never involved.
//!
//! ## Why we don't reuse [`RawModeGuard`]
//!
//! [`RawModeGuard`] holds a [`BorrowedFd`] with an explicit lifetime,
//! which doesn't compose with self-referential storage inside this
//! struct. We mirror [`crate::splash::input::SplashInput`]: own the
//! [`OwnedFd`] plus a saved [`Termios`] snapshot and restore it on
//! [`Drop`].
//!
//! ## Hang-up
//!
//! A remote pty whose operator went away reads EOF (or `EIO`) and polls
//! `POLLHUP` forever. For a remote console that is a disconnect: input
//! polling fails so the session ends and every fd is released, instead
//! of treating EOF as "no input" and spinning. The primary console keeps
//! its historic tolerance but waits out the poll slice after EOF.
//!
//! ## VT text mode
//!
//! When `/dev/console` is bound to a kernel VT (the framebuffer case,
//! not a serial line), ANSI output must use `KD_TEXT` to remain visible.
//! Kernel-printk is silenced separately with `PrintkQuiet`; on non-VT lines
//! (serial console) the ioctl returns `ENOTTY` and we tolerate it.
//!
//! See [`kd`] for the ioctl helpers.
//!
//! ## Input pipeline
//!
//! We own the read path. Bytes come off the same fd through `rustix::io::read`, get pre-filtered by
//! [`ResizeFilter`] to extract `CSI 8;rows;cols t` host-size reports
//! (which termwiz drops because it only synthesises `Resized` from
//! SIGWINCH, never from the in-band report a serial-attached
//! terminal sends), and the leftover bytes go through
//! [`TermwizToCrossterm`] which wraps `termwiz::input::InputParser`.
//! See `src/ui/console/parser.rs` for the byte-level state machine.

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::KeyEvent;
use ratatui::Terminal;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::termios::Termios;

use crate::error::Result;
use crate::log;
use crate::nmbl_warn;
use crate::sys::printk::PrintkQuiet;
use crate::sys::tty::{enter_raw, open_console as open_console_fd};
use crate::ui::console::ConsoleEvent;
use crate::ui::console::parser::{ResizeFilter, TermwizToCrossterm};

use self::caps::caps_from_env_with_fallback;
use self::kd::enter_kd_text;
use self::queued::QueuedBackend;
use self::util::{duration_to_ms, rustix_io_err, tui_err};

mod caps;
mod impls;
mod kd;
mod queued;
mod util;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests;

/// Default tty path the orchestrator opens at boot.
const CONSOLE_PATH: &str = "/dev/console";

/// Fallback grid geometry used when the line reports no winsize
/// (`TIOCGWINSZ` → 0x0, the serial-console case). The host's
/// `CSI 8;rows;cols t` report corrects this on the first resize.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Resolve the seed grid for a pty whose handshake winsize may be 0x0
/// (a freshly-allocated pty before the client's `TIOCSWINSZ`, or a
/// client that reported nothing). Returns `(rows, cols)`, substituting
/// the 80x24 default for any zero dimension so the first frame paints.
fn pty_seed_size(winsize: (u16, u16)) -> (u16, u16) {
    let (rows, cols) = winsize;
    let rows = if rows == 0 { DEFAULT_ROWS } else { rows };
    let cols = if cols == 0 { DEFAULT_COLS } else { cols };
    (rows, cols)
}

/// How long dropping or suspending a console may wait for queued output
/// to reach the primary console (a slow serial line) before giving up.
const PRIMARY_DRAIN_BUDGET: Duration = Duration::from_secs(5);
/// The same bound for a remote pty. The session is over by then; a
/// stalled peer must not hold PID 1 for long.
const REMOTE_DRAIN_BUDGET: Duration = Duration::from_millis(500);

/// Raw-mode tty backend. See module docs for the lifetime story.
pub struct TtyConsole {
    /// Owns the console fd for the lifetime of the console. Input and
    /// output both go through it with non-blocking rustix I/O.
    fd: OwnedFd,
    /// File-status flags found on the fd's open file description before
    /// we forced `O_NONBLOCK`. A remote pty's description is shared with
    /// the operator's shell, so drop restores these.
    saved_flags: Option<OFlags>,
    /// Whether input EOF / hang-up ends the console (remote pty) or is
    /// tolerated (the primary console).
    hangup_is_disconnect: bool,
    /// Set when the last input poll saw EOF on a console that tolerates
    /// it, so the async poll waits out its slice instead of spinning.
    input_at_eof: bool,
    /// Termios snapshot to restore on drop. `Option` so [`Drop`] can
    /// take it without leaving a dangling clone.
    saved_termios: Option<Termios>,
    /// Previous KD VT mode, captured iff we successfully switched the
    /// VT into `KD_TEXT`.
    previous_kd_mode: Option<libc::c_long>,
    /// Serial-console mitigation for the kernel-printk smear.
    printk_quiet: Option<PrintkQuiet>,
    /// Ratatui terminal over the queued termwiz renderer.
    terminal: Terminal<QueuedBackend>,
    /// Input pre-filter for `CSI 8;rows;cols t` host-resize reports.
    /// Drains bytes between `rustix::io::read` and the termwiz parser.
    resize_filter: ResizeFilter,
    /// Termwiz input parser that produces crossterm `KeyEvent`s. Owns
    /// the lone-ESC state, partial-sequence buffering, etc.
    key_parser: TermwizToCrossterm,
    /// Translated key events drained from `key_parser` but not yet
    /// surfaced to the caller. `poll_event` pops one per call.
    pending_keys: VecDeque<KeyEvent>,
    /// Mouse-wheel scroll notches drained from `key_parser` but not yet
    /// surfaced. `poll_event` pops one per call after pending keys.
    pending_scrolls: VecDeque<ConsoleEvent>,
    /// Latest grid size observed via a CSI 8;rows;cols t report from
    /// the host terminal. Wins over the backend's reported size.
    last_resize: Option<(u16, u16)>,
    /// Whether this console owns the process-global TUI state — the
    /// printk-quiet engagement and the stderr-suppression refcount
    /// (`log::set_tui_active`). The primary boot console (`open`/
    /// `open_path`) owns it; a remote-TUI console built on a received
    /// pty (`from_pty`) does NOT — it renders to its own pty only and
    /// must never silence the local console's printk or stderr, nor
    /// touch the kernel VT mode (a pty is never a VT).
    owns_global_tui_state: bool,
}

impl TtyConsole {
    /// Open the default console path (`/dev/console`).
    pub fn open() -> Result<TtyConsole> {
        Self::open_path(Path::new(CONSOLE_PATH))
    }

    /// Open a caller-specified tty path. Used by tests and any future
    /// caller that wants to drive a non-default console node.
    pub fn open_path(path: &Path) -> Result<TtyConsole> {
        let fd = open_console_fd(path)?;
        let saved = enter_raw(fd.as_fd())?;
        // The primary console may be a kernel VT (framebuffer case):
        // ANSI output must stay in text mode to remain visible.
        let previous_kd_mode = enter_kd_text(fd.as_fd());
        let saved_flags = Self::make_nonblocking(&fd);
        let terminal = Self::build_terminal(&fd, None)?;

        // Silence kernel-printk to console while we own the screen.
        let printk_quiet = Some(PrintkQuiet::engage());
        // Tell the `nmbl_*!` macros to stop writing to stderr.
        log::set_tui_active();

        Ok(TtyConsole {
            fd,
            saved_flags,
            hangup_is_disconnect: false,
            input_at_eof: false,
            saved_termios: Some(saved),
            previous_kd_mode,
            printk_quiet,
            terminal,
            resize_filter: ResizeFilter::new(),
            key_parser: TermwizToCrossterm::new(),
            pending_keys: VecDeque::new(),
            pending_scrolls: VecDeque::new(),
            last_resize: None,
            owns_global_tui_state: true,
        })
    }

    /// Build a [`TtyConsole`] on an already-open pty `OwnedFd`, seeding
    /// the render surface from `winsize` (`(rows, cols)`).
    ///
    /// This is the remote-TUI path: the operator's client passes its
    /// controlling terminal across the root-only socket via `SCM_RIGHTS`
    /// and PID 1 drives a TUI on that pty. Unlike [`open_path`] it:
    ///   * does NOT touch the kernel VT mode — a pty is never a VT, so
    ///     `KDSETMODE` is meaningless (and would `ENOTTY`);
    ///   * does NOT engage `PrintkQuiet` or `log::set_tui_active` — those
    ///     are process-global and belong to the local boot console; a
    ///     remote session must not silence the local console's printk or
    ///     stderr. The session renders ONLY to its own pty.
    ///
    /// The saved termios is restored on drop, so the client's terminal is
    /// left clean when the session ends.
    pub fn from_pty(fd: OwnedFd, winsize: (u16, u16)) -> Result<TtyConsole> {
        let saved = enter_raw(fd.as_fd())?;
        // `winsize` is `(rows, cols)` (handshake order). `build_terminal`
        // takes the same order; `last_resize`/`size()` use `(cols, rows)`.
        let (rows, cols) = pty_seed_size(winsize);
        let saved_flags = Self::make_nonblocking(&fd);
        let terminal = Self::build_terminal(&fd, Some((rows, cols)))?;
        Ok(TtyConsole {
            fd,
            saved_flags,
            // The remote operator's terminal hanging up IS the session
            // ending: never keep polling a dead pty.
            hangup_is_disconnect: true,
            input_at_eof: false,
            saved_termios: Some(saved),
            previous_kd_mode: None,
            printk_quiet: None,
            terminal,
            resize_filter: ResizeFilter::new(),
            key_parser: TermwizToCrossterm::new(),
            pending_keys: VecDeque::new(),
            pending_scrolls: VecDeque::new(),
            // Seed the cached size so the first render and any modal
            // layout use the client's reported geometry immediately,
            // before the in-band `CSI 8;rows;cols t` report (if any).
            last_resize: Some((cols, rows)),
            owns_global_tui_state: false,
        })
    }

    /// Put the console fd's open file description into non-blocking
    /// mode and return the flags it had before, for restoration on drop.
    fn make_nonblocking(fd: &OwnedFd) -> Option<OFlags> {
        let saved = match fcntl_getfl(fd.as_fd()) {
            Ok(flags) => flags,
            Err(e) => {
                nmbl_warn!(
                    "TtyConsole: F_GETFL on console fd {} failed: {e}",
                    fd.as_raw_fd()
                );
                return None;
            }
        };
        if let Err(e) = fcntl_setfl(fd.as_fd(), saved | OFlags::NONBLOCK) {
            nmbl_warn!(
                "TtyConsole: F_SETFL(O_NONBLOCK) on console fd {} failed: {e}; \
                 reads and writes may block",
                fd.as_raw_fd()
            );
        }
        Some(saved)
    }

    /// Shared ratatui terminal construction for both constructors.
    /// `seed` overrides the surface size when the fd reports no winsize
    /// (serial line or pty); `None` falls back to the historic 80x24.
    fn build_terminal(fd: &OwnedFd, seed: Option<(u16, u16)>) -> Result<Terminal<QueuedBackend>> {
        // We feed explicit caps (bundled terminfo + truecolor) rather
        // than reading `$TERM`, since NMBL boots with no environment.
        let caps = caps_from_env_with_fallback()?;

        // A serial line / freshly-allocated pty may report no winsize
        // (0x0): a zero-area surface renders nothing at all (the
        // empty-pane regression). Seed sane dimensions so the very first
        // frame paints; the host's `CSI 8;rows;cols t` report later
        // corrects the geometry via `apply_resize`.
        let (mut cols, mut rows) = rustix::termios::tcgetwinsize(fd.as_fd())
            .map(|ws| (ws.ws_col, ws.ws_row))
            .unwrap_or((0, 0));
        if cols == 0 || rows == 0 {
            (rows, cols) = seed.unwrap_or((DEFAULT_ROWS, DEFAULT_COLS));
        }

        let backend = QueuedBackend::new(caps, usize::from(cols), usize::from(rows));
        Terminal::new(backend).map_err(tui_err)
    }

    /// Whether rendered output is still waiting for the terminal.
    fn output_pending(&self) -> bool {
        !self.terminal.backend().pending().is_empty()
    }

    /// Write as much queued output as the terminal accepts right now,
    /// rendering deferred frames whenever the queue empties. `EAGAIN` is
    /// back-pressure: the rest stays queued for the next writable event.
    /// Any other error (`EIO` from a hung-up pty, ...) is a disconnect.
    fn pump_output(&mut self) -> Result<()> {
        loop {
            let backend = self.terminal.backend_mut();
            if backend.pending().is_empty() && !backend.render_deferred().map_err(tui_err)? {
                return Ok(());
            }
            match rustix::io::write(&self.fd, backend.pending()) {
                Ok(0) => {
                    return Err(tui_err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "console accepted no output",
                    )));
                }
                Ok(n) => backend.consume(n),
                Err(rustix::io::Errno::INTR) => {}
                Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::WOULDBLOCK => {
                    return Ok(());
                }
                Err(e) => return Err(rustix_io_err(e)),
            }
        }
    }

    /// Synchronously wait (bounded by `budget`) until queued output has
    /// been written. Used before the console is handed to someone else
    /// or dropped, where no later writable event will drain it.
    fn drain_output(&mut self, budget: Duration) -> Result<()> {
        let deadline = Instant::now() + budget;
        loop {
            self.pump_output()?;
            if !self.output_pending() {
                return Ok(());
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(());
            };
            let mut pfd = [PollFd::new(&self.fd, PollFlags::OUT)];
            match poll(&mut pfd, duration_to_ms(left).max(1)) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(rustix_io_err(e)),
            }
        }
    }

    fn drain_budget(&self) -> Duration {
        if self.hangup_is_disconnect {
            REMOTE_DRAIN_BUDGET
        } else {
            PRIMARY_DRAIN_BUDGET
        }
    }

    /// Input EOF: a disconnect for a remote pty, tolerated (but noted so
    /// the async poll does not spin) on the primary console.
    fn input_eof(&mut self) -> Result<Option<ConsoleEvent>> {
        if self.hangup_is_disconnect {
            return Err(tui_err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "remote terminal hung up",
            )));
        }
        self.input_at_eof = true;
        self.drain_after_eagain()
    }

    /// Read whatever bytes are ready on `self.fd`, run them through
    /// the resize pre-filter, feed the leftovers to termwiz's input
    /// parser, and stash any emitted key events into
    /// `self.pending_keys`. Returns at most one [`ConsoleEvent::Resize`]
    /// extracted from the byte stream (the pre-filter emits at most
    /// one per call).
    fn refill(&mut self, timeout_ms: i32) -> Result<Option<ConsoleEvent>> {
        self.input_at_eof = false;
        let mut pfd = [PollFd::new(&self.fd, PollFlags::IN)];
        let ready = poll(&mut pfd, timeout_ms).map_err(rustix_io_err)?;
        if ready == 0 {
            // No bytes arrived — tell the termwiz parser there's
            // nothing more right now so a dangling ESC commits.
            return self.drain_after_eagain();
        }
        let revents = pfd
            .first()
            .map(PollFd::revents)
            .unwrap_or_else(PollFlags::empty);
        if !revents.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
            return self.drain_after_eagain();
        }

        // Drain in a loop: a single resize burst can deliver dozens
        // of bytes and may interleave keystrokes. 256-byte chunks.
        loop {
            let mut chunk = [0u8; 256];
            match rustix::io::read(&self.fd, &mut chunk) {
                Ok(0) => {
                    // EOF: the terminal hung up.
                    return self.input_eof();
                }
                Ok(n) => {
                    let slice = chunk.get(..n).unwrap_or(&[]);
                    self.resize_filter.push(slice);
                    // Drain everything classified so far. If the
                    // filter emits a Resize, return immediately so
                    // the caller can react; remaining bytes stay in
                    // the filter for the next call.
                    if let Some(ev) = self.drain_filter_once(/*maybe_more=*/ true)? {
                        return Ok(Some(ev));
                    }
                    if n < chunk.len() {
                        continue;
                    }
                }
                Err(e) if e == rustix::io::Errno::AGAIN || e == rustix::io::Errno::WOULDBLOCK => {
                    return self.drain_after_eagain();
                }
                Err(e) => return Err(rustix_io_err(e)),
            }
        }
    }

    /// Drain one resize from the pre-filter and feed the
    /// pre-resize bytes into termwiz. `maybe_more` controls whether
    /// a lone ESC commits this round.
    fn drain_filter_once(&mut self, maybe_more: bool) -> Result<Option<ConsoleEvent>> {
        let mut scratch = [0u8; 256];
        let (n, ev) = self.resize_filter.drain(&mut scratch);
        if n > 0 {
            let bytes = scratch.get(..n).unwrap_or(&[]);
            let mut keys = Vec::new();
            let mut scrolls = Vec::new();
            self.key_parser
                .feed_events(bytes, maybe_more, &mut keys, &mut scrolls);
            for k in keys {
                self.pending_keys.push_back(k);
            }
            for s in scrolls {
                self.pending_scrolls.push_back(s);
            }
        }
        Ok(ev)
    }

    /// Drain pending bytes assuming no more input will arrive in this
    /// poll cycle. This flushes any dangling ESC sequences (so a lone
    /// ESC commits as `KeyCode::Esc`) and emits one final Resize if
    /// the filter has one queued.
    fn drain_after_eagain(&mut self) -> Result<Option<ConsoleEvent>> {
        self.drain_filter_once(/*maybe_more=*/ false)
    }

    /// Side-effect helper used by [`Console::poll_event`]: if the
    /// event is a [`ConsoleEvent::Resize`], cache the new size and
    /// retarget the ratatui terminal so the next render fills the
    /// reported area rather than the stale backend size.
    fn apply_resize(&mut self, ev: &ConsoleEvent) {
        let ConsoleEvent::Resize { rows, cols } = *ev else {
            return;
        };
        self.last_resize = Some((cols, rows));
        // Resize the termwiz `Surface` first. `backend.size()` reads
        // the surface dimensions, and ratatui's `draw()` calls
        // `autoresize()` which snaps `last_known_area` back to whatever
        // `backend.size()` reports. If we only resized the ratatui
        // terminal, the next `draw()` would immediately revert it to
        // the stale surface size, so the surface is the source of truth.
        self.terminal
            .backend_mut()
            .resize(usize::from(cols), usize::from(rows));
        if let Err(e) = self
            .terminal
            .resize(ratatui::layout::Rect::new(0, 0, cols, rows))
        {
            nmbl_warn!(
                "TtyConsole: ratatui resize to {cols}x{rows} failed: {e}; \
                 next render will recompute layout"
            );
        }
    }
}
