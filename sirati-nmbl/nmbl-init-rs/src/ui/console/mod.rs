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

use crate::config::Config;
use crate::error::Result;
use crate::ui::app::App;

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
    /// Poll for one key event. Returns `Ok(None)` on timeout so the
    /// caller can drive ticking countdowns at the same cadence.
    fn poll_key(&mut self, timeout: Duration) -> Result<Option<KeyEvent>>;
    /// Backend grid size in (cols, rows). Useful for centring modals
    /// without a redundant `Terminal::size()` round-trip.
    fn size(&self) -> (u16, u16);
    /// Which backend this is.
    fn kind(&self) -> ConsoleKind;
}

pub mod tty;
pub use self::tty::TtyConsole;

#[cfg(feature = "image-splash")]
pub mod splash;
#[cfg(feature = "image-splash")]
pub use self::splash::SplashConsole;

/// Open the appropriate backend for the current config.
///
/// See module docs for the decision tree.
pub fn open_console(config: &Config, panic_recovery: bool) -> Result<Box<dyn Console>> {
    if panic_recovery {
        // Rule 1: panic re-exec → never re-enter splash.
        let tty = TtyConsole::open()?;
        return Ok(Box::new(tty));
    }

    #[cfg(feature = "image-splash")]
    if config.splash.enable {
        match SplashConsole::open(config) {
            Ok(Some(s)) => return Ok(Box::new(s)),
            Ok(None) => {
                crate::nmbl_warn!(
                    "splash backend unavailable (no DRM device, no font, etc.); \
                     falling back to tty console"
                );
            }
            Err(e) => {
                crate::nmbl_warn!(
                    "splash backend bring-up failed: {e}; falling back to tty console"
                );
            }
        }
    }

    // Suppress unused-config warning when image-splash isn't compiled in.
    #[cfg(not(feature = "image-splash"))]
    let _ = config;

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

    /// Building a real [`TtyConsole`] in a test harness requires
    /// `/dev/console`, which CI typically doesn't have. The decision
    /// tree itself is what we want to pin here, so we lift it into a
    /// pure helper that returns a `ConsoleKind` discriminant without
    /// touching any hardware.
    fn decide_kind(config: &Config, panic_recovery: bool) -> ConsoleKind {
        if panic_recovery {
            return ConsoleKind::Tty;
        }
        #[cfg(feature = "image-splash")]
        if config.splash.enable {
            return ConsoleKind::Splash;
        }
        // Reference the config so the no-feature build still sees it
        // used (mirrors `open_console`).
        let _ = config;
        ConsoleKind::Tty
    }

    #[test]
    fn open_console_panic_recovery_returns_tty() {
        let config = Config::recovery_default();
        // panic_recovery wins regardless of splash.enable.
        assert_eq!(decide_kind(&config, true), ConsoleKind::Tty);

        #[cfg(feature = "image-splash")]
        {
            let mut config = Config::recovery_default();
            config.splash.enable = true;
            assert_eq!(
                decide_kind(&config, true),
                ConsoleKind::Tty,
                "panic_recovery must veto splash"
            );
        }
    }

    #[test]
    fn open_console_splash_disabled_returns_tty() {
        let config = Config::recovery_default();
        // recovery_default has splash.enable=false (and the field is
        // gated, so on no-feature builds the disabled-state is the
        // only state).
        assert_eq!(decide_kind(&config, false), ConsoleKind::Tty);
    }

    #[cfg(feature = "image-splash")]
    #[test]
    fn open_console_splash_enabled_prefers_splash() {
        let mut config = Config::recovery_default();
        config.splash.enable = true;
        assert_eq!(decide_kind(&config, false), ConsoleKind::Splash);
    }
}
