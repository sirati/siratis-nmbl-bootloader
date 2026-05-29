use crate::splash::types::{FramebufferDims, GlyphBitmap, RgbaColor};

use super::CellRect;
use super::blit::{read_bgrx, src_over, write_bgrx};

/// Intermediate foreground-text layer.
///
/// Mirrors [`super::halo::HaloMask`]'s bbox-bounded, framebuffer-sized
/// scratch approach but stores, per pixel, a resolved COLOR + a single
/// resolved ALPHA instead of a coverage byte. Foreground glyphs are
/// stamped here (no compositing onto the background during the pass),
/// then the whole layer is composited onto the framebuffer ONCE.
///
/// The point is to lay each pixel's foreground down exactly once. Where
/// two semi-transparent glyphs overlap (box-drawing runs, adjacent
/// chars bleeding a pixel at cell joins) the old per-glyph path
/// alpha-composited the same 60% white twice, brightening the overlap
/// into a bright dot. Here overlaps combine by **MAX alpha** (never
/// additively), so two identical 60%-white glyphs resolve to a single
/// 60% pixel — no doubled dot — while a non-overlapping pixel is bit for
/// bit what the old per-glyph composite produced.
pub struct TextLayer {
    /// Per-pixel RGB, row-major `w * h * 3`. Only meaningful where the
    /// matching `alpha` slot is non-zero.
    color: Vec<u8>,
    /// Per-pixel resolved alpha, row-major `w * h`. 0 = untouched.
    alpha: Vec<u8>,
    w: usize,
    h: usize,
    /// Inclusive bbox of touched pixels, `None` until the first stamp.
    /// Bounds the composite region (text doesn't blur → no spread).
    bbox: Option<(usize, usize, usize, usize)>,
}

impl TextLayer {
    /// Allocate a zeroed layer matching the framebuffer pixel
    /// dimensions. Like [`super::halo::HaloMask::new`] the buffer is
    /// full-frame for O(1) addressing but composite cost is bounded to
    /// the stamped bbox.
    #[must_use]
    pub fn new(fb_dims: FramebufferDims) -> Self {
        let w = fb_dims.w as usize;
        let h = fb_dims.h as usize;
        let pixels = w.saturating_mul(h);
        Self {
            color: vec![0u8; pixels.saturating_mul(3)],
            alpha: vec![0u8; pixels],
            w,
            h,
            bbox: None,
        }
    }

    /// Stamp one glyph's coverage into the layer using the same
    /// positioning and clipping as the old `blit_cell` glyph stage: the
    /// bitmap sits at `(cell.x + offset_x, cell.y + offset_y)` and
    /// out-of-framebuffer pixels are clipped. A space (empty glyph) or a
    /// fully-transparent `fg` contributes nothing.
    ///
    /// Per pixel the effective alpha is `round(coverage * fg.alpha /
    /// 255)`. Overlaps combine by MAX: if the new alpha exceeds the
    /// pixel's current alpha, BOTH the alpha and the stored color are
    /// overwritten so the higher-coverage contributor wins its color.
    /// Alpha is never additively accumulated.
    pub fn stamp(&mut self, glyph: &GlyphBitmap, cell: CellRect, fg: RgbaColor) {
        let RgbaColor(fr, fg_g, fb_c, fa) = fg;
        if fa == 0 || self.w == 0 || self.h == 0 {
            return;
        }
        let gw = glyph.width as usize;
        let gh = glyph.height as usize;
        if gw == 0 || gh == 0 {
            return;
        }
        let fa16 = u16::from(fa);
        let base_x = i64::from(cell.x) + i64::from(glyph.offset_x);
        let base_y = i64::from(cell.y) + i64::from(glyph.offset_y);
        for gy in 0..gh {
            let dy = base_y + gy as i64;
            if dy < 0 || dy as u64 >= self.h as u64 {
                continue;
            }
            let dy = dy as usize;
            let row = dy.saturating_mul(self.w);
            let cov_row = gy.saturating_mul(gw);
            for gx in 0..gw {
                let coverage = glyph
                    .coverage
                    .get(cov_row.saturating_add(gx))
                    .copied()
                    .unwrap_or(0);
                if coverage == 0 {
                    continue;
                }
                let dx = base_x + gx as i64;
                if dx < 0 || dx as u64 >= self.w as u64 {
                    continue;
                }
                let dx = dx as usize;
                // (coverage * fa + 127) / 255 — round-to-nearest, the
                // identical rounding the old per-glyph stage used so a
                // non-overlapping pixel lands on the same effective
                // alpha (and thus the same composite) as before.
                let effective = ((u16::from(coverage).saturating_mul(fa16)) + 127) / 255;
                let effective = if effective > 255 {
                    255u8
                } else {
                    effective as u8
                };
                if effective == 0 {
                    continue;
                }
                // MAX-combine: only the strongest contributor at this
                // pixel survives, taking its color with it. Two equal
                // 60%-white glyphs therefore resolve to one 60% pixel.
                let idx = row.saturating_add(dx);
                let cur = self.alpha.get(idx).copied().unwrap_or(0);
                if effective > cur {
                    if let Some(slot) = self.alpha.get_mut(idx) {
                        *slot = effective;
                    }
                    let coff = idx.saturating_mul(3);
                    if let Some([r, g, b]) = self.color.get_mut(coff..coff.saturating_add(3)) {
                        *r = fr;
                        *g = fg_g;
                        *b = fb_c;
                    }
                    self.bbox = Some(match self.bbox {
                        None => (dx, dy, dx, dy),
                        Some((x0, y0, x1, y1)) => (x0.min(dx), y0.min(dy), x1.max(dx), y1.max(dy)),
                    });
                }
            }
        }
    }

    /// Composite the whole layer onto the framebuffer ONCE: for every
    /// touched pixel with alpha > 0, run a single Oklab [`src_over`] of
    /// the stored color at the resolved alpha. Only the stamped bbox is
    /// walked, so an empty or small text region costs accordingly.
    ///
    /// Must be called *after* the cell-background fills so the text sits
    /// on top of any selection highlight.
    pub fn composite_onto(&self, fb: &mut [u8], fb_dims: FramebufferDims) {
        let Some((bx0, by0, bx1, by1)) = self.bbox else {
            return;
        };
        if self.w == 0 || self.h == 0 {
            return;
        }
        let stride = fb_dims.stride as usize;
        for py in by0..=by1 {
            if py as u64 >= u64::from(fb_dims.h) {
                break;
            }
            let row_off = py.saturating_mul(stride);
            let layer_row = py.saturating_mul(self.w);
            for px in bx0..=bx1 {
                if px as u64 >= u64::from(fb_dims.w) {
                    break;
                }
                let idx = layer_row.saturating_add(px);
                let a = self.alpha.get(idx).copied().unwrap_or(0);
                if a == 0 {
                    continue;
                }
                let coff = idx.saturating_mul(3);
                let Some(&[sr, sg, sb]) = self.color.get(coff..coff.saturating_add(3)) else {
                    continue;
                };
                let pix_off = row_off.saturating_add(px.saturating_mul(4));
                let Some(dst) = fb.get_mut(pix_off..pix_off.saturating_add(4)) else {
                    continue;
                };
                let (dr, dg, db) = read_bgrx(dst);
                let (nr, ng, nb) = src_over(sr, sg, sb, a, dr, dg, db);
                write_bgrx(dst, nr, ng, nb);
            }
        }
    }
}
