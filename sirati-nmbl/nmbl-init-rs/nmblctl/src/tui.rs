//! Interactive generation pickers for `reboot-into` and `default`.
//!
//! Uses ratatui + crossterm (the same TUI stack NMBL's own boot menu uses) to
//! render a simple list the operator drives with the arrow keys and Enter. When
//! stdout/stdin is not a real terminal (a pipe, a test), it falls back to a
//! numbered stdin prompt so the command is still usable non-interactively-ish
//! and testable. Both paths return `Ok(Some(choice))`, `Ok(None)` on cancel.

use std::io::{IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style as RStyle};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use nmblctl::flags::DefaultSelection;

use crate::state::System;

/// One selectable row.
struct Row {
    label: String,
    /// The value the row maps to: a generation number, or None for "latest".
    generation: Option<u32>,
}

/// Pick a generation for `reboot-into`. Returns the chosen number, or `None`
/// if the operator cancelled.
pub fn select_generation(sys: &System, title: &str) -> Result<Option<u32>, String> {
    if sys.generations.is_empty() {
        return Err("no installed generations to choose from".to_string());
    }
    let rows: Vec<Row> = sys
        .generations
        .iter()
        .map(|&n| Row {
            label: row_label(sys, n),
            generation: Some(n),
        })
        .collect();
    let chosen = pick(&rows, title, sys)?;
    Ok(chosen.and_then(|r| rows.get(r).and_then(|row| row.generation)))
}

/// Pick a persistent default for `default`, including the artificial "latest".
pub fn select_default(sys: &System) -> Result<Option<DefaultSelection>, String> {
    let mut rows: Vec<Row> = vec![Row {
        label: "latest (always the newest generation)".to_string(),
        generation: None,
    }];
    rows.extend(sys.generations.iter().map(|&n| Row {
        label: row_label(sys, n),
        generation: Some(n),
    }));
    let title = format!(
        "Set the persistent default (current: {})",
        sys.read_default()
            .map(|d| d.to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    let chosen = pick(&rows, &title, sys)?;
    Ok(chosen.and_then(|idx| {
        rows.get(idx).map(|r| match r.generation {
            Some(n) => DefaultSelection::Generation(n),
            None => DefaultSelection::Latest,
        })
    }))
}

fn row_label(sys: &System, n: u32) -> String {
    let active = if sys.active_generation == Some(n) {
        "  [current]"
    } else {
        ""
    };
    format!("generation {n}{active}")
}

/// Drive the picker: ratatui when interactive, numbered stdin prompt otherwise.
/// Returns the chosen row index, or `None` on cancel.
fn pick(rows: &[Row], title: &str, sys: &System) -> Result<Option<usize>, String> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        pick_tui(rows, title, sys)
    } else {
        pick_prompt(rows, title)
    }
}

fn pick_tui(rows: &[Row], title: &str, sys: &System) -> Result<Option<usize>, String> {
    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            return Err(format!("terminal: {e}"));
        }
    };
    let mut state = ListState::default();
    // Preselect the current default / active generation.
    let preselect = sys
        .active_generation
        .and_then(|a| rows.iter().position(|r| r.generation == Some(a)))
        .unwrap_or(0);
    state.select(Some(preselect));

    let result = run_tui_loop(&mut terminal, rows, title, &mut state);
    let _ = disable_raw_mode();
    let _ = terminal.clear();
    result
}

fn run_tui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    rows: &[Row],
    title: &str,
    state: &mut ListState,
) -> Result<Option<usize>, String> {
    loop {
        terminal
            .draw(|frame| {
                let [header_area, list_area, help_area] = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(3),
                        Constraint::Min(1),
                        Constraint::Length(1),
                    ])
                    .areas(frame.area());
                let header = Paragraph::new(Line::from(vec![Span::styled(
                    title,
                    RStyle::default().add_modifier(Modifier::BOLD),
                )]))
                .block(Block::default().borders(Borders::ALL).title(" nmblctl "));
                frame.render_widget(header, header_area);

                let items: Vec<ListItem> = rows
                    .iter()
                    .map(|r| ListItem::new(r.label.clone()))
                    .collect();
                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL))
                    .highlight_style(
                        RStyle::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("▶ ");
                frame.render_stateful_widget(list, list_area, state);

                let help = Paragraph::new(Line::from(vec![Span::styled(
                    "↑/↓ move   Enter select   Esc/q cancel",
                    RStyle::default().fg(Color::DarkGray),
                )]));
                frame.render_widget(help, help_area);
            })
            .map_err(|e| format!("draw: {e}"))?;

        match event::read().map_err(|e| format!("input: {e}"))? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Up => move_selection(state, rows.len(), -1),
                KeyCode::Down => move_selection(state, rows.len(), 1),
                KeyCode::Enter => return Ok(state.selected()),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
                _ => {}
            },
            _ => {}
        }
    }
}

fn move_selection(state: &mut ListState, len: usize, delta: isize) {
    if len == 0 {
        return;
    }
    let cur = state.selected().unwrap_or(0) as isize;
    let next = (cur + delta).rem_euclid(len as isize) as usize;
    state.select(Some(next));
}

/// Non-TTY fallback: print a numbered list and read a choice from stdin.
fn pick_prompt(rows: &[Row], title: &str) -> Result<Option<usize>, String> {
    println!("{title}");
    for (i, r) in rows.iter().enumerate() {
        println!("  {}) {}", i + 1, r.label);
    }
    print!("choose [1-{}] or blank to cancel: ", rows.len());
    std::io::stdout()
        .flush()
        .map_err(|e| format!("stdout: {e}"))?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| format!("stdin: {e}"))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let n: usize = trimmed
        .parse()
        .map_err(|_| format!("not a number: {trimmed}"))?;
    if n >= 1 && n <= rows.len() {
        Ok(Some(n - 1))
    } else {
        Err(format!("choice {n} out of range"))
    }
}
