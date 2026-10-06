//! Ratatui backend that renders into an in-memory output queue.
//!
//! The console fd is non-blocking (NMBL is a single-threaded PID 1 and
//! must never park on one slow terminal). termwiz's `UnixTerminal`
//! writes with `write_all` and keeps no record of a partial write, so a
//! full tty buffer (`EAGAIN`) surfaced as a fatal render error and a
//! retry would have re-sent bytes the terminal already received.
//!
//! This backend keeps the same termwiz pieces — a [`Surface`] that
//! computes the minimal change set and the [`TerminfoRenderer`] built
//! from NMBL's bundled terminfo — but renders into a byte queue that the
//! owner drains with non-blocking writes ([`QueuedBackend::pending`] /
//! [`QueuedBackend::consume`]). While bytes are still queued, new frames
//! only update the surface; their diff is rendered once the queue has
//! drained ([`QueuedBackend::render_deferred`]). A slow peer therefore
//! sees coalesced frames, never a torn or duplicated byte stream, and
//! the queue stays bounded by roughly one full repaint.

use std::io;

use ratatui::backend::{Backend, ClearType, IntoTermwiz, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::Modifier;
use termwiz::caps::Capabilities;
use termwiz::cell::{AttributeChange, Blink, Intensity, Underline};
use termwiz::render::RenderTty;
use termwiz::render::terminfo::TerminfoRenderer;
use termwiz::surface::{
    Change, CursorVisibility, Position as TermwizPosition, SequenceNo, Surface,
};

/// The rendered-but-unwritten bytes. `start` advances as writes succeed;
/// the vector is compacted once the queue empties.
struct OutQueue {
    bytes: Vec<u8>,
    start: usize,
}

impl OutQueue {
    fn pending(&self) -> &[u8] {
        self.bytes.get(self.start..).unwrap_or(&[])
    }
}

/// Write target handed to the termwiz renderer: appends to the queue and
/// reports the surface geometry (a serial line may report no winsize).
struct QueueSink<'a> {
    bytes: &'a mut Vec<u8>,
    size: (usize, usize),
}

impl io::Write for QueueSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl RenderTty for QueueSink<'_> {
    fn get_size_in_cells(&mut self) -> termwiz::Result<(usize, usize)> {
        Ok(self.size)
    }
}

pub(super) struct QueuedBackend {
    surface: Surface,
    /// Surface sequence number already rendered into the queue.
    rendered: SequenceNo,
    renderer: TerminfoRenderer,
    out: OutQueue,
}

impl QueuedBackend {
    pub(super) fn new(caps: Capabilities, cols: usize, rows: usize) -> Self {
        Self {
            surface: Surface::new(cols, rows),
            rendered: 0,
            renderer: TerminfoRenderer::new(caps),
            out: OutQueue {
                bytes: Vec::new(),
                start: 0,
            },
        }
    }

    /// Bytes rendered but not yet accepted by the terminal.
    pub(super) fn pending(&self) -> &[u8] {
        self.out.pending()
    }

    /// Record that the terminal accepted `n` more queued bytes.
    pub(super) fn consume(&mut self, n: usize) {
        self.out.start = self.out.start.saturating_add(n).min(self.out.bytes.len());
        if self.out.start == self.out.bytes.len() {
            self.out.bytes.clear();
            self.out.start = 0;
        }
    }

    /// Render surface changes that were deferred while the queue was
    /// busy. Returns whether new bytes were queued. A no-op while bytes
    /// are still pending, so frames coalesce instead of piling up.
    pub(super) fn render_deferred(&mut self) -> io::Result<bool> {
        if !self.pending().is_empty() || !self.surface.has_changes(self.rendered) {
            return Ok(false);
        }
        let (seq, changes) = self.surface.get_changes(self.rendered);
        let mut sink = QueueSink {
            bytes: &mut self.out.bytes,
            size: self.surface.dimensions(),
        };
        self.renderer
            .render_to(&changes, &mut sink)
            .map_err(io::Error::other)?;
        self.rendered = seq;
        self.surface.flush_changes_older_than(seq);
        Ok(!self.pending().is_empty())
    }

    /// Retarget the surface to a new grid. The next render repaints.
    pub(super) fn resize(&mut self, cols: usize, rows: usize) {
        self.surface.resize(cols, rows);
    }

    pub(super) fn dimensions(&self) -> (usize, usize) {
        self.surface.dimensions()
    }
}

fn u16_saturating(v: usize) -> u16 {
    u16::try_from(v).unwrap_or(u16::MAX)
}

