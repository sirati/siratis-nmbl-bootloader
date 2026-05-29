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
//! rustix 0.38 does not expose a wrapper for the kd ioctls, so this
//! file contains one tightly-scoped `unsafe { libc::ioctl(...) }` per
//! direction, each documented with a SAFETY comment naming the kernel
//! contract (linux/kd.h).
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
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Terminal;
use ratatui::backend::{Backend, TermwizBackend};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{OFlags, fcntl_setfl};
use rustix::termios::Termios;
use termwiz::caps::Capabilities;
use termwiz::terminal::buffered::BufferedTerminal;
use termwiz::terminal::unix::UnixTerminal;

use crate::error::{NmblError, Result};
use crate::log;
use crate::nmbl_warn;
use crate::sys::printk::PrintkQuiet;
use crate::sys::tty::{enter_raw, open_console as open_console_fd, restore_termios, save_termios};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::parser::{ResizeFilter, TermwizToCrossterm};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::render_current_screen;

/// Default tty path the orchestrator opens at boot.
const CONSOLE_PATH: &str = "/dev/console";

/// Fallback grid geometry used when the line reports no winsize
/// (`TIOCGWINSZ` → 0x0, the serial-console case). The host's
/// `CSI 8;rows;cols t` report corrects this on the first resize.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// `linux/kd.h` ioctl numbers. Stable kernel ABI.
const KDGETMODE: libc::Ioctl = 0x4B3B;
const KDSETMODE: libc::Ioctl = 0x4B3A;
/// VT in graphics mode: kernel stops painting printk to the framebuffer.
const KD_GRAPHICS: libc::c_long = 0x01;
/// VT in text mode (the default). Only referenced by tests; production
/// code never hard-codes `KD_TEXT` — it always restores the mode value
/// captured by `KDGETMODE` so we don't clobber a pre-graphics setup.
#[cfg(test)]
const KD_TEXT: libc::c_long = 0x00;

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
    /// Latest grid size observed via a CSI 8;rows;cols t report from
    /// the host terminal. Wins over the backend's reported size.
    last_resize: Option<(u16, u16)>,
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
        let previous_kd_mode = enter_kd_graphics(fd.as_fd());

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
        // `Capabilities::new_from_env()` reads `$TERM` to pick a
        // terminfo entry; we'd rather fall back to a minimal ANSI set
        // when `$TERM` is unset (NMBL boots with no environment).
        let caps = caps_from_env_with_fallback()?;
        let unix_term = UnixTerminal::new_with(caps, &fd, &fd).map_err(tw_err)?;
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
        // `TIOCGWINSZ`. A serial line reports no winsize (0x0), so the
        // surface would have zero area: ratatui's `draw()` autoresizes
        // to the backend's 0x0, renders into an empty frame, the diff
        // is empty, and *nothing* is ever written to the line — the
        // empty-pane regression. Seed a sane default so the very first
        // frame paints; the host's `CSI 8;rows;cols t` report later
        // corrects the geometry via `apply_resize`.
        let (cols0, rows0) = buf.dimensions();
        if cols0 == 0 || rows0 == 0 {
            buf.resize(usize::from(DEFAULT_COLS), usize::from(DEFAULT_ROWS));
        }

        let backend = TermwizBackend::with_buffered_terminal(buf);
        let terminal = Terminal::new(backend).map_err(tui_err)?;

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
            last_resize: None,
        })
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
            self.key_parser.feed(bytes, maybe_more, &mut keys);
            for k in keys {
                self.pending_keys.push_back(k);
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
}

impl Console for TtyConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        self.terminal
            .draw(|f| render_current_screen(f, app))
            .map(|_| ())
            .map_err(tui_err)
    }

    fn poll_event(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
        // First: drain any keys already classified from a previous
        // poll cycle without going to the fd again.
        if let Some(k) = self.pending_keys.pop_front() {
            return Ok(Some(ConsoleEvent::Key(k)));
        }

        // Cap the wait so backends are uniformly responsive to
        // ticking countdowns.
        let slice = timeout.min(POLL_SLICE);
        let timeout_ms = duration_to_ms(slice);
        let resize = self.refill(timeout_ms)?;
        // After refill, prefer surfacing a Resize first (so layout
        // catches up before the next key dispatches against the new
        // size); then surface a key from whatever the parser emitted.
        if let Some(ev) = resize {
            self.apply_resize(&ev);
            return Ok(Some(ev));
        }
        if let Some(k) = self.pending_keys.pop_front() {
            return Ok(Some(ConsoleEvent::Key(k)));
        }
        Ok(None)
    }

    fn size(&self) -> (u16, u16) {
        // A host-reported resize wins over the backend's cached size
        // — the backend caches the value it saw at construction time,
        // which on a serial line is the static `stty rows/cols` value
        // the kernel set at boot rather than the operator's live
        // tmux pane geometry.
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
        self.terminal.draw(|f| body(f)).map(|_| ()).map_err(tui_err)
    }

    fn suspend(&mut self) -> Result<()> {
        if let Some(mut q) = self.printk_quiet.take() {
            q.restore();
        }
        log::clear_tui_active();
        if let Some(previous) = self.previous_kd_mode.take() {
            restore_kd_mode(self.fd.as_fd(), previous);
        }
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            nmbl_warn!(
                "TtyConsole::suspend: failed to restore termios on fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        let saved = save_termios(self.fd.as_fd())?;
        let _ = enter_raw(self.fd.as_fd())?;
        self.saved_termios = Some(saved);
        self.previous_kd_mode = enter_kd_graphics(self.fd.as_fd());
        self.printk_quiet = Some(PrintkQuiet::engage());
        log::set_tui_active();
        self.terminal.clear().map_err(tui_err)?;
        Ok(())
    }

    fn caps_lock_active(&self) -> Option<bool> {
        // `/dev/console` is a VT in the framebuffer case and a serial
        // line otherwise. `caps_lock_active` returns `None` on the
        // latter (ENOTTY), so the passphrase warning is shown only when
        // a real VT keyboard reports Caps Lock.
        crate::sys::vt::caps_lock_active(self.fd.as_fd())
    }
}

