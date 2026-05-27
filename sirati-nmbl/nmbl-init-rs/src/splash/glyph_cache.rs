//! Pre-rasterized glyph cache backed by `fontdue`.
//!
//! Loads the configured font once, rasterizes ASCII printable plus
//! the box-drawing subset ratatui uses, and exposes a `(char, bold)`
//! → `GlyphBitmap` lookup. The cell dimensions are derived from the
//! font's horizontal advance and line metrics so the compositor can
//! place each grid cell deterministically.
//!
//! Only the predefined character set is rasterized; lookups for
//! unknown characters return `None` so the compositor can render an
//! empty cell. There is no eviction or dynamic insertion path, which
//! keeps the cache bounded and lock-free.
//!
//! The placeholder font shipped in `tests/data` has no separate bold
//! face, so bold is synthesised by stamping the regular coverage
//! over itself shifted one pixel to the right and saturating each
//! pixel at 255. This matches what ratatui's renderer expects: bold
//! draws a slightly heavier stroke without changing the cell box.

use std::collections::HashMap;
use std::path::Path;

use fontdue::{Font, FontSettings};

use crate::error::{NmblError, Result};
use crate::splash::types::{CellDims, GlyphBitmap};

/// Box-drawing glyphs ratatui renders with the default `Borders` and
/// `BorderType::Plain`. Kept in one constant so the test set and the
/// rasterizer agree.
const BOX_DRAWING: &[char] = &[
    '\u{2500}', // ─
    '\u{2502}', // │
    '\u{250C}', // ┌
    '\u{2510}', // ┐
    '\u{2514}', // └
    '\u{2518}', // ┘
    '\u{251C}', // ├
    '\u{2524}', // ┤
    '\u{252C}', // ┬
    '\u{2534}', // ┴
    '\u{253C}', // ┼
    '\u{258C}', // ▌
    '\u{2590}', // ▐
    '\u{2588}', // █
    '\u{2591}', // ░
    '\u{2592}', // ▒
    '\u{2593}', // ▓
];

/// Owns the rasterized glyphs and the cell dimensions derived from
/// the loaded font.
pub struct GlyphCache {
    cell_w: u32,
    cell_h: u32,
    glyphs: HashMap<(char, bool), GlyphBitmap>,
}

/// Round a non-negative `f32` to the nearest integer, half-up,
/// without ever returning a negative result.
fn round_half_up(v: f32) -> u32 {
    let r = (v + 0.5).floor();
    if r.is_finite() && r > 0.0 { r as u32 } else { 0 }
}

/// Build a `Tui` error from a `fontdue` static-string error message.
fn fontdue_err(stage: &str, e: &str) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::other(format!("fontdue: {stage}: {e}")),
    }
}

/// Rasterize one glyph at `px` and return its bitmap. Returns
/// `(width, height, coverage)` exactly as fontdue produced them, with
/// no padding to the cell box (the compositor positions the glyph
/// inside the cell).
fn rasterize_regular(font: &Font, c: char, px: f32) -> GlyphBitmap {
    let (metrics, coverage) = font.rasterize(c, px);
    GlyphBitmap {
        width: metrics.width as u32,
        height: metrics.height as u32,
        coverage,
    }
}

/// Emulate a bold weight by overlaying the regular glyph onto itself
/// shifted one pixel to the right and saturating at 255. The output
/// is one pixel wider than the source so the right-most stamp isn't
/// clipped; the compositor still places it at the same cell origin.
fn synthesize_bold(regular: &GlyphBitmap) -> GlyphBitmap {
    let src_w = regular.width as usize;
    let src_h = regular.height as usize;

    if src_w == 0 || src_h == 0 {
        return GlyphBitmap {
            width: regular.width,
            height: regular.height,
            coverage: Vec::new(),
        };
    }

    let dst_w = src_w + 1;
    let mut coverage = vec![0u8; dst_w * src_h];

    for y in 0..src_h {
        let src_row_start = y * src_w;
        let dst_row_start = y * dst_w;
        for x in 0..src_w {
            // Bounds are guaranteed by the loop ranges; use chunks_exact
            // to avoid clippy::indexing_slicing.
            let pixel = match regular.coverage.get(src_row_start + x) {
                Some(p) => *p,
                None => continue,
            };
            if let Some(slot) = coverage.get_mut(dst_row_start + x) {
                *slot = (*slot).saturating_add(pixel);
            }
            if let Some(slot) = coverage.get_mut(dst_row_start + x + 1) {
                *slot = (*slot).saturating_add(pixel);
            }
        }
    }

    GlyphBitmap {
        width: dst_w as u32,
        height: src_h as u32,
        coverage,
    }
}

