//! Screen for a downloaded rescue image that is not the pinned stage-2.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::error::Result;
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleEvent};

use super::helpers::render_banner;

/// Only an upper-case `U` boots the other image; Esc, `b` or Enter go
/// back to the source picker. A stray key or a pasted hash never
/// chooses an unpinned image.
pub(crate) fn handle_unpinned_key(key: KeyEvent) -> Option<bool> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Char('U') => Some(true),
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('b' | 'B') => Some(false),
        _ => None,
    }
}

pub(super) async fn run_unpinned(
    console: &mut dyn Console,
    pinned_hex: &str,
    actual_hex: &str,
    signed: bool,
) -> Result<bool> {
    let mut dirty = true;
    loop {
        if dirty {
            console.draw_with(&mut |f| render_unpinned(f, pinned_hex, actual_hex, signed))?;
            dirty = false;
        }
        match console.poll_event(POLL_SLICE).await? {
            Some(ConsoleEvent::Resize { .. }) => dirty = true,
            Some(ConsoleEvent::Key(k)) => {
                if let Some(choice) = handle_unpinned_key(k) {
                    return Ok(choice);
                }
            }
            Some(ConsoleEvent::Scroll { .. } | ConsoleEvent::UserHasInteracted) | None => {}
        }
    }
}

pub(crate) fn render_unpinned(
    frame: &mut Frame<'_>,
    pinned_hex: &str,
    actual_hex: &str,
    signed: bool,
) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(1),
    ])
    .areas::<3>(frame.area());

    render_banner(frame, header, "Not the pinned rescue image", Color::Red);

    let signature = if signed {
        Line::styled("Signature: verified", Style::default().fg(Color::Green))
    } else {
        Line::styled(
            "Signature: not verified (signing is off or in audit mode)",
            Style::default().fg(Color::Red),
        )
    };
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let text = Text::from(vec![
        Line::raw("The downloaded image is not the stage-2 image this host's configuration pins."),
        Line::raw(""),
        Line::styled("Pinned SHA-512:", bold),
        Line::raw(pinned_hex.to_owned()),
        Line::styled("Downloaded SHA-512:", bold),
        Line::raw(actual_hex.to_owned()),
        Line::raw(""),
        signature,
    ]);
    let para = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title("Stage-2 pin"));
    frame.render_widget(para, body);

    let hint = "Shift+U boot this image anyway  Esc/B back";
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), footer);
}
