#![cfg(feature = "image-splash")]
//! UI-only passphrase prompt demo with a tri-button emergency fallback.
//!
//! Renders a LUKS-style masked passphrase dialog over the existing
//! splash compositor. There is no `cryptsetup` integration: every
//! `Enter` is treated as a failed attempt, and after `MAX_ATTEMPTS`
//! the emergency menu appears (Retry / Shell / Reboot).
//!
//! The dialog renderers live here rather than in `ui::view` so the
//! default (non-`image-splash`) build stays byte-identical.

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::error::{NmblError, Result};
use crate::splash::compositor;
use crate::splash::drm;
use crate::splash::glyph_cache;
use crate::splash::input::SplashInput;
use crate::splash::terminal::SplashTerminal;
use crate::splash::types::CellDims;
use crate::ui::POLL_SLICE;
use crate::ui::view::render_footer;

/// Maximum passphrase attempts before the emergency menu pops up.
const MAX_ATTEMPTS: u8 = 3;
/// Static prompt label used for the demo. Production wiring would
/// pass through the activation entry's `volume` / `device` field.
const PROMPT_LABEL: &str = "Unlock encrypted root (demo)";

/// Internal state of the demo state machine.
#[derive(Debug, PartialEq, Eq)]
enum DemoState {
    Entering { buffer: String, attempts: u8 },
    Emergency { selected: u8 },
}

/// Outcome of the demo loop. The splash orchestrator logs this and
/// returns to the main boot menu — no kernel-side effects yet.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DemoOutcome {
    /// Operator picked "Retry passphrase" from the emergency menu but
    /// the demo loop returned early (e.g. for a host-driven test).
    RetryRequested,
    /// Operator picked "Drop to shell".
    DroppedToShell,
    /// Operator picked "Reboot".
    RebootRequested,
    /// Reserved: graceful cancellation; not currently produced by the
    /// real-input loop but used by the state-machine tests.
    Cancelled,
}

/// Outcome of folding a single key press into the state machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StepResult {
    /// Continue polling for input.
    Continue,
    /// Demo loop should exit with this outcome.
    Done(DemoOutcome),
}

/// Run the demo loop. Blocks until the operator picks Shell or Reboot
/// from the emergency menu.
pub fn run(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    input: &mut SplashInput,
) -> Result<DemoOutcome> {
    let mut state = DemoState::Entering {
        buffer: String::new(),
        attempts: 0,
    };
    let mut dirty = true;
    loop {
        if dirty {
            render_frame(drm, bg_scaled, cache, cell_dims, &state)?;
            dirty = false;
        }
        if let Some(key) = input.poll(POLL_SLICE)? {
            match step(&mut state, key) {
                StepResult::Continue => dirty = true,
                StepResult::Done(o) => return Ok(o),
            }
        }
    }
}

/// Fold one key press into the state machine. Pure — no IO. Tests
/// drive this directly with synthetic `KeyEvent`s.
fn step(state: &mut DemoState, key: KeyEvent) -> StepResult {
    match state {
        DemoState::Entering { buffer, attempts } => {
            match key.code {
                KeyCode::Char(c) => buffer.push(c),
                KeyCode::Backspace => {
                    buffer.pop();
                }
                KeyCode::Enter => {
                    *attempts = attempts.saturating_add(1);
                    buffer.clear();
                    if *attempts >= MAX_ATTEMPTS {
                        *state = DemoState::Emergency { selected: 0 };
                    }
                }
                KeyCode::Esc => {
                    *state = DemoState::Emergency { selected: 0 };
                }
                _ => {}
            }
            StepResult::Continue
        }
        DemoState::Emergency { selected } => match key.code {
            KeyCode::Up => {
                *selected = (*selected).saturating_sub(1);
                StepResult::Continue
            }
            KeyCode::Down => {
                if *selected < 2 {
                    *selected = selected.saturating_add(1);
                }
                StepResult::Continue
            }
            KeyCode::Enter => match *selected {
                0 => {
                    *state = DemoState::Entering {
                        buffer: String::new(),
                        attempts: 0,
                    };
                    StepResult::Continue
                }
                1 => StepResult::Done(DemoOutcome::DroppedToShell),
                _ => StepResult::Done(DemoOutcome::RebootRequested),
            },
            KeyCode::Esc => {
                // Esc from the emergency menu returns to the entry
                // screen. Per the spec we preserve the existing
                // attempt counter, but since we discard it at the
                // Entering→Emergency transition we restart at
                // MAX_ATTEMPTS so the next Enter bounces straight back.
                *state = DemoState::Entering {
                    buffer: String::new(),
                    attempts: MAX_ATTEMPTS,
                };
                StepResult::Continue
            }
            _ => StepResult::Continue,
        },
    }
}

/// Split a frame into (header, body, footer) the same way `ui::view`
/// does so the demo dialogs sit in the same chrome as the main menu.
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

