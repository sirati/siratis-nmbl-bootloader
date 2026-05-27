//! Ratatui screens for the network-rescue flow (PLAN.md Phase E.2).
//!
//! E.1 landed [`crate::rescue::net::try_network_rescue`] driving a
//! [`crate::rescue::net::RescueUi`] trait object. This module ships
//! [`RatatuiRescueUi`] — the production implementation that paints
//! over `/dev/console` using the same raw-mode + ratatui plumbing as
//! the boot selector. The fallback [`crate::rescue::net::ConsoleRescueUi`]
//! stays in `src/rescue/net.rs` as a test/serial-console double.
//!
//! Four screens, each in its own private function:
//!
//! * [`pick_source`] — operator chooses Network / Reboot / Halt after
//!   disk rescue failed. Header surfaces the disk error reason verbatim.
//! * [`prompt_url`] — single-line URL editor pre-filled from
//!   `rescue.default_url`. Enter confirms, Esc aborts.
//! * [`progress`] — gauge bar over the download. Falls back to a byte
//!   counter + spinner when `Content-Length` is unknown.
//! * [`confirm_hash`] — side-by-side computed vs. expected hex panes
//!   with an editable expected field and a red MISMATCH banner when
//!   the two disagree.
//!
//! Each method opens a fresh [`crate::ui::with_console_terminal`]
//! session so a failed render or a panic during one screen does not
//! poison the surrounding flow's terminal state.

use std::io::Write;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Gauge, Paragraph, Wrap};

use crate::error::{NmblError, Result};
use crate::rescue::net::{DownloadStatus, HashConfirmation, RescueSource, RescueUi};
use crate::ui::POLL_SLICE;
use crate::ui::with_console_terminal;

/// Throttle progress repaints so a multi-megabyte download doesn't
/// burn the serial line at gigabyte-per-second redraw rates.
const PROGRESS_REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// Production [`RescueUi`] backed by ratatui. Renders each screen in
/// its own raw-mode session so a failure on one screen leaves the
/// terminal cleanly restored for the next.
pub struct RatatuiRescueUi {
    /// Cursor position in the URL editor — preserved across redraws
    /// inside `prompt_url`. Stored on the struct so future screens
    /// can resume the same buffer.
    url_cursor: usize,
    /// Last expected-hash buffer the operator typed in `confirm_hash`.
    /// Lets repeated calls (e.g. after a Mismatch loop) keep the
    /// operator's manual override.
    expected_cursor: usize,
    /// Spinner phase index for the indeterminate progress bar. Stays
    /// across `progress` calls so the spinner actually spins instead
    /// of resetting to frame 0 on every chunk.
    spinner_phase: usize,
    /// Last time we painted a progress frame — used to throttle the
    /// redraw cadence.
    last_redraw: Option<std::time::Instant>,
}

impl Default for RatatuiRescueUi {
    fn default() -> Self {
        Self::new()
    }
}

impl RatatuiRescueUi {
    /// Construct a fresh UI. Cheap; allocates no terminal resources
    /// until a screen method is called.
    pub fn new() -> Self {
        Self {
            url_cursor: 0,
            expected_cursor: 0,
            spinner_phase: 0,
            last_redraw: None,
        }
    }
}

impl RescueUi for RatatuiRescueUi {
    fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource> {
        with_console_terminal(|terminal| pick_source(terminal, disk_reason))
    }

    fn prompt_url(&mut self, prefill: &str) -> Result<String> {
        let cursor_seed = if self.url_cursor == 0 {
            prefill.len()
        } else {
            self.url_cursor.min(prefill.len())
        };
        let (out, final_cursor) =
            with_console_terminal(|terminal| prompt_url(terminal, prefill, cursor_seed))?;
        self.url_cursor = final_cursor;
        Ok(out)
    }

