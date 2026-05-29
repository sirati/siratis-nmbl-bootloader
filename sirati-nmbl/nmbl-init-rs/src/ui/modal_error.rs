//! Error/info modal — standalone and overlay variants.

use std::time::{Duration, Instant};

use crate::error::Result;
use crate::ui::POLL_SLICE;
use crate::ui::app::{App, ModalKind};
use crate::ui::console::Console;
use crate::ui::modal_helpers::{ModalPollOutcome, handle_modal_scroll_key, modal_poll};
use crate::ui::view;

/// Show a centred modal dialog with `title` + `message` on the supplied
/// console and block until the operator presses any key (or
/// `timeout_secs` elapses, whichever comes first). Use this for
/// surfacing action failures (PTY allocation, network mount, …) so the
/// operator sees what just went wrong instead of staring at the stale
/// screen underneath.
///
/// Falls back to a serial-style stderr dump when the render fails so
/// the operator on a degraded console still gets the diagnostic.
pub async fn show_modal_error(
    console: &mut dyn Console,
    title: &str,
    message: &str,
    timeout: Duration,
) -> Result<()> {
    let hint = "press any key to continue";
    let mut scroll_offset: u16 = 0;
    let mut dirty = true;
    let deadline = Instant::now().checked_add(timeout);
    loop {
        if dirty {
            let data = view::ModalErrorScreenData {
                title,
                message,
                hint,
                scroll_offset,
            };
            if let Err(e) = console.draw_with(&mut |frame| view::render_modal_error(frame, &data)) {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-error render failed: {e}");
                return Ok(());
            }
            dirty = false;
        }
        let slice = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(remaining) => remaining.min(POLL_SLICE),
                None => return Ok(()),
            },
            None => POLL_SLICE,
        };
        let key = match modal_poll(console, slice).await? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        // Scroll keys advance the viewport instead of dismissing; any
        // other key dismisses the modal.
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, false, 0, console, scroll_offset)
        {
            scroll_offset = new_off;
            dirty = true;
            continue;
        }
        return Ok(());
    }
}

/// Overlay variant of [`show_modal_error`] that paints the modal ON
/// TOP of `app.screen` so the menu underneath stays visible. Closing
/// the modal restores `app.modal` to `None`.
pub async fn show_modal_error_over(
    console: &mut dyn Console,
    app: &mut App<'_>,
    title: &str,
    message: &str,
    timeout: Duration,
) -> Result<()> {
    let hint = "press any key to continue";
    app.modal_scroll_reset();
    app.modal = Some(ModalKind::Error {
        title: title.to_owned(),
        message: message.to_owned(),
        hint: hint.to_owned(),
    });
    let deadline = Instant::now().checked_add(timeout);
    let mut dirty = true;
    let res = loop {
        if dirty {
            if let Err(e) = console.render(app) {
                eprintln!("[nmbl] {title}: {message}");
                crate::nmbl_warn!("modal-error render failed: {e}");
                break Ok(());
            }
            dirty = false;
        }
        let slice = match deadline {
            Some(d) => match d.checked_duration_since(Instant::now()) {
                Some(remaining) => remaining.min(POLL_SLICE),
                None => break Ok(()),
            },
            None => POLL_SLICE,
        };
        let key = match modal_poll(console, slice).await? {
            ModalPollOutcome::Key(k) => k,
            ModalPollOutcome::Resized => {
                dirty = true;
                continue;
            }
            ModalPollOutcome::Idle => continue,
        };
        if let Some(new_off) =
            handle_modal_scroll_key(key, message, false, 0, console, app.modal_scroll_offset)
        {
            app.modal_scroll_offset = new_off;
            dirty = true;
            continue;
        }
        break Ok(());
    };
    app.modal = None;
    app.modal_scroll_reset();
    res
}
