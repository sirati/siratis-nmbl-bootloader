//! Raw-mode tty backend for the [`Console`] abstraction.
//!
//! Opens `/dev/console`, enters raw mode, and drives a
//! [`ratatui::Terminal`] over a [`TermwizBackend`] that writes through
//! a [`BufferedTerminal`] wrapping a [`UnixTerminal`] built from our
//! owned fd. Crossterm's `OnceLock`-backed stdin reader is never
//! involved.
//!
//! ## Why we don't reuse [`RawModeGuard`]
//!
//! [`RawModeGuard`] holds a [`BorrowedFd`] with an explicit lifetime,
//! which doesn't compose with self-referential storage inside this
//! struct. We mirror [`crate::splash::input::SplashInput`]: own the
//! [`OwnedFd`] plus a saved [`Termios`] snapshot and restore it on
//! [`Drop`].
//!
//! ## VT graphics mode
//!
//! When `/dev/console` is bound to a kernel VT (the framebuffer case,
//! not a serial line), the kernel keeps writing printk output to the
//! same framebuffer the TUI is drawing to. We `ioctl(KDSETMODE,
//! KD_GRAPHICS)` to suppress that until [`Drop`]; on non-VT lines
//! (serial console) the ioctl returns `ENOTTY` and we tolerate it.
//!
//! See [`kd`] for the ioctl helpers.
//!
//! ## Input pipeline
//!
//! Termwiz's `UnixTerminal` installs its own SIGWINCH signal handler
//! and would happily read input bytes itself via `poll_input`. We
//! don't call `poll_input`: instead we own the read path. Bytes come
//! off the same fd through `rustix::io::read`, get pre-filtered by
//! [`ResizeFilter`] to extract `CSI 8;rows;cols t` host-size reports
//! (which termwiz drops because it only synthesises `Resized` from
//! SIGWINCH, never from the in-band report a serial-attached
//! terminal sends), and the leftover bytes go through
//! [`TermwizToCrossterm`] which wraps `termwiz::input::InputParser`.
//! See `src/ui/console/parser.rs` for the byte-level state machine.

use std::collections::VecDeque;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;

use crossterm::event::KeyEvent;
use ratatui::Terminal;
use ratatui::backend::TermwizBackend;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{OFlags, fcntl_setfl};
use rustix::termios::Termios;
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::unix::UnixTerminal;

use crate::error::Result;
use crate::log;
use crate::nmbl_warn;
use crate::sys::printk::PrintkQuiet;
use crate::sys::tty::{enter_raw, open_console as open_console_fd};
use crate::ui::console::ConsoleEvent;
use crate::ui::console::parser::{ResizeFilter, TermwizToCrossterm};

use self::caps::caps_from_env_with_fallback;
use self::kd::enter_kd_graphics;
use self::util::{rustix_io_err, tui_err, tw_err};

mod caps;
mod impls;
mod kd;
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

/// Raw-mode tty backend. See module docs for the lifetime story.
pub struct TtyConsole {
    /// Owns the `/dev/console` fd for the lifetime of the console.
    /// Termwiz's `UnixTerminal` `dup()`s this internally for its own
    /// writer; the input path reads through `self.fd` directly via
    /// rustix non-blocking I/O.
    fd: OwnedFd,
    /// Termios snapshot to restore on drop. `Option` so [`Drop`] can
    /// take it without leaving a dangling clone.
    saved_termios: Option<Termios>,
    /// Previous KD VT mode, captured iff we successfully switched the
    /// VT into `KD_GRAPHICS`.
    previous_kd_mode: Option<libc::c_long>,
    /// Serial-console mitigation for the kernel-printk smear.
    printk_quiet: Option<PrintkQuiet>,
    /// Ratatui terminal over the termwiz backend wrapping our owned fd.
    terminal: Terminal<TermwizBackend>,
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
        // grab graphics mode so printk stops smearing the framebuffer.
        let previous_kd_mode = enter_kd_graphics(fd.as_fd());
        let terminal = Self::build_terminal(&fd, None)?;

