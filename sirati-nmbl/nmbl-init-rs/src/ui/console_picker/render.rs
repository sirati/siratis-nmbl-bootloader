use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph};

use super::types::{ButtonCursor, CustomValidation, FocusZone, PickerState};

/// Pure render function — exported `pub(crate)` so the renderer can be
/// exercised by unit tests with a `TestBackend`.
pub(crate) fn render_picker_frame(frame: &mut Frame<'_>, state: &PickerState) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(area);

    // Header.
    let header_para = Paragraph::new(Line::from(vec![Span::styled(
        "Spawn shell on:",
        Style::default().add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Left);
    frame.render_widget(header_para, header);

    // Centred modal over the body so the dialog reads as a focus
    // shift rather than a full-screen replacement.
    let modal = centered_rect(body, 64, body.height.saturating_div(2).max(12));
    frame.render_widget(Clear, modal);

    // Layout inside the modal: candidate list + custom-input + buttons.
    let list_height = u16::try_from(state.candidates.len().saturating_add(2)).unwrap_or(u16::MAX);
    let [list_area, custom_area, button_area] = Layout::vertical([
        Constraint::Length(list_height),
        // 4 rows: 2 borders + the input line + the caret line.
        Constraint::Length(4),
        Constraint::Length(3),
    ])
    .areas::<3>(modal);

    let items: Vec<ListItem<'_>> = state
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let on = state.selected.get(i).copied().unwrap_or(false);
            let inner = if on { 'x' } else { ' ' };
            ListItem::new(Line::from(format!(
                "[{inner}]  {label}  {suffix}",
                label = c.label,
                suffix = c.origin.label_suffix(),
            )))
        })
        .collect();

    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let list = List::new(items)
        .block(Block::bordered().title("targets"))
        .highlight_style(highlight)
        .highlight_symbol("> ");

    let mut list_state = ListState::default();
    if !state.candidates.is_empty() && state.focus() == FocusZone::List {
        let last_idx = state.candidates.len().saturating_sub(1);
        list_state.select(Some(state.cursor.min(last_idx)));
    }
    frame.render_stateful_widget(list, list_area, &mut list_state);

    // Custom-input field. Colour-coded by validation verdict.
    render_custom_input(frame, custom_area, state);

    // Buttons row: [Spawn] [Cancel].
    render_buttons(frame, button_area, state);

    let footer_text = "up/down move  Space toggle  Tab check custom  Enter confirm  Esc cancel";
    frame.render_widget(
        Paragraph::new(footer_text).alignment(Alignment::Left),
        footer,
    );
}

/// Render the [Spawn] and [Cancel] buttons into `area`. Extracted from
/// [`render_picker_frame`] to keep that function within the line limit.
fn render_buttons(frame: &mut Frame<'_>, area: Rect, state: &PickerState) {
    let [spawn_area, cancel_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas::<2>(area);

    let on_buttons = state.focus() == FocusZone::Buttons;
    let spawn_focused = on_buttons && state.button_cursor == ButtonCursor::Spawn;
    let cancel_focused = on_buttons && state.button_cursor == ButtonCursor::Cancel;

    let spawn_disabled = state.nothing_selected();
    let spawn_label = if spawn_disabled {
        "[Spawn (no target)]"
    } else {
        "[Spawn]"
    };
    // Disabled wins over focused: when the operator has no target the
    // button is dim even if cursor sits on it, mirroring the pp-spinner
    // / empty-pw-block pattern in `render_passphrase`.
    let spawn_style = if spawn_disabled {
        Style::default().add_modifier(Modifier::DIM)
    } else if spawn_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let spawn_para = Paragraph::new(Span::styled(spawn_label, spawn_style))
        .alignment(Alignment::Center)
        .block(Block::bordered());
    frame.render_widget(spawn_para, spawn_area);

    let cancel_style = if cancel_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let cancel_para = Paragraph::new(Span::styled("[Cancel]", cancel_style))
        .alignment(Alignment::Center)
        .block(Block::bordered());
    frame.render_widget(cancel_para, cancel_area);
}

/// Render the single-line custom-path input plus a validation glyph.
/// Splits out from [`render_picker_frame`] so the colour-coding logic
/// stays readable.
fn render_custom_input(frame: &mut Frame<'_>, area: Rect, state: &PickerState) {
    let validation = state.custom_validation();
    let focused = state.focus() == FocusZone::CustomInput;
    let (text_style, marker, marker_style) = match validation {
        CustomValidation::Empty => (
            Style::default().add_modifier(Modifier::DIM),
            " ".to_string(),
            Style::default().add_modifier(Modifier::DIM),
        ),
        CustomValidation::Valid => (
            Style::default().fg(Color::Green),
            if state.custom_checked {
                "[x]".to_string()
            } else {
                "[ ]".to_string()
            },
            Style::default().fg(Color::Green),
        ),
        CustomValidation::Invalid => (
            Style::default().fg(Color::Red),
            "[!]".to_string(),
            Style::default().fg(Color::Red),
        ),
    };
    let title = if focused {
        "custom (typing)"
    } else {
        "custom (/dev/X)"
    };
    // Width of the "[x] " (marker + separating space) prefix in display
    // columns, so the caret line below lines up under the input text.
    let marker_cols = marker.chars().count().saturating_add(1);
    let text_line = Line::from(vec![
        Span::styled(marker, marker_style),
        Span::raw(" "),
        Span::styled(state.custom_input.clone(), text_style),
    ]);
    let block_style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let block = Block::bordered().title(Span::styled(title, block_style));
    // When focused, draw a caret on a second line under the cursor
    // column (byte→char column conversion mirrors the cmdline editor's
    // `render_edit`), offset by the marker prefix. Unfocused fields
    // omit the caret to avoid implying input focus.
    let lines = if focused {
        // Reuse the cmdline editor's caret helper; the marker prefix is
        // passed as the lead-in column offset so the caret aligns under
        // the input text rather than the "[x] " marker.
        let caret =
            crate::ui::view::caret_line(&state.custom_input, state.custom_cursor, marker_cols);
        vec![text_line, caret]
    } else {
        vec![text_line]
    };
    let para = Paragraph::new(lines).block(block);
    frame.render_widget(para, area);
}

/// Centre a width×height rect inside `area`. Mirrors the same helper
/// in `view.rs`; duplicated here to keep this module self-contained
/// (the picker doesn't otherwise touch `view`).
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
}