    fn progress(&mut self, status: DownloadStatus) {
        // Drop the redraw entirely if we painted too recently. Avoids
        // saturating /dev/console on multi-MB downloads.
        let now = std::time::Instant::now();
        if let Some(prev) = self.last_redraw
            && now.duration_since(prev) < PROGRESS_REDRAW_INTERVAL
        {
            return;
        }
        self.last_redraw = Some(now);
        self.spinner_phase = self.spinner_phase.wrapping_add(1);
        let phase = self.spinner_phase;
        // Errors on the progress repaint must not abort the download —
        // the operator can still confirm-or-abort on the hash screen.
        let _ = with_console_terminal(|terminal| {
            terminal
                .draw(|f| render_progress(f, status, phase))
                .map_err(tui_err)?;
            Ok(())
        });
    }

    fn confirm_hash(
        &mut self,
        computed_hex: &str,
        prefill_expected: &str,
    ) -> Result<HashConfirmation> {
        let cursor_seed = if self.expected_cursor == 0 {
            prefill_expected.len()
        } else {
            self.expected_cursor.min(prefill_expected.len())
        };
        let (out, final_cursor) = with_console_terminal(|terminal| {
            confirm_hash(terminal, computed_hex, prefill_expected, cursor_seed)
        })?;
        self.expected_cursor = final_cursor;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Screen: pick_source
// ---------------------------------------------------------------------------

/// Drive the source-picker screen in a poll-input + render loop until
/// the operator commits with N/R/H (or arrow + Enter).
fn pick_source<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    disk_reason: &str,
) -> Result<RescueSource> {
    poll_loop(terminal, |frame| render_pick_source(frame, disk_reason, 0))?;
    let mut highlight: usize = 0;
    let options = [RescueSource::Network, RescueSource::Reboot, RescueSource::Halt];
    loop {
        terminal
            .draw(|f| render_pick_source(f, disk_reason, highlight))
            .map_err(tui_err)?;
        let evt = read_key_event()?;
        let Event::Key(key) = evt else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('n') | KeyCode::Char('N') => return Ok(RescueSource::Network),
            KeyCode::Char('r') | KeyCode::Char('R') => return Ok(RescueSource::Reboot),
            KeyCode::Char('h') | KeyCode::Char('H') => return Ok(RescueSource::Halt),
            KeyCode::Up | KeyCode::Char('k') => {
                highlight = highlight.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j')
                if highlight < options.len().saturating_sub(1) =>
            {
                highlight = highlight.saturating_add(1);
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

/// Pure-render side of [`pick_source`]. Header banner + disk reason
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
    frame.render_widget(
        Paragraph::new(hint).alignment(Alignment::Right),
        footer,
    );
}

// ---------------------------------------------------------------------------
// Screen: prompt_url
// ---------------------------------------------------------------------------

/// Single-line URL editor. Returns the confirmed URL and the final
/// cursor position so a follow-up call can resume editing.
fn prompt_url<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    prefill: &str,
    cursor_seed: usize,
) -> Result<(String, usize)> {
    let mut buffer = prefill.to_string();
    let mut cursor = cursor_seed.min(buffer.len());

    loop {
        terminal
            .draw(|f| render_prompt_url(f, &buffer, cursor))
            .map_err(tui_err)?;
        let evt = read_key_event()?;
        let Event::Key(key) = evt else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Ctrl-U clears the buffer (matches readline muscle memory and
        // makes "wipe the prefill" a one-shot operation).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('u'))
        {
            buffer.clear();
            cursor = 0;
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
            }
            KeyCode::Backspace => {
                let current = clamp_to_char_boundary(&buffer, cursor);
                if let Some(prev) = prev_char_boundary(&buffer, current) {
                    buffer.replace_range(prev..current, "");
                    cursor = prev;
                }
            }
            KeyCode::Left => {
                let current = clamp_to_char_boundary(&buffer, cursor);
                cursor = prev_char_boundary(&buffer, current).unwrap_or(0);
            }
            KeyCode::Right => {
                let current = clamp_to_char_boundary(&buffer, cursor);
                cursor = next_char_boundary(&buffer, current).unwrap_or(buffer.len());
            }
            KeyCode::Home => cursor = 0,
            KeyCode::End => cursor = buffer.len(),
            _ => {}
        }
    }
}

/// Pure-render side of [`prompt_url`]. Header banner + bordered single-line
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
    frame.render_widget(
        Paragraph::new(hint).alignment(Alignment::Right),
        footer,
    );
}

// ---------------------------------------------------------------------------
// Screen: progress
// ---------------------------------------------------------------------------

/// Pure-render side of `progress`. Gauge widget when total bytes are
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
            let glyph = SPINNER.get(spinner_phase % SPINNER.len()).copied().unwrap_or("|");
            let text = Text::from(vec![
                Line::raw(format!("{glyph} {} bytes (Content-Length unknown)", status.bytes)),
            ]);
            let para = Paragraph::new(text).block(Block::bordered().title("Progress"));
            frame.render_widget(para, body);
        }
    }

    let hint = "downloading…  hash confirmation follows";
    frame.render_widget(
        Paragraph::new(hint).alignment(Alignment::Right),
        footer,
    );
}

