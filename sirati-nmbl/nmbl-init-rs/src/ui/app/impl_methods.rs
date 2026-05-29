use std::collections::VecDeque;
use std::time::Instant;

use super::{App, BootStatusData, KEY_ECHO_RING_CAP, SPINNER_FRAMES, Screen, SessionInteraction};

impl<'a> App<'a> {
    pub fn new(generations: &'a [crate::generations::Generation]) -> Self {
        Self {
            generations,
            selected_index: 0,
            screen: Screen::List,
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
            modal: None,
            error_countdown_deadline: None,
            modal_scroll_offset: 0,
            interaction: SessionInteraction::new(),
            exit_session: false,
            return_screen: None,
            caps_lock_warning: false,
        }
    }

    /// Same as [`App::new`] but joins an existing session so the
    /// interaction latch is shared with the other Apps of this boot.
    pub fn new_in_session(
        generations: &'a [crate::generations::Generation],
        session: &SessionInteraction,
    ) -> Self {
        let mut app = Self::new(generations);
        app.interaction = session.clone();
        app
    }

    /// Construct an App parked on the [`Screen::BootStatus`] view with
    /// the given phase label, an empty log buffer, and spinner_frame=0.
    ///
    /// `generations` is empty because the boot-status screen runs
    /// before the selector has anything to show. A future caller can
    /// transition out of the boot-status screen by replacing
    /// `self.screen` directly.
    pub fn boot_status(phase: impl Into<std::borrow::Cow<'a, str>>) -> App<'a> {
        App {
            generations: &[],
            selected_index: 0,
            screen: Screen::BootStatus(BootStatusData {
                phase: phase.into(),
                log_lines: Vec::new(),
                spinner_frame: 0,
            }),
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
            modal: None,
            error_countdown_deadline: None,
            modal_scroll_offset: 0,
            interaction: SessionInteraction::new(),
            exit_session: false,
            return_screen: None,
            caps_lock_warning: false,
        }
    }

    /// Construct an App parked on [`Screen::KeyEcho`] with empty ring
    /// buffers. The diagnostic loop in [`crate::ui::key_echo`] drives
    /// further mutations via [`App::push_key_echo_event`] and
    /// [`App::push_key_echo_bytes`].
    pub fn key_echo() -> App<'a> {
        App {
            generations: &[],
            selected_index: 0,
            screen: Screen::KeyEcho {
                events: VecDeque::new(),
                byte_log: VecDeque::new(),
            },
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
            modal: None,
            error_countdown_deadline: None,
            modal_scroll_offset: 0,
            interaction: SessionInteraction::new(),
            exit_session: false,
            return_screen: None,
            caps_lock_warning: false,
        }
    }

    /// Scroll the modal text viewport up by `n` rows (towards the top
    /// of the buffer). Saturates at 0.
    pub fn modal_scroll_up(&mut self, n: u16) {
        self.modal_scroll_offset = self.modal_scroll_offset.saturating_sub(n);
    }

    /// Scroll the modal text viewport down by `n` rows, clamped at
    /// `total - visible`. Saturates so the last visible row never
    /// scrolls past the buffer's last row.
    pub fn modal_scroll_down(&mut self, n: u16, total: u16, visible: u16) {
        let max_off = total.saturating_sub(visible);
        let new_off = self.modal_scroll_offset.saturating_add(n);
        self.modal_scroll_offset = new_off.min(max_off);
    }

    /// Reset the modal scroll offset to 0. Called every modal open/close
    /// path so a re-entry never inherits the previous modal's offset.
    pub fn modal_scroll_reset(&mut self) {
        self.modal_scroll_offset = 0;
    }

    /// Latch the auto-reboot deadline for the error (emergency) screen.
    ///
    /// Sets `error_countdown_deadline` only when it is currently
    /// `None` — re-entries (after dismissing a modal and returning to
    /// the error screen) find the deadline already present so the
    /// timer never restarts. If the deadline already elapsed during
    /// time spent on another screen, the next visit will observe
    /// `now >= deadline` and the loop driver reboots immediately.
    pub fn latch_error_countdown(&mut self, auto_reboot_in: std::time::Duration) {
        if self.error_countdown_deadline.is_none() {
            let now = Instant::now();
            self.error_countdown_deadline = Some(now.checked_add(auto_reboot_in).unwrap_or(now));
        }
    }

    /// Replace the error text shown on the emergency screen so the
    /// operator always sees the *latest* failure, not the first one
    /// the session ever hit. Called whenever a new error is surfaced
    /// (an emergency action failing, a re-entry after a sub-flow) so
    /// the menu's "error" box tracks the most recent diagnostic
    /// instead of latching the original boot error forever. No-op when
    /// the App is on any other screen.
    pub fn set_emergency_message(&mut self, new_message: impl Into<String>) {
        if let Screen::Emergency { message, .. } = &mut self.screen {
            *message = new_message.into();
        } else {
            debug_assert!(
                false,
                "set_emergency_message called on non-Emergency screen"
            );
        }
    }

