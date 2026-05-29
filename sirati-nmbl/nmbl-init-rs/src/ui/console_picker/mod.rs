//! Console picker dialog + shell-relay driver.
//!
//! When the operator selects `[Shell]` on the emergency screen, NMBL no
//! longer `execve(2)`s into the rescue shell as PID 1. Instead it:
//!
//! 1. Resolves the kernel-elected primary interactive console from
//!    `/sys/class/tty/console/active`, AND enumerates every plausible
//!    operator-attached tty via [`crate::ui::tty_enum::enumerate_ttys`]
//!    (framebuffer VT, `/dev/ttyS<0..3>`, USB serial). The kernel
//!    console is pre-checked and labelled `(kernel console)`; every
//!    other discovered tty is offered unchecked, labelled by kind.
//! 2. Lets the operator toggle the per-tty checkboxes AND type a
//!    custom `/dev/<X>` path into a single-line input below the list.
//!    The custom field is live-validated (green when the path exists
//!    as a chardev and is not a duplicate of an enumerated entry; red
//!    otherwise); valid custom entries are auto-checked and treated as
//!    additional targets.
//! 3. On `[Spawn]`, decides between three regimes:
//!    - **No overlap with display tty** → fork ONE shell per selected
//!      target with its stdio dup'd to that tty, then return to the
//!      previous screen with a success-modal confirmation. The shell
//!      runs detached on the operator's chosen line(s); NMBL never
//!      enters a relay loop on the wrong fd.
//!    - **Display tty in the selection** → run the multi-target
//!      multiplex relay loop (PTY master fan-out / fan-in via
//!      [`crate::ui::console_relay`]). Required because the operator
//!      cannot see the splash and the shell simultaneously.
//!    - **Both** → relay loop covers the display tty AND every
//!      additional tty in one PTY pair.
//! 4. Returns to the caller after the shell exits (relay regime) or
//!    immediately after the fire-and-forget spawn (no-overlap regime).
//!
//! ## Why no `Screen::ConsolePicker` variant?
//!
//! The picker is an entirely contained sub-flow: it never coexists with
//! the boot menu or the editing screen. Keeping its state local to this
//! module's driver loop (rather than in [`crate::ui::app::Screen`])
//! avoids growing the central state machine for a transient modal —
//! same pattern [`crate::ui::show_modal_error`] uses.

mod render;
mod session;
mod state;
mod types;

#[cfg(test)]
mod tests;

// Public re-exports — preserve the original crate::ui::console_picker::* paths.
pub use session::display_overlaps_targets;
pub use session::{PickerSessionOutcome, run_picker_session};
pub use types::{ButtonCursor, CandidateOrigin, PickerCandidate, PickerOutcome, PickerState};
