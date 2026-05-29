use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::splash::types::{FramebufferDims, GlyphBitmap};

use super::blit::{read_bgrx, src_over, write_bgrx};

/// Maximum alpha (0..255) of the pure-black halo composited behind
/// default-foreground glyphs. It feeds [`src_over`], whose Oklab mix
/// toward black scales the destination lightness by `(1 - alpha)`. Two
/// consequences fall out of that "multiply toward black" formulation:
///
///   * the absolute darkening is `L_bg * alpha`, so it shrinks to zero
///     as the background approaches black — the haze is invisible on
///     dark areas and strongest on bright ones (not a fixed-colour
///     linear-transparency overlay, which would lighten/flatten dark
///     pixels instead);
///   * the result lightness `L_bg * (1 - alpha)` is monotonic in the
///     background lightness, so at any fixed halo strength a darker
///     background can never end up brighter than a lighter one.
///
/// ~0.63 keeps the strongest core a deep grey rather than full black.
const HALO_MAX_ALPHA: u8 = 160;

/// Blur radius (per pass) of the halo spread, in pixels. Several
/// separable box passes (see [`HALO_PASSES`]) give a soft, slowly-fading
/// Gaussian-ish falloff. Kept small (relative to the old per-glyph value)
/// so the halo *hugs the combined text outline* instead of blooming a
/// thin `-` into a circular blob — the mask is the union of every glyph,
/// so the blur only needs to soften its edge, not invent shape.
const HALO_RADIUS: usize = 2;

/// Number of separable box-blur passes (each run as an H then V pair).
/// Two passes of radius 2 approximate a small Gaussian with a gentle
/// tail while keeping the spread tight to the letters.
const HALO_PASSES: usize = 2;

/// Total spread (in pixels) of the cumulative blur on each side. Each
/// box pass of radius `HALO_RADIUS` spreads coverage by `HALO_RADIUS`, so
/// `HALO_PASSES` passes reach `HALO_PASSES * HALO_RADIUS` pixels. Used to
/// expand the stamped bounding box before blurring so the whole tail is
/// computed without clipping.
pub(super) const HALO_SPREAD: usize = HALO_RADIUS * HALO_PASSES;

/// Whether a cell should get the dark contrast halo behind its glyph.
///
/// Keyed on the cell *background*: only cells whose background is the
/// terminal default (transparent — [`named_color`] resolves
/// [`NamedColor::Background`] to alpha 0, letting the PNG show through)
/// qualify. Every glyph drawn straight onto the photo therefore gets the
/// dark backing regardless of its foreground colour, so coloured text on
/// the image is legible too. Cells with an explicit, opaque background
/// (e.g. the selection highlight) paint their own backing and are left
/// alone. A blank cell (a space) contributes no ink to the [`HaloMask`],
/// so this never paints behind whitespace either way.
///
/// [`named_color`]: super::colors
pub fn wants_halo(bg: Color) -> bool {
    matches!(bg, Color::Named(NamedColor::Background))
}

/// A single framebuffer-sized coverage mask that unifies the ink of
/// every glyph that [`wants_halo`]. The dark contrast halo is derived
/// from this one mask — built, blurred, and composited *once* per frame
/// — rather than per glyph. That kills the old per-glyph defects:
///
///   * **no rings / no double-darkening** — overlapping glyph
///     contributions combine with `max` (a union), and the black is
///     composited a single time, so an overlap is darkened to the
///     stronger of the two, never twice;
///   * **shape-following** — blurring the union of the real glyph
///     outlines means a lone `-` yields a soft horizontal bar, not a
///     circular blob;
///   * **no bright-dot gaps** — the mask is the *same* coverage that
///     draws the text, so every ink pixel sits over a fully-stamped
///     (255) mask cell; there is never an untouched pixel between glyph
///     and halo.
///
/// The mask is stamped in framebuffer pixel space. To keep
/// [`HaloMask::composite_onto`] cheap (it runs every redraw on a buffer
/// up to 1920×1080), the bounding box of stamped pixels is tracked and
/// only that region — expanded by [`HALO_SPREAD`] — is blurred and
/// composited; the blur itself is an O(n) running-sum box blur.
pub struct HaloMask {
    mask: Vec<u8>,
    w: usize,
    h: usize,
    /// Inclusive bbox of stamped (non-zero) pixels, `None` until the
    /// first stamp. Bounds the blur/composite region.
    bbox: Option<(usize, usize, usize, usize)>,
}

