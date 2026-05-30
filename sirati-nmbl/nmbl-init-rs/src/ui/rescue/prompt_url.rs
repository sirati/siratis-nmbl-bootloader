//! URL-editor screen for the rescue flow.

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::error::{NmblError, Result};
use crate::ui::POLL_SLICE;
use crate::ui::console::{Console, ConsoleEvent};

use super::helpers::{
    char_column_for_byte_cursor, clamp_to_char_boundary, next_char_boundary, prev_char_boundary,
    render_banner,
};

/// Single-line URL editor. Returns the confirmed URL and the final
/// cursor position so a follow-up call can resume editing. All paint
/// and input goes through the orchestrator-held [`Console`].
pub(super) async fn run_prompt_url(
    console: &mut dyn Console,
    prefill: &str,
    cursor_seed: usize,
) -> Result<(String, usize)> {
    let mut buffer = prefill.to_string();
    let mut cursor = cursor_seed.min(buffer.len());

    let mut dirty = true;
    loop {
        if dirty {
            let snapshot_buf = buffer.clone();
            let snapshot_cursor = cursor;
            console.draw_with(&mut |f| render_prompt_url(f, &snapshot_buf, snapshot_cursor))?;
            dirty = false;
        }
        let key = match console.poll_event(POLL_SLICE).await? {
            Some(ConsoleEvent::Resize { .. }) => {
                dirty = true;
                continue;
            }
            Some(ConsoleEvent::Key(k)) => k,
            Some(ConsoleEvent::Scroll { .. } | ConsoleEvent::UserHasInteracted) | None => continue,
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Ctrl-U clears the buffer (matches readline muscle memory and
        // makes "wipe the prefill" a one-shot operation).
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('u')) {
            buffer.clear();
            cursor = 0;
            dirty = true;
            continue;
        }

        match key.code {
            KeyCode::Enter => return Ok((buffer, cursor)),
            KeyCode::Esc => {
                return Err(NmblError::Rescue {
                    stage: "net-ui-prompt-url",
                    source: Box::new(NmblError::Tui {
                        source: std::io::Error::other("operator aborted URL prompt"),
                    }),
                });
            }
            KeyCode::Char(c) => {
                let insert_at = clamp_to_char_boundary(&buffer, cursor);
                buffer.insert(insert_at, c);
                cursor = insert_at.saturating_add(c.len_utf8());
                dirty = true;
            }
            KeyCode::Backspace => {
                let current = clamp_to_char_boundary(&buffer, cursor);
                if let Some(prev) = prev_char_boundary(&buffer, current) {
                    buffer.replace_range(prev..current, "");
                    cursor = prev;
                    dirty = true;
                }
            }
            KeyCode::Left => {
                let current = clamp_to_char_boundary(&buffer, cursor);
                cursor = prev_char_boundary(&buffer, current).unwrap_or(0);
                dirty = true;
            }
            KeyCode::Right => {
                let current = clamp_to_char_boundary(&buffer, cursor);
                cursor = next_char_boundary(&buffer, current).unwrap_or(buffer.len());
                dirty = true;
            }
            KeyCode::Home => {
                cursor = 0;
                dirty = true;
            }
            KeyCode::End => {
                cursor = buffer.len();
                dirty = true;
            }
            _ => {}
        }
    }
}

/// Pure-render side of [`run_prompt_url`]. Header banner + bordered single-line
/// edit Paragraph + caret indicator + footer hint.
pub(crate) fn render_prompt_url(frame: &mut Frame<'_>, buffer: &str, cursor: usize) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(1),
    ])
    .areas::<3>(frame.area());

    render_banner(frame, header, "Rescue URL", Color::Cyan);

    let column = char_column_for_byte_cursor(buffer, cursor);
    let caret = format!("{}{}", " ".repeat(column), "^");
    let text = Text::from(vec![
        Line::raw(buffer.to_owned()),
        Line::styled(caret, Style::default().add_modifier(Modifier::BOLD)),
    ]);
    let para = Paragraph::new(text)
        .block(Block::bordered().title("Enter rescue URL (Enter=confirm, Esc=abort, Ctrl-U=clear)"))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, body);

    let hint = "type/edit URL  Enter=confirm  Esc=abort  Ctrl-U=clear";
    frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), footer);
}
