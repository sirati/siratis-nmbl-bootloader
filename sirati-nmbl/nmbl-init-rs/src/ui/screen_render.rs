//! Screen dispatch and modal-overlay render helpers.
//!
//! `render_current_screen` is the single entry point: it dispatches to
//! the right per-screen renderer then overlays any active modal on top.
//! `pub(crate)` so the splash orchestrator, the tty backend, and the
//! mocking backend can all reuse the same dispatch without forking the
//! per-screen branching.

use crate::ui::app::{App, BootStatusData, ModalKind, Screen};
use crate::ui::view::{
    self, EditScreenData, EmergencyScreenData, KeyEchoScreenData, ListScreenData,
    PassphraseScreenData, render_boot_status, render_edit, render_emergency, render_key_echo,
    render_list, render_log, render_passphrase,
};

/// Dispatch render based on which screen the App is currently on,
/// then paint the modal overlay on top when `app.modal` is `Some`.
///
/// The underlying screen renders first so the operator sees "where
/// they were" behind a confirmation / error / progress dialog. The
/// modal renderers use ratatui's `Clear` widget on their rect, so
/// they punch a hole without bleeding the menu into the modal body.
///
/// `pub(crate)` so the splash orchestrator can reuse the same dispatch
/// without forking the per-screen branching.
pub(crate) fn render_current_screen(frame: &mut ratatui::Frame<'_>, app: &App<'_>) {
    render_screen_body(frame, app);
    if let Some(modal) = &app.modal {
        render_modal_overlay(frame, modal, app.modal_scroll_offset);
    }
}

fn render_modal_overlay(frame: &mut ratatui::Frame<'_>, modal: &ModalKind, scroll_offset: u16) {
    match modal {
        ModalKind::Confirm {
            title,
            message,
            yes_label,
            no_label,
            yes_selected,
            hint,
        } => {
            let data = view::ModalConfirmScreenData {
                title,
                message,
                yes_label,
                no_label,
                yes_selected: *yes_selected,
                hint,
                scroll_offset,
            };
            view::render_modal_confirm(frame, &data);
        }
        ModalKind::Error {
            title,
            message,
            hint,
        } => {
            let data = view::ModalErrorScreenData {
                title,
                message,
                hint,
                scroll_offset,
            };
            view::render_modal_error(frame, &data);
        }
        ModalKind::Buttons {
            title,
            message,
            labels,
            selected,
            hint,
        } => {
            // ModalButtonsScreenData borrows &[&str]; rebuild a slice
            // of borrowed views into the owned `labels` Vec.
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let data = view::ModalButtonsScreenData {
                title,
                message,
                labels: &label_refs,
                selected: *selected,
                hint,
                scroll_offset,
            };
            view::render_modal_buttons(frame, &data);
        }
        ModalKind::Status {
            phase,
            log_lines,
            spinner_frame,
        } => {
            let data = BootStatusData {
                phase: std::borrow::Cow::Borrowed(phase),
                // Clone is unavoidable: BootStatusData wants Vec<String>
                // and the renderer iterates over the slice. The status
                // overlay only paints a handful of log lines so the
                // clone is cheap.
                log_lines: log_lines.clone(),
                spinner_frame: *spinner_frame,
            };
            view::render_boot_status(frame, &data);
        }
    }
}

fn render_screen_body(frame: &mut ratatui::Frame<'_>, app: &App<'_>) {
    match &app.screen {
        Screen::List => render_list(frame, &list_data(app)),
        Screen::Log { lines, offset } => render_log(frame, frame.area(), lines, *offset),
        Screen::Editing {
            generation_index,
            line,
        } => {
            if let Some(g) = app.generations.get(*generation_index) {
                let data = EditScreenData {
                    generation: g,
                    edited_cmdline: line.text(),
                    cursor_position: line.cursor(),
                };
                render_edit(frame, &data);
            }
        }
        Screen::Passphrase {
            prompt_label,
            buffer,
            cursor,
            verifying,
            spinner_frame,
        } => {
            let data = PassphraseScreenData {
                prompt_label,
                buffer_len: buffer.len(),
                // Convert the real byte cursor into a char-column count
                // so the masked caret lands at the right dot even when
                // the secret holds multi-byte chars.
                cursor_column: view::char_column_for_byte_cursor(buffer, *cursor),
                verifying: *verifying,
                spinner_frame: *spinner_frame,
                caps_lock_on: app.caps_lock_warning,
            };
            render_passphrase(frame, &data);
        }
        Screen::Emergency {
            message,
            items,
            selected,
            ..
        } => {
            let data = EmergencyScreenData {
                message,
                items,
                selected_index: *selected,
                countdown_remaining_secs: app.countdown_remaining_secs,
            };
            render_emergency(frame, &data);
        }
        Screen::BootStatus(data) => render_boot_status(frame, data),
        Screen::KeyEcho { events, byte_log } => {
            // VecDeque is not necessarily contiguous, so flatten through
            // make_contiguous-free iteration: collect via a slice pair.
            // Cheap because we only ever store ≤20 entries per panel.
            let events_vec: Vec<String> = events.iter().cloned().collect();
            let bytes_vec: Vec<String> = byte_log.iter().cloned().collect();
            let data = KeyEchoScreenData {
                events: &events_vec,
                byte_log: &bytes_vec,
            };
            render_key_echo(frame, &data);
        }
    }
}

fn list_data<'a>(app: &'a App<'a>) -> ListScreenData<'a> {
    ListScreenData {
        generations: app.generations,
        selected_index: app.selected_index,
        countdown_remaining_secs: app.countdown_remaining_secs,
        show_kernel_params: app.show_kernel_params,
    }
}
