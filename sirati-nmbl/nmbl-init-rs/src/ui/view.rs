//! Pure render functions for the NMBL TUI. State and event handling live
//! in the sibling `app` module; this file only knows how to paint frames.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::generations::Generation;
use crate::ui::app::{BootStatusData, EmergencyItem, SPINNER_FRAMES, SPINNER_GLYPHS};
use crate::ui::modal_layout::{
    ModalLayout, SCROLL_HINT, compute_modal_layout_with_button_width,
};

/// Char-width of the rendered button row: sum of `[Label]` cells plus
/// the 2-col gutters between buttons. Used as a width floor by the
/// layout pass so a short message can't shrink the box past where the
/// buttons fit.
fn button_row_width(labels: &[&str]) -> u16 {
    if labels.is_empty() {
        return 0;
    }
    let mut total: usize = 0;
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            total = total.saturating_add(2);
        }
        // "[<label>]" is label_chars + 2 brackets.
        total = total
            .saturating_add(label.chars().count())
            .saturating_add(2);
    }
    u16::try_from(total).unwrap_or(u16::MAX)
}

/// State needed to render the generation-picker screen.
pub struct ListScreenData<'a> {
    pub generations: &'a [Generation],
    pub selected_index: usize,
    /// `Some(n)` while auto-booting; `None` once cancelled by a keypress.
    pub countdown_remaining_secs: Option<u64>,
    pub show_kernel_params: bool,
}

/// State needed to render the cmdline-editor screen.
pub struct EditScreenData<'a> {
    pub generation: &'a Generation,
    pub edited_cmdline: &'a str,
    pub cursor_position: usize,
}

/// State for the passphrase modal. Only the buffer length crosses this
/// boundary — the secret stays in App's zeroizing storage.
pub struct PassphraseScreenData<'a> {
    pub prompt_label: &'a str,
    pub buffer_len: usize,
    /// Cursor position as a CHAR column within the masked input (0-based,
    /// `0..=buffer_len`). The caret `|` is painted after this many dots
    /// so the operator sees where their next keystroke lands even though
    /// the characters themselves are masked.
    pub cursor_column: usize,
    /// `true` while the activation runner is verifying the passphrase
    /// (cryptsetup running). The renderer overlays a spinner so the
    /// operator sees the boot is alive rather than hung.
    pub verifying: bool,
    /// Spinner phase; indexes [`SPINNER_GLYPHS`] modulo [`SPINNER_FRAMES`].
    /// Only meaningful when `verifying = true`; ignored otherwise.
    pub spinner_frame: u8,
    /// `true` when Caps Lock is engaged on the input keyboard. Drives a
    /// warning rendered into a permanently-reserved row so the modal
    /// geometry is identical whether the warning shows or not.
    pub caps_lock_on: bool,
}

/// State needed to render the emergency-on-boot-failure screen.
pub struct EmergencyScreenData<'a> {
    /// Pre-formatted error chain (line-wrapped by ratatui).
    pub message: &'a str,
    pub items: &'a [EmergencyItem],
    /// Index into `items`; rendered clamped to `items.len() - 1`.
    pub selected_index: usize,
    /// `Some(n)` while the auto-reboot countdown is still running.
    pub countdown_remaining_secs: Option<u64>,
}

/// Split frame into (header, body, footer). Small frames degrade gracefully.
fn split_chrome(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(area)
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
}

/// Paint the wrapped text region of a modal at `layout.inner_text_rect`.
/// When `layout.scrollable` is true the offset selects which slice of
/// `layout.wrapped_lines` is visible; otherwise the slice starts at 0.
fn paint_modal_text(frame: &mut Frame<'_>, layout: &ModalLayout, scroll_offset: u16) {
    let total = u16::try_from(layout.wrapped_lines.len()).unwrap_or(u16::MAX);
    let visible = layout.inner_text_rect.height;
    let offset = if layout.scrollable {
        let max_off = total.saturating_sub(visible);
        scroll_offset.min(max_off)
    } else {
        0
    };
    let start = offset as usize;
    let end = start.saturating_add(visible as usize).min(layout.wrapped_lines.len());
    let lines: Vec<Line<'_>> = layout
        .wrapped_lines
        .get(start..end)
        .unwrap_or(&[])
        .iter()
        .map(|s| Line::raw(s.clone()))
        .collect();
    let para = Paragraph::new(Text::from(lines));
    frame.render_widget(para, layout.inner_text_rect);
}

/// Paint the `- - -` separator row across the inner width.
fn paint_separator(frame: &mut Frame<'_>, layout: &ModalLayout) {
    let inner_w = layout.inner_text_rect.width as usize;
    // Each "dash space" pair takes 2 cols. Final col can be either a
    // dash or a space, whichever fills the row.
    let mut sep = String::with_capacity(inner_w);
    let mut want_dash = true;
    for _ in 0..inner_w {
        sep.push(if want_dash { '-' } else { ' ' });
        want_dash = !want_dash;
    }
    let sep_rect = Rect::new(
        layout.inner_text_rect.x,
        layout.separator_y,
        layout.inner_text_rect.width,
        1,
    );
    let para = Paragraph::new(sep)
        .alignment(Alignment::Center)
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(para, sep_rect);
}

/// Paint the right-aligned scroll hint below the box when stage H4
/// triggered. No-op when the layout fits without scrolling.
fn paint_scroll_hint(frame: &mut Frame<'_>, layout: &ModalLayout) {
    let Some(rect) = layout.scroll_hint else {
        return;
    };
    let hint = Paragraph::new(Span::styled(
        SCROLL_HINT,
        Style::default().add_modifier(Modifier::DIM),
    ))
    .alignment(Alignment::Right);
    frame.render_widget(hint, rect);
}