impl Drop for TtyConsole {
    fn drop(&mut self) {
        if let Some(mut q) = self.printk_quiet.take() {
            q.restore();
        }
        log::clear_tui_active();
        if let Some(previous) = self.previous_kd_mode.take() {
            restore_kd_mode(self.fd.as_fd(), previous);
        }
        if let Some(saved) = self.saved_termios.take()
            && let Err(e) = restore_termios(self.fd.as_fd(), &saved)
        {
            nmbl_warn!(
                "failed to restore termios on tty console fd {}: {e}",
                self.fd.as_raw_fd()
            );
        }
    }
}

/// A compiled `xterm-256color` terminfo entry, bundled into the binary.
///
/// The initramfs ships no terminfo database. Without one, termwiz's
/// `Capabilities` carry no `cup` (`CursorAddress`) capability, and its
/// terminfo renderer falls back to a hand-rolled CSI cursor-address path
/// (`render/terminfo.rs::move_cursor_absolute`) that emits
/// `CSI {x+1};{y+1} H` — transposing row and column. ratatui's
/// `TermwizBackend` positions every changed cell with an absolute
/// `CursorPosition`, so on every incremental repaint that transposition
/// turns a horizontal run of cells into a vertical-down stair-step (a
/// full repaint after a resize is immune because `repaint_all` moves
/// between lines with `\r\n`, not absolute addressing).
///
/// Bundling a terminfo entry that defines `cup` makes termwiz take the
/// correct `CursorAddress` path, which fixes the corruption. This is the
/// same byte-for-byte entry termwiz ships for its own Windows
/// `apply_builtin_terminfo` path.
const BUNDLED_TERMINFO: &[u8] = include_bytes!("data/xterm-256color");

/// Build a termwiz `Capabilities` set for the NMBL serial/VT console.
///
/// We deliberately do **not** trust the runtime environment: PID-1 boots
/// with no `$TERM` and no terminfo database on disk. Instead we feed
/// termwiz an explicit [`ProbeHints`] carrying:
///
/// - the bundled `xterm-256color` terminfo (for a correct `cup` —
///   see [`BUNDLED_TERMINFO`]),
/// - [`ColorLevel::TrueColor`] so 24-bit RGB is emitted directly as
///   `CSI 38;2;r;g;b m` rather than being quantised to a palette index,
/// - every optional capability enabled (hyperlinks, sixel, iTerm2 image
///   protocol, bracketed paste, mouse reporting) so the full terminal
///   feature set is available to any modern emulator on the other end of
///   the serial line,
/// - `force_terminfo_render_to_use_ansi_sgr` so SGR attributes are
///   emitted as standard ECMA-48 sequences, which render correctly even
///   through pagers and minimal emulators.
///
/// On the (extremely unlikely) failure to even parse the bundled
/// terminfo we fall back to the same hints without a database; the
/// truecolor/feature overrides still apply, only `cup` is missing.
fn caps_from_env_with_fallback() -> Result<Capabilities> {
    use termwiz::caps::{ColorLevel, ProbeHints};

    let hints = ProbeHints::default()
        .term(Some("xterm-256color".to_owned()))
        .color_level(Some(ColorLevel::TrueColor))
        .hyperlinks(Some(true))
        .sixel(Some(true))
        .iterm2_image(Some(true))
        .bracketed_paste(Some(true))
        .mouse_reporting(Some(true))
        .force_terminfo_render_to_use_ansi_sgr(Some(true));

    let hints = match terminfo::Database::from_buffer(BUNDLED_TERMINFO) {
        Ok(db) => hints.terminfo_db(Some(db)),
        Err(e) => {
            nmbl_warn!(
                "TtyConsole: bundled terminfo failed to parse ({e}); \
                 cursor addressing may be wrong on incremental repaints"
            );
            hints
        }
    };

    Capabilities::new_with_hints(hints).map_err(tw_err)
}

