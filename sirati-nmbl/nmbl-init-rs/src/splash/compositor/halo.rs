use alacritty_terminal::vte::ansi::{Color, NamedColor};

use crate::splash::types::{FramebufferDims, GlyphBitmap};

use super::{
    CellRect,
    blit::{read_bgrx, src_over, write_bgrx},
};

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
pub(crate) const HALO_MAX_ALPHA: u8 = 160;

/// Blur radius (per pass) of the halo spread, in pixels. Several
/// separable box passes (see [`HALO_PASSES`]) give a soft, slowly-fading
/// Gaussian-ish falloff; the canvas is padded by [`HALO_PAD`] so the full
/// cumulative spread fits without clipping at the canvas edge.
pub(crate) const HALO_RADIUS: u32 = 4;

/// Number of separable box-blur passes (each run as an H then V pair).
/// More passes widen the soft tail and smooth the falloff toward a
/// Gaussian, pushing the faint near-invisible edge further out.
pub(crate) const HALO_PASSES: u32 = 3;

/// Canvas padding on every side. Each box pass of radius `HALO_RADIUS`
/// spreads coverage by `HALO_RADIUS`, so `HALO_PASSES` passes reach
/// `HALO_PASSES * HALO_RADIUS` pixels — pad by exactly that so the whole
/// tail fits.
pub(crate) const HALO_PAD: u32 = HALO_RADIUS * HALO_PASSES;

/// Whether a cell should get the dark contrast halo behind its glyph.
///
/// Keyed on the cell *background*: only cells whose background is the
/// terminal default (transparent — [`named_color`] resolves
/// [`NamedColor::Background`] to alpha 0, letting the PNG show through)
/// qualify. Every glyph drawn straight onto the photo therefore gets the
/// dark backing regardless of its foreground colour, so coloured text on
/// the image is legible too. Cells with an explicit, opaque background
/// (e.g. the selection highlight) paint their own backing and are left
/// alone. A blank cell (a space) early-returns in [`blit_halo`] on empty
/// coverage, so this never paints behind whitespace.
pub fn wants_halo(bg: Color) -> bool {
    matches!(bg, Color::Named(NamedColor::Background))
}

/// Paint a dark, quickly-fading contrast halo behind a glyph.
///
/// The halo is a blurred, gained copy of the glyph coverage composited
/// as pure black through the Oklab [`src_over`] blend, so it darkens
/// the background image proportionally to that pixel's own lightness
/// (see [`HALO_MAX_ALPHA`]). Drawn in a pass *before* any glyph so it
/// only ever darkens the background photo, never adjacent text.
///
/// Out-of-framebuffer pixels are clipped without overflow; an empty
/// glyph (a space) is a no-op.
pub fn blit_halo(fb: &mut [u8], fb_dims: FramebufferDims, glyph: &GlyphBitmap, cell: CellRect) {
    let gw = glyph.width as usize;
    let gh = glyph.height as usize;
    if gw == 0 || gh == 0 {
        return;
    }
    let pad = HALO_PAD as usize;
    // Halo canvas: glyph bbox padded by `pad` on every side so the full
    // multi-pass blur spread fits without clipping at the canvas edge.
    let hw = gw.saturating_add(pad.saturating_mul(2));
    let hh = gh.saturating_add(pad.saturating_mul(2));
    let Some(area) = hw.checked_mul(hh) else {
        return;
    };

    let field = build_halo_field(glyph, gw, gh, hw, hh, area, pad);

    // Composite black over the framebuffer, per-pixel alpha from the
    // (gained) blurred coverage. The canvas top-left maps to the glyph
    // origin shifted back by `pad`.
    let base_x = i64::from(cell.x) + i64::from(glyph.offset_x) - pad as i64;
    let base_y = i64::from(cell.y) + i64::from(glyph.offset_y) - pad as i64;
    composite_halo_field(fb, fb_dims, &field, hw, hh, base_x, base_y);
}

