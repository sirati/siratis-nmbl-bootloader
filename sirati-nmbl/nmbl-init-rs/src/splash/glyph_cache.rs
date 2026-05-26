//! Pre-rasterized glyph cache backed by `fontdue`.
//!
//! Loads the configured font once, rasterizes ASCII printable plus
//! the box-drawing subset ratatui uses, and exposes a `(char, bold)`
//! → `GlyphBitmap` lookup. The cell dimensions are derived from the
//! font's horizontal advance and line metrics so the compositor can
//! place each grid cell deterministically.
//!
//! Skeleton only — Phase 4b fills in the body.

#![allow(dead_code, unused_variables)]

use std::path::Path;

use crate::error::{NmblError, Result};
use crate::splash::types::{CellDims, GlyphBitmap};

pub struct GlyphCache {
    _private: (),
}

pub fn load(_font_path: &Path, _px: f32) -> Result<GlyphCache> {
    Err(NmblError::Tui {
        source: std::io::Error::other("splash::glyph_cache not implemented"),
    })
}

impl GlyphCache {
    pub fn cell_dims(&self) -> CellDims {
        CellDims {
            cols: 0,
            rows: 0,
            cell_w: 0,
            cell_h: 0,
        }
    }

    pub fn get(&self, _c: char, _bold: bool) -> Option<&GlyphBitmap> {
        None
    }
}