impl HaloMask {
    /// Allocate a zeroed mask matching the framebuffer pixel dimensions.
    #[must_use]
    pub fn new(fb_dims: FramebufferDims) -> Self {
        let w = fb_dims.w as usize;
        let h = fb_dims.h as usize;
        Self {
            mask: vec![0u8; w.saturating_mul(h)],
            w,
            h,
            bbox: None,
        }
    }

    /// Stamp a glyph's coverage into the mask at the cell's pixel origin
    /// plus the glyph offset, combining with `max` so overlapping glyphs
    /// union instead of summing. Out-of-mask pixels are clipped. An empty
    /// glyph (a space) contributes nothing.
    pub fn stamp(&mut self, glyph: &GlyphBitmap, cell: super::CellRect) {
        let gw = glyph.width as usize;
        let gh = glyph.height as usize;
        if gw == 0 || gh == 0 || self.w == 0 || self.h == 0 {
            return;
        }
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
                let cov = glyph
                    .coverage
                    .get(cov_row.saturating_add(gx))
                    .copied()
                    .unwrap_or(0);
                if cov == 0 {
                    continue;
                }
                let dx = base_x + gx as i64;
                if dx < 0 || dx as u64 >= self.w as u64 {
                    continue;
                }
                let dx = dx as usize;
                if let Some(slot) = self.mask.get_mut(row.saturating_add(dx))
                    && cov > *slot
                {
                    *slot = cov;
                }
                self.bbox = Some(match self.bbox {
                    None => (dx, dy, dx, dy),
                    Some((x0, y0, x1, y1)) => (x0.min(dx), y0.min(dy), x1.max(dx), y1.max(dy)),
                });
            }
        }
    }

    /// Blur the mask once and composite the dark halo onto `fb`.
    ///
    /// For each masked pixel, pure black is composited through the Oklab
    /// [`src_over`] blend at `alpha = curve(blurred) * HALO_MAX_ALPHA`.
    /// Keeping that multiply-toward-black blend preserves the
    /// brightness-aware darkening: the effect weakens as the background
    /// approaches black (it darkens by `L_bg * alpha`) and is monotonic
    /// in background lightness, so a darker pixel can never end up
    /// brighter than a lighter one at equal halo strength.
    ///
    /// Only the stamped bounding box, expanded by [`HALO_SPREAD`] (so the
    /// full blur tail is included), is touched — distant pixels are left
    /// pristine. Must be called *after* [`super::blit_background`] and
    /// *before* the per-cell text pass.
    pub fn composite_onto(&self, fb: &mut [u8], fb_dims: FramebufferDims) {
        let Some((bx0, by0, bx1, by1)) = self.bbox else {
            return;
        };
        if self.w == 0 || self.h == 0 {
            return;
        }
        // Expand the bbox by the cumulative blur spread, clamped to the
        // mask, and work in a tight scratch buffer covering only it.
        let rx0 = bx0.saturating_sub(HALO_SPREAD);
        let ry0 = by0.saturating_sub(HALO_SPREAD);
        let rx1 = (bx1.saturating_add(HALO_SPREAD)).min(self.w.saturating_sub(1));
        let ry1 = (by1.saturating_add(HALO_SPREAD)).min(self.h.saturating_sub(1));
        let rw = rx1.saturating_sub(rx0).saturating_add(1);
        let rh = ry1.saturating_sub(ry0).saturating_add(1);
        let Some(area) = rw.checked_mul(rh) else {
            return;
        };

        // Copy the mask sub-region into a tight buffer.
        let mut field = vec![0u8; area];
        for y in 0..rh {
            let src_row = (ry0.saturating_add(y))
                .saturating_mul(self.w)
                .saturating_add(rx0);
            let dst_row = y.saturating_mul(rw);
            for x in 0..rw {
                if let (Some(&s), Some(d)) = (
                    self.mask.get(src_row.saturating_add(x)),
                    field.get_mut(dst_row.saturating_add(x)),
                ) {
                    *d = s;
                }
            }
        }

        // Separable running-sum box passes (O(n) per pass): H then V.
        let mut scratch = vec![0u8; area];
        for _ in 0..HALO_PASSES {
            box_blur_h(&field, &mut scratch, rw, rh, HALO_RADIUS);
            box_blur_v(&scratch, &mut field, rw, rh, HALO_RADIUS);
        }

        // Composite black, alpha from the curved blurred coverage.
        let stride = fb_dims.stride as usize;
        for y in 0..rh {
            let py = ry0.saturating_add(y);
            if py as u64 >= u64::from(fb_dims.h) {
                break;
            }
            let row_off = py.saturating_mul(stride);
            let field_row = y.saturating_mul(rw);
            for x in 0..rw {
                let v = field.get(field_row.saturating_add(x)).copied().unwrap_or(0);
                if v == 0 {
                    continue;
                }
                let px = rx0.saturating_add(x);
                if px as u64 >= u64::from(fb_dims.w) {
                    break;
                }
                // Gentle concave curve (gamma 0.5) for a soft tail. The
                // mask is 255 under ink, and sqrt(1)=1, so the core stays
                // at full HALO_MAX_ALPHA — no dead zone under the glyph.
                // This shapes only the spatial alpha; the Oklab
                // multiply-toward-black blend below is untouched, so the
                // brightness-monotonicity / weaker-on-dark property holds.
                let shaped = (f32::from(v) / 255.0).sqrt();
                let alpha = (shaped * f32::from(HALO_MAX_ALPHA)).round();
                let alpha = if alpha >= 255.0 {
                    255u8
                } else if alpha <= 0.0 {
                    0u8
                } else {
                    alpha as u8
                };
                if alpha == 0 {
                    continue;
                }
                let pix_off = row_off.saturating_add(px.saturating_mul(4));
                let Some(dst) = fb.get_mut(pix_off..pix_off.saturating_add(4)) else {
                    continue;
                };
                let (dr, dg, db) = read_bgrx(dst);
                let (nr, ng, nb) = src_over(0, 0, 0, alpha, dr, dg, db);
                write_bgrx(dst, nr, ng, nb);
            }
        }
    }
}

