//! Cell→pixel compositor.
//!
//! Blits the scaled PNG background into the framebuffer, then for
//! each terminal cell fills the cell rectangle with the resolved
//! background color and alpha-blends the glyph coverage with the
//! resolved foreground color on top.
//!
//! Skeleton only — Phase 4c fills in the body.

#![allow(dead_code, unused_variables)]

use crate::splash::types::{FramebufferDims, GlyphBitmap, RgbaColor};

/// Copy the scaled background RGBA buffer into the framebuffer,
/// respecting `fb_dims.stride`.
pub fn blit_background(_fb: &mut [u8], _fb_dims: FramebufferDims, _bg_rgba: &[u8]) {}

/// Fill the cell rectangle at (cell_x, cell_y) pixels with `bg`,
/// then alpha-blend the glyph in `fg` color over it.
pub fn blit_cell(
    _fb: &mut [u8],
    _fb_dims: FramebufferDims,
    _glyph: &GlyphBitmap,
    _cell_x: u32,
    _cell_y: u32,
    _fg: RgbaColor,
    _bg: RgbaColor,
) {
}
