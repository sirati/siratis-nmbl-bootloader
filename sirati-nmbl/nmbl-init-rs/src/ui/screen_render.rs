//! Screen dispatch and modal-overlay render helpers.
//!
//! `render_current_screen` is the single entry point: it dispatches to
//! the right per-screen renderer then overlays any active modal on top.
//! `pub(crate)` so the splash orchestrator, the tty backend, and the
//! mocking backend can all reuse the same dispatch without forking the
//! per-screen branching.

use crate::ui::app::{App, BootStatusData, ModalKind, SPINNER_FRAMES, SPINNER_GLYPHS, Screen};
use crate::ui::event_tick;
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
    // Always-on diagnostic spinner. Painted LAST so it overlays whatever
    // any screen or modal drew in the very corner. Its frame is driven by
    // the global event-loop tick, so it spins iff the loop is iterating.
    render_diagnostic_spinner(frame, event_tick::current());
}

/// Glyph the diagnostic spinner shows for a given event-loop `tick`.
/// Pure (tick in → glyph out) so the tick→glyph contract is unit
/// testable without a live frame buffer.
pub(crate) fn diagnostic_spinner_glyph(tick: u64) -> char {
    let idx = event_tick::frame_index(tick, SPINNER_FRAMES);
    SPINNER_GLYPHS.get(idx).copied().unwrap_or('|')
}

/// Overlay the diagnostic spinner into the top-most row, right-most cell.
///
/// Writes the glyph straight into the frame's cell buffer (rather than a
/// widget) so it composes on top of any content already there — including
/// modals that `Clear` their own rect — and never gets punched out. One
/// cell only: unobtrusive, and placed in the literal corner so it clobbers
/// at most one column of whatever screen owns that corner.
fn render_diagnostic_spinner(frame: &mut ratatui::Frame<'_>, tick: u64) {
    let area = frame.area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let x = area.right().saturating_sub(1);
    let y = area.y;
    let glyph = diagnostic_spinner_glyph(tick);
    let buf = frame.buffer_mut();
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(glyph);
        cell.set_style(
            ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM),
        );
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
        Screen::Log {
            lines,
            offset,
            follow_bottom,
            source,
        } => render_log(frame, frame.area(), lines, offset, follow_bottom, *source),
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
            select_generation,
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
                select_generation: *select_generation,
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::{diagnostic_spinner_glyph, render_current_screen};
    use crate::ui::app::{App, SPINNER_FRAMES, SPINNER_GLYPHS};
    use crate::ui::event_tick;

    /// Read the cell at the top-right corner of a freshly-drawn frame.
    fn corner_glyph(width: u16, height: u16, app: &App<'_>) -> String {
        let mut term = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        term.draw(|f| render_current_screen(f, app)).expect("draw");
        let buf = term.backend().buffer();
        let x = width - 1;
        buf.cell((x, 0)).expect("corner cell").symbol().to_owned()
    }

    #[test]
    fn diagnostic_glyph_cycles_through_all_frames() {
        // tick i → glyph i for a full rotor cycle.
        for (i, expected) in SPINNER_GLYPHS.iter().enumerate() {
            assert_eq!(
                diagnostic_spinner_glyph(i as u64),
                *expected,
                "tick {i} must select glyph index {i}"
            );
        }
        // And wraps after a full cycle.
        assert_eq!(
            diagnostic_spinner_glyph(u64::from(SPINNER_FRAMES)),
            SPINNER_GLYPHS[0],
            "one full cycle wraps back to frame 0"
        );
    }

    #[test]
    fn spinner_overlays_top_right_corner_over_screen_content() {
        // The List screen draws a bordered block whose top-right corner is
        // a box-drawing junction. The diagnostic spinner is painted LAST,
        // so that corner cell must instead show the current rotor glyph —
        // proving the overlay composes on top of underlying content.
        let gens = [];
        let app = App::new(&gens);
        let expected = diagnostic_spinner_glyph(event_tick::current());
        let got = corner_glyph(80, 24, &app);
        assert_eq!(
            got,
            expected.to_string(),
            "top-right corner must show the diagnostic spinner glyph, not screen content"
        );
        assert_ne!(got, "┐", "spinner must overlay the box-corner junction");
    }

    #[test]
    fn spinner_glyph_advances_when_the_tick_advances() {
        // Bumping the global tick (as the event loop does each poll cycle)
        // must change the rendered corner glyph once per SPINNER_FRAMES-1
        // steps — concretely, the glyph after a single tick differs from
        // before it (the 4-frame rotor has no adjacent repeats).
        let gens = [];
        let app = App::new(&gens);
        let before = corner_glyph(80, 24, &app);
        event_tick::tick();
        let after = corner_glyph(80, 24, &app);
        assert_ne!(
            before, after,
            "advancing the event-loop tick must advance the spinner glyph"
        );
    }
}
