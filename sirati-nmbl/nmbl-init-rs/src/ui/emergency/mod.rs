//! Emergency-screen orchestrator.
//!
//! When a top-level phase returns `Err`, `shell::drop_to_emergency`
//! used to immediately `execve` the shell. That bypassed the splash
//! backend entirely: operators on a VNC console saw nothing useful,
//! and there was no way to choose between rebooting and dropping to
//! the shell. This module replaces that behaviour with a proper TUI
//! that renders into the already-open boot [`Console`].
//!
//! Architectural rule: **all UI is TUI code**. The splash backend is
//! only a render target. The state machine that drives the screen
//! lives in [`crate::ui::app::Screen::Emergency`]; the renderer lives
//! in [`crate::ui::view::render_emergency`]. This module wires the
//! two together against the caller-supplied console and applies a
//! 30-second default-to-reboot timer.
//!
//! ## Console ownership
//!
//! The boot orchestrator (main.rs) brings the [`Console`] up once at
//! boot, hands it through every phase, and — on phase failure — passes
//! the same handle into [`run_emergency_screen`]. The backend choice
//! (splash vs tty, with panic-recovery skipping splash) is therefore
//! already made by [`crate::ui::console::open_console`]; this module
//! is purely a state-machine driver. The serial-console code path is
//! the operator's existing tty console — `/dev/console` already routes
//! to the serial line in that deployment.
//!
//! ## Timer
//!
//! The countdown runs only on a fully unattended boot — one in which the
//! operator has not pressed any key this session. In that case, with no
//! input for 30 seconds we default to [`EmergencyChoice::Reboot`].
//! Operators on a remote VNC console may not be sitting there when boot
//! fails; rebooting is the safe default — if the next boot also fails
//! they'll just land back here. Once any key has been pressed (boot
//! menu, LUKS passphrase, a prior visit to this screen) the operator is
//! present, so the error screen shows no countdown and waits
//! indefinitely for an explicit choice.
//!
//! The clock is injected as a `Fn() -> Instant` so unit tests can run
//! the timer machinery without sleeping a real wall-clock second.

use std::time::{Duration, Instant};

use crate::error::NmblError;
use crate::ui::app::{App, EmergencyChoice, SessionInteraction};
use crate::ui::console::Console;

mod builders;
pub(crate) mod loop_driver;

pub(crate) use builders::{build_emergency_app, build_message, default_items};

/// Default countdown to auto-reboot when the operator is not present.
pub(crate) const EMERGENCY_TIMEOUT: Duration = Duration::from_secs(30);

/// Run the emergency screen on the supplied [`Console`] and return the
/// operator's choice.
///
/// The caller (main.rs / shell.rs) owns the console lifecycle: this
/// function only drives the TUI event loop. With no input for 30s the
/// timer expires and the function returns [`EmergencyChoice::Reboot`].
/// On a backend error mid-loop we also fall back to Reboot — the
/// safest default when the operator can't see the screen anyway.
///
/// One-shot convenience used by tests and any caller that hasn't yet
/// adopted the persistent-App overlay model. The auto-reboot countdown
/// starts fresh on every call. Production code uses
/// [`run_emergency_screen_with_app`] instead so re-entries don't
/// restart the timer.
pub async fn run_emergency_screen(console: &mut dyn Console, err: &NmblError) -> EmergencyChoice {
    let message = build_message(err);
    let items = default_items();
    // Convenience wrapper: no prior session to inherit, so start fresh.
    let session = SessionInteraction::new();
    let mut app = build_emergency_app(&message, &items, &session);
    run_emergency_screen_with_app(console, &mut app, EMERGENCY_TIMEOUT).await
}

/// Same as [`run_emergency_screen`] but reuses an externally-owned
/// `App` so the auto-reboot countdown deadline (held in
/// `app.error_countdown_deadline`) survives a re-entry after the
/// operator dismisses a modal and lands back on the error screen.
///
/// The countdown is armed only when the operator has not yet pressed
/// any key this session; once they have (including on the LUKS
/// passphrase screen), re-entries to the error screen show no countdown
/// and wait indefinitely. On a fully unattended first call the helper
/// latches the deadline at `now + 30s`; on re-entry the existing
/// deadline is preserved. If the deadline has already elapsed on
/// re-entry the loop reboots immediately.
///
/// `timeout` is the resolved auto-reboot budget — callers pass
/// [`EMERGENCY_TIMEOUT`] for the historic 30 s default or an
/// operator-configured override (see `boot.nmbl.emergencyTimeoutSecs`).
pub async fn run_emergency_screen_with_app(
    console: &mut dyn Console,
    app: &mut App<'_>,
    timeout: Duration,
) -> EmergencyChoice {
    // The loop itself latches on first entry — subsequent calls find
    // Some(_) and keep the original deadline. Re-entry after an
    // elapsed deadline trips the "remaining = None" branch inside the
    // loop and returns Reboot at once.
    loop_driver::drive_emergency_loop(app, timeout, Instant::now, console)
        .await
        .unwrap_or(EmergencyChoice::Reboot)
}

/// Resolve the emergency auto-reboot timeout from runtime config,
/// falling back to the built-in [`EMERGENCY_TIMEOUT`] default when the
/// operator has not set `general.emergency_timeout_secs`.
pub fn resolve_emergency_timeout(config: &crate::config::Config) -> Duration {
    config
        .general
        .emergency_timeout_secs
        .map_or(EMERGENCY_TIMEOUT, Duration::from_secs)
}

#[cfg(test)]
mod tests;