fn render_header(frame: &mut Frame<'_>, area: Rect, countdown: Option<u64>) {
    let mut spans = vec![
        Span::styled("sirati's NMBL ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("— bootloader"),
    ];
    if let Some(secs) = countdown {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("auto-boot in {secs}s"),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    let p = Paragraph::new(Line::from(spans)).alignment(Alignment::Left);
    frame.render_widget(p, area);
}

/// Common footer line used on every screen.
pub fn render_footer(frame: &mut Frame<'_>, area: Rect, hint: &str) {
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), area);
}

/// Convert a byte index into `s` into a char-column count suitable for
/// caret positioning. Clamps to the end of `s`, then walks back to the
/// nearest char boundary so a stale cursor mid-codepoint doesn't panic
/// when sliced.
pub fn char_column_for_byte_cursor(s: &str, byte_idx: usize) -> usize {
    let clamped = byte_idx.min(s.len());
    // Walk back to the nearest char boundary. Index 0 is always a
    // boundary, so `(0..=clamped).rev().find(..)` is never empty —
    // but pattern-match instead of unwrap to keep the code total.
    let Some(safe) = (0..=clamped).rev().find(|&i| s.is_char_boundary(i)) else {
        return 0;
    };
    s.get(..safe).map_or(0, |prefix| prefix.chars().count())
}

fn generation_item<'a>(g: &'a Generation, show_kernel_params: bool, body_width: u16) -> ListItem<'a> {
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
    // Convert to a CHAR-column count so multi-byte text (e.g. "héllo")
    // doesn't shove the caret one cell too far to the right.
    let offset = char_column_for_byte_cursor(data.edited_cmdline, data.cursor_position);
    let caret = format!("{}{}", " ".repeat(offset), "^");
    let title = format!("Edit cmdline — generation #{}", data.generation.number);
    let text = Text::from(vec![
        Line::raw(data.edited_cmdline.to_owned()),
        Line::styled(caret, Style::default().add_modifier(Modifier::BOLD)),
    ]);
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
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD),
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
        "up/down select  Enter confirm  r reboot  s shell",
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
            "Esc to abort",
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
    let end = start
        .saturating_add(visible as usize)
        .min(lines.len());
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

/// State needed to render the [`Screen::KeyEcho`] diagnostic view.
///
/// Both ring buffers are caller-owned (`App` holds the
/// [`std::collections::VecDeque`]s); we only borrow slices to avoid
/// cloning every frame. Most recent entries are at the back of each
/// slice and end up at the bottom of their panel after rendering.
pub struct KeyEchoScreenData<'a> {
    pub events: &'a [String],
    pub byte_log: &'a [String],
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

/// State needed to render a yes/no confirmation modal (used by the
/// `[Verify kexec readiness]` emergency action to confirm "found N
/// generations, boot one?" before handing off to the selector).
///
/// Two-button modal: the highlighted button is whatever
/// `yes_selected == true` implies. The renderer paints both buttons
/// bracketed; the driver loop in `crate::ui::mod::show_modal_confirm`
/// toggles `yes_selected` on left/right/tab and commits on Enter.
pub struct ModalConfirmScreenData<'a> {
    /// Short title shown on the modal's title bar.
    pub title: &'a str,
    /// Pre-formatted body text; rendered with `Wrap { trim: false }`.
    pub message: &'a str,
    /// Label for the affirmative button (typically "Yes" or "Boot").
    pub yes_label: &'a str,
    /// Label for the negative button (typically "No" or "Back").
    pub no_label: &'a str,
    /// `true` when the yes button is currently highlighted.
    pub yes_selected: bool,
    /// Footer hint, typically "←/→ select  Enter confirm  Esc cancel".
    pub hint: &'a str,
    /// Scroll viewport offset. Ignored when the layout decides the
    /// content fits without scrolling.
    pub scroll_offset: u16,
}

/// Render a centred yes/no confirmation modal over the body area. The
/// bordered modal carries the body in default colour on a fresh
/// `Clear` so the underlying emergency picker doesn't bleed through;
/// the two buttons are painted on the bottom row of the modal with
/// the selected one inverted.
///
/// Sizing goes through [`compute_modal_layout`] so every modal in the
/// crate shares the same "hug the text, degrade in steps" shape.
pub fn render_modal_confirm(frame: &mut Frame<'_>, data: &ModalConfirmScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);

    let labels = [data.yes_label, data.no_label];
    let btn_w = button_row_width(&labels);
    let layout = compute_modal_layout_with_button_width(data.message, true, 2, btn_w, body);
    frame.render_widget(Clear, layout.box_rect);
    let block = Block::bordered().title(data.title.to_owned());
    frame.render_widget(block, layout.box_rect);

    paint_modal_text(frame, &layout, data.scroll_offset);
    paint_separator(frame, &layout);

    // Button bar: "[Yes]  [Back]" with the highlighted one inverted.
    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let unselected_style = Style::default();
    let (yes_style, no_style) = if data.yes_selected {
        (selected_style, unselected_style)
    } else {
        (unselected_style, selected_style)
    };
    let yes_text = format!("[{}]", data.yes_label);
    let no_text = format!("[{}]", data.no_label);
    let line = Line::from(vec![
        Span::styled(yes_text, yes_style),
        Span::raw("  "),
        Span::styled(no_text, no_style),
    ]);
    let btn_rect = Rect::new(
        layout.inner_text_rect.x,
        layout.button_row_y,
        layout.inner_text_rect.width,
        1,
    );
    let buttons = Paragraph::new(line).alignment(Alignment::Right);
    frame.render_widget(buttons, btn_rect);

    paint_scroll_hint(frame, &layout);
    render_footer(frame, footer, data.hint);
}