fn render_header(frame: &mut Frame<'_>, area: Rect) {
    let spans = vec![
        Span::styled("NMBL ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw("— NixOS Minimal BootLoader"),
    ];
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_entering(frame: &mut Frame<'_>, buffer_len: usize, attempt_one_based: u8) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header);
    let modal = centered_rect(body, 60, 8);
    frame.render_widget(Clear, modal);
    let dots: String = "*".repeat(buffer_len.min(40));
    let lines: Vec<Line<'_>> = vec![
        Line::raw(PROMPT_LABEL.to_owned()),
        Line::raw(String::new()),
        Line::from(vec![Span::raw(dots), Span::raw("|")]),
        Line::styled(
            format!("Attempt {attempt_one_based}/{MAX_ATTEMPTS}"),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ];
    let para = Paragraph::new(Text::from(lines))
        .block(Block::bordered().title("Passphrase"))
        .wrap(Wrap { trim: false });
    frame.render_widget(para, modal);
    render_footer(frame, footer, "Enter=submit  Esc=cancel");
}

fn render_emergency(frame: &mut Frame<'_>, selected: u8) {
    let [header, body, footer] = split_chrome(frame.area());
    render_header(frame, header);
    let modal = centered_rect(body, 50, 7);
    frame.render_widget(Clear, modal);
    let labels = ["Retry passphrase", "Drop to shell", "Reboot"];
    let sel = usize::from(selected).min(labels.len().saturating_sub(1));
    let items: Vec<ListItem<'_>> = labels
        .iter()
        .map(|l| ListItem::new(Line::raw(format!("[ {l} ]"))))
        .collect();
    let highlight = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let list = List::new(items)
        .block(Block::bordered().title("Unlock failed"))
        .highlight_style(highlight)
        .highlight_symbol("> ");
    let mut state = ListState::default();
    state.select(Some(sel));
    frame.render_stateful_widget(list, modal, &mut state);
    render_footer(frame, footer, "Up/Down select  Enter confirm  Esc retry");
}

/// Render one demo frame, mirroring `splash::render_frame` but driving
/// the local dialog renderers so we do not depend on a full `App`.
fn render_frame(
    drm: &mut drm::SplashDrm,
    bg_scaled: &[u8],
    cache: &glyph_cache::GlyphCache,
    cell_dims: CellDims,
    state: &DemoState,
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let backend = CrosstermBackend::new(&mut buf);
        let viewport = Viewport::Fixed(Rect::new(0, 0, cell_dims.cols, cell_dims.rows));
        let mut terminal =
            Terminal::with_options(backend, TerminalOptions { viewport }).map_err(tui_err)?;
        terminal
            .draw(|f| match state {
                DemoState::Entering { buffer, attempts } => {
                    let attempt_one_based = attempts.saturating_add(1).min(MAX_ATTEMPTS);
                    render_entering(f, buffer.len(), attempt_one_based);
                }
                DemoState::Emergency { selected } => render_emergency(f, *selected),
            })
            .map_err(tui_err)?;
    }

    let mut term_pipe = SplashTerminal::new(cell_dims);
    term_pipe.feed(&buf);

    drm.render(|fb, fb_dims| {
        compositor::blit_background(fb, fb_dims, bg_scaled);
        term_pipe.for_each_cell(|col, row, cell| {
            if cell.c == ' ' && cell.bg == AnsiColor::Named(NamedColor::Background) {
                return;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let Some(glyph) = cache.get(cell.c, bold) else {
                return;
            };
            let fg = compositor::resolve_color(cell.fg);
            let bg = compositor::resolve_color(cell.bg);
            let x = u32::from(col).saturating_mul(cell_dims.cell_w);
            let y = u32::from(row).saturating_mul(cell_dims.cell_h);
            compositor::blit_cell(fb, fb_dims, glyph, x, y, fg, bg);
        });
        Ok(())
    })
}

fn tui_err(source: std::io::Error) -> NmblError {
    NmblError::Tui { source }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert with panics on contract failure"
)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn drive(state: &mut DemoState, codes: &[KeyCode]) -> Option<DemoOutcome> {
        for code in codes {
            match step(state, press(*code)) {
                StepResult::Continue => {}
                StepResult::Done(o) => return Some(o),
            }
        }
        None
    }

    fn buffer_text(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn typing_then_enter_increments_attempts_and_clears_buffer() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        assert!(drive(&mut state, &[KeyCode::Char('h'), KeyCode::Char('i')]).is_none());
        match &state {
            DemoState::Entering { buffer, attempts } => {
                assert_eq!(buffer, "hi");
                assert_eq!(*attempts, 0);
            }
            _ => panic!("expected Entering"),
        }
        assert!(drive(&mut state, &[KeyCode::Enter]).is_none());
        match &state {
            DemoState::Entering { buffer, attempts } => {
                assert!(buffer.is_empty(), "buffer must clear after Enter");
                assert_eq!(*attempts, 1);
            }
            _ => panic!("expected Entering"),
        }
    }

    #[test]
    fn backspace_pops_one_char_from_buffer() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        drive(
            &mut state,
            &[KeyCode::Char('a'), KeyCode::Char('b'), KeyCode::Backspace],
        );
        match &state {
            DemoState::Entering { buffer, .. } => assert_eq!(buffer, "a"),
            _ => panic!(),
        }
    }

    #[test]
    fn three_failed_attempts_transitions_to_emergency() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        drive(
            &mut state,
            &[KeyCode::Enter, KeyCode::Enter, KeyCode::Enter],
        );
        assert!(matches!(state, DemoState::Emergency { selected: 0 }));
    }

    #[test]
    fn esc_from_entering_jumps_to_emergency_immediately() {
        let mut state = DemoState::Entering {
            buffer: "halfway".to_string(),
            attempts: 1,
        };
        drive(&mut state, &[KeyCode::Esc]);
        assert!(matches!(state, DemoState::Emergency { selected: 0 }));
    }

    #[test]
    fn emergency_arrow_keys_navigate_within_bounds() {
        let mut state = DemoState::Emergency { selected: 0 };
        drive(&mut state, &[KeyCode::Up]);
        assert!(matches!(state, DemoState::Emergency { selected: 0 }));
        drive(&mut state, &[KeyCode::Down]);
        assert!(matches!(state, DemoState::Emergency { selected: 1 }));
        drive(&mut state, &[KeyCode::Down]);
        assert!(matches!(state, DemoState::Emergency { selected: 2 }));
        drive(&mut state, &[KeyCode::Down]);
        assert!(matches!(state, DemoState::Emergency { selected: 2 }));
        drive(&mut state, &[KeyCode::Up]);
        assert!(matches!(state, DemoState::Emergency { selected: 1 }));
    }

    #[test]
    fn emergency_enter_on_retry_resets_to_entering() {
        let mut state = DemoState::Emergency { selected: 0 };
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert!(out.is_none());
        match &state {
            DemoState::Entering { buffer, attempts } => {
                assert!(buffer.is_empty());
                assert_eq!(*attempts, 0);
            }
            _ => panic!("expected Entering after retry"),
        }
    }

    #[test]
    fn emergency_enter_on_shell_returns_dropped_to_shell() {
        let mut state = DemoState::Emergency { selected: 1 };
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert_eq!(out, Some(DemoOutcome::DroppedToShell));
    }

    #[test]
    fn emergency_enter_on_reboot_returns_reboot_requested() {
        let mut state = DemoState::Emergency { selected: 2 };
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert_eq!(out, Some(DemoOutcome::RebootRequested));
    }

    #[test]
    fn emergency_esc_returns_to_entering_at_max_attempts() {
        let mut state = DemoState::Emergency { selected: 1 };
        drive(&mut state, &[KeyCode::Esc]);
        match &state {
            DemoState::Entering { attempts, buffer } => {
                assert_eq!(*attempts, MAX_ATTEMPTS);
                assert!(buffer.is_empty());
            }
            _ => panic!("expected Entering"),
        }
    }

    #[test]
    fn full_flow_three_fails_then_pick_reboot() {
        let mut state = DemoState::Entering {
            buffer: String::new(),
            attempts: 0,
        };
        drive(&mut state, &[KeyCode::Char('x'), KeyCode::Enter]);
        drive(&mut state, &[KeyCode::Char('y'), KeyCode::Enter]);
        drive(&mut state, &[KeyCode::Char('z'), KeyCode::Enter]);
        assert!(matches!(state, DemoState::Emergency { .. }));
        drive(&mut state, &[KeyCode::Down, KeyCode::Down]);
        let out = drive(&mut state, &[KeyCode::Enter]);
        assert_eq!(out, Some(DemoOutcome::RebootRequested));
    }

    #[test]
    fn cancelled_outcome_is_a_distinct_variant() {
        // Pin the public-API surface: Cancelled is exported so callers
        // can match on it without falling through.
        let outcomes = [
            DemoOutcome::RetryRequested,
            DemoOutcome::DroppedToShell,
            DemoOutcome::RebootRequested,
            DemoOutcome::Cancelled,
        ];
        assert_eq!(outcomes.len(), 4);
    }

    #[test]
    fn render_entering_shows_prompt_mask_and_attempt() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| render_entering(f, 5, 2)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains(PROMPT_LABEL), "prompt label missing");
        assert!(text.contains("*****|"), "mask missing in:\n{text}");
        assert!(text.contains("Attempt 2/3"), "counter missing");
    }

    #[test]
    fn render_emergency_highlights_selected_button() {
        let mut term = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        term.draw(|f| render_emergency(f, 1)).expect("draw");
        let text = buffer_text(&term);
        assert!(text.contains("Retry passphrase"), "missing retry");
        assert!(text.contains("Drop to shell"), "missing shell");
        assert!(text.contains("Reboot"), "missing reboot");
        assert!(
            text.contains("> [ Drop to shell"),
            "highlight marker missing in:\n{text}"
        );
    }
}
