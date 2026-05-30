//! Console abstraction. The TUI renders into a `&mut dyn Console`
//! without knowing whether the underlying backend is the splash
//! framebuffer or a raw-mode tty. `open_console` brings up the right
//! backend with fallback rules; the orchestrator (main.rs) holds the
//! returned handle for the lifetime of the boot.
//!
//! ## Fallback rules
//!
//! 1. `panic_recovery` always returns a [`TtyConsole`]. A panic may have
//!    originated in the splash code path (DRM, font, compositor), so the
//!    panic-handler re-exec must not re-enter it.
//! 2. With the `image-splash` feature built in and `config.splash.enable`
//!    set, try [`SplashConsole`]. On any bring-up failure, log a warning
//!    via [`crate::nmbl_warn!`] and fall through to the tty.
//! 3. Otherwise [`TtyConsole`].
//!
//! The two backends are *only* render targets. All screen content lives
//! in [`crate::ui::app::App`] and the renderers in [`crate::ui::view`];
//! switching backends does not change what is shown, only where.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Frame;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::ui::app::App;

/// A single input event from a console backend.
///
/// The enum is open-coded so future additions (paste, clicks) can land
/// additively without breaking the `poll_event` signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEvent {
    /// A key press / repeat / release synthesised by the backend.
    Key(KeyEvent),
    /// The host terminal reported a new grid size via CSI 8; rows; cols t.
    /// Backends with a fixed grid (the DRM splash framebuffer) never
    /// emit this variant.
    Resize { rows: u16, cols: u16 },
    /// A mouse-wheel scroll notch. `up` is `true` for wheel-up (scroll
    /// toward older scrollback) and `false` for wheel-down. Only the
    /// tty/termwiz path with xterm mouse reporting produces this; the
    /// kernel-VT splash path never emits it (no xterm mouse sequences).
    Scroll { up: bool },
}

/// Which backend a [`Console`] is. Surfaced via [`Console::kind`] so
/// callers that need to behave differently (e.g. tests, or activation
/// paths that want to know whether DRM is still owned) can branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleKind {
    /// DRM framebuffer + alacritty-parsed ratatui output.
    Splash,
    /// Raw-mode `/dev/console` driven by crossterm.
    Tty,
}