/// Try to switch `fd`'s VT into `KD_GRAPHICS`.
fn enter_kd_graphics(fd: BorrowedFd<'_>) -> Option<libc::c_long> {
    let mut mode: libc::c_long = 0;
    // SAFETY: KDGETMODE (linux/kd.h) reads an `unsigned long` through
    // the pointer in the third ioctl argument. `&mut mode` is a valid,
    // properly-aligned pointer to a live `c_long` that outlives the
    // call. The fd is a live open file descriptor by contract.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDGETMODE, &mut mode) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::ENOTTY) {
            nmbl_warn!(
                "KDGETMODE on console fd {} failed: {err}; \
                 leaving VT in current mode (printk may bleed into TUI)",
                fd.as_raw_fd()
            );
        }
        return None;
    }
    if mode == KD_GRAPHICS {
        return None;
    }
    // SAFETY: KDSETMODE (linux/kd.h) takes its third argument as an
    // `unsigned long` value. The fd is a live open VT.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSETMODE, KD_GRAPHICS) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "KDSETMODE(KD_GRAPHICS) on console fd {} failed: {err}; \
             printk may bleed into TUI",
            fd.as_raw_fd()
        );
        return None;
    }
    Some(mode)
}

fn restore_kd_mode(fd: BorrowedFd<'_>, previous: libc::c_long) {
    // SAFETY: same contract as `enter_kd_graphics`.
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), KDSETMODE, previous) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        nmbl_warn!(
            "KDSETMODE restore on console fd {} failed: {err}; \
             VT may remain in graphics mode (try `kbd_mode -a`)",
            fd.as_raw_fd()
        );
    }
}

fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

fn tw_err(e: termwiz::Error) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::other(format!("termwiz: {e}")),
    }
}

fn rustix_io_err(e: rustix::io::Errno) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::from(e),
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

impl TtyConsole {
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    /// `/dev/null` is not a tty, so opening it as a [`TtyConsole`]
    /// must fail at the `enter_raw` step (ENOTTY).
    #[test]
    fn open_path_on_non_tty_errors() {
        if std::fs::metadata("/dev/null").is_err() {
            return;
        }
        let res = TtyConsole::open_path(Path::new("/dev/null"));
        assert!(res.is_err(), "expected ENOTTY-style failure on /dev/null");
    }

    #[test]
    fn enter_kd_graphics_on_non_vt_returns_none() {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            Ok(f) => f,
            Err(_) => return,
        };
        let result = enter_kd_graphics(file.as_fd());
        assert!(
            result.is_none(),
            "expected None on non-VT fd (KDGETMODE→ENOTTY), got {result:?}"
        );
    }

    #[test]
    fn restore_kd_mode_on_non_vt_does_not_panic() {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
        {
            Ok(f) => f,
            Err(_) => return,
        };
        restore_kd_mode(file.as_fd(), KD_TEXT);
    }

    /// The bundled terminfo must parse and define `cup`
    /// (`CursorAddress`). This is the single fact that keeps termwiz off
    /// its row/col-transposing CSI fallback in `move_cursor_absolute`.
    #[test]
    fn bundled_terminfo_defines_cursor_address() {
        use terminfo::capability::CursorAddress;
        let db = terminfo::Database::from_buffer(BUNDLED_TERMINFO)
            .expect("bundled xterm-256color terminfo must parse");
        assert!(
            db.get::<CursorAddress>().is_some(),
            "bundled terminfo must define cup (CursorAddress); without it \
             termwiz transposes row/col on every incremental repaint"
        );
    }

    /// Regression pin for the horizontal→vertical-down flip. Render an
    /// absolute cursor move `(x=col, y=row)` through the *actual*
    /// capabilities NMBL builds and assert termwiz emits
    /// `CSI {row+1};{col+1} H` — row first, then column. The pre-fix
    /// no-terminfo fallback emitted `CSI {col+1};{row+1} H` (transposed),
    /// which is exactly the corruption the operator reported.
    #[test]
    fn absolute_cursor_move_is_row_then_col() {
        use std::io::Write;
        use termwiz::render::RenderTty;
        use termwiz::render::terminfo::TerminfoRenderer;
        use termwiz::surface::{Change, Position};

        struct CaptureTty {
            buf: Vec<u8>,
        }
        impl Write for CaptureTty {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.buf.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl RenderTty for CaptureTty {
            fn get_size_in_cells(&mut self) -> termwiz::Result<(usize, usize)> {
                Ok((200, 60))
            }
        }

        let caps = caps_from_env_with_fallback().expect("caps must build");
        let mut renderer = TerminfoRenderer::new(caps);
        let mut tty = CaptureTty { buf: Vec::new() };

        // x = column 7, y = row 3. A correct backend emits a move to
        // row 3, column 7.
        let change = Change::CursorPosition {
            x: Position::Absolute(7),
            y: Position::Absolute(3),
        };
        renderer
            .render_to(&[change], &mut tty)
            .expect("render must succeed");

        let out = String::from_utf8_lossy(&tty.buf);
        assert!(
            out.contains("\x1b[4;8H"),
            "expected row-first CSI cursor address \\x1b[4;8H (row 3+1, col 7+1), got {out:?}"
        );
        assert!(
            !out.contains("\x1b[8;4H"),
            "transposed (col-first) cursor address \\x1b[8;4H must NOT appear: {out:?}"
        );
    }
}