/// Horizontal box blur of radius `r` via a running sum: O(w) per row
/// instead of O(w·r). Each output pixel is the mean of `[x - r, x + r]`
/// clamped to the row; samples outside the buffer are not counted (the
/// scratch region is zero-padded at its expanded edge).
fn box_blur_h(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    if w == 0 {
        return;
    }
    for y in 0..h {
        let row = y.saturating_mul(w);
        // Prime the window over [0, r]; the count is clamped to the row.
        let mut sum: u32 = 0;
        for xx in 0..=r.min(w.saturating_sub(1)) {
            sum = sum.saturating_add(u32::from(
                src.get(row.saturating_add(xx)).copied().unwrap_or(0),
            ));
        }
        for x in 0..w {
            let lo = x.saturating_sub(r);
            let hi = (x.saturating_add(r)).min(w.saturating_sub(1));
            let n = (hi - lo + 1) as u32;
            if let Some(slot) = dst.get_mut(row.saturating_add(x)) {
                *slot = sum.checked_div(n).unwrap_or(0) as u8;
            }
            // Slide: drop the sample leaving at `x - r`, add the one
            // entering at `x + r + 1`.
            if x + 1 < w {
                if x >= r {
                    let out = src.get(row.saturating_add(x - r)).copied().unwrap_or(0);
                    sum = sum.saturating_sub(u32::from(out));
                }
                let add_idx = x.saturating_add(r).saturating_add(1);
                if add_idx < w {
                    let inb = src.get(row.saturating_add(add_idx)).copied().unwrap_or(0);
                    sum = sum.saturating_add(u32::from(inb));
                }
            }
        }
    }
}

/// Vertical counterpart to [`box_blur_h`], running-sum down each column.
fn box_blur_v(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    if h == 0 {
        return;
    }
    for x in 0..w {
        let mut sum: u32 = 0;
        for yy in 0..=r.min(h.saturating_sub(1)) {
            sum = sum.saturating_add(u32::from(
                src.get(yy.saturating_mul(w).saturating_add(x))
                    .copied()
                    .unwrap_or(0),
            ));
        }
        for y in 0..h {
            let lo = y.saturating_sub(r);
            let hi = (y.saturating_add(r)).min(h.saturating_sub(1));
            let n = (hi - lo + 1) as u32;
            if let Some(slot) = dst.get_mut(y.saturating_mul(w).saturating_add(x)) {
                *slot = sum.checked_div(n).unwrap_or(0) as u8;
            }
            if y + 1 < h {
                if y >= r {
                    let out = src
                        .get((y - r).saturating_mul(w).saturating_add(x))
                        .copied()
                        .unwrap_or(0);
                    sum = sum.saturating_sub(u32::from(out));
                }
                let add_idx = y.saturating_add(r).saturating_add(1);
                if add_idx < h {
                    let inb = src
                        .get(add_idx.saturating_mul(w).saturating_add(x))
                        .copied()
                        .unwrap_or(0);
                    sum = sum.saturating_add(u32::from(inb));
                }
            }
        }
    }
}
