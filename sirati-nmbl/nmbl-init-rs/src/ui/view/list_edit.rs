//! Render functions for list, edit, emergency, boot-status, log and key-echo screens.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Wrap};

use crate::generations::Generation;
use crate::ui::app::{BootStatusData, SPINNER_FRAMES, SPINNER_GLYPHS};

use super::{
    EditScreenData, EmergencyScreenData, KeyEchoScreenData, ListScreenData, caret_line,
    render_footer, render_header, split_chrome,
};

fn generation_item<'a>(
    g: &'a Generation,
    show_kernel_params: bool,
    body_width: u16,
) -> ListItem<'a> {
    let head = if g.label.is_empty() {
        format!("#{}", g.number)
    } else {
        format!("#{}  {}", g.number, g.label)
    };
    if !show_kernel_params || g.kernel_params.is_empty() {
        return ListItem::new(Line::from(head));
    }
    // Compose head + (right-aligned) kernel params on a single line.
    // Reserved chrome per row: 1 col border, 2 cols highlight symbol,
    // 1 col gutter, 1 col border = 5. The list widget already accounts
    // for the borders, so we subtract only the highlight symbol gutter.
    let avail = body_width.saturating_sub(2) as usize;
    let head_cols = head.chars().count();
    let kp = g.kernel_params.join(" ");
    let max_kp = avail.saturating_sub(head_cols).saturating_sub(2);
    if max_kp == 0 {
        return ListItem::new(Line::from(head));
    }
    let kp_truncated: String = if kp.chars().count() > max_kp {
        let take = max_kp.saturating_sub(1);
        kp.chars().take(take).chain(std::iter::once('…')).collect()
    } else {
        kp
    };
    let kp_cols = kp_truncated.chars().count();
    let pad = avail.saturating_sub(head_cols).saturating_sub(kp_cols);
    let line = Line::from(vec![
        Span::raw(head),
        Span::raw(" ".repeat(pad)),
        Span::styled(kp_truncated, Style::default().add_modifier(Modifier::DIM)),
    ]);
    ListItem::new(line)
}

/// Render the generation-picker screen.
pub fn render_list(frame: &mut Frame<'_>, data: &ListScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, data.countdown_remaining_secs);
    // Bordered block + highlight symbol consume 1 + 1 + 2 = 4 cols.
    let inner_width = body.width.saturating_sub(4);
    let items: Vec<ListItem<'_>> = data
        .generations
        .iter()
        .map(|g| generation_item(g, data.show_kernel_params, inner_width))
        .collect();
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let list = List::new(items)
        .block(Block::bordered().title("Generations"))
        .highlight_style(highlight)
        .highlight_symbol("> ");
    // Clamp selection so an out-of-range value snaps to the last row.
    let max_idx = data.generations.len().saturating_sub(1);
    let mut state = ListState::default();
    if !data.generations.is_empty() {
        state.select(Some(data.selected_index.min(max_idx)));
    }
    frame.render_stateful_widget(list, body, &mut state);
    let hint = "up/down select  Enter boot  e edit  p toggle params  s shell  q reboot";
    render_footer(frame, footer, hint);
}

/// Render the cmdline-editor screen.
pub fn render_edit(frame: &mut Frame<'_>, data: &EditScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);
    // `cursor_position` is a BYTE index (the `EditableLine` cursor).
    // `caret_line` converts it to a CHAR column so multi-byte text
    // (e.g. "héllo") doesn't shove the caret one cell too far right.
    // No prefix here: the text starts at column 0 inside the block.
    let caret = caret_line(data.edited_cmdline, data.cursor_position, 0);
    let title = format!("Edit cmdline — generation #{}", data.generation.number);
    let text = Text::from(vec![Line::raw(data.edited_cmdline.to_owned()), caret]);
    let para = Paragraph::new(text)
        .block(Block::bordered().title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, body);
    render_footer(frame, footer, "Enter=apply  Esc=cancel");
}