/// State needed to render an N-button modal (used by the
/// wrong-password retry flow). The driver loop in
/// `crate::ui::mod::show_wrong_password_modal` paints every button
/// label in order and inverts whichever index `selected` points at.
pub struct ModalButtonsScreenData<'a> {
    /// Short title shown on the modal's title bar.
    pub title: &'a str,
    /// Pre-formatted body text; rendered with `Wrap { trim: false }`.
    pub message: &'a str,
    /// Bracketed button labels, painted left-to-right.
    pub labels: &'a [&'a str],
    /// Index in `labels` of the currently highlighted button; values
    /// out of range are clamped to the last legal index by the renderer.
    pub selected: usize,
    /// Footer hint, typically "Left/Right select  Enter confirm  Esc …".
    pub hint: &'a str,
    /// Scroll viewport offset. Ignored when the layout decides the
    /// content fits without scrolling.
    pub scroll_offset: u16,
}

/// Render a centred N-button modal over the body area. Mirrors the
/// layout of [`render_modal_confirm`]: bordered modal on a fresh
/// `Clear`, wrapped message above a button bar, footer hint underneath.
///
/// Sizing goes through [`compute_modal_layout`] so every modal in the
/// crate shares the same "hug the text, degrade in steps" shape.
pub fn render_modal_buttons(frame: &mut Frame<'_>, data: &ModalButtonsScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);

    let btn_count = u16::try_from(data.labels.len()).unwrap_or(u16::MAX);
    let btn_w = button_row_width(data.labels);
    let layout =
        compute_modal_layout_with_button_width(data.message, true, btn_count, btn_w, body);
    frame.render_widget(Clear, layout.box_rect);
    let block = Block::bordered().title(data.title.to_owned());
    frame.render_widget(block, layout.box_rect);

    paint_modal_text(frame, &layout, data.scroll_offset);
    paint_separator(frame, &layout);

    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let unselected_style = Style::default();
    let last_idx = data.labels.len().saturating_sub(1);
    let selected = data.selected.min(last_idx);
    let style_for = |i: usize| {
        if i == selected {
            selected_style
        } else {
            unselected_style
        }
    };
    let mut spans: Vec<Span<'_>> = Vec::with_capacity(data.labels.len().saturating_mul(2));
    for (i, label) in data.labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("[{label}]"), style_for(i)));
    }
    let btn_rect = Rect::new(
        layout.inner_text_rect.x,
        layout.button_row_y,
        layout.inner_text_rect.width,
        1,
    );
    let alignment = if data.labels.len() == 1 {
        Alignment::Center
    } else {
        Alignment::Right
    };
    let buttons = Paragraph::new(Line::from(spans)).alignment(alignment);
    frame.render_widget(buttons, btn_rect);

    paint_scroll_hint(frame, &layout);
    render_footer(frame, footer, data.hint);
}

/// State needed to render a transient modal-error dialog (used by the
/// pretty-shell path when openpty / fork / mount fails so the operator
/// sees what happened instead of a stale "boot failed" panel underneath).
pub struct ModalErrorScreenData<'a> {
    /// Short title shown on the modal's title bar.
    pub title: &'a str,
    /// Pre-formatted error chain. Rendered with `Wrap { trim: false }`.
    pub message: &'a str,
    /// Footer hint, typically "press any key to continue".
    pub hint: &'a str,
    /// Scroll viewport offset. Ignored when the layout decides the
    /// content fits without scrolling.
    pub scroll_offset: u16,
}

