//! Pure render functions for the NMBL TUI. State and event handling live
//! in the sibling `app` module; this file only knows how to paint frames.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::generations::Generation;
use crate::ui::app::{BootStatusData, EmergencyItem, SPINNER_FRAMES, SPINNER_GLYPHS};

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

fn render_header(frame: &mut Frame<'_>, area: Rect, countdown: Option<u64>) {
    let mut spans = vec![
        Span::styled("sirati's NMBL ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("— no more boot loader"),
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
fn char_column_for_byte_cursor(s: &str, byte_idx: usize) -> usize {
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
    // `cursor_position` is a BYTE index (per `Screen::Editing.cursor`).
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

/// Render the passphrase modal over the body area.
pub fn render_passphrase(frame: &mut Frame<'_>, data: &PassphraseScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);
    let modal = centered_rect(body, 60, 7);
    frame.render_widget(Clear, modal);
    // Cap mask so a huge typo doesn't overflow the box.
    let dots: String = "*".repeat(data.buffer_len.min(40));
    let lines: Vec<Line<'_>> = vec![
        Line::raw(data.prompt_label.to_owned()),
        Line::raw(String::new()),
        Line::from(vec![Span::raw(dots), Span::raw("|")]),
    ];
    let para = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title("Passphrase"))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, modal);
    render_footer(frame, footer, "Enter=submit  Esc=cancel");
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
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_passphrase(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("*****|"), "wrong mask count in:\n{text}");
        assert!(text.contains("Unlock /dev/sda2"));
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
