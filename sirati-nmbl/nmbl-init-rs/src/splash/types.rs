//! Shared types used across the splash submodules.
//!
//! Keeping these in one place lets the per-phase subagents fill in
//! their modules in parallel without redefining colliding types.

/// Terminal grid geometry chosen at runtime from the framebuffer
/// dimensions and the cell size derived from the loaded font.
#[derive(Copy, Clone, Debug)]
pub struct CellDims {
    pub cols: u16,
    pub rows: u16,
    pub cell_w: u32,
    pub cell_h: u32,
}

/// Pixel size of a single terminal cell, derived from the font.
/// Independent of how many cells fit in the framebuffer (that's
/// [`CellDims`]).
#[derive(Copy, Clone, Debug)]
pub struct CellSize {
    pub w: u32,
    pub h: u32,
}

/// 8-bit RGBA color. simpledrm exposes XRGB8888 so the alpha byte is
/// ignored on flip, but the compositor uses it for src-over math.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RgbaColor(pub u8, pub u8, pub u8, pub u8);

/// Coverage bitmap for a rasterized glyph.
/// `coverage[y * width + x]` is the alpha byte: 0 = transparent,
/// 255 = opaque.
#[derive(Debug)]
pub struct GlyphBitmap {
    pub width: u32,
    pub height: u32,
    pub coverage: Vec<u8>,
}

/// Framebuffer dimensions returned by the DRM bring-up. `stride` is
/// the byte distance between successive scanlines (`>= w * 4`).
#[derive(Copy, Clone, Debug)]
pub struct FramebufferDims {
    pub w: u32,
    pub h: u32,
    pub stride: u32,
}