/// Render the emergency screen: a red "boot failed" header, a wrapped
/// error message, and a list of [Reboot]/[Shell] items with selection
/// highlight.
///
/// All chrome and colour is ratatui — the splash backend is purely a
/// render target. This keeps every UI decision (layout, wrap, colour,
/// hotkey hint) in one place.
pub fn render_emergency(frame: &mut Frame<'_>, data: &EmergencyScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());

    // Header: red bold "boot failed". Plus optional countdown.
    let mut header_spans = vec![Span::styled(
        "boot failed",
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    )];
    if let Some(secs) = data.countdown_remaining_secs {
        header_spans.push(Span::raw("   "));
        header_spans.push(Span::styled(
            format!("auto-reboot in {secs}s"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let header_para = Paragraph::new(Line::from(header_spans)).alignment(Alignment::Left);
    frame.render_widget(header_para, header);

    // Split the body horizontally: top area for the wrapped error
    // message, bottom area (sized to the item list) for the picker.
    let item_rows = u16::try_from(data.items.len().saturating_add(2)).unwrap_or(u16::MAX);
    let [msg_area, list_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(item_rows)]).areas::<2>(body);

    let msg_para = Paragraph::new(Text::from(data.message.to_owned()))
        .block(Block::bordered().title("error"))
        .wrap(Wrap { trim: false });
    frame.render_widget(msg_para, msg_area);

    // Picker. Build ListItems with bracketed labels so the operator
    // immediately sees "[Reboot]" / "[Shell]" — the brackets reinforce
    // that this is a discrete choice, not a free-form prompt.
    let items: Vec<ListItem<'_>> = data
        .items
        .iter()
        .map(|item| ListItem::new(Line::from(format!("[{}]", item.label))))
        .collect();
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let list = List::new(items)
        .block(Block::bordered().title("action"))
        .highlight_style(highlight)
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !data.items.is_empty() {
        let last_idx = data.items.len().saturating_sub(1);
        state.select(Some(data.selected_index.min(last_idx)));
    }
    frame.render_stateful_widget(list, list_area, &mut state);

    render_footer(
        frame,
        footer,
        "up/down select  Enter confirm  r reboot  s shell  Ctrl+L logs",
    );
}

/// Render the early-boot status screen: project header, scrolling log
/// panel, and a single status line with an animated spinner glyph.
///
/// Layout (top to bottom):
///   1. Project header line (same style as the selector header).
///   2. Bordered "log" panel showing the most recent log lines (most
///      recent at the bottom). Lines exceeding the panel are clipped.
///   3. Status line: " {spinner} {phase}".
///
/// The spinner uses a 4-frame ASCII rotor (`|/-\`) rather than the
/// 10-frame braille sequence systemd uses, because the splash glyph
/// cache only pre-rasterises ASCII printable plus a small box-drawing
/// subset (see `src/splash/glyph_cache.rs`). Braille (U+2800 block) is
/// not cached and would render as blank cells on the framebuffer
/// backend. ASCII works on both crossterm and splash, so we trade
/// fidelity for portability.
pub fn render_boot_status(frame: &mut Frame<'_>, data: &BootStatusData<'_>) {
    // Reuse the chrome split so the project header style matches the
    // selector exactly. The footer slot is repurposed for the status
    // line — same height (1 row), same alignment surface.
    let [header, body, status] = split_chrome(frame.area());
    render_header(frame, header, None);

    // Log panel. The bordered block subtracts 2 rows of chrome
    // (top + bottom border). We pre-clip the source line list to
    // `inner_rows` to bound copy work; ratatui still does the final
    // visible-row clipping when a long line wraps under
    // `Wrap { trim: false }`. We intentionally don't pay for a
    // unicode-width-aware wrap count here — operator log lines are
    // typically one row each, and the paragraph widget handles the
    // overflow case correctly.
    let log_block = Block::bordered().title("log");
    let inner = log_block.inner(body);
    let inner_rows = inner.height as usize;

    let start = data.log_lines.len().saturating_sub(inner_rows);
    let visible_lines: Vec<Line<'_>> = data
        .log_lines
        .get(start..)
        .unwrap_or(&[])
        .iter()
        .map(|s| Line::raw(s.clone()))
        .collect();

    let log_para = Paragraph::new(Text::from(visible_lines))
        .block(log_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(log_para, body);

    // Bottom-of-box hint. Paint a dim "Esc to abort" indicator on the
    // last inner row of the log panel — same placement pattern as
    // pretty_shell's "Ctrl+Shift+Up/Dn scroll" hint. The hint is
    // unconditional so the operator always knows the escape route is
    // available; render only when the box has at least one inner row.
    if inner.height > 0 {
        let hint_row = inner.y.saturating_add(inner.height.saturating_sub(1));
        let hint_rect = Rect::new(inner.x, hint_row, inner.width, 1);
        let hint = Paragraph::new(Span::styled(
            "Ctrl+L logs  Esc abort",
            Style::default().add_modifier(Modifier::DIM),
        ))
        .alignment(Alignment::Right);
        frame.render_widget(hint, hint_rect);
    }

    // Status line. SPINNER_FRAMES is non-zero (it's a const = 4), but
    // we still defend against a degenerate config: an empty glyph
    // array would underflow the modulo. `get` returns `None` for the
    // pathological case and we fall back to a space — never panic.
    let idx = (data.spinner_frame % SPINNER_FRAMES) as usize;
    let glyph = SPINNER_GLYPHS.get(idx).copied().unwrap_or(' ');
    let status_line = format!(" {glyph} {phase}", phase = data.phase);
    let status_para = Paragraph::new(status_line).alignment(Alignment::Left);
    frame.render_widget(status_para, status);
}

/// Render the full boot-transcript log viewer ([`crate::ui::app::Screen::Log`]).
///
/// `lines` is the snapshot (oldest first) and `offset` is the operator's
/// scroll position from the top; it is clamped here to
/// `total - visible_rows` so an over-scroll (e.g. `End` setting
/// `u16::MAX`) lands on the last full page rather than off the end.
/// A bordered "boot log" block fills `area` above a single dim footer
/// hint row.
pub fn render_log(frame: &mut Frame<'_>, area: Rect, lines: &[String], offset: u16) {
    // Reserve the bottom row for the footer hint; the rest is the box.
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas::<2>(area);

    let block = Block::bordered().title("boot log");
    let inner = block.inner(body);
    let visible = inner.height;
    let total = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let max_off = total.saturating_sub(visible);
    let clamped = offset.min(max_off);

    let start = clamped as usize;
    let end = start.saturating_add(visible as usize).min(lines.len());
    let visible_lines: Vec<Line<'_>> = lines
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .map(|s| Line::raw(s.clone()))
        .collect();

    let para = Paragraph::new(Text::from(visible_lines))
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, body);

    let hint = Paragraph::new(Span::styled(
        "\u{2191}/\u{2193} PgUp/PgDn  Esc/Ctrl+L close",
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Right);
    frame.render_widget(hint, footer);
}

/// Render the key-echo diagnostic screen: header, two side-by-side
/// ring-buffer panels (parsed events on the left, raw bytes on the
/// right), and a single status hint at the bottom.
pub fn render_key_echo(frame: &mut Frame<'_>, data: &KeyEchoScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .areas::<2>(body);

    let events_block = Block::bordered().title("KeyEvents");
    let bytes_block = Block::bordered().title("Raw bytes");
    let events_inner_rows = events_block.inner(left).height as usize;
    let bytes_inner_rows = bytes_block.inner(right).height as usize;

    let events_start = data.events.len().saturating_sub(events_inner_rows);
    let events_visible: Vec<Line<'_>> = data
        .events
        .get(events_start..)
        .unwrap_or(&[])
        .iter()
        .map(|s| Line::raw(s.clone()))
        .collect();
    let events_para = Paragraph::new(Text::from(events_visible))
        .block(events_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(events_para, left);

    let bytes_start = data.byte_log.len().saturating_sub(bytes_inner_rows);
    let bytes_visible: Vec<Line<'_>> = data
        .byte_log
        .get(bytes_start..)
        .unwrap_or(&[])
        .iter()
        .map(|s| Line::raw(s.clone()))
        .collect();
    let bytes_para = Paragraph::new(Text::from(bytes_visible))
        .block(bytes_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(bytes_para, right);

    render_footer(
        frame,
        footer,
        "key-echo test - Ctrl+C to exit (would be quit if not test)",
    );
}