/// Load `font_path` at `px` size, derive cell dimensions, and
/// pre-rasterize the ASCII printable plus box-drawing subset.
pub fn load(font_path: &Path, px: f32) -> Result<GlyphCache> {
    if !(px.is_finite() && px > 0.0) {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!("invalid font size {px}")),
        });
    }

    let bytes = std::fs::read(font_path).map_err(|source| NmblError::Io {
        source,
        context: format!("reading font {}", font_path.display()),
    })?;

    let font = Font::from_bytes(bytes, FontSettings::default())
        .map_err(|e| fontdue_err("from_bytes", e))?;

    // Cell width: use the 'M' glyph advance for deterministic sizing
    // even on proportional fonts; on the monospace face we ship this
    // matches `horizontal_line_metrics(px).advance_width` anyway.
    let m_metrics = font.metrics('M', px);
    let cell_w = round_half_up(m_metrics.advance_width);

    let line = font
        .horizontal_line_metrics(px)
        .ok_or_else(|| fontdue_err("horizontal_line_metrics", "missing"))?;
    let cell_h = round_half_up(line.ascent - line.descent + line.line_gap);

    if cell_w == 0 || cell_h == 0 {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "font produced degenerate cell ({cell_w}x{cell_h})"
            )),
        });
    }

    let mut glyphs: HashMap<(char, bool), GlyphBitmap> = HashMap::new();

    // ASCII printable 0x20..=0x7E.
    for code in 0x20u8..=0x7E {
        let c = code as char;
        let regular = rasterize_regular(&font, c, px);
        let bold = synthesize_bold(&regular);
        glyphs.insert((c, false), regular);
        glyphs.insert((c, true), bold);
    }

    // Box-drawing subset.
    for &c in BOX_DRAWING {
        let regular = rasterize_regular(&font, c, px);
        let bold = synthesize_bold(&regular);
        glyphs.insert((c, false), regular);
        glyphs.insert((c, true), bold);
    }

    Ok(GlyphCache {
        cell_w,
        cell_h,
        glyphs,
    })
}

impl GlyphCache {
    /// Cell dimensions derived from the loaded font. `cols`/`rows`
    /// are left at zero; the compositor divides the framebuffer
    /// dimensions by `cell_w`/`cell_h` to fill them in.
    pub fn cell_dims(&self) -> CellDims {
        CellDims {
            cols: 0,
            rows: 0,
            cell_w: self.cell_w,
            cell_h: self.cell_h,
        }
    }

    /// Look up a pre-rasterized glyph. Falls back to the regular
    /// weight if a bold variant isn't cached. Returns `None` for
    /// characters outside the predefined set.
    pub fn get(&self, c: char, bold: bool) -> Option<&GlyphBitmap> {
        if bold && let Some(g) = self.glyphs.get(&(c, true)) {
            return Some(g);
        }
        self.glyphs.get(&(c, false))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert with panics on contract failure"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Resolve the test font via the `NMBL_TEST_FONT` env var captured
    /// at compile time. The dev shell sets it to a DejaVu Sans Mono
    /// path from nixpkgs; outside the dev shell the variable is unset
    /// and the test skips cleanly rather than panicking.
    fn test_font() -> Option<PathBuf> {
        option_env!("NMBL_TEST_FONT").map(PathBuf::from)
    }

    #[test]
    fn load_synthetic_font() {
        let Some(path) = test_font() else {
            eprintln!("skipping: NMBL_TEST_FONT not set (run inside `nix develop`)");
            return;
        };
        let cache = match load(&path, 16.0) {
            Ok(c) => c,
            Err(e) => panic!("load() failed: {e}"),
        };

        let dims = cache.cell_dims();
        assert!(dims.cell_w > 0, "cell_w must be positive");
        assert!(dims.cell_h > 0, "cell_h must be positive");
        assert_eq!(dims.cols, 0);
        assert_eq!(dims.rows, 0);

        assert!(
            matches!(cache.get('A', false), Some(g) if g.width > 0 && g.height > 0),
            "regular 'A' glyph must be cached with non-zero extents"
        );
        assert!(
            matches!(cache.get('A', true), Some(g) if g.width > 0),
            "bold 'A' glyph must be cached"
        );

        // The bold synthesis stamps one extra column.
        if let (Some(reg), Some(bold)) = (cache.get('A', false), cache.get('A', true)) {
            assert_eq!(bold.width, reg.width + 1);
            assert_eq!(bold.height, reg.height);
            assert_eq!(bold.coverage.len(), (bold.width * bold.height) as usize);
        }

        // Unknown character returns None.
        assert!(cache.get('\u{1F600}', false).is_none());

        // Box-drawing subset is present.
        assert!(cache.get('\u{2500}', false).is_some());
        assert!(cache.get('\u{2502}', false).is_some());
    }

    #[test]
    fn rejects_invalid_size() {
        let Some(path) = test_font() else {
            eprintln!("skipping: NMBL_TEST_FONT not set (run inside `nix develop`)");
            return;
        };
        assert!(load(&path, 0.0).is_err());
        assert!(load(&path, -1.0).is_err());
        assert!(load(&path, f32::NAN).is_err());
    }

    #[test]
    fn missing_font_file_is_io_error() {
        let path = PathBuf::from("/nonexistent/font/file.ttf");
        let err = match load(&path, 16.0) {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        };
        assert!(matches!(err, NmblError::Io { .. }));
    }
}