// ---------------------------------------------------------------------------
// Screen: confirm_hash
// ---------------------------------------------------------------------------

/// Two-pane hash confirm screen. Returns the chosen outcome and the
/// final cursor offset on the (editable) expected pane.
fn confirm_hash<W: Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    computed_hex: &str,
    prefill_expected: &str,
    cursor_seed: usize,
) -> Result<(HashConfirmation, usize)> {
    let mut expected = prefill_expected.to_string();
    let mut cursor = cursor_seed.min(expected.len());

    loop {
        terminal
            .draw(|f| render_confirm_hash(f, computed_hex, &expected, cursor))
            .map_err(tui_err)?;
        let evt = read_key_event()?;
        let Event::Key(key) = evt else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if computed_hex.eq_ignore_ascii_case(expected.as_str()) {
                    return Ok((HashConfirmation::Confirmed, cursor));
                }
                // Operator pressed y but the panes disagree: treat as
                // mismatch so the orchestrator restarts the download
                // rather than pivoting into a tampered blob.
                return Ok((HashConfirmation::Mismatch, cursor));
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                return Ok((HashConfirmation::Mismatch, cursor));
            }
            KeyCode::Char('a') | KeyCode::Char('A') | KeyCode::Esc => {
                return Ok((HashConfirmation::Aborted, cursor));
            }
            KeyCode::Enter => {
                let outcome = if computed_hex.eq_ignore_ascii_case(expected.as_str()) {
                    HashConfirmation::Confirmed
                } else {
                    HashConfirmation::Mismatch
                };
                return Ok((outcome, cursor));
            }
            KeyCode::Char(c) => {
                let insert_at = clamp_to_char_boundary(&expected, cursor);
                expected.insert(insert_at, c);
                cursor = insert_at.saturating_add(c.len_utf8());
            }
            KeyCode::Backspace => {
                let current = clamp_to_char_boundary(&expected, cursor);
                if let Some(prev) = prev_char_boundary(&expected, current) {
                    expected.replace_range(prev..current, "");
                    cursor = prev;
                }
            }
            KeyCode::Left => {
                let current = clamp_to_char_boundary(&expected, cursor);
                cursor = prev_char_boundary(&expected, current).unwrap_or(0);
            }
            KeyCode::Right => {
                let current = clamp_to_char_boundary(&expected, cursor);
                cursor = next_char_boundary(&expected, current).unwrap_or(expected.len());
            }
            KeyCode::Home => cursor = 0,
            KeyCode::End => cursor = expected.len(),
            _ => {}
        }
    }
}