    /// Append a human-readable parsed-event string to the key-echo
    /// events ring, evicting the oldest entry when full. No-op when
    /// the App is on any other screen.
    pub fn push_key_echo_event(&mut self, line: impl Into<String>) {
        if let Screen::KeyEcho { events, .. } = &mut self.screen {
            if events.len() >= KEY_ECHO_RING_CAP {
                events.pop_front();
            }
            events.push_back(line.into());
        } else {
            debug_assert!(false, "push_key_echo_event called on non-KeyEcho screen");
        }
    }

    /// Append a hex-printed raw-byte string to the key-echo byte-log
    /// ring, evicting the oldest entry when full. No-op when the App
    /// is on any other screen.
    pub fn push_key_echo_bytes(&mut self, line: impl Into<String>) {
        if let Screen::KeyEcho { byte_log, .. } = &mut self.screen {
            if byte_log.len() >= KEY_ECHO_RING_CAP {
                byte_log.pop_front();
            }
            byte_log.push_back(line.into());
        } else {
            debug_assert!(false, "push_key_echo_bytes called on non-KeyEcho screen");
        }
    }

    /// Replace the phase label of the boot-status screen. No-op when
    /// the App is on any other screen so a stray phase update from a
    /// late-firing supervisor task can't crash production.
    pub fn set_boot_phase(&mut self, phase: impl Into<std::borrow::Cow<'a, str>>) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.phase = phase.into();
        } else {
            debug_assert!(false, "set_boot_phase called on non-BootStatus screen");
        }
    }

    /// Replace the log-line snapshot. The caller (typically holding a
    /// log-ring snapshot via `crate::log::snapshot`) is responsible for
    /// ordering: most recent last.
    pub fn set_boot_log_lines(&mut self, lines: Vec<String>) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.log_lines = lines;
        } else {
            debug_assert!(false, "set_boot_log_lines called on non-BootStatus screen");
        }
    }

    /// Advance the spinner one frame. Wraps modulo [`SPINNER_FRAMES`]
    /// so callers can tick on any interval without checking the count.
    pub fn tick_boot_spinner(&mut self) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.spinner_frame = data.spinner_frame.wrapping_add(1) % SPINNER_FRAMES;
        } else {
            debug_assert!(false, "tick_boot_spinner called on non-BootStatus screen");
        }
    }

    /// Flip the passphrase modal into "verifying" mode (cryptsetup is
    /// running). The renderer paints a spinner overlay so the operator
    /// sees the boot is alive — closes the visual gap between Enter and
    /// the LUKS-unlock result. No-op when the App is on another screen.
    ///
    /// Setting `verifying = false` also resets `spinner_frame` to 0 so
    /// a subsequent re-verify starts from a known phase rather than
    /// inheriting the last frame from the previous attempt.
    pub fn set_passphrase_verifying(&mut self, verifying: bool) {
        if let Screen::Passphrase {
            verifying: v,
            spinner_frame,
            ..
        } = &mut self.screen
        {
            *v = verifying;
            if !verifying {
                *spinner_frame = 0;
            }
        } else {
            debug_assert!(
                false,
                "set_passphrase_verifying called on non-Passphrase screen"
            );
        }
    }

    /// Advance the passphrase verifying-spinner one frame. Wraps modulo
    /// [`SPINNER_FRAMES`]. No-op when the App is on another screen.
    pub fn tick_passphrase_spinner(&mut self) {
        if let Screen::Passphrase { spinner_frame, .. } = &mut self.screen {
            *spinner_frame = spinner_frame.wrapping_add(1) % SPINNER_FRAMES;
        } else {
            debug_assert!(
                false,
                "tick_passphrase_spinner called on non-Passphrase screen"
            );
        }
    }

    /// Clear the passphrase buffer (zeroizing it) and reset spinner /
    /// verifying flags. Used by the wrong-password retry path so a
    /// re-prompt starts from a clean slate. No-op when the App is on
    /// another screen.
    pub fn clear_passphrase_buffer(&mut self) {
        if let Screen::Passphrase {
            buffer,
            cursor,
            verifying,
            spinner_frame,
            ..
        } = &mut self.screen
        {
            buffer.clear();
            *cursor = 0;
            *verifying = false;
            *spinner_frame = 0;
        } else {
            debug_assert!(
                false,
                "clear_passphrase_buffer called on non-Passphrase screen"
            );
        }
    }
}
