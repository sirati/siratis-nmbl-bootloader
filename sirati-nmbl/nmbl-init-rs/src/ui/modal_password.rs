//! Wrong-password modal and generic N-button modal.

use crate::error::Result;
use crate::ui::POLL_SLICE;
use crate::ui::WrongPasswordOutcome;
use crate::ui::console::Console;
use crate::ui::modal_helpers::{ModalPollOutcome, handle_modal_scroll_key, modal_poll};
use crate::ui::view;

/// Show the wrong-password modal after a `luks-password` activation
/// returns cryptsetup exit code 2 (no key available). Four buttons when
/// the `image-splash` feature is on (three otherwise): `[Try again]`
/// (default), `[Reboot]`, `[Pretty Shell]` (feature-gated),
/// `[Raw Shell]`. Esc maps to [`WrongPasswordOutcome::TryAgain`] so a
/// stray Esc doesn't reboot the machine.
///
/// `attempt` is 1-indexed; the title reads "Wrong password (attempt N)".
///
/// If the backend itself fails to render, we fall back to
/// [`WrongPasswordOutcome::TryAgain`] — same principle as
/// [`show_modal_error`]/[`show_modal_confirm`]: when the operator can't
/// see the modal, default to the safest action (which here is to
/// re-prompt rather than reboot or open a shell).
pub async fn show_wrong_password_modal(
    console: &mut dyn Console,
    attempt: u32,
) -> Result<WrongPasswordOutcome> {
    use crossterm::event::KeyCode;

    let title = format!("Wrong password (attempt {attempt})");
    let message =
        "cryptsetup rejected the passphrase. Try again, reboot, or open a recovery shell.";
    let hint = "Left/Right select  Enter confirm  Esc = Try again";
    // Button layout is feature-dependent: Pretty Shell only exists when
    // the `pretty-shell` feature compiled the alacritty-backed PTY
    // emulator into the binary. We materialise the label list once at
    // entry so the render loop and the key handler share the same
    // indexing.
    #[cfg(feature = "pretty-shell")]
    let labels: &[&str] = &["Try again", "Reboot", "Pretty Shell", "Raw Shell"];
    #[cfg(not(feature = "pretty-shell"))]
    let labels: &[&str] = &["Try again", "Reboot", "Raw Shell"];
    let n = labels.len();
    let mut selected: usize = 0;
    let mut scroll_offset: u16 = 0;

    let mut dirty = true;
    loop {
        if dirty {
            let data = view::ModalButtonsScreenData {
                title: &title,
                message,
                labels,
                selected,
                hint,
                scroll_offset,
            };
            if let Err(e) = console.draw_with(&mut |frame| view::render_modal_buttons(frame, &data))
            {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("wrong-password modal render failed: {e}");
                return Ok(WrongPasswordOutcome::TryAgain);
            }
            dirty = false;
        }

        let key = match modal_poll(console, POLL_SLICE).await? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        let btn_count = u16::try_from(n).unwrap_or(u16::MAX);
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, true, btn_count, console, scroll_offset)
        {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                selected = if selected == 0 {
                    n.saturating_sub(1)
                } else {
                    selected.saturating_sub(1)
                };
                dirty = true;
            }
            KeyCode::Right | KeyCode::Tab => {
                selected = selected.saturating_add(1) % n;
                dirty = true;
            }
            KeyCode::Enter => return Ok(decode_wrong_password_selection(selected)),
            KeyCode::Char('t') | KeyCode::Char('T') => {
                return Ok(WrongPasswordOutcome::TryAgain);
            }
            KeyCode::Char('r') | KeyCode::Char('R') => {
                return Ok(WrongPasswordOutcome::Reboot);
            }
            #[cfg(feature = "pretty-shell")]
            KeyCode::Char('p') | KeyCode::Char('P') => {
                return Ok(WrongPasswordOutcome::PrettyShell);
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                return Ok(WrongPasswordOutcome::RawShell);
            }
            KeyCode::Esc => return Ok(WrongPasswordOutcome::TryAgain),
            _ => {}
        }
    }
}

/// Show a generic N-button modal and return the committed button
/// index. Used by callers that don't fit the wrong-password layout
/// (the only specialised driver today) but want the same look-and-feel:
/// bordered modal, wrapped message, right-aligned button row.
///
/// Esc returns the LAST button index (caller convention: rightmost is
/// "Cancel" / "Back"). Empty `labels` returns 0 immediately so the
/// caller's caller never indexes off the end.
pub async fn show_modal_buttons(
    console: &mut dyn Console,
    title: &str,
    message: &str,
    labels: &[&str],
    hint: &str,
) -> Result<usize> {
    use crossterm::event::KeyCode;
    let n = labels.len();
    if n == 0 {
        return Ok(0);
    }
    let mut selected: usize = 0;
    let mut scroll_offset: u16 = 0;
    let mut dirty = true;
    loop {
        if dirty {
            let data = view::ModalButtonsScreenData {
                title,
                message,
                labels,
                selected,
                hint,
                scroll_offset,
            };
            if let Err(e) = console.draw_with(&mut |frame| view::render_modal_buttons(frame, &data))
            {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-buttons render failed: {e}");
                return Ok(n.saturating_sub(1));
            }
            dirty = false;
        }
        let key = match modal_poll(console, POLL_SLICE).await? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        let btn_count = u16::try_from(n).unwrap_or(u16::MAX);
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, true, btn_count, console, scroll_offset)
        {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::BackTab => {
                selected = if selected == 0 {
                    n.saturating_sub(1)
                } else {
                    selected.saturating_sub(1)
                };
                dirty = true;
            }
            KeyCode::Right | KeyCode::Tab => {
                selected = selected.saturating_add(1) % n;
                dirty = true;
            }
            KeyCode::Enter => return Ok(selected),
            KeyCode::Esc => return Ok(n.saturating_sub(1)),
            _ => {}
        }
    }
}

/// Map a wrong-password modal button index to its outcome. Index 0 is
/// always Try again, index 1 is Reboot, then Pretty Shell (only when
/// `pretty-shell` is on), then Raw Shell. Out-of-range indices fall
/// back to TryAgain so a future button-layout drift can't crash boot.
fn decode_wrong_password_selection(idx: usize) -> WrongPasswordOutcome {
    #[cfg(feature = "pretty-shell")]
    {
        match idx {
            1 => WrongPasswordOutcome::Reboot,
            2 => WrongPasswordOutcome::PrettyShell,
            3 => WrongPasswordOutcome::RawShell,
            _ => WrongPasswordOutcome::TryAgain,
        }
    }
    #[cfg(not(feature = "pretty-shell"))]
    {
        match idx {
            1 => WrongPasswordOutcome::Reboot,
            2 => WrongPasswordOutcome::RawShell,
            _ => WrongPasswordOutcome::TryAgain,
        }
    }
}
