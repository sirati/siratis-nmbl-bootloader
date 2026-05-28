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

use std::time::Duration;

use crossterm::event::KeyEvent;
use ratatui::Frame;

use crate::config::Config;
use crate::error::Result;
use crate::ui::app::App;

/// A single input event from a console backend.
///
/// Today only key and resize events are produced; the enum is open-coded
/// so future additions (mouse, paste) can land additively without
/// breaking the `poll_event` signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleEvent {
    /// A key press / repeat / release synthesised by the backend.
    Key(KeyEvent),
    /// The host terminal reported a new grid size via CSI 8; rows; cols t.
    /// Backends with a fixed grid (the DRM splash framebuffer) never
    /// emit this variant.
    Resize { rows: u16, cols: u16 },
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
    /// Poll for one input event. Returns `Ok(None)` at or before `timeout`.
    ///
    /// Backends may return early — in particular, both current backends
    /// cap the effective wait at [`crate::ui::POLL_SLICE`] (~100ms) so
    /// callers can drive ticking countdowns and spinner animations with
    /// consistent cadence regardless of backend. Callers that want a
    /// longer effective block must loop.
    ///
    /// Long-running render loops that need to redraw on host-terminal
    /// resize (passphrase modal, generations picker, rescue menu,
    /// console picker) should call `poll_event` directly so they can
    /// react to [`ConsoleEvent::Resize`]. Everything else can keep using
    /// the [`Console::poll_key`] default below, which silently drains
    /// resize events and applies them to the backend.
    fn poll_event(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>>;

    /// Convenience wrapper around [`Console::poll_event`] that drops
    /// resize events on the floor and only surfaces keys to the caller.
    ///
    /// Backends override `poll_event`; this default takes care of the
    /// common "I only care about keypresses" path while still letting
    /// the backend update its own grid state internally as resize
    /// events go by.
    fn poll_key(&mut self, timeout: Duration) -> Result<Option<KeyEvent>> {
        match self.poll_event(timeout)? {
            Some(ConsoleEvent::Key(k)) => Ok(Some(k)),
            // Resize events were consumed by `poll_event` (which is
            // responsible for re-sizing the backend's render target);
            // the caller asked for a key, so report no key this slice.
            Some(ConsoleEvent::Resize { .. }) | None => Ok(None),
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
pub(super) fn decide_backend(config: &Config, panic_recovery: bool) -> BackendChoice {
    if panic_recovery {
        // Rule 1: panic re-exec → never re-enter splash.
        return BackendChoice::Tty;
    }
    #[cfg(feature = "image-splash")]
    if config.splash.enable {
        return BackendChoice::SplashOrTty;
    }
    // Suppress unused-config warning when image-splash isn't compiled in.
    #[cfg(not(feature = "image-splash"))]
    let _ = config;
    BackendChoice::Tty
}

/// Open the appropriate backend for the current config.
///
/// See module docs for the decision tree; [`decide_backend`] holds the
/// pure decision logic and is the helper tests pin.
pub fn open_console(config: &Config, panic_recovery: bool) -> Result<Box<dyn Console>> {
    match decide_backend(config, panic_recovery) {
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
        // panic_recovery wins regardless of splash.enable.
        assert_eq!(decide_backend(&config, true), BackendChoice::Tty);

        #[cfg(feature = "image-splash")]
        {
            let mut config = Config::recovery_default();
            config.splash.enable = true;
            assert_eq!(
                decide_backend(&config, true),
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
        assert_eq!(decide_backend(&config, false), BackendChoice::Tty);
    }

    #[cfg(feature = "image-splash")]
    #[test]
    fn open_console_splash_enabled_picks_splash_or_tty() {
        let mut config = Config::recovery_default();
        config.splash.enable = true;
        assert_eq!(
            decide_backend(&config, false),
            BackendChoice::SplashOrTty,
            "splash.enable=true must opt into the splash path",
        );
    }
}
