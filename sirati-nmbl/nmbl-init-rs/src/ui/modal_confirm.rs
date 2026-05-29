//! Yes/no confirmation modal — standalone and overlay variants.

use crate::error::Result;
use crate::ui::ConfirmOutcome;
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, ModalKind};
use crate::ui::console::Console;
use crate::ui::modal_helpers::{ModalPollOutcome, handle_modal_scroll_key, modal_poll};
use crate::ui::view;

/// Show a centred yes/no confirmation modal with `title` + `message`
/// on the supplied console and block until the operator commits.
///
/// This is the standalone variant: it draws onto a fresh frame with no
/// underlying screen. Used by call sites that have no persistent App
/// (e.g. early-boot activation). Emergency-menu actions should use
/// [`show_modal_confirm_over`] instead so the menu remains visible
/// behind the modal.
///
/// Returns:
///   - `Ok(ConfirmOutcome::Yes)`       — Enter on Yes, or hotkey 'y'.
///   - `Ok(ConfirmOutcome::No)`        — Enter on No, or hotkey 'n'.
///   - `Ok(ConfirmOutcome::Cancelled)` — Esc.
///
/// `yes_default = true` highlights the Yes button on first paint;
/// pass `false` for "are you sure?"-style prompts where the safer
/// answer is No.
///
/// Falls back to `ConfirmOutcome::No` if rendering fails — same
/// principle as [`show_modal_error`]: when the operator can't see the
/// modal, default to the safer non-action.
pub async fn show_modal_confirm(
    console: &mut dyn Console,
    title: &str,
    message: &str,
    yes_label: &str,
    no_label: &str,
    yes_default: bool,
) -> Result<ConfirmOutcome> {
    use crossterm::event::KeyCode;

    let hint = "Left/Right select  Enter confirm  Esc cancel";
    let mut yes_selected = yes_default;
    let mut scroll_offset: u16 = 0;

    let mut dirty = true;
    loop {
        if dirty {
            let data = view::ModalConfirmScreenData {
                title,
                message,
                yes_label,
                no_label,
                yes_selected,
                hint,
                scroll_offset,
            };
            if let Err(e) = console.draw_with(&mut |frame| view::render_modal_confirm(frame, &data))
            {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-confirm render failed: {e}");
                return Ok(ConfirmOutcome::No);
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
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, true, 2, console, scroll_offset)
        {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                yes_selected = !yes_selected;
                dirty = true;
            }
            KeyCode::Enter => {
                return Ok(if yes_selected {
                    ConfirmOutcome::Yes
                } else {
                    ConfirmOutcome::No
                });
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(ConfirmOutcome::Yes),
            KeyCode::Char('n') | KeyCode::Char('N') => return Ok(ConfirmOutcome::No),
            KeyCode::Esc => return Ok(ConfirmOutcome::Cancelled),
            _ => {}
        }
    }
}

/// Overlay variant of [`show_modal_confirm`] that paints the modal ON
/// TOP of `app.screen` so the underlying menu (typically the
/// emergency picker) stays visible behind. Closing the modal restores
/// `app.modal` to `None` and the next render returns to the same
/// selection / scroll state.
pub async fn show_modal_confirm_over(
    console: &mut dyn Console,
    app: &mut App<'_>,
    title: &str,
    message: &str,
    yes_label: &str,
    no_label: &str,
    yes_default: bool,
) -> Result<ConfirmOutcome> {
    use crossterm::event::KeyCode;

    let hint = "Left/Right select  Enter confirm  Esc cancel";
    app.modal_scroll_reset();
    let outcome = async {
        app.modal = Some(ModalKind::Confirm {
            title: title.to_owned(),
            message: message.to_owned(),
            yes_label: yes_label.to_owned(),
            no_label: no_label.to_owned(),
            yes_selected: yes_default,
            hint: hint.to_owned(),
        });

        let mut dirty = true;
        loop {
            if dirty {
                if let Err(e) = console.render(app) {
                    eprintln!("[nmbl] {title}: {message}");
                    crate::nmbl_warn!("modal-confirm render failed: {e}");
                    return Ok(ConfirmOutcome::No);
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
            // Pull the modal's message out for the scroll helper. We
            // need an immutable read here BEFORE the mutable borrow on
            // app.modal for `yes_selected` below; otherwise borrowck
            // rejects the second access.
            let modal_message = match &app.modal {
                Some(ModalKind::Confirm { message, .. }) => message.clone(),
                _ => return Ok(ConfirmOutcome::No),
            };
            if let Some(new_off) = handle_modal_scroll_key(
                key,
                &modal_message,
                true,
                2,
                console,
                app.modal_scroll_offset,
            ) {
                app.modal_scroll_offset = new_off;
                dirty = true;
                continue;
            }
            let Some(ModalKind::Confirm { yes_selected, .. }) = &mut app.modal else {
                return Ok(ConfirmOutcome::No);
            };
            match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                    *yes_selected = !*yes_selected;
                    dirty = true;
                }
                KeyCode::Enter => {
                    return Ok(if *yes_selected {
                        ConfirmOutcome::Yes
                    } else {
                        ConfirmOutcome::No
                    });
                }
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(ConfirmOutcome::Yes),
                KeyCode::Char('n') | KeyCode::Char('N') => return Ok(ConfirmOutcome::No),
                KeyCode::Esc => return Ok(ConfirmOutcome::Cancelled),
                _ => {}
            }
        }
    }
    .await;
    app.modal = None;
    app.modal_scroll_reset();
    outcome
}
