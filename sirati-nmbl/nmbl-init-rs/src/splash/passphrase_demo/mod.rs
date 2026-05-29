#![cfg(feature = "image-splash")]
//! UI-only passphrase prompt demo with a tri-button emergency fallback.
//!
//! Renders a LUKS-style masked passphrase dialog over the existing
//! splash compositor. There is no `cryptsetup` integration: every
//! `Enter` is treated as a failed attempt, and after `MAX_ATTEMPTS`
//! the emergency menu appears (Retry / Shell / Reboot).
//!
//! The dialog renderers live here rather than in `ui::view` so the
//! default (non-`image-splash`) build stays byte-identical.

mod render;
mod state;

pub use state::run;

/// Maximum passphrase attempts before the emergency menu pops up.
pub(super) const MAX_ATTEMPTS: u8 = 3;
/// Static prompt label used for the demo. Production wiring would
/// pass through the activation entry's `volume` / `device` field.
pub(super) const PROMPT_LABEL: &str = "Unlock encrypted root (demo)";

/// Internal state of the demo state machine.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DemoState {
    Entering { buffer: String, attempts: u8 },
    Emergency { selected: u8, attempts: u8 },
}

/// Outcome of the demo loop. The splash orchestrator logs this and
/// returns to the main boot menu — no kernel-side effects yet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DemoOutcome {
    /// Operator picked "Retry passphrase" from the emergency menu but
    /// the demo loop returned early (e.g. for a host-driven test).
    RetryRequested,
    /// Operator picked "Drop to shell".
    DroppedToShell,
    /// Operator picked "Reboot".
    RebootRequested,
    /// Reserved: graceful cancellation; not currently produced by the
    /// real-input loop but used by the state-machine tests.
    Cancelled,
}

/// Outcome of folding a single key press into the state machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum StepResult {
    /// Continue polling for input.
    Continue,
    /// Demo loop should exit with this outcome.
    Done(DemoOutcome),
}
