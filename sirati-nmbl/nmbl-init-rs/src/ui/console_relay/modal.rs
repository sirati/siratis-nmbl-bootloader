//! TUI modal widgets for the console relay.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

/// Render a "shell is running" modal over the live console. Mirrors
/// `view::render_modal_error` shape but emphasises that the shell is
/// alive, not erroring.
pub(super) fn render_running_modal(frame: &mut Frame<'_>, message: &str) {
    let area = frame.area();
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas::<3>(area);

    let title = Paragraph::new(Line::from(vec![Span::styled(
        "Emergency shell",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )]))
    .alignment(Alignment::Left);
    frame.render_widget(title, header);

    let modal = centered_rect(body, 70, body.height.saturating_div(2).max(8));
    frame.render_widget(Clear, modal);

    let block = Block::bordered().title(Span::styled(
        "shell running",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    let para = Paragraph::new(message.to_owned())
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(para, modal);

    frame.render_widget(
        Paragraph::new("Esc: terminate shell").alignment(Alignment::Left),
        footer,
    );
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x.saturating_add(area.width.saturating_sub(w) / 2);
    let y = area.y.saturating_add(area.height.saturating_sub(h) / 2);
    Rect::new(x, y, w, h)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn render_running_modal_includes_targets_and_hint() {
        let mut term = Terminal::new(TestBackend::new(80, 16)).expect("test terminal");
        let msg = "Shell running on:\n  /dev/tty0\nType into those consoles.";
        term.draw(|f| render_running_modal(f, msg)).expect("draw");
        let buf = term.backend().buffer();
        let dump: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_owned()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(dump.contains("Emergency shell"), "title missing: \n{dump}");
        assert!(dump.contains("/dev/tty0"), "target missing: \n{dump}");
        assert!(dump.contains("Esc"), "hint missing: \n{dump}");
    }
}
