use crate::splash::types::{FramebufferDims, GlyphBitmap, RgbaColor};

use super::CellRect;
use super::text_layer::TextLayer;

/// Copy the scaled background RGBA buffer into the framebuffer,
/// respecting `fb_dims.stride`. The input is tight RGBA8 of exactly
/// `fb_dims.w * fb_dims.h * 4` bytes; rows shorter than that are
/// skipped silently rather than panicking, so a malformed scaler
/// output downgrades to a partial paint instead of a crash.
pub fn blit_background(fb: &mut [u8], fb_dims: FramebufferDims, bg_rgba: &[u8]) {
    let row_pixels = fb_dims.w as usize;
    let row_bytes = row_pixels.saturating_mul(4);
    let stride = fb_dims.stride as usize;
    let rows = fb_dims.h as usize;

    for y in 0..rows {
        let src_start = y.saturating_mul(row_bytes);
        let src_end = src_start.saturating_add(row_bytes);
        let dst_start = y.saturating_mul(stride);
        let dst_end = dst_start.saturating_add(row_bytes);

        let Some(src_row) = bg_rgba.get(src_start..src_end) else {
            break;
        };
        let Some(dst_row) = fb.get_mut(dst_start..dst_end) else {
            break;
        };

        // Walk one pixel at a time: RGBA → BGRX. Using chunks_exact pairs
        // keeps the bounds check to once per pair (the compiler is happy
        // to vectorize four-byte copies of a four-byte chunk).
        let src_pixels = src_row.chunks_exact(4);
        let dst_pixels = dst_row.chunks_exact_mut(4);
        for (s, d) in src_pixels.zip(dst_pixels) {
            // `chunks_exact(4)` guarantees length 4, so the `.get()`
            // lookups all yield Some. Pattern-match to keep this total.
            let (Some(&r), Some(&g), Some(&b)) = (s.first(), s.get(1), s.get(2)) else {
                continue;
            };
            if let Some(slot) = d.first_mut() {
                *slot = b;
            }
            if let Some(slot) = d.get_mut(1) {
                *slot = g;
            }
            if let Some(slot) = d.get_mut(2) {
                *slot = r;
            }
            if let Some(slot) = d.get_mut(3) {
                *slot = 0;
            }
        }
    }
}

/// Fill the cell rectangle at `(cell_x, cell_y)` pixels with `bg`,
/// alpha-blending it over whatever the framebuffer already holds (PNG
/// background, or the halo). No-op when `bg.3 == 0`. Any pixel that
/// would fall outside the framebuffer is silently clipped.
///
/// This is the **background** half of the old single-pass `blit_cell`:
/// it always covers the whole cell box (`cell_w` × `cell_h`). Glyph
/// foreground is no longer painted here — text is collected into a
/// [`TextLayer`] and composited once, on top of all the cell-bg fills,
/// so overlapping semi-transparent glyphs do not double-composite (the
/// "white dots along borders" bug).
pub fn fill_cell_bg(fb: &mut [u8], fb_dims: FramebufferDims, cell: CellRect, bg: RgbaColor) {
    if bg.3 == 0 {
        return;
    }
    let stride = fb_dims.stride as usize;
    let fb_w = fb_dims.w;
    let fb_h = fb_dims.h;
    let CellRect {
        x: cell_x,
        y: cell_y,
        w: cell_w,
        h: cell_h,
    } = cell;
    let RgbaColor(br, bg_g, bb, ba) = bg;
    for cy in 0..cell_h {
        let py = cell_y.saturating_add(cy);
        if py >= fb_h {
            break;
        }
        let row_off = (py as usize).saturating_mul(stride);
        for cx in 0..cell_w {
            let px = cell_x.saturating_add(cx);
            if px >= fb_w {
                break;
            }
            let pix_off = row_off.saturating_add((px as usize).saturating_mul(4));
            let Some(dst) = fb.get_mut(pix_off..pix_off.saturating_add(4)) else {
                continue;
            };
            let (dr, dg, db) = read_bgrx(dst);
            let (nr, ng, nb) = src_over(br, bg_g, bb, ba, dr, dg, db);
            write_bgrx(dst, nr, ng, nb);
        }
    }
}

/// Fill the cell rectangle with `bg` then alpha-blend the glyph in `fg`
/// over it, in a single self-contained pass. Retained as a convenience
/// for callers that paint one cell in isolation (and for the unit
/// tests); the live splash pipeline now uses [`fill_cell_bg`] +
/// [`TextLayer`] so overlapping glyphs composite once. The visible
/// result for a single isolated cell is identical to the layered path.
pub fn blit_cell(
    fb: &mut [u8],
    fb_dims: FramebufferDims,
    glyph: &GlyphBitmap,
    cell: CellRect,
    fg: RgbaColor,
    bg: RgbaColor,
) {
    fill_cell_bg(fb, fb_dims, cell, bg);
    let mut layer = TextLayer::new(fb_dims);
    layer.stamp(glyph, cell, fg);
    layer.composite_onto(fb, fb_dims);
}

/// Perceptually-correct src-over blend using Oklab interpolation.
///
/// Alpha-weighted mix in Oklab space avoids the gamma-incorrect
/// darkening that sRGB-linear math produces (e.g. white-at-30%-alpha
/// over a mid-tone photo reading muddy instead of soft white).
///
/// Short-circuits for `a == 0` (fully transparent → dst unchanged) and
/// `a == 255` (fully opaque → src replaces dst) to skip the round-trip.
#[inline]
pub(crate) fn src_over(sr: u8, sg: u8, sb: u8, a: u8, dr: u8, dg: u8, db: u8) -> (u8, u8, u8) {
    if a == 0 {
        return (dr, dg, db);
    }
    if a == 255 {
        return (sr, sg, sb);
    }
    let alpha = f32::from(a) / 255.0;
    let inv = 1.0 - alpha;
    let src_lab = oklab::srgb_to_oklab(oklab::Rgb {
        r: sr,
        g: sg,
        b: sb,
    });
    let dst_lab = oklab::srgb_to_oklab(oklab::Rgb {
        r: dr,
        g: dg,
        b: db,
    });
    let out_lab = oklab::Oklab {
        l: dst_lab.l * inv + src_lab.l * alpha,
        a: dst_lab.a * inv + src_lab.a * alpha,
        b: dst_lab.b * inv + src_lab.b * alpha,
    };
    let out_rgb = oklab::oklab_to_srgb(out_lab);
    (out_rgb.r, out_rgb.g, out_rgb.b)
}

#[inline]
pub(crate) fn read_bgrx(slot: &[u8]) -> (u8, u8, u8) {
    let b = slot.first().copied().unwrap_or(0);
    let g = slot.get(1).copied().unwrap_or(0);
    let r = slot.get(2).copied().unwrap_or(0);
    (r, g, b)
}

#[inline]
pub(crate) fn write_bgrx(slot: &mut [u8], r: u8, g: u8, b: u8) {
    if let Some(p) = slot.first_mut() {
        *p = b;
    }
    if let Some(p) = slot.get_mut(1) {
        *p = g;
    }
    if let Some(p) = slot.get_mut(2) {
        *p = r;
    }
    if let Some(p) = slot.get_mut(3) {
        *p = 0;
    }
}
