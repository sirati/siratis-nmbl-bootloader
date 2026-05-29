//! Source-picker screen for the rescue flow.

use crossterm::event::{KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::error::Result;
use crate::rescue::net::RescueSource;
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleEvent};

use super::helpers::render_banner;

/// Drive the source-picker screen in a poll-input + render loop until
/// the operator commits with N/R/H (or arrow + Enter). All paint and
/// input goes through the orchestrator-held [`Console`].
pub(super) async fn run_pick_source(
    console: &mut dyn Console,
    disk_reason: &str,
) -> Result<RescueSource> {
    let mut highlight: usize = 0;
    let options = [
        RescueSource::Network,
        RescueSource::Reboot,
        RescueSource::Halt,
    ];
    let mut dirty = true;
    loop {
        if dirty {
            console.draw_with(&mut |f| render_pick_source(f, disk_reason, highlight))?;
            dirty = false;
        }
        let key = match console.poll_event(POLL_SLICE).await? {
            Some(ConsoleEvent::Resize { .. }) => {
                dirty = true;
                continue;
            }
            Some(ConsoleEvent::Key(k)) => k,
            None => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('n') | KeyCode::Char('N') => return Ok(RescueSource::Network),
            KeyCode::Char('r') | KeyCode::Char('R') => return Ok(RescueSource::Reboot),
            KeyCode::Char('h') | KeyCode::Char('H') => return Ok(RescueSource::Halt),
            KeyCode::Up | KeyCode::Char('k') => {
                highlight = highlight.saturating_sub(1);
                dirty = true;
            }
            KeyCode::Down | KeyCode::Char('j') if highlight < options.len().saturating_sub(1) => {
                highlight = highlight.saturating_add(1);
                dirty = true;
            }
            KeyCode::Enter => {
                if let Some(choice) = options.get(highlight) {
                    return Ok(*choice);
                }
            }
            _ => {}
        }
    }
}

/// Pure-render side of [`run_pick_source`]. Header banner + disk reason
/// paragraph + bordered choices block.
pub(crate) fn render_pick_source(frame: &mut Frame<'_>, disk_reason: &str, highlight: usize) {
    let [header, reason, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(7),
        Constraint::Length(1),
    ])
    .areas::<4>(frame.area());

    render_banner(frame, header, "Disk rescue unavailable", Color::Red);

    let reason_para = Paragraph::new(Text::from(vec![
        Line::raw("Disk rescue failed. Original reason:"),
        Line::styled(
            disk_reason.to_owned(),
            Style::default().add_modifier(Modifier::ITALIC),
        ),
    ]))
    .wrap(Wrap { trim: false })
    .block(Block::bordered().title("Diagnostic"));
    frame.render_widget(reason_para, reason);

    let option_lines: Vec<Line<'_>> = [
        ("N", "Network rescue (HTTP download)"),
        ("R", "Reboot the system"),
        ("H", "Halt the system"),
    ]
    .iter()
    .enumerate()
    .map(|(i, (key, desc))| {
        let style = if i == highlight {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Line::from(vec![
            Span::styled(format!(" [{key}] "), style),
            Span::styled((*desc).to_string(), style),
        ])
    })
    .collect();
    let choices = Paragraph::new(Text::from(option_lines))
        .block(Block::bordered().title("Choose recovery action"));
    frame.render_widget(choices, body);

    let hint = "N/R/H select  Up/Down move  Enter confirm";
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), footer);
}
