//! Download-progress screen for the rescue flow.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Gauge, Paragraph};

use crate::rescue::net::DownloadStatus;

use super::helpers::render_banner;

/// Pure-render side of the progress screen. Gauge widget when total bytes are
/// known; byte counter + spinner otherwise.
pub(crate) fn render_progress(frame: &mut Frame<'_>, status: DownloadStatus, spinner_phase: usize) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas::<3>(frame.area());

    render_banner(frame, header, "Downloading rescue blob", Color::Yellow);

    match status.total {
        Some(total) if total > 0 => {
            // Gauge widget pegs at 100% when bytes >= total so a stale
            // chunk after the body finishes doesn't crash on overflow.
            let ratio_num = status.bytes.min(total) as f64;
            let ratio_den = total as f64;
            let ratio = (ratio_num / ratio_den).clamp(0.0, 1.0);
            let pct = (ratio * 100.0) as u16;
            let label = format!("{} / {} bytes ({pct}%)", status.bytes, total);
            let gauge = Gauge::default()
                .block(Block::bordered().title("Progress"))
                .gauge_style(Style::default().fg(Color::Green))
                .ratio(ratio)
                .label(label);
            frame.render_widget(gauge, body);
        }
        _ => {
            const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];
            let glyph = SPINNER
                .get(spinner_phase % SPINNER.len())
                .copied()
                .unwrap_or("|");
            let text = Text::from(vec![Line::raw(format!(
                "{glyph} {} bytes (Content-Length unknown)",
                status.bytes
            ))]);
            let para = Paragraph::new(text).block(Block::bordered().title("Progress"));
            frame.render_widget(para, body);
        }
    }

    let hint = "downloading…  hash confirmation follows";
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), footer);
}
