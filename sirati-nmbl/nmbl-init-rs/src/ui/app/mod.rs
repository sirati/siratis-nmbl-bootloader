//! TUI state machine. Pure logic: takes `crossterm::event::KeyEvent`s
//! and mutates [`App`]; the surrounding `ui::mod` is responsible for
//! actually polling input and rendering frames via [`crate::ui::view`].
//!
//! The state machine has six screens:
//! - [`Screen::List`]    — generation picker, default landing page.
//! - [`Screen::Editing`] — single-line kernel-cmdline editor.
//! - [`Screen::Passphrase`] — modal LUKS prompt driven by activation.rs.
//! - [`Screen::Emergency`] — boot-failed picker between Reboot and Shell.
//! - [`Screen::BootStatus`] — non-interactive progress + log view shown
//!   during early boot phases (before the selector / activation).
//! - [`Screen::KeyEcho`] — diagnostic test screen that echoes every key
//!   event and raw byte sequence to two panels. Inaccessible from
//!   normal boot; only reached when `nmbl.key_echo=1` appears on the
//!   kernel cmdline. Used to debug VNC/PS-2 → splash input plumbing.
//!
//! When the user makes a final decision the `decision` field is set
//! and [`App::on_key`] returns `true`, signalling the run loop to exit.
//! The passphrase modal is the exception: Enter on a passphrase screen
//! leaves the App alive (the caller — the supplier driving
//! [`crate::ui::passphrase_prompt_on_console`] — drains the buffer and
//! returns it without exiting the App), and only Esc on the passphrase
//! modal sets a [`Decision::Shell`] exit.

mod handlers;
mod impl_methods;
mod types;

pub(super) use types::LOG_PAGE;
pub use types::{
    App, BootStatusData, Decision, EmergencyChoice, EmergencyItem, KEY_ECHO_RING_CAP, ModalKind,
    SPINNER_FRAMES, SPINNER_GLYPHS, Screen, SessionInteraction,
};

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert with panics on contract failure"
)]
mod tests_list_edit_pass;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert with panics on contract failure"
)]
mod tests_emergency;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert with panics on contract failure"
)]
mod tests_misc;
