//! Cell→pixel compositor.
//!
//! Blits the scaled PNG background into the framebuffer, then for
//! each terminal cell fills the cell rectangle with the resolved
//! background color (if non-transparent) and alpha-blends the glyph
//! coverage with the resolved foreground color on top of whatever is
//! already there (PNG background or filled cell bg).
//!
//! Pixel order: the framebuffer is XRGB8888 in DRM nomenclature, which
//! on little-endian hardware (the only thing the kernel's simpledrm
//! ever produces) lays the bytes out as `[B, G, R, X]`. The PNG decode
//! and the scaler both produce tight RGBA8 (`[R, G, B, A]`). The
//! compositor is therefore the one place where the byte swap happens.

mod blit;
mod colors;
mod halo;

#[cfg(test)]
mod tests;

pub use blit::{blit_background, blit_cell};
pub use colors::{resolve_bg_color, resolve_color};
pub use halo::{blit_halo, wants_halo};

/// Pixel rectangle describing one terminal cell on the framebuffer:
/// origin in pixels (`x`, `y`) plus dimensions in pixels (`w`, `h`).
/// Kept as a small POD so [`blit_cell`] stays under clippy's
/// `too_many_arguments` threshold.
#[derive(Copy, Clone, Debug)]
pub struct CellRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}
