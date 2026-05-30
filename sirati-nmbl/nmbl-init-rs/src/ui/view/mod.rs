//! Pure render functions for the NMBL TUI. State and event handling live
//! in the sibling `app` module; this file only knows how to paint frames.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

use crate::ui::app::EmergencyItem;
use crate::ui::modal_layout::{ModalLayout, SCROLL_HINT};

pub mod list_edit;
pub mod modals;

#[cfg(test)]
mod tests_modals;
#[cfg(test)]
mod tests_screens;

// Re-export the public API so external code using `crate::ui::view::*` is unchanged.
pub use list_edit::{
    render_boot_status, render_edit, render_emergency, render_key_echo, render_list, render_log,
};
pub use modals::{
    render_modal_buttons, render_modal_confirm, render_modal_error, render_passphrase,
    render_pty_shell,
};

/// Char-width of the rendered button row: sum of `[Label]` cells plus
/// the 2-col gutters between buttons. Used as a width floor by the
/// layout pass so a short message can't shrink the box past where the
/// buttons fit.
pub(crate) fn button_row_width(labels: &[&str]) -> u16 {
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
    pub generations: &'a [crate::generations::Generation],
    pub selected_index: usize,
    /// `Some(n)` while auto-booting; `None` once cancelled by a keypress.
    pub countdown_remaining_secs: Option<u64>,
    pub show_kernel_params: bool,
}

/// State needed to render the cmdline-editor screen.
pub struct EditScreenData<'a> {
    pub generation: &'a crate::generations::Generation,
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
    /// State of the "Select NixOS Generation" checkbox. `false`
    /// (default/unchecked) renders `[ ]` and means a plain unlock skips
    /// the selector; `true` renders `[x]` and shows the selector. Always
    /// drawn with the `(Ctrl+G)` hint so the operator knows the hotkey.
    pub select_generation: bool,
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

/// Split frame into (header, body, footer). Small frames degrade gracefully.
pub(crate) fn split_chrome(area: Rect) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(area)
}

pub(crate) fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
}

/// Paint the wrapped text region of a modal at `layout.inner_text_rect`.
/// When `layout.scrollable` is true the offset selects which slice of
/// `layout.wrapped_lines` is visible; otherwise the slice starts at 0.
pub(crate) fn paint_modal_text(frame: &mut Frame<'_>, layout: &ModalLayout, scroll_offset: u16) {
    let total = u16::try_from(layout.wrapped_lines.len()).unwrap_or(u16::MAX);
    let visible = layout.inner_text_rect.height;
    let offset = if layout.scrollable {
        let max_off = total.saturating_sub(visible);
        scroll_offset.min(max_off)
    } else {
        0
    };
    let start = offset as usize;
    let end = start
        .saturating_add(visible as usize)
        .min(layout.wrapped_lines.len());
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
pub(crate) fn paint_separator(frame: &mut Frame<'_>, layout: &ModalLayout) {
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
pub(crate) fn paint_scroll_hint(frame: &mut Frame<'_>, layout: &ModalLayout) {
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

pub(crate) fn render_header(frame: &mut Frame<'_>, area: Rect, countdown: Option<u64>) {
    let mut spans = vec![
        Span::styled(
            "sirati's NMBL ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
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

/// Build the bold `^` caret line that sits under a single-line text
/// input, positioned at the char column of `byte_cursor` within `text`
/// plus a fixed `prefix_cols` lead-in (e.g. a checkbox marker rendered
/// to the left of the text). Shared by the cmdline editor
/// ([`list_edit::render_edit`]) and the console picker's custom-path
/// field so the byte→char conversion and spacing logic live in exactly
/// one place.
///
/// [`list_edit::render_edit`]: super::list_edit::render_edit
pub fn caret_line<'a>(text: &str, byte_cursor: usize, prefix_cols: usize) -> Line<'a> {
    let col = prefix_cols.saturating_add(char_column_for_byte_cursor(text, byte_cursor));
    let caret = format!("{}{}", " ".repeat(col), "^");
    Line::styled(caret, Style::default().add_modifier(Modifier::BOLD))
}
