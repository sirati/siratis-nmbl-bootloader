//! Frame rendering and console-resize handling for the pretty-shell.

use alacritty_terminal::grid::Dimensions;

use crate::error::Result;
use crate::nmbl_warn;
use crate::ui::console::Console;
use crate::ui::view::{PtyShellScreenData, render_pty_shell};

use super::state::{GridSize, PtyShellState};
use super::{CHROME_COLS, CHROME_ROWS, PRETTY_SHELL_MIN_COLS, PRETTY_SHELL_MIN_ROWS};

/// Render one frame. Backends use `draw_with` because the alacritty
/// grid is dynamic content that doesn't map neatly onto the
/// `App`-typed `Console::render` path.
pub(super) fn render(state: &PtyShellState, console: &mut dyn Console) -> Result<()> {
    let cols = state.cols;
    let rows = state.rows;
    let scroll = state.scroll_offset();
    let grid_rows = collect_visible_rows(state);
    let data = PtyShellScreenData {
        cols,
        rows,
        rows_text: &grid_rows,
        scroll_offset: scroll,
    };
    console.draw_with(&mut |frame| render_pty_shell(frame, &data))
}

/// Snapshot the visible portion of the alacritty grid as a vector of
/// per-row strings. The row count equals `state.rows` so the renderer
/// can paint each row at a fixed position without scanning bounds.
pub(super) fn collect_visible_rows(state: &PtyShellState) -> Vec<String> {
    let grid = state.term.grid();
    let cols = grid.columns();
    let rows = grid.screen_lines();
    // `Grid`'s `Index<Point>` ignores the scrollback `display_offset`:
    // `Line(0)` is always the top of the *live* viewport regardless of
    // how far the operator has scrolled back. To render the DISPLAYED
    // region we shift every line up by the offset, so `display_offset =
    // N` shows the screenful that starts `N` rows above the live tail.
    // The shifted lines stay within `[topmost_line, bottommost_line]`
    // because `scroll_display` clamps the offset to the history size.
    let offset = grid.display_offset() as i32;
    let mut out: Vec<String> = Vec::with_capacity(rows);
    for row in 0..rows {
        let line_idx = row as i32 - offset;
        let mut line = String::with_capacity(cols);
        for col in 0..cols {
            let point = alacritty_terminal::index::Point::new(
                alacritty_terminal::index::Line(line_idx),
                alacritty_terminal::index::Column(col),
            );
            let cell = &grid[point];
            // Treat NUL as space so the line is renderable.
            let c = if cell.c == '\0' { ' ' } else { cell.c };
            line.push(c);
        }
        out.push(line);
    }
    out
}

/// Derive the pretty-shell grid geometry from the current console
/// frame size, the same way [`super::driver::run_pretty_shell`] does at startup.
fn grid_size_from_console(console: &dyn Console) -> (u16, u16) {
    let (frame_cols, frame_rows) = console.size();
    let cols = frame_cols
        .saturating_sub(CHROME_COLS)
        .max(PRETTY_SHELL_MIN_COLS);
    let rows = frame_rows
        .saturating_sub(CHROME_ROWS)
        .max(PRETTY_SHELL_MIN_ROWS);
    (cols, rows)
}

/// React to a host-terminal resize: re-derive the grid geometry from
/// the (already-updated) console size, resize the alacritty emulator
/// grid, update the cached `state.cols`/`state.rows`, and push the new
/// winsize down to the PTY so the child shell and any full-screen
/// program running on it get `SIGWINCH`. Returns `true` when the grid
/// actually changed (so the caller should repaint).
pub(super) fn apply_resize(state: &mut PtyShellState, console: &dyn Console) -> bool {
    let (cols, rows) = grid_size_from_console(console);
    if cols == state.cols && rows == state.rows {
        return false;
    }
    let size = GridSize {
        columns: cols as usize,
        screen_lines: rows as usize,
    };
    state.term.resize(size);
    state.cols = cols;
    state.rows = rows;
    // Best-effort: the in-process grid has already reflowed; a failure
    // here only means the child keeps stale `$LINES`/`$COLUMNS`.
    if let Err(e) = state.child.resize(cols, rows) {
        nmbl_warn!("pretty-shell PTY winsize update to {cols}x{rows} failed: {e}");
    }
    true
}
