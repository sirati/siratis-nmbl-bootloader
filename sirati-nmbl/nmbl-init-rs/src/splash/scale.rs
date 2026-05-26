//! Nearest-neighbor cover-style image scaler.
//!
//! Scales an RGBA8 source so it fills the framebuffer while preserving
//! aspect ratio; overflow on the longer axis is cropped (cover-style,
//! like CSS `background-size: cover`). Sampling is strictly
//! nearest-neighbor — no interpolation, no extra dependency.
//!
//! Skeleton only — Phase 4a fills in the body.

#![allow(dead_code, unused_variables)]

use crate::splash::types::FramebufferDims;

pub fn cover_scale_nearest(
    _src: &[u8],
    _src_w: u32,
    _src_h: u32,
    _dst: FramebufferDims,
) -> Vec<u8> {
    Vec::new()
}
