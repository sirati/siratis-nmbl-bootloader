//! Pure render functions for the NMBL TUI. State and event handling live
//! in the sibling `app` module; this file only knows how to paint frames.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::generations::Generation;

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
    pub error_message: Option<&'a str>,
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
        Span::styled("NMBL ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("— NixOS Minimal BootLoader"),
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

fn generation_item<'a>(g: &'a Generation, show_kernel_params: bool) -> ListItem<'a> {
    let head = if g.label.is_empty() {
        format!("#{}", g.number)
    } else {
        format!("#{}  {}", g.number, g.label)
    };
    let mut lines: Vec<Line<'a>> = vec![Line::from(head)];
    if show_kernel_params {
        lines.push(Line::styled(
            format!("    {}", g.kernel_params.join(" ")),
            Style::default().add_modifier(Modifier::DIM),
        ));
    }
    ListItem::new(Text::from(lines))
}

/// Render the generation-picker screen.
pub fn render_list(frame: &mut Frame<'_>, data: &ListScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, data.countdown_remaining_secs);
    let items: Vec<ListItem<'_>> = data
        .generations
        .iter()
        .map(|g| generation_item(g, data.show_kernel_params))
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
    // Caret indicator counts characters, not bytes.
    let offset = data
        .edited_cmdline
        .chars()
        .take(data.cursor_position)
        .count();
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

/// Render the passphrase modal over the body area.
pub fn render_passphrase(frame: &mut Frame<'_>, data: &PassphraseScreenData<'_>) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header, None);
    let modal = centered_rect(body, 60, 7);
    frame.render_widget(Clear, modal);
    // Cap mask so a huge typo doesn't overflow the box.
    let dots: String = "*".repeat(data.buffer_len.min(40));
    let mut lines: Vec<Line<'_>> = vec![
        Line::raw(data.prompt_label.to_owned()),
        Line::raw(String::new()),
        Line::from(vec![Span::raw(dots), Span::raw("|")]),
    ];
    if let Some(err) = data.error_message {
        lines.push(Line::raw(String::new()));
        lines.push(Line::styled(
            err.to_owned(),
            Style::default().fg(Color::Red),
        ));
    }
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
    fn test_render_passphrase_dots_label_and_error() {
        let data = PassphraseScreenData {
            prompt_label: "Unlock /dev/sda2",
            buffer_len: 5,
            error_message: Some("wrong passphrase"),
        };
        let mut term = new_term(80, 24);
        term.draw(|f| render_passphrase(f, &data)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("*****|"), "wrong mask count in:\n{text}");
        assert!(text.contains("Unlock /dev/sda2"));
        assert!(text.contains("wrong passphrase"));
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