/// Backend-agnostic console handle. The orchestrator holds one of
/// these for the entire boot; every UI phase renders into it and polls
/// it for keys.
pub trait Console {
    /// Render one frame from the current [`App`] state.
    fn render(&mut self, app: &App<'_>) -> Result<()>;

    /// Poll for one input event, `.await`ing readiness instead of
    /// blocking the OS thread. Returns `Ok(None)` at or before `timeout`.
    ///
    /// The future is boxed (rather than expressed as a native
    /// `async fn` in the trait) so the trait stays **object-safe**: the
    /// whole codebase drives the UI through `&mut dyn Console`, which a
    /// native async-fn-in-trait would forbid. Each backend returns an
    /// `async move` block from this method; callers `.await` it.
    ///
    /// Backends may return early — in particular, both real backends cap
    /// the effective wait at [`crate::ui::POLL_SLICE`] (~100ms) so callers
    /// can drive ticking countdowns and spinner animations with a
    /// consistent cadence regardless of backend. Callers that want a
    /// longer effective block must loop.
    ///
    /// Long-running render loops that need to redraw on host-terminal
    /// resize (passphrase modal, generations picker, rescue menu,
    /// console picker) should `.await` `poll_event` directly so they can
    /// react to [`ConsoleEvent::Resize`].
    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>>;

    /// Synchronous, blocking poll for one input event. The async
    /// [`Console::poll_event`] is the path every interactive loop uses;
    /// this blocking variant exists for the early-boot
    /// [`crate::ui::reporter::BootReporter`] which runs on the
    /// synchronous mount/module/device path (those phases are
    /// deliberately NOT async). Backends implement the real poll logic
    /// here and the async `poll_event` simply awaits readiness then
    /// delegates to this. Behaviour is identical.
    fn poll_event_blocking(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>>;

    /// Convenience wrapper around [`Console::poll_event_blocking`] that
    /// drops resize events on the floor and only surfaces keys. Used by
    /// the synchronous early-boot reporter's Esc-abort poll.
    ///
    /// Async interactive loops never call this — they `.await`
    /// [`Console::poll_event`] and match on the event directly.
    fn poll_key(&mut self, timeout: Duration) -> Result<Option<KeyEvent>> {
        match self.poll_event_blocking(timeout)? {
            Some(ConsoleEvent::Key(k)) => Ok(Some(k)),
            // Resize events were consumed by `poll_event_blocking`
            // (which is responsible for re-sizing the backend's render
            // target). Scroll events only matter to scrollback-aware
            // consumers (the pretty shell) that call `poll_event`
            // directly. The caller asked for a key, so report no key.
            Some(ConsoleEvent::Resize { .. } | ConsoleEvent::Scroll { .. }) | None => Ok(None),
        }
    }
    /// Backend grid size in (cols, rows). Useful for centring modals
    /// without a redundant `Terminal::size()` round-trip.
    fn size(&self) -> (u16, u16);
    /// Which backend this is.
    fn kind(&self) -> ConsoleKind;
    /// Render an ad-hoc frame via a ratatui closure. Used by code paths
    /// that paint dynamic widgets (download gauges, cursor-tracking
    /// editors) that don't fit the App+Screen state-machine model.
    /// The same backend (splash or tty) is reused — no new terminal
    /// is constructed.
    fn draw_with(&mut self, body: &mut dyn FnMut(&mut Frame<'_>)) -> Result<()>;

    /// Release the display so the kernel (or another userspace owner)
    /// can paint to it without contention. Used by the
    /// [`crate::ui::console_picker`] shell-relay path: when the
    /// operator opts to multiplex the emergency shell onto the same
    /// console the TUI is drawing to, we hand the display back to the
    /// kernel VT / printk so the shell's output is visible.
    ///
    /// The backend keeps its fds open. A matching [`Console::resume`]
    /// call must follow once the foreign user is done. Backends that
    /// don't own a display (e.g. [`NoopConsole`]) implement this as a
    /// no-op.
    fn suspend(&mut self) -> Result<()>;

    /// Inverse of [`Console::suspend`]: re-acquire the display and
    /// repaint. After `resume` the next [`Console::render`] /
    /// [`Console::draw_with`] call must produce a visible frame, even
    /// if nothing about the underlying [`App`] state changed.
    fn resume(&mut self) -> Result<()>;

    /// Best-effort query of the keyboard's Caps-Lock lock state on the
    /// backend's input VT, used by the passphrase prompt to show a
    /// (non-resizing) warning.
    ///
    /// Returns `Some(true)` / `Some(false)` only on a kernel VT where
    /// the `KDGKBLED` ioctl succeeds (the splash and tty backends), and
    /// `None` ("unknown") everywhere else — serial lines, the sentinel
    /// [`NoopConsole`], the mock harness. The default impl returns
    /// `None` so a backend that owns no queryable keyboard never shows
    /// the warning. Polled every render tick; must never block, log, or
    /// error.
    fn caps_lock_active(&self) -> Option<bool> {
        None
    }
}

/// Await readability on a raw input fd through tokio's reactor, capped
/// at `timeout`. Returns `Ok(true)` when the fd became readable,
/// `Ok(false)` on the timeout, and an error only on a reactor
/// registration failure.
///
/// Used by the real backends' async [`Console::poll_event`]: instead of
/// a blocking `poll(2)` that parks the single OS thread, register the fd
/// with [`tokio::io::unix::AsyncFd`] and `.await` readiness, racing a
/// [`tokio::time::sleep`] for the slice deadline. After this resolves
/// the backend runs its identical synchronous drain. The fd must be in
/// non-blocking mode (both backends set `O_NONBLOCK` at open time).
///
/// `AsyncFd` is constructed per call from a [`BorrowedFd`]; it registers
/// on construction and deregisters on drop, so no long-lived reactor
/// state leaks between polls and the fd ownership stays with the
/// backend.
pub(crate) async fn await_fd_readable(
    fd: std::os::fd::BorrowedFd<'_>,
    timeout: Duration,
) -> Result<bool> {
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;

    let async_fd = AsyncFd::with_interest(fd, Interest::READABLE).map_err(|e| NmblError::Tui {
        source: std::io::Error::other(format!("AsyncFd registration failed: {e}")),
    })?;
    let ready = async_fd.readable();
    match tokio::time::timeout(timeout, ready).await {
        // Readable: clear the readiness so the next poll re-arms, then
        // tell the caller to drain. A reactor error surfaces as Tui.
        Ok(Ok(mut guard)) => {
            guard.clear_ready();
            Ok(true)
        }
        Ok(Err(e)) => Err(NmblError::Tui {
            source: std::io::Error::other(format!("AsyncFd readiness failed: {e}")),
        }),
        // Slice deadline elapsed with no input — caller drains nothing.
        Err(_elapsed) => Ok(false),
    }
}

pub mod noop;
pub use self::noop::NoopConsole;

pub mod tty;
pub use self::tty::TtyConsole;

pub(crate) mod parser;

#[cfg(feature = "image-splash")]
pub mod splash;
#[cfg(feature = "image-splash")]
pub use self::splash::SplashConsole;

/// Outcome of the backend-selection decision. Single source of truth for
/// "which path will `open_console` take" — pinned by tests so a future
/// edit to the decision tree gets exercised without needing real
/// hardware.
///
/// Note this does NOT promise a splash bring-up succeeded; it only says
/// `open_console` *will try* splash first, with a tty fall-through if
/// splash fails. The two-stage shape keeps the decision pure and
/// hardware-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BackendChoice {
    /// Skip splash entirely and bring up [`TtyConsole`].
    Tty,
    /// Attempt [`SplashConsole`]; on `Ok(None)` or `Err(_)` fall back
    /// to [`TtyConsole`]. Only constructible when the `image-splash`
    /// feature is compiled in — in no-feature builds the splash code
    /// path doesn't exist, so the variant would be dead code.
    #[cfg(feature = "image-splash")]
    SplashOrTty,
}

/// Pure decision helper: which backend will [`open_console`] try?
///
/// Mirrors the rules in the module docs without touching hardware, so
/// tests can pin the same logic `open_console` runs in production.
///
/// `console_is_serial` is the kernel-elected-console classification
/// (see [`crate::sys::tty::active_console_is_serial`]). When the primary
/// interactive console is a serial line the splash backend would render
/// to the (often QEMU-emulated) VGA framebuffer but read keyboard input
/// from `/dev/tty1`, a VT the operator's serial keystrokes never reach —
/// so we skip splash and use the tty backend, which reads `/dev/console`
/// (the serial line) directly. Real VGA machines elect a `tty0`/`tty1`
/// primary console, classify as non-serial, and keep the splash path.
pub(super) fn decide_backend(
    config: &Config,
    panic_recovery: bool,
    console_is_serial: bool,
) -> BackendChoice {
    if panic_recovery {
        // Rule 1: panic re-exec → never re-enter splash.
        return BackendChoice::Tty;
    }
    #[cfg(feature = "image-splash")]
    if config.splash.enable && !console_is_serial {
        return BackendChoice::SplashOrTty;
    }
    // Suppress unused-config warning when image-splash isn't compiled in.
    #[cfg(not(feature = "image-splash"))]
    let _ = config;
    // Suppress unused warning when the splash arm above is compiled out.
    #[cfg(not(feature = "image-splash"))]
    let _ = console_is_serial;
    BackendChoice::Tty
}

/// Open the appropriate backend for the current config.
///
/// See module docs for the decision tree; [`decide_backend`] holds the
/// pure decision logic and is the helper tests pin.
pub fn open_console(config: &Config, panic_recovery: bool) -> Result<Box<dyn Console>> {
    // Classify the kernel-elected primary console: a serial line has no
    // keyboard on `/dev/tty1`, so the splash backend (which reads input
    // there) would render but never see a keystroke. Detect that and
    // route to the tty backend, which reads `/dev/console` — the serial
    // line itself. Read-failure assumes a VT to keep the splash path on
    // the common framebuffer machines.
    let console_is_serial = crate::sys::tty::active_console_is_serial();
    #[cfg(feature = "image-splash")]
    if console_is_serial && config.splash.enable && !panic_recovery {
        crate::nmbl_warn!(
            "primary console is a serial line; using tty backend so serial \
             keystrokes reach the selector (splash input on /dev/tty1 would be \
             unreachable)"
        );
    }
    match decide_backend(config, panic_recovery, console_is_serial) {
        #[cfg(feature = "image-splash")]
        BackendChoice::SplashOrTty => match SplashConsole::open(config) {
            Ok(Some(s)) => Ok(Box::new(s)),
            Ok(None) => {
                crate::nmbl_warn!(
                    "splash backend unavailable (no DRM device, no font, etc.); \
                     falling back to tty console"
                );
                open_tty()
            }
            Err(e) => {
                crate::nmbl_warn!(
                    "splash backend bring-up failed: {e}; falling back to tty console"
                );
                open_tty()
            }
        },
        BackendChoice::Tty => {
            // Suppress unused-config warning when image-splash isn't
            // compiled in — `decide_backend` reads it but the splash
            // arm above is the only consumer of the actual value.
            #[cfg(not(feature = "image-splash"))]
            let _ = config;
            open_tty()
        }
    }
}

fn open_tty() -> Result<Box<dyn Console>> {
    let tty = TtyConsole::open()?;
    Ok(Box::new(tty))
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

    // Building a real [`TtyConsole`] in a test harness requires
    // `/dev/console`, which CI typically doesn't have. We pin the
    // pure `decide_backend` helper instead — `open_console` calls
    // the exact same function, so the production path is exercised
    // without touching any hardware.

    #[test]
    fn open_console_panic_recovery_picks_tty() {
        let config = Config::recovery_default();
        // panic_recovery wins regardless of splash.enable / serial.
        assert_eq!(decide_backend(&config, true, false), BackendChoice::Tty);
        assert_eq!(decide_backend(&config, true, true), BackendChoice::Tty);

        #[cfg(feature = "image-splash")]
        {
            let mut config = Config::recovery_default();
            config.splash.enable = true;
            assert_eq!(
                decide_backend(&config, true, false),
                BackendChoice::Tty,
                "panic_recovery must veto splash"
            );
        }
    }

    #[test]
    fn open_console_splash_disabled_picks_tty() {
        let config = Config::recovery_default();
        // recovery_default has splash.enable=false (and the field is
        // gated, so on no-feature builds the disabled-state is the
        // only state).
        assert_eq!(decide_backend(&config, false, false), BackendChoice::Tty);
    }

    #[cfg(feature = "image-splash")]
    #[test]
    fn open_console_splash_enabled_picks_splash_or_tty() {
        let mut config = Config::recovery_default();
        config.splash.enable = true;
        assert_eq!(
            decide_backend(&config, false, false),
            BackendChoice::SplashOrTty,
            "splash.enable=true on a VT console must opt into the splash path",
        );
    }

    #[cfg(feature = "image-splash")]
    #[test]
    fn open_console_serial_console_vetoes_splash() {
        let mut config = Config::recovery_default();
        config.splash.enable = true;
        // A serial primary console has no keyboard on /dev/tty1, so even
        // with splash enabled we must fall back to the tty backend that
        // reads /dev/console (the serial line).
        assert_eq!(
            decide_backend(&config, false, true),
            BackendChoice::Tty,
            "serial primary console must veto splash so keystrokes are read",
        );
    }
}
