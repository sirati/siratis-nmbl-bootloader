use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor};
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
use crate::splash::terminal::SplashTerminal;
use crate::splash::types::CellDims;
use crate::ui::view::render_footer;

use super::{DemoState, MAX_ATTEMPTS, PROMPT_LABEL};

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

pub(super) fn render_entering(frame: &mut Frame<'_>, buffer_len: usize, attempt_one_based: u8) {
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

pub(super) fn render_emergency(frame: &mut Frame<'_>, selected: u8) {
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
pub(super) fn render_frame(
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
                DemoState::Emergency { selected, .. } => render_emergency(f, *selected),
            })
            .map_err(tui_err)?;
    }

    let mut term_pipe = SplashTerminal::new(cell_dims);
    term_pipe.feed(&buf);

    drm.render(|fb, fb_dims| {
        compositor::blit_background(fb, fb_dims, bg_scaled);
        // Pass 1: build ONE unified text-coverage mask from every cell
        // that wants the halo (transparent default bg + inked glyph),
        // then blur + composite the dark contrast halo a single time.
        // Painted before any glyph so it only darkens the background
        // photo, never adjacent drawn text; the mask uses max-combine so
        // overlapping glyphs union (no rings / no double-darkening).
        let mut halo = compositor::HaloMask::new(fb_dims);
        term_pipe.for_each_cell(|col, row, cell| {
            if !compositor::wants_halo(cell.bg) {
                return;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let Some(glyph) = cache.get(cell.c, bold) else {
                return;
            };
            let x = u32::from(col).saturating_mul(cell_dims.cell_w);
            let y = u32::from(row).saturating_mul(cell_dims.cell_h);
            let rect = compositor::CellRect {
                x,
                y,
                w: cell_dims.cell_w,
                h: cell_dims.cell_h,
            };
            halo.stamp(glyph, rect);
        });
        halo.composite_onto(fb, fb_dims);
        // Pass 2: cell-background fills first, then all glyphs collected
        // into one text layer composited once on top (mirrors
        // ui::render_splash_frame_with — kills the doubled "white dots").
        let mut text_layer = compositor::TextLayer::new(fb_dims);
        term_pipe.for_each_cell(|col, row, cell| {
            if cell.c == ' ' && cell.bg == AnsiColor::Named(NamedColor::Background) {
                return;
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let Some(glyph) = cache.get(cell.c, bold) else {
                return;
            };
            let fg = compositor::resolve_color(cell.fg);
            let bg = compositor::resolve_bg_color(cell.bg);
            let x = u32::from(col).saturating_mul(cell_dims.cell_w);
            let y = u32::from(row).saturating_mul(cell_dims.cell_h);
            let rect = compositor::CellRect {
                x,
                y,
                w: cell_dims.cell_w,
                h: cell_dims.cell_h,
            };
            compositor::fill_cell_bg(fb, fb_dims, rect, bg);
            text_layer.stamp(glyph, rect, fg);
        });
        text_layer.composite_onto(fb, fb_dims);
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::PROMPT_LABEL;
    use super::{render_emergency, render_entering};

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