        // Silence kernel-printk to console while we own the screen.
        let printk_quiet = Some(PrintkQuiet::engage());
        // Tell the `nmbl_*!` macros to stop writing to stderr.
        log::set_tui_active();

        Ok(TtyConsole {
            fd,
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
        let terminal = Self::build_terminal(&fd, Some((rows, cols)))?;
        Ok(TtyConsole {
            fd,
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

    /// Shared termwiz/ratatui terminal construction for both
    /// constructors. `seed` overrides the surface size when the fd
    /// reports no winsize (serial line or pty); `None` falls back to the
    /// historic 80x24 default.
    fn build_terminal(fd: &OwnedFd, seed: Option<(u16, u16)>) -> Result<Terminal<TermwizBackend>> {
        // We read input from `self.fd` directly via rustix poll/read
        // in `poll_event`, so the fd must be non-blocking.
        if let Err(e) = fcntl_setfl(fd.as_fd(), OFlags::NONBLOCK) {
            nmbl_warn!(
                "TtyConsole: F_SETFL(O_NONBLOCK) on console fd {} failed: {e}; \
                 reads may briefly block on partial sequences",
                fd.as_raw_fd()
            );
        }

        // Build a termwiz UnixTerminal pointing at our fd. `new_with`
        // duplicates the fd internally for its own writer; the dup'd
        // reader is never used because we never call `poll_input` —
        // input flows through our own rustix loop and the parser.
        // We feed explicit caps (bundled terminfo + truecolor) rather
        // than reading `$TERM`, since NMBL boots with no environment.
        let caps = caps_from_env_with_fallback()?;
        let unix_term = UnixTerminal::new_with(caps, fd, fd).map_err(tw_err)?;
        let mut buf = BufferedTerminal::new(unix_term).map_err(tw_err)?;

        // `UnixTerminal::new_with` `dup()`s our fd and, during
        // construction, calls `set_blocking(Wait)` on the read dup.
        // `O_NONBLOCK` lives on the shared open-file-description, so
        // that clears the flag we set above for *every* fd pointing at
        // this OFD — including the one our rustix read loop polls.
        // Re-assert it so `poll_event`'s reads never block.
        if let Err(e) = fcntl_setfl(fd.as_fd(), OFlags::NONBLOCK) {
            nmbl_warn!(
                "TtyConsole: re-asserting O_NONBLOCK on console fd {} after termwiz \
                 construction failed: {e}; reads may briefly block on partial sequences",
                fd.as_raw_fd()
            );
        }

        // `BufferedTerminal::new` seeds its `Surface` from
        // `TIOCGWINSZ`. A serial line / freshly-allocated pty may report
        // no winsize (0x0), so the surface would have zero area:
        // ratatui's `draw()` autoresizes to 0x0, renders into an empty
        // frame, the diff is empty, and *nothing* is ever written — the
        // empty-pane regression. Seed sane dimensions so the very first
        // frame paints; the host's `CSI 8;rows;cols t` report later
        // corrects the geometry via `apply_resize`.
        let (cols0, rows0) = buf.dimensions();
        if cols0 == 0 || rows0 == 0 {
            let (rows, cols) = seed.unwrap_or((DEFAULT_ROWS, DEFAULT_COLS));
            buf.resize(usize::from(cols), usize::from(rows));
        }

        let backend = TermwizBackend::with_buffered_terminal(buf);
        Terminal::new(backend).map_err(tui_err)
    }

    /// Read whatever bytes are ready on `self.fd`, run them through
    /// the resize pre-filter, feed the leftovers to termwiz's input
    /// parser, and stash any emitted key events into
    /// `self.pending_keys`. Returns at most one [`ConsoleEvent::Resize`]
    /// extracted from the byte stream (the pre-filter emits at most
    /// one per call).
    fn refill(&mut self, timeout_ms: i32) -> Result<Option<ConsoleEvent>> {
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
                    // EOF — flush and stop.
                    return self.drain_after_eagain();
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
            .buffered_terminal_mut()
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