impl Backend for QueuedBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Cell translation mirrors ratatui-termwiz's `TermwizBackend`.
        for (x, y, cell) in content {
            let m = cell.modifier;
            self.surface.add_changes(vec![
                Change::CursorPosition {
                    x: TermwizPosition::Absolute(usize::from(x)),
                    y: TermwizPosition::Absolute(usize::from(y)),
                },
                Change::Attribute(AttributeChange::Foreground(cell.fg.into_termwiz())),
                Change::Attribute(AttributeChange::Background(cell.bg.into_termwiz())),
                Change::Attribute(AttributeChange::Intensity(if m.contains(Modifier::BOLD) {
                    Intensity::Bold
                } else if m.contains(Modifier::DIM) {
                    Intensity::Half
                } else {
                    Intensity::Normal
                })),
                Change::Attribute(AttributeChange::Italic(m.contains(Modifier::ITALIC))),
                Change::Attribute(AttributeChange::Underline(
                    if m.contains(Modifier::UNDERLINED) {
                        Underline::Single
                    } else {
                        Underline::None
                    },
                )),
                Change::Attribute(AttributeChange::Reverse(m.contains(Modifier::REVERSED))),
                Change::Attribute(AttributeChange::Invisible(m.contains(Modifier::HIDDEN))),
                Change::Attribute(AttributeChange::StrikeThrough(
                    m.contains(Modifier::CROSSED_OUT),
                )),
                Change::Attribute(AttributeChange::Blink(
                    if m.contains(Modifier::SLOW_BLINK) {
                        Blink::Slow
                    } else if m.contains(Modifier::RAPID_BLINK) {
                        Blink::Rapid
                    } else {
                        Blink::None
                    },
                )),
            ]);
            self.surface.add_change(cell.symbol());
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.surface
            .add_change(Change::CursorVisibility(CursorVisibility::Hidden));
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.surface
            .add_change(Change::CursorVisibility(CursorVisibility::Visible));
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        let (x, y) = self.surface.cursor_position();
        Ok(Position::new(u16_saturating(x), u16_saturating(y)))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let Position { x, y } = position.into();
        self.surface.add_change(Change::CursorPosition {
            x: TermwizPosition::Absolute(usize::from(x)),
            y: TermwizPosition::Absolute(usize::from(y)),
        });
        Ok(())
    }

    fn clear(&mut self) -> io::Result<()> {
        self.surface
            .add_change(Change::ClearScreen(termwiz::color::ColorAttribute::Default));
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        match clear_type {
            ClearType::All => self.clear(),
            other => Err(io::Error::other(format!(
                "clear_type [{other:?}] not supported with this backend"
            ))),
        }
    }

    fn size(&self) -> io::Result<Size> {
        let (cols, rows) = self.surface.dimensions();
        Ok(Size::new(u16_saturating(cols), u16_saturating(rows)))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.size()?,
            pixels: Size::new(0, 0),
        })
    }

    /// Render the frame's changes into the queue unless earlier output is
    /// still pending; the owner writes the queue without blocking.
    fn flush(&mut self) -> io::Result<()> {
        self.render_deferred().map(|_| ())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert on contract failures")]
mod tests {
    use super::*;
    use crate::ui::console::tty::caps::caps_from_env_with_fallback;

    fn backend() -> QueuedBackend {
        QueuedBackend::new(caps_from_env_with_fallback().expect("caps"), 20, 4)
    }

    fn put(backend: &mut QueuedBackend, text: &str) {
        let mut cell = Cell::default();
        cell.set_symbol(text);
        backend.draw(std::iter::once((0, 0, &cell))).expect("draw");
    }

    #[test]
    fn frames_coalesce_while_output_is_pending() {
        let mut b = backend();
        put(&mut b, "A");
        b.flush().expect("flush");
        let first = b.pending().len();
        assert!(first > 0 && String::from_utf8_lossy(b.pending()).contains('A'));

        // The terminal accepted nothing yet: the next frame must not be
        // appended behind the first.
        put(&mut b, "B");
        b.flush().expect("flush");
        assert_eq!(
            b.pending().len(),
            first,
            "frame queued behind pending output"
        );

        // Partial acceptance never re-sends accepted bytes.
        b.consume(1);
        assert_eq!(b.pending().len(), first - 1);
        b.consume(first);
        assert!(b.pending().is_empty());

        // Drained: the coalesced diff renders, ending at the newest frame.
        assert!(b.render_deferred().expect("render"));
        let text = String::from_utf8_lossy(b.pending()).into_owned();
        assert!(text.contains('B') && !text.contains('A'), "got {text:?}");
    }
}
