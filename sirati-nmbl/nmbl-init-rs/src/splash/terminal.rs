//! Headless terminal pipeline.
//!
//! Pipes ratatui-rendered bytes through an `alacritty_terminal::Term`
//! so the compositor can walk the resulting cell grid. No PTY, no
//! child process, no event loop — `VoidListener` no-ops every event
//! the `Term` would otherwise raise, and `vte::ansi::Processor`
//! consumes the bytes ratatui's `CrosstermBackend` would have written
//! to `/dev/console`.

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::vte::ansi::Processor;

use crate::splash::types::CellDims;

/// `Dimensions` impl for our `CellDims` so we don't have to pull in
/// `alacritty_terminal::term::test::TermSize` (which is `cfg(test)`-
/// gated upstream).
struct GridSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Wraps an `alacritty_terminal::Term` plus its `vte` parser.
///
/// `feed` drives the parser; `for_each_cell` walks the resulting
/// visible grid for the compositor. The Term is held by value (no
/// heap, no Arc) — its grid storage lives inside it.
pub struct SplashTerminal {
    term: Term<VoidListener>,
    parser: Processor,
    cols: u16,
    rows: u16,
}

impl SplashTerminal {
    /// Build a Term sized to the given grid.
    ///
    /// The cell pixel size carried by `CellDims` is irrelevant here —
    /// only `cols × rows` reach the terminal model. The compositor
    /// later multiplies by `cell_w` / `cell_h` to project cells onto
    /// the framebuffer.
    pub fn new(dims: CellDims) -> Self {
        let size = GridSize {
            columns: dims.cols as usize,
            screen_lines: dims.rows as usize,
        };
        let term = Term::new(Config::default(), &size, VoidListener);
        Self {
            term,
            parser: Processor::new(),
            cols: dims.cols,
            rows: dims.rows,
        }
    }

    /// Push parsed bytes from a ratatui frame into the underlying Term.
    ///
    /// `vte::ansi::Processor::advance` already iterates the slice
    /// internally, parsing each byte through the VT state machine and
    /// dispatching writes/SGR attributes/cursor moves into the Term.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Visible grid width in cells.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Visible grid height in cells.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Walk every visible cell in row-major order.
    ///
    /// The callback receives `(col, row, &Cell)` with `col ∈ 0..cols`
    /// and `row ∈ 0..rows`. The compositor uses this to project each
    /// cell onto its `(col * cell_w, row * cell_h)` framebuffer slot.
    ///
    /// We index the grid directly via `Point::new(Line, Column)`
    /// rather than walking `display_iter()`. The iterator starts
    /// *before* the first visible cell and increments inside `next`,
    /// so its first `Indexed.point` is `(line=0, col=1)` — a footgun
    /// when the caller wants `(0, 0)` to be the top-left character.
    /// Direct indexing is total (every `(row, col)` in the declared
    /// dimensions is in-bounds by construction of the grid).
    pub fn for_each_cell<F>(&self, mut f: F)
    where
        F: FnMut(u16, u16, &Cell),
    {
        let grid = self.term.grid();
        let cols = grid.columns();
        let rows = grid.screen_lines();
        for row in 0..rows {
            for col in 0..cols {
                let point = Point::new(Line(row as i32), Column(col));
                let cell: &Cell = &grid[point];
                // `rows`/`cols` came from the Term we built with u16
                // dimensions, so the cast back is lossless.
                f(col as u16, row as u16, cell);
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests"
)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    fn small_dims() -> CellDims {
        CellDims {
            cols: 10,
            rows: 3,
            cell_w: 8,
            cell_h: 16,
        }
    }

    #[test]
    fn test_new_grid_is_blank() {
        let term = SplashTerminal::new(small_dims());
        assert_eq!(term.cols(), 10);
        assert_eq!(term.rows(), 3);
        // A fresh grid contains default cells: spaces, no flags.
        let mut seen = 0u32;
        term.for_each_cell(|_, _, cell| {
            assert_eq!(cell.c, ' ', "fresh grid cell must be a space");
            assert!(cell.flags.is_empty(), "fresh grid cell must have no flags");
            seen += 1;
        });
        assert_eq!(
            seen,
            10 * 3,
            "for_each_cell must visit every cell exactly once"
        );
    }

    #[test]
    fn test_feed_bold_red_hello_sets_cell_attrs() {
        // SGR 1;31 = bold + red foreground; SGR 0 resets after.
        // After processing, cell (0, 0) must hold 'H', red fg, bold.
        let mut term = SplashTerminal::new(small_dims());
        term.feed(b"\x1b[1;31mHELLO\x1b[0m\n");

        let mut head: Option<Cell> = None;
        term.for_each_cell(|col, row, cell| {
            if col == 0 && row == 0 {
                head = Some(cell.clone());
            }
        });
        let head = head.expect("cell (0, 0) must exist in a 10x3 grid");

        assert_eq!(head.c, 'H', "expected 'H' at (0, 0), got {:?}", head.c);
        assert_eq!(
            head.fg,
            Color::Named(NamedColor::Red),
            "expected red foreground, got {:?}",
            head.fg
        );
        assert!(
            head.flags.contains(Flags::BOLD),
            "expected BOLD flag, got {:?}",
            head.flags
        );
    }

    #[test]
    fn test_feed_hello_rest_of_word() {
        // Sanity-check that the parser walks past the first cell.
        let mut term = SplashTerminal::new(small_dims());
        term.feed(b"\x1b[1;31mHELLO\x1b[0m\n");

        let expected = ['H', 'E', 'L', 'L', 'O'];
        let mut got: Vec<char> = Vec::with_capacity(5);
        term.for_each_cell(|col, row, cell| {
            if row == 0 && (col as usize) < expected.len() {
                got.push(cell.c);
            }
        });
        assert_eq!(got, expected, "HELLO must land in row-0 cells 0..5");
    }
}