/// Seed glyph coverage into a padded canvas and run the box-blur passes.
fn build_halo_field(
    glyph: &GlyphBitmap,
    gw: usize,
    gh: usize,
    hw: usize,
    hh: usize,
    area: usize,
    pad: usize,
) -> Vec<u8> {
    let r = HALO_RADIUS as usize;
    // Seed the glyph coverage into the centre of a zero-padded canvas.
    let mut field = vec![0u8; area];
    for gy in 0..gh {
        for gx in 0..gw {
            let cov = glyph
                .coverage
                .get(gy.saturating_mul(gw).saturating_add(gx))
                .copied()
                .unwrap_or(0);
            if cov == 0 {
                continue;
            }
            let idx = (gy.saturating_add(pad))
                .saturating_mul(hw)
                .saturating_add(gx.saturating_add(pad));
            if let Some(slot) = field.get_mut(idx) {
                *slot = cov;
            }
        }
    }

    // `HALO_PASSES` separable box passes ≈ a wide Gaussian spread. Each
    // pass is an H then V box blur sharing one scratch buffer.
    let mut scratch = vec![0u8; area];
    for _ in 0..HALO_PASSES {
        box_blur_h(&field, &mut scratch, hw, hh, r);
        box_blur_v(&scratch, &mut field, hw, hh, r);
    }

    field
}

/// Composite the blurred halo field onto the framebuffer as pure black.
fn composite_halo_field(
    fb: &mut [u8],
    fb_dims: FramebufferDims,
    field: &[u8],
    hw: usize,
    hh: usize,
    base_x: i64,
    base_y: i64,
) {
    let stride = fb_dims.stride as usize;
    for fy in 0..hh {
        let dy = base_y + fy as i64;
        if dy < 0 {
            continue;
        }
        let dy = dy as u64;
        if dy >= u64::from(fb_dims.h) {
            continue;
        }
        let row_off = (dy as usize).saturating_mul(stride);
        for fx in 0..hw {
            let v = field
                .get(fy.saturating_mul(hw).saturating_add(fx))
                .copied()
                .unwrap_or(0);
            if v == 0 {
                continue;
            }
            // Shape the spatial falloff with a concave curve so the core
            // stays a solid backing while low coverage is lifted into a
            // gentle, wide tail (rather than a linear ramp that fades too
            // fast). `sqrt` of the normalized blurred coverage is a
            // gamma-0.5 lift; an extra small gain keeps thin strokes
            // backed. This only shapes the SPATIAL alpha — the Oklab
            // multiply-toward-black blend below is untouched, so the
            // brightness-monotonicity property is preserved.
            let norm = f32::from(v) / 255.0;
            let shaped = norm.sqrt() * 1.25;
            let shaped = if shaped > 1.0 { 1.0 } else { shaped };
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
            let dx = base_x + fx as i64;
            if dx < 0 {
                continue;
            }
            let dx = dx as u64;
            if dx >= u64::from(fb_dims.w) {
                continue;
            }
            let pix_off = row_off.saturating_add((dx as usize).saturating_mul(4));
            let Some(dst) = fb.get_mut(pix_off..pix_off.saturating_add(4)) else {
                continue;
            };
            let (dr, dg, db) = read_bgrx(dst);
            let (nr, ng, nb) = src_over(0, 0, 0, alpha, dr, dg, db);
            write_bgrx(dst, nr, ng, nb);
        }
    }
}

/// Horizontal box blur of radius `r`: each output pixel is the mean of
/// `[x - r, x + r]` clamped to the row. Edge samples outside the canvas
/// are simply not counted (the canvas is zero-padded, so this is a
/// faithful clamp-to-edge of near-zero values).
fn box_blur_h(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    for y in 0..h {
        let row = y.saturating_mul(w);
        for x in 0..w {
            let lo = x.saturating_sub(r);
            let hi = (x.saturating_add(r)).min(w.saturating_sub(1));
            let mut sum: u32 = 0;
            let mut n: u32 = 0;
            for xx in lo..=hi {
                sum = sum.saturating_add(u32::from(
                    src.get(row.saturating_add(xx)).copied().unwrap_or(0),
                ));
                n = n.saturating_add(1);
            }
            if let Some(slot) = dst.get_mut(row.saturating_add(x)) {
                *slot = sum.checked_div(n).unwrap_or(0) as u8;
            }
        }
    }
}

/// Vertical counterpart to [`box_blur_h`].
fn box_blur_v(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    for x in 0..w {
        for y in 0..h {
            let lo = y.saturating_sub(r);
            let hi = (y.saturating_add(r)).min(h.saturating_sub(1));
            let mut sum: u32 = 0;
            let mut n: u32 = 0;
            for yy in lo..=hi {
                sum = sum.saturating_add(u32::from(
                    src.get(yy.saturating_mul(w).saturating_add(x))
                        .copied()
                        .unwrap_or(0),
                ));
                n = n.saturating_add(1);
            }
            if let Some(slot) = dst.get_mut(y.saturating_mul(w).saturating_add(x)) {
                *slot = sum.checked_div(n).unwrap_or(0) as u8;
            }
        }
    }
}
