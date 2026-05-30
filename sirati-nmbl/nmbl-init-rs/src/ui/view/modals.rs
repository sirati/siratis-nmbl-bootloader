//! Render functions for modal dialogs, pretty-shell and passphrase screens.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::ui::app::{SPINNER_FRAMES, SPINNER_GLYPHS};
use crate::ui::modal_layout::compute_modal_layout_with_button_width;

use super::{
    ModalButtonsScreenData, ModalConfirmScreenData, ModalErrorScreenData, PassphraseScreenData,
    PtyShellScreenData, button_row_width, centered_rect, paint_modal_text, paint_scroll_hint,
    paint_separator, render_footer, render_header, split_chrome,
};

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
    let layout = compute_modal_layout_with_button_width(data.message, true, btn_count, btn_w, body);
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
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    ));
    frame.render_widget(block, layout.box_rect);
    paint_modal_text(frame, &layout, data.scroll_offset);
    paint_scroll_hint(frame, &layout);

    render_footer(frame, footer, data.hint);
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
    // The "Select NixOS Generation" checkbox adds one permanent row.
    // Verifying mode adds one more row for the spinner line, as before.
    let modal_height: u16 = if data.verifying { 10 } else { 9 };
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
    // "Select NixOS Generation" checkbox row. ASCII `[ ]` / `[x]` so the
    // splash glyph cache (ASCII + box-drawing only) renders it on the
    // framebuffer; the `(Ctrl+G)` hint is always shown so the operator
    // knows the toggle. Unchecked (default) skips the selector on unlock.
    let checkbox_mark = if data.select_generation { "[x]" } else { "[ ]" };
    let mut lines: Vec<Line<'_>> = vec![
        Line::raw(data.prompt_label.to_owned()),
        Line::raw(String::new()),
        Line::from(vec![Span::raw(before), Span::raw("|"), Span::raw(after)]),
        Line::from(vec![
            Span::raw(format!("{checkbox_mark} Select NixOS Generation")),
            Span::styled("   (Ctrl+G)", Style::default().add_modifier(Modifier::DIM)),
        ]),
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