/// Render a centred modal dialog over the body area. The bordered
/// modal carries the error chain in red on a fresh `Clear` so the
/// stale emergency-screen content does not bleed through.
///
/// Sizing goes through [`compute_modal_layout`] so every modal in the
/// crate shares the same "hug the text, degrade in steps" shape. The
/// error modal has no buttons (any keystroke dismisses), so the
/// layout skips the separator + button rows.
pub fn render_modal_error(frame: &mut Frame<'_>, data: &ModalErrorScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);

    let layout = compute_modal_layout_with_button_width(data.message, false, 0, 0, body);
    frame.render_widget(Clear, layout.box_rect);
    let block = Block::bordered().title(Span::styled(
        data.title.to_owned(),
        Style::default()
            .fg(Color::Red)
            .add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(block, layout.box_rect);
    paint_modal_text(frame, &layout, data.scroll_offset);
    paint_scroll_hint(frame, &layout);

    render_footer(frame, footer, data.hint);
}

/// State needed to render the pretty-shell screen.
///
/// Owned by the [`crate::ui::pretty_shell::PtyShellState`] driver; the
/// renderer is a pure consumer of the snapshot. The grid is supplied
/// pre-flattened as `rows_text` so this file can stay independent of
/// `alacritty_terminal` (which is only compiled in when `pretty-shell`
/// is on, but this struct is unconditionally visible here so the
/// `view` module's tests don't fragment over feature flags).
pub struct PtyShellScreenData<'a> {
    /// Grid width in cells. Used to clamp / pad the rendered rows.
    pub cols: u16,
    /// Grid height in cells. Used for layout decisions only — the
    /// actual rendered height comes from `rows_text.len()`.
    pub rows: u16,
    /// One pre-built `String` per grid row, in row-major order. The
    /// renderer trusts the caller to have produced exactly `rows` of
    /// `cols` chars each; degraded inputs (short rows, missing rows)
    /// just render shorter lines without panicking.
    pub rows_text: &'a [String],
    /// `Grid::display_offset` — rows above the live tail currently
    /// visible. Zero means the live grid is shown.
    pub scroll_offset: usize,
}

/// Render the pretty-shell screen: header, bordered "Shell" box
/// containing the alacritty grid snapshot, and a footer showing the
/// combined exit + scroll hint.
///
/// The bordered block's inner area is given over entirely to the
/// alacritty terminal — no overlay text — so the operator sees an
/// unobstructed shell. All hints live in the outer footer row.
/// ASCII glyphs only — the splash glyph cache rasterises ASCII
/// printable plus a box-drawing subset (see
/// `src/splash/glyph_cache.rs`), so Unicode arrows (U+2191 / U+2193)
/// would render as blank cells on the framebuffer backend.
pub fn render_pty_shell(frame: &mut Frame<'_>, data: &PtyShellScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);

    let block = Block::bordered().title("Pretty Shell");
    let inner = block.inner(body);
    frame.render_widget(block, body);

    // Inner area drives the visible rows. We always paint from the
    // first cell of each grid row at the inner top-left; rows that
    // overflow the inner area are clipped by ratatui.
    let row_count = (inner.height as usize).min(data.rows_text.len());
    let col_count = inner.width as usize;
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(row_count);
    for row in data.rows_text.iter().take(row_count) {
        // Clamp to inner width so a stray wide row doesn't bleed into
        // the right border. `chars().take(n)` is char-correct.
        let truncated: String = row.chars().take(col_count).collect();
        lines.push(Line::raw(truncated));
    }
    let para = Paragraph::new(Text::from(lines));
    frame.render_widget(para, inner);

    // Footer hint covers both the exit shortcut and the scrollback
    // bindings; when the operator has scrolled back, prefix a "[scrolled
    // N]" tag so the indicator the inner overlay used to carry still
    // reaches them.
    let mut hint = String::new();
    if data.scroll_offset > 0 {
        hint.push_str(&format!("[scrolled {} lines]  ", data.scroll_offset));
    }
    hint.push_str(
        "exit shell or press Enter then ~. to return to emergency  \
         Ctrl+Shift+Up/Dn scroll",
    );
    render_footer(frame, footer, &hint);
}

/// Render the passphrase modal over the body area.
///
/// When `data.verifying` is `true` the modal grows by one row and the
/// extra row carries an ASCII spinner glyph plus a "verifying..." label,
/// so the operator sees the boot is alive while cryptsetup runs. We
/// pick the glyph from [`SPINNER_GLYPHS`] (same `|/-\\` rotor as the
/// boot-status spinner) for backend parity — splash glyph cache lacks
/// Unicode braille and would render blanks.
pub fn render_passphrase(frame: &mut Frame<'_>, data: &PassphraseScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);
    // Modal height is FIXED regardless of the Caps-Lock warning so the
    // box geometry never reflows when the operator toggles Caps Lock —
    // the warning row is always present in the layout (blank when off).
    // Verifying mode adds one row for the spinner line, as before.
    let modal_height: u16 = if data.verifying { 9 } else { 8 };
    let modal = centered_rect(body, 60, modal_height);
    frame.render_widget(Clear, modal);
    // Cap the visible mask so a huge typo doesn't overflow the box. The
    // caret `|` is drawn at the real cursor column (also capped) so the
    // operator sees where the next keystroke lands; the characters
    // themselves stay masked.
    let visible = data.buffer_len.min(40);
    let caret_at = data.cursor_column.min(visible);
    let before: String = "*".repeat(caret_at);
    let after: String = "*".repeat(visible.saturating_sub(caret_at));
    let mut lines: Vec<Line<'_>> = vec![
        Line::raw(data.prompt_label.to_owned()),
        Line::raw(String::new()),
        Line::from(vec![Span::raw(before), Span::raw("|"), Span::raw(after)]),
        // Permanently-reserved Caps-Lock warning row. Always emitted so
        // the box height is identical whether or not the warning shows;
        // blank when Caps Lock is off.
        if data.caps_lock_on {
            // ASCII only: the splash glyph cache rasterises ASCII
            // printable plus a box-drawing subset (see
            // `src/splash/glyph_cache.rs`); a Unicode warning sign
            // (U+26A0) would render as a blank cell on the framebuffer.
            Line::from(Span::styled(
                "! Caps Lock is ON",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::raw(String::new())
        },
    ];
    let hint_line: Line<'_> = if data.verifying {
        // Reuse the boot-status spinner glyphs (see crate::ui::app::
        // SPINNER_GLYPHS) so both screens animate identically.
        let glyph_idx = (data.spinner_frame % SPINNER_FRAMES) as usize;
        let glyph = SPINNER_GLYPHS.get(glyph_idx).copied().unwrap_or('|');
        lines.push(Line::from(vec![
            Span::styled(
                format!("{glyph} "),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "verifying passphrase...",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
        Line::raw("verifying...  please wait")
    } else {
        // Empty buffer → "Enter=submit" hint is rendered DIM so the
        // disabled state is visible; Enter is silently ignored in the
        // read loop.
        let submit_style = if data.buffer_len == 0 {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        };
        Line::from(vec![
            Span::styled("Enter=submit", submit_style),
            Span::raw("  Esc=cancel"),
        ])
    };
    let para = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title("Passphrase"))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, modal);
    frame.render_widget(
        Paragraph::new(hint_line).alignment(Alignment::Right),
        footer,
    );
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests"
)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn fake_gen(number: u32, label: &str, params: &[&str]) -> Generation {
        Generation {
            number,
            profile_link: PathBuf::from(format!("/p/system-{number}-link")),
            kernel: PathBuf::from("/p/kernel"),
            initrd: PathBuf::from("/p/initrd"),
            init_path: PathBuf::from(format!("/p/system-{number}-link/init")),
            kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
            label: label.to_string(),
        }
    }

    fn buffer_lines(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect()
    }

    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        buffer_lines(term).join("\n")
    }
    fn new_term(w: u16, h: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(w, h)).expect("terminal")
    }

    #[test]
    fn test_render_list_shows_generation_numbers() {
        let gens = vec![
            fake_gen(42, "nixos-25.05", &["root=/dev/sda1", "quiet"]),
            fake_gen(10, "nixos-24.11", &["console=ttyS0"]),
        ];
        // selected_index past the end exercises clamping; show_kernel_params
        // exercises the dim second-line branch.
        let data = ListScreenData {
            generations: &gens,
            selected_index: 99,
            countdown_remaining_secs: Some(3),
            show_kernel_params: true,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_list(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("#42"), "missing #42 in:\n{text}");
        assert!(text.contains("#10"), "missing #10 in:\n{text}");
        assert!(text.contains("auto-boot"), "missing countdown");
        assert!(text.contains("console=ttyS0"), "missing params line");
    }

    #[test]
    fn test_render_edit_shows_generation_number_and_cmdline() {
        let g = fake_gen(99, "", &[]);
        let data = EditScreenData {
            generation: &g,
            edited_cmdline: "init=/sbin/init quiet",
            cursor_position: 5,
        };
        let mut term = new_term(80, 10);
        term.draw(|f| render_edit(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("#99"), "missing gen number");
        assert!(text.contains("init=/sbin/init"), "missing cmdline");
        assert!(text.contains('^'), "missing caret indicator");
    }

    #[test]
    fn test_render_edit_caret_column_for_multibyte_cmdline() {
        // "héllo" is 6 bytes ('h' 'é'×2 'l' 'l' 'o'); cursor at byte
        // index 4 sits right after the 'l' that follows 'é', which
        // is char column 3. A naive `chars().take(cursor).count()`
        // would render the caret at column 4 — this test pins the
        // byte→char conversion.
        let g = fake_gen(7, "", &[]);
        let data = EditScreenData {
            generation: &g,
            edited_cmdline: "héllo",
            cursor_position: 4,
        };
        let mut term = new_term(40, 10);
        term.draw(|f| render_edit(f, &data)).expect("draw");
        let lines = buffer_lines(&term);

        // Find the caret row (the line that contains '^') and the
        // text row that precedes it.
        let caret_row = lines
            .iter()
            .position(|l| l.contains('^'))
            .expect("caret '^' must appear in rendered buffer");
        let caret_line = &lines[caret_row];
        // Count chars (not bytes) so multi-byte border glyphs like '│'
        // don't throw the column off.
        let caret_col = caret_line
            .chars()
            .position(|c| c == '^')
            .expect("'^' present");

        // The border draws on column 0, so the body starts at col 1.
        // Column-3 of the edited string is column-4 of the row.
        assert_eq!(
            caret_col,
            4,
            "caret rendered at row col {caret_col}, expected 4 (string col 3) \
             in rendered lines:\n{}",
            lines.join("\n")
        );
    }

    #[test]
    fn test_render_passphrase_dots_and_label() {
        let data = PassphraseScreenData {
            prompt_label: "Unlock /dev/sda2",
            buffer_len: 5,
            cursor_column: 5,
            caps_lock_on: false,
            verifying: false,
            spinner_frame: 0,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_passphrase(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("*****|"), "wrong mask count in:\n{text}");
        assert!(text.contains("Unlock /dev/sda2"));
        assert!(
            !text.contains("verifying"),
            "non-verifying render must not show the spinner label"
        );
        assert!(
            text.contains("Enter=submit"),
            "default footer hint must appear: {text}"
        );
    }

    #[test]
    fn test_render_passphrase_verifying_shows_spinner_and_label() {
        // When verifying=true the modal must paint a spinner glyph and
        // the "verifying passphrase..." label so the operator doesn't
        // think the UI hung while cryptsetup runs.
        for frame_idx in 0..SPINNER_FRAMES {
            let data = PassphraseScreenData {
                prompt_label: "Unlock /dev/sda2",
                buffer_len: 8,
                cursor_column: 8,
                caps_lock_on: false,
                verifying: true,
                spinner_frame: frame_idx,
            };
            let mut term = new_term(80, 24);
            term.draw(|f| render_passphrase(f, &data)).expect("draw");
            let text = buffer_text(&term);
            // The dotted input row is still present.
            assert!(text.contains("********|"), "input row missing: {text}");
            // The verifying overlay row.
            assert!(
                text.contains("verifying passphrase"),
                "missing verifying label at frame {frame_idx}:\n{text}"
            );
            // The expected spinner glyph for this frame.
            let expected = SPINNER_GLYPHS[frame_idx as usize];
            assert!(
                text.contains(expected),
                "expected spinner glyph '{expected}' at frame {frame_idx}:\n{text}"
            );
            // Footer hint switches.
            assert!(
                text.contains("verifying..."),
                "verifying footer hint missing at frame {frame_idx}:\n{text}"
            );
        }
    }

    #[test]
    fn test_render_passphrase_verifying_out_of_range_frame_does_not_panic() {
        // Defence-in-depth: a caller that didn't wrap modulo
        // SPINNER_FRAMES must not crash the renderer. We pin that the
        // out-of-range frame falls back to a sensible default glyph.
        let data = PassphraseScreenData {
            prompt_label: "Unlock",
            buffer_len: 2,
            cursor_column: 2,
            caps_lock_on: false,
            verifying: true,
            spinner_frame: 99,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_passphrase(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("verifying passphrase"));
    }

    /// Walk the rendered buffer and return the cell style under the first
    /// occurrence of `needle`'s leading char on the line that contains it.
    fn style_under_first_match(term: &Terminal<TestBackend>, needle: &str) -> Style {
        let buf = term.backend().buffer();
        let head = needle.chars().next().expect("non-empty needle");
        let head_str = head.to_string();
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width)
                .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                .collect();
            if let Some(byte_idx) = row.find(needle) {
                // Byte index in a single-byte ASCII needle equals column.
                let col_chars = row
                    .get(..byte_idx)
                    .map_or(0, |prefix| prefix.chars().count());
                // Locate cell at (col_chars, y) and assert its symbol matches.
                let cell = buf
                    .cell((col_chars as u16, y))
                    .expect("cell in rendered area");
                assert_eq!(cell.symbol(), head_str);
                return cell.style();
            }
        }
        panic!("needle {needle:?} not found in rendered buffer");
    }

    #[test]
    fn test_render_passphrase_submit_hint_dim_when_buffer_empty() {
        // Empty buffer → "Enter=submit" rendered DIM so the disabled
        // state is visible to the operator. Non-empty buffer → default.
        let empty = PassphraseScreenData {
            prompt_label: "Unlock root",
            buffer_len: 0,
            cursor_column: 0,
            caps_lock_on: false,
            verifying: false,
            spinner_frame: 0,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_passphrase(f, &empty)).expect("draw");
        let style_empty = style_under_first_match(&term, "Enter=submit");
        assert!(
            style_empty.add_modifier.contains(Modifier::DIM),
            "Enter=submit must be DIM when buffer is empty; got {style_empty:?}",
        );

        let filled = PassphraseScreenData {
            prompt_label: "Unlock root",
            buffer_len: 3,
            cursor_column: 3,
            caps_lock_on: false,
            verifying: false,
            spinner_frame: 0,
        };
        let mut term2 = new_term(80, 24);
        term2
            .draw(|f| render_passphrase(f, &filled))
            .expect("draw");
        let style_filled = style_under_first_match(&term2, "Enter=submit");
        assert!(
            !style_filled.add_modifier.contains(Modifier::DIM),
            "Enter=submit must NOT be DIM with non-empty buffer; got {style_filled:?}",
        );
    }

    /// The masked caret must sit at `cursor_column`, not always at the
    /// end: a 5-char secret with the cursor at column 2 renders
    /// `**|***` so the operator sees where a mid-string edit lands.
    #[test]
    fn test_render_passphrase_caret_tracks_cursor_column() {
        let data = PassphraseScreenData {
            prompt_label: "Unlock",
            buffer_len: 5,
            cursor_column: 2,
            verifying: false,
            spinner_frame: 0,
            caps_lock_on: false,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_passphrase(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(
            text.contains("**|***"),
            "caret must render after 2 dots with 3 trailing dots:\n{text}"
        );
    }

    /// Caps-Lock warning must appear when on and the box geometry must be
    /// byte-for-byte identical whether the warning shows or not — the
    /// reserved row guarantees zero reflow.
    #[test]
    fn test_render_passphrase_caps_lock_warning_does_not_resize_box() {
        let base = |caps: bool| PassphraseScreenData {
            prompt_label: "Unlock root",
            buffer_len: 4,
            cursor_column: 4,
            verifying: false,
            spinner_frame: 0,
            caps_lock_on: caps,
        };

        let mut term_off = new_term(80, 24);
        term_off
            .draw(|f| render_passphrase(f, &base(false)))
            .expect("draw");
        let mut term_on = new_term(80, 24);
        term_on
            .draw(|f| render_passphrase(f, &base(true)))
            .expect("draw");

        let off = buffer_lines(&term_off);
        let on = buffer_lines(&term_on);

        // Warning present only when on.
        assert!(
            on.join("\n").contains("Caps Lock is ON"),
            "warning must show when caps on:\n{}",
            on.join("\n")
        );
        assert!(
            !off.join("\n").contains("Caps Lock is ON"),
            "warning must be absent when caps off:\n{}",
            off.join("\n")
        );

        // The box border rows must be at the SAME positions in both
        // renders — i.e. the modal occupies identical rows. Compare the
        // set of rows that contain a vertical border glyph.
        let border_rows = |lines: &[String]| -> Vec<usize> {
            lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.contains('│') || l.contains('┌') || l.contains('└'))
                .map(|(i, _)| i)
                .collect()
        };
        assert_eq!(
            border_rows(&off),
            border_rows(&on),
            "modal box must occupy identical rows regardless of caps warning"
        );
    }

    #[test]
    fn test_render_modal_confirm_shows_title_buttons_and_hint() {
        let data = ModalConfirmScreenData {
            title: "Boot one?",
            message: "Found 3 generations.",
            yes_label: "Yes",
            no_label: "Back",
            yes_selected: true,
            hint: "Left/Right select  Enter confirm  Esc cancel",
            scroll_offset: 0,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("Boot one?"), "title missing in:\n{text}");
        assert!(
            text.contains("Found 3 generations"),
            "message missing in:\n{text}"
        );
        assert!(text.contains("[Yes]"), "yes button missing in:\n{text}");
        assert!(text.contains("[Back]"), "no button missing in:\n{text}");
        assert!(
            text.contains("Enter confirm"),
            "hint missing in:\n{text}"
        );
    }

    #[test]
    fn test_render_modal_confirm_renders_with_no_selected() {
        // Pin the other branch of yes_selected: when false, the "No"
        // button gets the highlight. The plain-text scan can't see
        // colour but it can confirm both labels paint.
        let data = ModalConfirmScreenData {
            title: "Confirm",
            message: "Proceed?",
            yes_label: "Boot",
            no_label: "Back",
            yes_selected: false,
            hint: "h",
            scroll_offset: 0,
        };
        let mut term = new_term(60, 16);
        term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("[Boot]"), "yes label missing in:\n{text}");
        assert!(text.contains("[Back]"), "no label missing in:\n{text}");
    }

    #[test]
    fn test_render_modal_confirm_separator_row_dash_space_filling_inner_width() {
        // Pin the spec's separator row: `- ` repeated across the inner
        // text width. Cell scan finds the dash pattern on the
        // separator row.
        let data = ModalConfirmScreenData {
            title: "T",
            message: "hello world",
            yes_label: "Yes",
            no_label: "No",
            yes_selected: true,
            hint: "h",
            scroll_offset: 0,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
        let text = buffer_text(&term);
        // The dash-space pattern must appear at least once.
        assert!(text.contains("- - -"), "separator missing: \n{text}");
    }

    #[test]
    fn test_render_modal_confirm_short_msg_box_hugs_message_not_padded_to_min() {
        // 11-char "hello world" message: the box must not pad out to 40
        // — `min(MIN_TEXT_WIDTH, max_line_length(msg)) = 11`, so the
        // floor is 11 and the box hugs the message + buttons.
        let data = ModalConfirmScreenData {
            title: "T",
            message: "hello world",
            yes_label: "Yes",
            no_label: "No",
            yes_selected: true,
            hint: "h",
            scroll_offset: 0,
        };
        let mut term = new_term(120, 30);
        term.draw(|f| render_modal_confirm(f, &data)).expect("draw");
        let lines = buffer_lines(&term);
        // Find the title row and measure the box width.
        let row = lines.iter().find(|l| l.contains("┌T")).expect("title row");
        // Box width = sum of chars between the corners; quick proxy:
        // the row's leading spaces + box span + trailing spaces.
        let lead = row.chars().take_while(|c| *c == ' ').count();
        let trail = row.chars().rev().take_while(|c| *c == ' ').count();
        // Symmetry check: roughly equal left/right margins.
        assert!(
            lead.abs_diff(trail) <= 1,
            "box must be centred: lead={lead} trail={trail}, row={row:?}"
        );
    }

    #[test]
    fn test_render_modal_error_shows_title_message_and_hint() {
        // Modal must surface the title, the error chain body, and the
        // any-key hint in the footer. Cell-by-cell text scan is enough
        // — the actual layout/colour is incidental detail tested in
        // ratatui's own suite.
        let data = ModalErrorScreenData {
            title: "Pretty Shell failed to start",
            message: "openpty failed: ENOENT: No such file or directory",
            hint: "press any key to continue",
            scroll_offset: 0,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_modal_error(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(
            text.contains("Pretty Shell failed to start"),
            "title missing in:\n{text}"
        );
        assert!(text.contains("openpty failed"), "message missing in:\n{text}");
        assert!(text.contains("press any key"), "hint missing in:\n{text}");
    }

    #[test]
    fn test_render_modal_error_tall_message_shows_scroll_hint_outside_box() {
        // 50 short lines on a 24-row screen: the layout MUST enter
        // scroll mode (stage H4) and paint "Ctrl+Shift+Up/Dn scroll"
        // on the row just BELOW the box.
        let msg = (0..50)
            .map(|i| format!("line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let data = ModalErrorScreenData {
            title: "Tall",
            message: &msg,
            hint: "press any key to continue",
            scroll_offset: 0,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_modal_error(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(
            text.contains("Ctrl+Shift+Up/Dn scroll"),
            "scroll hint missing:\n{text}"
        );
    }

    #[test]
    fn test_render_modal_error_tall_message_respects_scroll_offset() {
        // With offset 5 the first visible line must be "line 05",
        // not "line 00".
        let msg = (0..50)
            .map(|i| format!("line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let data = ModalErrorScreenData {
            title: "Tall",
            message: &msg,
            hint: "press any key to continue",
            scroll_offset: 5,
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_modal_error(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(
            text.contains("line 05"),
            "offset 5 should show line 05:\n{text}"
        );
        assert!(
            !text.contains("line 00"),
            "offset 5 should hide line 00:\n{text}"
        );
    }

    fn boot_status_data<'a>(
        phase: &'a str,
        lines: &[&str],
        spinner_frame: u8,
    ) -> BootStatusData<'a> {
        BootStatusData {
            phase: std::borrow::Cow::Borrowed(phase),
            log_lines: lines.iter().map(|s| (*s).to_string()).collect(),
            spinner_frame,
        }
    }

    #[test]
    fn test_render_boot_status_shows_header_lines_phase_and_spinner_frame0() {
        // Layout assumes a 24-row terminal: 3-row header, body in the
        // middle, 1-row status. Three log lines fit comfortably in the
        // body so all three should be visible.
        let data = boot_status_data(
            "phase 1: udev coldplug",
            &["mount /proc", "mount /sys", "starting udev"],
            0,
        );
        let mut term = new_term(80, 24);
        term.draw(|f| render_boot_status(f, &data)).expect("draw");
        let text = buffer_text(&term);

        assert!(text.contains("sirati's NMBL"), "missing header in:\n{text}");
        assert!(
            text.contains("phase 1: udev coldplug"),
            "missing phase in:\n{text}"
        );
        for line in ["mount /proc", "mount /sys", "starting udev"] {
            assert!(text.contains(line), "missing log line {line:?} in:\n{text}");
        }
        // Frame 0 -> '|'.
        assert!(text.contains('|'), "missing spinner glyph '|' in:\n{text}");
    }

    #[test]
    fn test_render_boot_status_spinner_advances_with_frame() {
        // Render at frame 0 and frame 1 and assert the status row
        // differs. Frame 1 must contain '/'; frame 0 must not (the
        // header / log content is fixed in this fixture, so no other
        // '/' appears in the buffer apart from the spinner cell).
        let data0 = boot_status_data("waiting", &["a"], 0);
        let data1 = boot_status_data("waiting", &["a"], 1);

        let mut t0 = new_term(40, 10);
        t0.draw(|f| render_boot_status(f, &data0)).expect("draw");
        let mut t1 = new_term(40, 10);
        t1.draw(|f| render_boot_status(f, &data1)).expect("draw");

        let txt0 = buffer_text(&t0);
        let txt1 = buffer_text(&t1);

        assert!(txt0.contains('|'), "frame0 must contain |");
        assert!(txt1.contains('/'), "frame1 must contain /");
        assert_ne!(txt0, txt1, "spinner advance must change buffer");
    }

    #[test]
    fn test_render_boot_status_shows_esc_to_abort_hint() {
        // The "Esc to abort" hint must always be present on the
        // BootStatus screen so the operator knows the wait is
        // interruptible without having to read the docs.
        let data = boot_status_data("phase 3b: waiting", &["mount /proc"], 0);
        let mut term = new_term(80, 24);
        term.draw(|f| render_boot_status(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(
            text.contains("Esc to abort"),
            "missing 'Esc to abort' hint in:\n{text}"
        );
    }

    #[test]
    fn test_render_boot_status_esc_hint_is_bottom_right_of_log_panel() {
        // Stronger assertion: the hint must sit on the bottom-right of
        // the log box, mirroring pretty-shell's scroll-hint placement.
        // We pin the row (last interior row of the log panel) and the
        // alignment (right edge) so a future refactor can't silently
        // move the hint to a place the operator won't notice.
        let data = boot_status_data("p", &["line"], 0);
        let mut term = new_term(80, 24);
        term.draw(|f| render_boot_status(f, &data)).expect("draw");
        let lines = buffer_lines(&term);

        // Layout: 3-row header, body fills the middle, 1-row status.
        // The log box borders the body, so its bottom border row is at
        // `header_h + body_h - 1 = 3 + 20 - 1 = 22`. The hint paints on
        // the LAST INNER row, which is one above the bottom border:
        // row 21.
        let hint_row_idx = 21;
        let hint_row = lines
            .get(hint_row_idx)
            .expect("hint row must exist in 24-row term");
        assert!(
            hint_row.contains("Esc to abort"),
            "hint missing on expected row {hint_row_idx}: {hint_row:?}"
        );
        // Right alignment: the hint should sit near the right border,
        // not the left. Specifically, the column where "Esc to abort"
        // starts should be well past column 40 (mid-width on an 80-col
        // terminal).
        let hint_col = hint_row.find("Esc to abort").expect("hint substring");
        assert!(
            hint_col > 40,
            "hint must be right-aligned (col {hint_col} should exceed 40): {hint_row:?}"
        );
    }

    #[test]
    fn test_render_boot_status_clips_to_panel_height() {
        // 50 lines into a 10-row terminal. The header eats 3 rows and
        // the status line eats 1, leaving 6 rows for the bordered log
        // panel; the panel borders eat 2 more, so only ~4 lines of
        // content are visible. The exact panel height isn't load-
        // bearing for this test — what matters is that *only* the
        // most recent lines appear and *none* of the earliest ones
        // do, regardless of clipping math.
        //
        // Zero-padded width-2 indices make each label uniquely
        // identifiable as a substring (e.g. "log-00" is not a prefix
        // of "log-49"), so `str::contains` is a safe substring check.
        let lines: Vec<String> = (0..50).map(|i| format!("log-{i:02}")).collect();
        let lines_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let data = boot_status_data("phase X", &lines_refs, 0);

        let mut term = new_term(40, 10);
        term.draw(|f| render_boot_status(f, &data)).expect("draw");
        let text = buffer_text(&term);

        // The last line must appear; the first and one mid-range
        // sample must not.
        assert!(
            text.contains("log-49"),
            "most-recent line missing in:\n{text}"
        );
        assert!(
            !text.contains("log-00"),
            "earliest line leaked through clipping in:\n{text}"
        );
        assert!(
            !text.contains("log-10"),
            "mid-range line leaked through clipping in:\n{text}"
        );
    }

    #[test]
    fn test_render_footer_text_present() {
        let mut term = new_term(40, 5);
        term.draw(|f| {
            let area = f.area();
            render_footer(
                f,
                Rect::new(0, area.height - 1, area.width, 1),
                "Enter=boot",
            );
        })
        .expect("draw");
        let last = buffer_lines(&term).pop().expect("row");
        assert!(last.contains("Enter=boot"), "footer missing in: {last:?}");
    }
}
