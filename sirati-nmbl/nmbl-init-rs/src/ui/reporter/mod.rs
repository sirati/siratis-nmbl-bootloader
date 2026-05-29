//! Boot-status reporter.
//!
//! Thin wrapper around `&mut dyn Console` plus the active [`App`]. Phase
//! code calls `reporter.set_phase(...)` and `reporter.tick()` without
//! needing to know about [`Console::render`] directly.
//!
//! ## Spinner plumbing for blocking waits
//!
//! Long-running device-wait and activation-wait loops should not look
//! frozen on the splash. The [`ProgressSink`] trait gives those loops a
//! one-method handle they can call every poll iteration to:
//!
//! * advance the spinner one frame,
//! * replace the phase label with a "waiting for X (Ns / Ms)" string,
//! * pull the latest log snapshot from the global ring,
//! * push a fresh frame to the underlying [`Console`].
//!
//! [`BootReporter`] implements [`ProgressSink`] so the same handle that
//! drives phase transitions also drives the spinner. Tests use a counting
//! mock that doesn't open a console.
//!
//! ## Sibling-subagent contract
//!
//! The screen the reporter mutates is [`crate::ui::app::Screen::BootStatus`],
//! which the renderer in [`crate::ui::view::render_boot_status`] already
//! handles for both backends.

mod reporter_impl;
mod types;

#[cfg(test)]
mod tests;

pub use reporter_impl::BootReporter;
pub use types::{ProgressSink, TickOutcome};