/// Pure-render side of [`confirm_hash`]. Two side-by-side panes
/// (computed = read-only 4-column lowercase hex; expected = editable
/// pre-filled) plus a red MISMATCH banner when the two disagree.
pub(crate) fn render_confirm_hash(
    frame: &mut Frame<'_>,
    computed_hex: &str,
    expected: &str,
    cursor: usize,
) {
    let (banner_text, banner_colour) = if expected.is_empty() {
        // No prefill is its own distinct state — louder than the
        // computed/expected mismatch banner so the operator knows the
        // disagreement is "you haven't typed anything yet" rather
        // than "the upstream hash drifted".
        ("No expected hash pre-filled", Color::Yellow)
    } else if !computed_hex.eq_ignore_ascii_case(expected) {
        ("MISMATCH — refusing to confirm", Color::Red)
    } else {
        ("Hash matches expected", Color::Green)
    };

    let [header, banner, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas::<4>(frame.area());

    render_banner(frame, header, "Hash confirmation", Color::Cyan);
    render_banner(frame, banner, banner_text, banner_colour);

    let [left, right] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas::<2>(body);

    let computed_lines: Vec<Line<'_>> = group_hex(computed_hex, 4)
        .into_iter()
        .map(Line::raw)
        .collect();
    let computed_para = Paragraph::new(Text::from(computed_lines))
        .block(Block::bordered().title("Computed (SHA-256)"))
        .wrap(Wrap { trim: false });
    frame.render_widget(computed_para, left);

    let column = char_column_for_byte_cursor(expected, cursor);
    let caret = format!("{}{}", " ".repeat(column), "^");
    let expected_lines: Vec<Line<'_>> = {
        let mut v: Vec<Line<'_>> = group_hex(expected, 4).into_iter().map(Line::raw).collect();
        v.push(Line::styled(
            caret,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        v
    };
    let expected_para = Paragraph::new(Text::from(expected_lines))
        .block(Block::bordered().title("Expected (editable)"))
        .wrap(Wrap { trim: false });
    frame.render_widget(expected_para, right);

    let hint = "Y=confirm  N=mismatch  A/Esc=abort  Enter=auto  edit expected to override";
    frame.render_widget(
        Paragraph::new(hint).alignment(Alignment::Right),
        footer,
    );
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Render a single-line bordered banner. Used by every screen so the
/// header chrome stays consistent.
fn render_banner(frame: &mut Frame<'_>, area: Rect, title: &str, fg: Color) {
    let para = Paragraph::new(Line::styled(
        title.to_owned(),
        Style::default().fg(fg).add_modifier(Modifier::BOLD),
    ))
    .alignment(Alignment::Center)
    .block(Block::bordered());
    frame.render_widget(para, area);
}

/// Map an `io::Error` from ratatui/crossterm into the project's
/// [`NmblError::Tui`] variant.
fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

/// Group `hex` into space-separated chunks of `chunk` chars per line,
/// wrapping every 16 chars (= 4 chunks of 4) so a 64-char SHA-256
/// digest renders as four rows. Empty input renders as one empty row.
fn group_hex(hex: &str, chunk: usize) -> Vec<String> {
    let chunk = chunk.max(1);
    if hex.is_empty() {
        return vec![String::new()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut chunks_on_row = 0usize;
    let mut chars = hex.chars();
    loop {
        let mut group = String::with_capacity(chunk);
        for _ in 0..chunk {
            let Some(c) = chars.next() else { break };
            group.push(c);
        }
        if group.is_empty() {
            break;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&group);
        chunks_on_row = chunks_on_row.saturating_add(1);
        if chunks_on_row >= 4 {
            rows.push(std::mem::take(&mut current));
            chunks_on_row = 0;
        }
    }
    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// Block on `crossterm::event::read` indirectly: poll forever with
/// the shared `POLL_SLICE` cadence until a non-empty event is
/// available, then read it. Matches the cadence the boot selector
/// uses so the user-visible responsiveness is identical.
fn read_key_event() -> Result<Event> {
    loop {
        if event::poll(POLL_SLICE).map_err(tui_err)? {
            return event::read().map_err(tui_err);
        }
    }
}

/// Single throw-away render — used to paint a frame before entering
/// the input loop so the operator sees the screen during the first
/// poll cycle. Mirrors the "draw once before polling" idiom in the
/// boot selector.
fn poll_loop<W, F>(terminal: &mut Terminal<CrosstermBackend<W>>, render: F) -> Result<()>
where
    W: Write,
    F: FnOnce(&mut Frame<'_>),
{
    terminal.draw(render).map_err(tui_err)?;
    Ok(())
}

/// Same byte→char column conversion as `ui::view::char_column_for_byte_cursor`.
/// Duplicated rather than re-exported because the original is
/// private; flagging it `pub(crate)` would be a wider refactor than
/// E.2 should land.
fn char_column_for_byte_cursor(s: &str, byte_idx: usize) -> usize {
    let clamped = byte_idx.min(s.len());
    let Some(safe) = (0..=clamped).rev().find(|&i| s.is_char_boundary(i)) else {
        return 0;
    };
    s.get(..safe).map_or(0, |prefix| prefix.chars().count())
}

/// Round `byte_idx` down to the nearest char boundary in `s`.
fn clamp_to_char_boundary(s: &str, byte_idx: usize) -> usize {
    let len = s.len();
    if byte_idx >= len {
        return len;
    }
    let mut idx = byte_idx;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    idx
}

/// Byte index of the char boundary strictly before `byte_idx`, or
/// `None` if `byte_idx` is at the start (or before).
fn prev_char_boundary(s: &str, byte_idx: usize) -> Option<usize> {
    if byte_idx == 0 {
        return None;
    }
    let mut idx = byte_idx.saturating_sub(1);
    while idx > 0 && !s.is_char_boundary(idx) {
        idx = idx.saturating_sub(1);
    }
    Some(idx)
}

/// Byte index of the next char boundary after `byte_idx`, or `None`
/// if `byte_idx` is at or past the end.
fn next_char_boundary(s: &str, byte_idx: usize) -> Option<usize> {
    let len = s.len();
    if byte_idx >= len {
        return None;
    }
    let mut idx = byte_idx.saturating_add(1);
    while idx < len && !s.is_char_boundary(idx) {
        idx = idx.saturating_add(1);
    }
    Some(idx)
}

// ---------------------------------------------------------------------------
// Constructor for the rescue dispatch path
// ---------------------------------------------------------------------------

/// Convenience constructor for the rescue dispatcher: returns the
/// production ratatui-backed UI as a concrete type so callers can
/// `&mut` it through the [`RescueUi`] trait. Kept here rather than
/// in `rescue/mod.rs` so the trait wiring stays inside the `ui`
/// module.
#[must_use]
pub fn make_rescue_ui() -> RatatuiRescueUi {
    RatatuiRescueUi::new()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on render contract"
)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
    fn render_pick_source_shows_header_and_options() {
        let mut term = new_term(80, 24);
        term.draw(|f| render_pick_source(f, "loop_dev: ENOSPC", 0))
            .expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("Disk rescue unavailable"), "missing header in:\n{text}");
        assert!(text.contains("loop_dev: ENOSPC"), "missing disk reason");
        assert!(text.contains("[N]"), "missing N hotkey");
        assert!(text.contains("[R]"), "missing R hotkey");
        assert!(text.contains("[H]"), "missing H hotkey");
        assert!(text.contains("Network rescue"), "missing network label");
    }

    #[test]
    fn render_prompt_url_shows_prefill_and_caret() {
        let mut term = new_term(80, 12);
        let url = "https://example.invalid/rescue.sfs";
        term.draw(|f| render_prompt_url(f, url, url.len())).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("Rescue URL"), "missing header");
        assert!(text.contains(url), "missing prefill in:\n{text}");
        assert!(text.contains('^'), "missing caret indicator");
        assert!(text.contains("Enter=confirm"), "missing footer hint");
    }

    #[test]
    fn render_progress_gauge_shows_percentage_when_total_known() {
        let mut term = new_term(80, 12);
        term.draw(|f| {
            render_progress(
                f,
                DownloadStatus {
                    bytes: 50,
                    total: Some(200),
                },
                0,
            );
        })
        .expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("Downloading rescue blob"), "missing banner");
        assert!(text.contains("50 / 200 bytes"), "missing byte counter in:\n{text}");
        assert!(text.contains("25%"), "missing percentage label in:\n{text}");
    }

    #[test]
    fn render_progress_spinner_shows_byte_count_when_total_unknown() {
        let mut term = new_term(80, 12);
        term.draw(|f| {
            render_progress(
                f,
                DownloadStatus {
                    bytes: 1234,
                    total: None,
                },
                2,
            );
        })
        .expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("1234 bytes"), "missing byte count in:\n{text}");
        assert!(text.contains("Content-Length unknown"), "missing fallback label");
    }

    #[test]
    fn render_confirm_hash_shows_both_panes_and_match_banner() {
        let mut term = new_term(120, 24);
        let h = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        term.draw(|f| render_confirm_hash(f, h, h, h.len())).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("Computed (SHA-256)"), "missing computed pane");
        assert!(text.contains("Expected (editable)"), "missing expected pane");
        assert!(text.contains("Hash matches expected"), "missing match banner in:\n{text}");
        // First 4-char chunk of the canonical empty digest.
        assert!(text.contains("e3b0"), "missing grouped hex chunk in:\n{text}");
    }

    #[test]
    fn render_confirm_hash_shows_mismatch_banner_when_panes_disagree() {
        let mut term = new_term(120, 24);
        let computed = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let expected = "deadbeef";
        term.draw(|f| render_confirm_hash(f, computed, expected, expected.len()))
            .expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("MISMATCH"), "missing MISMATCH banner in:\n{text}");
    }

    #[test]
    fn render_confirm_hash_shows_no_prefill_banner_when_expected_empty() {
        let mut term = new_term(120, 24);
        let computed = "abcd1234";
        term.draw(|f| render_confirm_hash(f, computed, "", 0)).expect("draw");
        let text = buffer_text(&term);
        assert!(
            text.contains("No expected hash pre-filled"),
            "missing no-prefill banner in:\n{text}",
        );
    }

    #[test]
    fn group_hex_breaks_into_four_chunks_of_four() {
        let h = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let rows = group_hex(h, 4);
        // 64 chars / (4 chars * 4 chunks) = 4 rows.
        assert_eq!(rows.len(), 4, "expected 4 rows, got {rows:?}");
        for row in &rows {
            // 4 chunks * 4 chars = 16 chars + 3 spaces = 19 chars.
            assert_eq!(row.len(), 19, "wrong row width: {row:?}");
        }
    }

    #[test]
    fn group_hex_handles_empty_input() {
        assert_eq!(group_hex("", 4), vec![String::new()]);
    }

    #[test]
    fn char_column_clamps_inside_multibyte_string() {
        // "héllo" — 'é' is 2 bytes; byte index 4 = inside/after 'l'.
        // Char column should be 3 (h, é, l = three chars before).
        assert_eq!(char_column_for_byte_cursor("héllo", 4), 3);
        // Past the end clamps to the last column.
        assert_eq!(char_column_for_byte_cursor("héllo", 99), 5);
    }

    #[test]
    fn clamp_to_char_boundary_walks_back_inside_multibyte() {
        // "héllo": valid boundaries are 0,1,3,4,5,6. byte 2 is mid-char.
        assert_eq!(clamp_to_char_boundary("héllo", 2), 1);
        assert_eq!(clamp_to_char_boundary("héllo", 5), 5);
        assert_eq!(clamp_to_char_boundary("héllo", 99), 6);
    }

    #[test]
    fn make_rescue_ui_returns_default_state() {
        let ui = make_rescue_ui();
        assert_eq!(ui.url_cursor, 0);
        assert_eq!(ui.expected_cursor, 0);
        assert_eq!(ui.spinner_phase, 0);
        assert!(ui.last_redraw.is_none());
    }
}
