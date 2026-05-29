//! UI orchestrator. The frame loop lives here; pure render functions
//! live in [`view`], the state machine in [`app`], and the backend
//! abstraction (splash framebuffer vs raw-mode tty) in [`console`].
//! Every interactive screen — selector, cmdline editor, passphrase
//! modal, emergency picker — renders through the same `&mut dyn Console`,
//! regardless of whether the underlying device is a framebuffer VT, a
//! `/dev/tty1` keyboard line, or a serial UART. Ratatui's crossterm
//! backend emits vt100/xterm escape sequences which every modern
//! serial terminal emulator (xterm, tmux, picocom, screen, minicom)
//! understands, so there is no longer a line-mode fallback path.
//!
//! ## Activation passphrase wiring
//!
//! [`TuiPasswordSupplier`] implements
//! [`crate::activation::PasswordSupplier`] so the top-level boot
//! flow can pass it to
//! [`crate::activation::run_all_activations`] as
//! `&mut dyn PasswordSupplier`. When the activation runner reaches a
//! `luks-password` entry it calls `prompt(console, label)` once; the
//! supplier reuses the LIVE boot console (splash framebuffer or
//! raw-mode tty) the orchestrator already holds, drives a render-poll
//! loop over the [`Screen::Passphrase`] modal, and returns the entered
//! string in a `Zeroizing<String>` so the buffer is wiped after
//! `cryptsetup` drains it. No new console is opened — that would
//! duplicate the splash bring-up and flicker between backends.
//!
//! Esc on the modal returns a [`NmblError::Tui`] which
//! `run_all_activations` wraps as [`NmblError::Activation`] and the
//! top-level driver routes to the emergency shell.

pub mod app;
pub mod console;
pub mod console_picker;
pub mod console_relay;
pub mod editline;
pub mod emergency;
pub mod emergency_actions;
pub mod key_echo;
pub mod modal_confirm;
pub mod modal_error;
pub(crate) mod modal_helpers;
pub mod modal_layout;
pub mod modal_password;
pub mod password_supplier;
#[cfg(feature = "pretty-shell")]
pub mod pretty_shell;
pub mod reporter;
#[cfg(feature = "network-rescue")]
pub mod rescue;
pub mod runtime;
pub mod screen_render;
pub mod selector;
pub mod splash_render;
pub mod timeout;
pub mod tty_enum;
pub mod view;

#[cfg(test)]
mod tests;

use std::time::Duration;

pub use app::{
    App, BootStatusData, Decision, EmergencyChoice, EmergencyItem, ModalKind, Screen,
    SessionInteraction,
};
pub(crate) use emergency::{build_emergency_app, build_message, default_items};
pub use emergency::{run_emergency_screen, run_emergency_screen_with_app};
pub use reporter::{BootReporter, ProgressSink, TickOutcome};
pub use runtime::{block_on_tui, build_local_runtime, spawn_poller};

pub use modal_confirm::{show_modal_confirm, show_modal_confirm_over};
pub use modal_error::{show_modal_error, show_modal_error_over};
pub use modal_password::{show_modal_buttons, show_wrong_password_modal};
pub use password_supplier::TuiPasswordSupplier;
#[cfg(feature = "mocking")]
pub(crate) use password_supplier::passphrase_prompt_on_console;
pub(crate) use screen_render::render_current_screen;
pub use selector::run_selector;

#[cfg(feature = "image-splash")]
pub(crate) use splash_render::{render_splash_frame, render_splash_frame_with};

/// Slice we wait on input per iteration. Shared by the event loop and
/// the countdown ticker so they have the same responsiveness profile
/// and only one knob to tune.
pub(crate) const POLL_SLICE: Duration = Duration::from_millis(100);

/// Outcome of a yes/no modal confirmation prompt.
///
/// Kept as a dedicated enum (rather than `bool`) so call sites read
/// at a glance and so a future "third option" (e.g. `Defer`) can be
/// added without rippling through every match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// Operator picked the affirmative button (Yes / Boot / …) via
    /// Enter, hotkey 'y', or Enter on the highlighted Yes button.
    Yes,
    /// Operator picked the negative button (Back / No / …) via Enter
    /// on the highlighted No button or hotkey 'n'.
    No,
    /// Operator pressed Esc; treated as "go back without committing".
    /// Production callers typically lump this into `No`, but keeping
    /// it distinct lets tests assert on the exact key path that
    /// dismissed the modal.
    Cancelled,
}

/// Outcome of the wrong-password modal shown after a `luks-password`
/// activation returns exit code 2 (cryptsetup's "no key available"). The
/// activation loop matches on this to decide whether to re-prompt, hand
/// back to `main` for a reboot, or detour through an in-process shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrongPasswordOutcome {
    /// Re-prompt for the passphrase and re-run the same activation.
    TryAgain,
    /// Operator picked [Reboot]; the activation layer propagates this
    /// as [`NmblError::OperatorChoseReboot`] so `main` short-circuits
    /// to [`crate::terminal::TerminalAction::Reboot`] without dropping
    /// to the emergency menu.
    Reboot,
    /// Operator picked [Pretty Shell]; the caller runs the alacritty-
    /// backed PTY session inside the TUI box. Exposed on the
    /// `pretty-shell` feature (default-on; also pulled in by
    /// `image-splash`) — a `--no-default-features` build hides the
    /// button entirely so there is nothing to dispatch.
    #[cfg(feature = "pretty-shell")]
    PrettyShell,
    /// Operator picked [Raw Shell]; the caller opens the console-
    /// picker dialog and runs the multiplexed busybox PTY relay.
    RawShell,
}
