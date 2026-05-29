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

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::splash::types::{FramebufferDims, GlyphBitmap, RgbaColor};

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
const HALO_SPREAD: usize = HALO_RADIUS * HALO_PASSES;

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

/// Intermediate foreground-text layer.
///
/// Mirrors [`HaloMask`]'s bbox-bounded, framebuffer-sized scratch
/// approach but stores, per pixel, a resolved COLOR + a single resolved
/// ALPHA instead of a coverage byte. Foreground glyphs are stamped here
/// (no compositing onto the background during the pass), then the whole
/// layer is composited onto the framebuffer ONCE.
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
    /// dimensions. Like [`HaloMask::new`] the buffer is full-frame for
    /// O(1) addressing but composite cost is bounded to the stamped bbox.
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
                    if let Some(c) = self.color.get_mut(coff..coff.saturating_add(3)) {
                        c[0] = fr;
                        c[1] = fg_g;
                        c[2] = fb_c;
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
                let Some(c) = self.color.get(coff..coff.saturating_add(3)) else {
                    continue;
                };
                let (sr, sg, sb) = (c[0], c[1], c[2]);
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
    pub fn stamp(&mut self, glyph: &GlyphBitmap, cell: CellRect) {
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
                if let Some(slot) = self.mask.get_mut(row.saturating_add(dx)) {
                    if cov > *slot {
                        *slot = cov;
                    }
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
    /// pristine. Must be called *after* [`blit_background`] and *before*
    /// the per-cell [`blit_cell`] pass.
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

/// Perceptually-correct src-over blend using Oklab interpolation.
///
/// Alpha-weighted mix in Oklab space avoids the gamma-incorrect
/// darkening that sRGB-linear math produces (e.g. white-at-30%-alpha
/// over a mid-tone photo reading muddy instead of soft white).
///
/// Short-circuits for `a == 0` (fully transparent → dst unchanged) and
/// `a == 255` (fully opaque → src replaces dst) to skip the round-trip.
#[inline]
fn src_over(sr: u8, sg: u8, sb: u8, a: u8, dr: u8, dg: u8, db: u8) -> (u8, u8, u8) {
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
fn read_bgrx(slot: &[u8]) -> (u8, u8, u8) {
    let b = slot.first().copied().unwrap_or(0);
    let g = slot.get(1).copied().unwrap_or(0);
    let r = slot.get(2).copied().unwrap_or(0);
    (r, g, b)
}

#[inline]
fn write_bgrx(slot: &mut [u8], r: u8, g: u8, b: u8) {
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

/// Resolve a `vte::ansi::Color` to a concrete RGBA. The palette is
/// Tango (the default for xterm and GNOME Terminal). Foreground
/// defaults to opaque white; background defaults to fully transparent
/// so the PNG behind the cell shows through.
pub fn resolve_color(c: Color) -> RgbaColor {
    match c {
        Color::Named(n) => named_color(n),
        Color::Spec(Rgb { r, g, b }) => RgbaColor(r, g, b, 0xFF),
        Color::Indexed(i) => indexed_color(i),
    }
}

/// Alpha (0..255) of the selection-highlight background. ratatui's
/// `Color::Gray` highlight maps through SGR 37 to [`NamedColor::White`];
/// rendered in the *background* role it should read as a faint tint so
/// the photo stays visible. Kept separate from the foreground mapping so
/// white *text* (which also resolves through the White slot) is never
/// made fainter — see [`resolve_bg_color`].
const SELECTION_BG_ALPHA: u8 = 0x40;

/// Resolve a `Color` for the *background* cell role. Identical to
/// [`resolve_color`] except the selection-highlight tone
/// ([`NamedColor::White`] / `DimWhite`, i.e. ratatui `Color::Gray`) is
/// rendered more transparent so the highlight is a soft tint rather than
/// a solid bar. Foreground resolution still goes through
/// [`resolve_color`], so white text keeps its full opacity.
pub fn resolve_bg_color(c: Color) -> RgbaColor {
    match c {
        Color::Named(NamedColor::White | NamedColor::DimWhite) => {
            let RgbaColor(r, g, b, _) = named_color(NamedColor::White);
            RgbaColor(r, g, b, SELECTION_BG_ALPHA)
        }
        other => resolve_color(other),
    }
}

/// Tango-palette mapping for the 16 ANSI names, with the
/// foreground/background/cursor special slots picked to produce a
/// readable splash overlay: white text on a transparent cell (so the
/// PNG shows through), dim grey for the cursor.
fn named_color(n: NamedColor) -> RgbaColor {
    match n {
        NamedColor::Black | NamedColor::DimBlack => RgbaColor(0x00, 0x00, 0x00, 0xFF),
        NamedColor::Red | NamedColor::DimRed => RgbaColor(0xCC, 0x00, 0x00, 0xFF),
        NamedColor::Green | NamedColor::DimGreen => RgbaColor(0x4E, 0x9A, 0x06, 0xFF),
        NamedColor::Yellow | NamedColor::DimYellow => RgbaColor(0xC4, 0xA0, 0x00, 0xFF),
        NamedColor::Blue | NamedColor::DimBlue => RgbaColor(0x34, 0x65, 0xA4, 0xFF),
        NamedColor::Magenta | NamedColor::DimMagenta => RgbaColor(0x75, 0x50, 0x7B, 0xFF),
        NamedColor::Cyan | NamedColor::DimCyan => RgbaColor(0x06, 0x98, 0x9A, 0xFF),
        // Tango "white" tone. ratatui `Color::Gray` lands here; in the
        // background role [`resolve_bg_color`] drops the alpha further to
        // `SELECTION_BG_ALPHA` so the selection reads as a soft tint. The
        // 0x80 kept here applies to the foreground role only.
        NamedColor::White | NamedColor::DimWhite => RgbaColor(0xD3, 0xD7, 0xCF, 0x80),
        NamedColor::BrightBlack => RgbaColor(0x55, 0x57, 0x53, 0xFF),
        NamedColor::BrightRed => RgbaColor(0xEF, 0x29, 0x29, 0xFF),
        NamedColor::BrightGreen => RgbaColor(0x8A, 0xE2, 0x34, 0xFF),
        NamedColor::BrightYellow => RgbaColor(0xFC, 0xE9, 0x4F, 0xFF),
        NamedColor::BrightBlue => RgbaColor(0x72, 0x9F, 0xCF, 0xFF),
        NamedColor::BrightMagenta => RgbaColor(0xAD, 0x7F, 0xA8, 0xFF),
        NamedColor::BrightCyan => RgbaColor(0x34, 0xE2, 0xE2, 0xFF),
        NamedColor::BrightWhite | NamedColor::BrightForeground => RgbaColor(0xEE, 0xEE, 0xEC, 0xFF),
        // Foreground: white at 60% alpha so unset-fg text sits clearly
        // on top of the background image instead of glaring fully opaque.
        NamedColor::Foreground => RgbaColor(0xFF, 0xFF, 0xFF, 0x99),
        // Dim foreground: the Tango "white" tone.
        NamedColor::DimForeground => RgbaColor(0xD3, 0xD7, 0xCF, 0xFF),
        // Background: fully transparent so the PNG underneath shows
        // through any cell whose bg attribute is the terminal default.
        NamedColor::Background => RgbaColor(0x00, 0x00, 0x00, 0x00),
        // Cursor: a dim grey block; arbitrary but consistent.
        NamedColor::Cursor => RgbaColor(0xAA, 0xAA, 0xAA, 0xFF),
    }
}

/// Standard 256-color xterm palette:
/// 0..=7 = ANSI normal, 8..=15 = ANSI bright, 16..=231 = 6×6×6 RGB
/// cube, 232..=255 = 24-step greyscale ramp.
fn indexed_color(i: u8) -> RgbaColor {
    match i {
        0 => named_color(NamedColor::Black),
        1 => named_color(NamedColor::Red),
        2 => named_color(NamedColor::Green),
        3 => named_color(NamedColor::Yellow),
        4 => named_color(NamedColor::Blue),
        5 => named_color(NamedColor::Magenta),
        6 => named_color(NamedColor::Cyan),
        7 => named_color(NamedColor::White),
        8 => named_color(NamedColor::BrightBlack),
        9 => named_color(NamedColor::BrightRed),
        10 => named_color(NamedColor::BrightGreen),
        11 => named_color(NamedColor::BrightYellow),
        12 => named_color(NamedColor::BrightBlue),
        13 => named_color(NamedColor::BrightMagenta),
        14 => named_color(NamedColor::BrightCyan),
        15 => named_color(NamedColor::BrightWhite),
        16..=231 => {
            let n = i.saturating_sub(16);
            let r_idx = n / 36;
            let g_idx = (n / 6) % 6;
            let b_idx = n % 6;
            RgbaColor(
                cube_channel(r_idx),
                cube_channel(g_idx),
                cube_channel(b_idx),
                0xFF,
            )
        }
        232..=255 => {
            let step = i.saturating_sub(232);
            // level = 8 + 10 * step ∈ {8, 18, …, 238}, all ≤ 255.
            let level = 8u16.saturating_add(u16::from(step).saturating_mul(10));
            let v = if level > 255 { 255 } else { level as u8 };
            RgbaColor(v, v, v, 0xFF)
        }
    }
}

/// Cube channel mapping: index 0 → 0, index 1..=5 → 55 + 40 * idx.
#[inline]
fn cube_channel(idx: u8) -> u8 {
    if idx == 0 {
        0
    } else {
        // 55 + 40 * 5 = 255, fits in u8 without ever overflowing.
        let v = 55u16.saturating_add(u16::from(idx).saturating_mul(40));
        if v > 255 { 255 } else { v as u8 }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn blit_background_byte_order() {
        // 2×2 framebuffer, stride = 2*4 = 8 bytes.
        let dims = FramebufferDims {
            w: 2,
            h: 2,
            stride: 8,
        };
        let mut fb = vec![0xAAu8; (dims.stride * dims.h) as usize];

        // Source row 0: pixel (0,0) = #112233FF, pixel (1,0) = #445566FF.
        // Source row 1: pixel (0,1) = #778899FF, pixel (1,1) = #AABBCCFF.
        let src: Vec<u8> = vec![
            0x11, 0x22, 0x33, 0xFF, 0x44, 0x55, 0x66, 0xFF, // row 0
            0x77, 0x88, 0x99, 0xFF, 0xAA, 0xBB, 0xCC, 0xFF, // row 1
        ];

        blit_background(&mut fb, dims, &src);

        // Expected: each pixel laid out as [B, G, R, 0].
        let expected: Vec<u8> = vec![
            0x33, 0x22, 0x11, 0x00, 0x66, 0x55, 0x44, 0x00, // row 0
            0x99, 0x88, 0x77, 0x00, 0xCC, 0xBB, 0xAA, 0x00, // row 1
        ];
        assert_eq!(fb, expected);
    }

    #[test]
    fn blit_background_respects_stride() {
        // 1×2 framebuffer with stride = 8 (4 bytes of padding per row).
        let dims = FramebufferDims {
            w: 1,
            h: 2,
            stride: 8,
        };
        let mut fb = vec![0xEEu8; (dims.stride * dims.h) as usize];
        let src: Vec<u8> = vec![
            0x11, 0x22, 0x33, 0xFF, // row 0, one pixel
            0x44, 0x55, 0x66, 0xFF, // row 1, one pixel
        ];
        blit_background(&mut fb, dims, &src);

        // Row 0, pixel 0: BGRX = 33 22 11 00; bytes 4..8 untouched.
        assert_eq!(&fb[0..4], &[0x33, 0x22, 0x11, 0x00]);
        assert_eq!(&fb[4..8], &[0xEE, 0xEE, 0xEE, 0xEE]);
        // Row 1.
        assert_eq!(&fb[8..12], &[0x66, 0x55, 0x44, 0x00]);
        assert_eq!(&fb[12..16], &[0xEE, 0xEE, 0xEE, 0xEE]);
    }

    #[test]
    fn blit_background_short_input_doesnt_panic() {
        // Framebuffer claims 2 rows but the source only has data for 1.
        // The compositor must paint what it can and bail without panic.
        let dims = FramebufferDims {
            w: 1,
            h: 2,
            stride: 4,
        };
        let mut fb = vec![0x55u8; (dims.stride * dims.h) as usize];
        let src: Vec<u8> = vec![0x11, 0x22, 0x33, 0xFF];
        blit_background(&mut fb, dims, &src);
        assert_eq!(&fb[0..4], &[0x33, 0x22, 0x11, 0x00]);
        // Row 1 untouched.
        assert_eq!(&fb[4..8], &[0x55, 0x55, 0x55, 0x55]);
    }

    #[test]
    fn blit_cell_alpha_blend_center() {
        // 4×4 framebuffer pre-filled with bg = (10, 20, 30) stored as
        // BGRX = (30, 20, 10, 0).
        let dims = FramebufferDims {
            w: 4,
            h: 4,
            stride: 16,
        };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];
        for y in 0..4 {
            for x in 0..4 {
                let off = y * 16 + x * 4;
                fb[off] = 30;
                fb[off + 1] = 20;
                fb[off + 2] = 10;
                fb[off + 3] = 0;
            }
        }

        // 2×2 glyph: full-coverage top-left, 50% coverage at other three.
        let glyph = GlyphBitmap {
            width: 2,
            height: 2,
            coverage: vec![255, 128, 128, 128],
            offset_x: 0,
            offset_y: 0,
        };
        let fg = RgbaColor(200, 100, 50, 0xFF);
        // Transparent bg → only the glyph blend stage runs.
        let bg = RgbaColor(0, 0, 0, 0);

        // Place the glyph at (1, 1) so the top-left lands on pixel (1, 1).
        // Use a 2×2 cell box so the bg-fill stage stays within the glyph
        // extents (the bg here is transparent anyway).
        let rect = CellRect {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        };
        blit_cell(&mut fb, dims, &glyph, rect, fg, bg);

        // Pixel (1, 1): full coverage → fg overwrites dst exactly.
        let p11: usize = 16 + 4;
        assert_eq!(fb[p11], 50); // B = fg.b
        assert_eq!(fb[p11 + 1], 100); // G = fg.g
        assert_eq!(fb[p11 + 2], 200); // R = fg.r
        assert_eq!(fb[p11 + 3], 0); // X always 0

        // Pixel (2, 1): coverage = 128, effective alpha ≈ 0.5. The Oklab
        // blend must produce a value strictly between src and dst in each
        // channel (perceptual interpolation cannot flip the ordering).
        // fg = (r=200, g=100, b=50), dst = (r=10, g=20, b=30).
        let p21: usize = 16 + 2 * 4;
        let got_b = fb[p21];
        let got_g = fb[p21 + 1];
        let got_r = fb[p21 + 2];
        assert!(
            got_r > 10 && got_r < 200,
            "R at (2,1) should be between dst=10 and src=200, got {got_r}"
        );
        assert!(
            got_g > 20 && got_g < 100,
            "G at (2,1) should be between dst=20 and src=100, got {got_g}"
        );
        assert!(
            got_b > 30 && got_b < 50,
            "B at (2,1) should be between dst=30 and src=50, got {got_b}"
        );

        // Pixel (0, 0): outside the glyph rect — must remain bg.
        let p00 = 0;
        assert_eq!(&fb[p00..p00 + 4], &[30, 20, 10, 0]);

        // Pixel (3, 3): outside the 2×2 glyph (which lives at (1,1)..(3,3)).
        // The glyph touches (1,1), (2,1), (1,2), (2,2) — pixel (3,3)
        // must remain pristine.
        let p33 = 3 * 16 + 3 * 4;
        assert_eq!(&fb[p33..p33 + 4], &[30, 20, 10, 0]);
    }

    #[test]
    fn blit_cell_fills_bg_then_glyph() {
        // 2×2 framebuffer with garbage so we can see the bg fill paint
        // over it before the glyph layer applies.
        let dims = FramebufferDims {
            w: 2,
            h: 2,
            stride: 8,
        };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];

        let glyph = GlyphBitmap {
            width: 2,
            height: 2,
            coverage: vec![0, 0, 0, 0], // glyph contributes nothing
            offset_x: 0,
            offset_y: 0,
        };
        let fg = RgbaColor(255, 255, 255, 0xFF);
        let bg = RgbaColor(0x40, 0x60, 0x80, 0xFF);

        let rect = CellRect {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
        };
        blit_cell(&mut fb, dims, &glyph, rect, fg, bg);

        // All four pixels should be bg (BGRX = 80 60 40 00).
        for y in 0..2 {
            for x in 0..2 {
                let off = y * 8 + x * 4;
                assert_eq!(fb[off], 0x80, "B at ({x},{y})");
                assert_eq!(fb[off + 1], 0x60, "G at ({x},{y})");
                assert_eq!(fb[off + 2], 0x40, "R at ({x},{y})");
                assert_eq!(fb[off + 3], 0x00, "X at ({x},{y})");
            }
        }
    }

    #[test]
    fn blit_cell_clips_to_framebuffer() {
        // 2×2 framebuffer, draw a 2×2 glyph at offset (1, 1). Only the
        // pixel at (1, 1) is in-bounds.
        let dims = FramebufferDims {
            w: 2,
            h: 2,
            stride: 8,
        };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];
        let glyph = GlyphBitmap {
            width: 2,
            height: 2,
            coverage: vec![255, 255, 255, 255],
            offset_x: 0,
            offset_y: 0,
        };
        let fg = RgbaColor(10, 20, 30, 0xFF);
        let bg = RgbaColor(0, 0, 0, 0);

        let rect = CellRect {
            x: 1,
            y: 1,
            w: 2,
            h: 2,
        };
        blit_cell(&mut fb, dims, &glyph, rect, fg, bg);

        // Pixel (1, 1) painted.
        let p11: usize = 8 + 4;
        assert_eq!(fb[p11], 30); // B
        assert_eq!(fb[p11 + 1], 20); // G
        assert_eq!(fb[p11 + 2], 10); // R

        // Other three pixels unchanged.
        assert_eq!(&fb[0..4], &[0, 0, 0, 0]);
        assert_eq!(&fb[4..8], &[0, 0, 0, 0]);
        assert_eq!(&fb[8..12], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_cell_shifts_glyph_by_offset_y() {
        // 4×8 framebuffer; one cell occupies the whole height. Stamp a
        // 1×2 fully-opaque glyph with offset_y = 4. The glyph's first
        // row must land on framebuffer row 4 (not row 0), proving the
        // baseline shift takes effect.
        let dims = FramebufferDims {
            w: 4,
            h: 8,
            stride: 16,
        };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];

        let glyph = GlyphBitmap {
            width: 1,
            height: 2,
            coverage: vec![255, 255],
            offset_x: 0,
            offset_y: 4,
        };
        let fg = RgbaColor(0xAA, 0xBB, 0xCC, 0xFF);
        let bg = RgbaColor(0, 0, 0, 0);

        let rect = CellRect {
            x: 0,
            y: 0,
            w: 4,
            h: 8,
        };
        blit_cell(&mut fb, dims, &glyph, rect, fg, bg);

        // Rows 0..3 untouched (the glyph offset shifted past them).
        for y in 0..4 {
            let off = y * 16;
            assert_eq!(&fb[off..off + 16], &[0u8; 16], "row {y} must remain zero");
        }
        // Row 4 col 0: glyph stamped here (BGRX = CC BB AA 00).
        let r4 = 4 * 16;
        assert_eq!(fb[r4], 0xCC, "B at (0,4)");
        assert_eq!(fb[r4 + 1], 0xBB, "G at (0,4)");
        assert_eq!(fb[r4 + 2], 0xAA, "R at (0,4)");
        // Row 5 col 0: second glyph row also stamped.
        let r5 = 5 * 16;
        assert_eq!(fb[r5], 0xCC, "B at (0,5)");
        assert_eq!(fb[r5 + 1], 0xBB, "G at (0,5)");
        assert_eq!(fb[r5 + 2], 0xAA, "R at (0,5)");
        // Rows 6..7 untouched (glyph height is 2).
        for y in 6..8 {
            let off = y * 16;
            assert_eq!(&fb[off..off + 16], &[0u8; 16], "row {y} must remain zero");
        }
    }

    #[test]
    fn blit_cell_negative_offset_clips_without_underflow() {
        // A 2×2 glyph placed at cell origin (0, 0) with offset_y = -1
        // must skip its first row (would land at framebuffer row -1) and
        // paint only its second row at framebuffer row 0.
        let dims = FramebufferDims {
            w: 2,
            h: 2,
            stride: 8,
        };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];

        let glyph = GlyphBitmap {
            width: 2,
            height: 2,
            coverage: vec![255, 255, 255, 255],
            offset_x: 0,
            offset_y: -1,
        };
        let fg = RgbaColor(10, 20, 30, 0xFF);
        let bg = RgbaColor(0, 0, 0, 0);

        let rect = CellRect {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
        };
        blit_cell(&mut fb, dims, &glyph, rect, fg, bg);

        // Row 0: glyph row 1 lands here (full coverage).
        assert_eq!(fb[0], 30);
        assert_eq!(fb[1], 20);
        assert_eq!(fb[2], 10);
        assert_eq!(fb[4], 30);
        assert_eq!(fb[5], 20);
        assert_eq!(fb[6], 10);
        // Row 1: glyph row 2 would land at framebuffer row 1 → off the
        // glyph extent (height 2). Stays zero.
        assert_eq!(&fb[8..16], &[0u8; 8]);
    }

    /// Build a `dim × dim` framebuffer (stride = dim*4) filled with an
    /// opaque grey `v` in BGRX order, plus its dims.
    fn grey_fb(dim: u32, v: u8) -> (Vec<u8>, FramebufferDims) {
        let dims = FramebufferDims {
            w: dim,
            h: dim,
            stride: dim * 4,
        };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];
        for px in fb.chunks_exact_mut(4) {
            px[0] = v;
            px[1] = v;
            px[2] = v;
            px[3] = 0;
        }
        (fb, dims)
    }

    /// Read the R channel (BGRX index 2) of pixel (x, y).
    fn pixel_r(fb: &[u8], dims: FramebufferDims, x: u32, y: u32) -> u8 {
        let off = (y as usize) * (dims.stride as usize) + (x as usize) * 4;
        fb.get(off + 2).copied().unwrap_or(0)
    }

    /// A solid `n × n` fully-opaque glyph at cell-relative offset (0, 0).
    fn solid_glyph(n: u32) -> GlyphBitmap {
        GlyphBitmap {
            width: n,
            height: n,
            coverage: vec![255u8; (n * n) as usize],
            offset_x: 0,
            offset_y: 0,
        }
    }

    #[test]
    fn wants_halo_only_transparent_default_background() {
        // Keyed on the background now: only the transparent terminal
        // default qualifies, regardless of foreground colour.
        assert!(wants_halo(Color::Named(NamedColor::Background)));
        // Any explicit/opaque background paints its own backing → no halo.
        assert!(!wants_halo(Color::Named(NamedColor::White)));
        assert!(!wants_halo(Color::Named(NamedColor::Foreground)));
        assert!(!wants_halo(Color::Named(NamedColor::BrightWhite)));
        assert!(!wants_halo(Color::Indexed(15)));
        assert!(!wants_halo(Color::Spec(Rgb {
            r: 0xFF,
            g: 0xFF,
            b: 0xFF
        })));
    }

    #[test]
    fn halo_mask_overlap_unions_via_max_not_sum() {
        // Stamping the SAME glyph twice at the SAME spot must be identical
        // to stamping it once: the mask combines with `max` (a union), so
        // a pixel covered by two contributions holds 255 — not 510 — and
        // the black is composited a single time. A summing implementation
        // would over-darken (or, with clamping, still composite the field
        // identically but a doubled mask value would shift the blurred
        // tail); pinning the two framebuffers equal proves the union.
        let dims = FramebufferDims {
            w: 16,
            h: 16,
            stride: 64,
        };
        let glyph = solid_glyph(3);
        let rect = CellRect {
            x: 6,
            y: 6,
            w: 3,
            h: 3,
        };

        let (mut fb_once, _) = grey_fb(16, 200);
        {
            let mut m = HaloMask::new(dims);
            m.stamp(&glyph, rect);
            m.composite_onto(&mut fb_once, dims);
        }

        let (mut fb_twice, _) = grey_fb(16, 200);
        {
            let mut m = HaloMask::new(dims);
            m.stamp(&glyph, rect);
            m.stamp(&glyph, rect); // exact overlap
            m.composite_onto(&mut fb_twice, dims);
        }

        // Darkening actually happened (so the equality below is meaningful).
        assert!(
            pixel_r(&fb_once, dims, 7, 7) < 200,
            "the overlap core must be darkened"
        );
        // Max-combine: the doubled stamp produces a byte-identical result.
        assert_eq!(
            fb_once, fb_twice,
            "overlapping stamps must union via max (composited once), \
             not sum/double-darken"
        );
    }

    #[test]
    fn halo_mask_ink_pixel_is_darkened_no_bright_dot_gap() {
        // A pixel under glyph coverage must end up darker than the bare
        // background: the mask is 255 there, so there is no untouched
        // bright-dot gap between glyph and halo.
        let dims = FramebufferDims {
            w: 16,
            h: 16,
            stride: 64,
        };
        let (mut fb, _) = grey_fb(16, 200);
        let glyph = solid_glyph(3);
        let mut m = HaloMask::new(dims);
        m.stamp(
            &glyph,
            CellRect {
                x: 6,
                y: 6,
                w: 3,
                h: 3,
            },
        );
        m.composite_onto(&mut fb, dims);
        // (7,7) sits squarely under the ink.
        assert!(
            pixel_r(&fb, dims, 7, 7) < 200,
            "ink pixel must be darkened (no bright-dot gap under the glyph)"
        );
    }

    #[test]
    fn halo_mask_empty_is_noop() {
        let dims = FramebufferDims {
            w: 16,
            h: 16,
            stride: 64,
        };
        let (mut fb, _) = grey_fb(16, 123);
        let before = fb.clone();
        // No stamps at all → bbox stays None → composite is a no-op.
        let m = HaloMask::new(dims);
        m.composite_onto(&mut fb, dims);
        assert_eq!(fb, before, "empty mask must not touch the framebuffer");

        // A space (empty glyph) stamps nothing either.
        let mut m2 = HaloMask::new(dims);
        let empty = GlyphBitmap {
            width: 0,
            height: 0,
            coverage: Vec::new(),
            offset_x: 0,
            offset_y: 0,
        };
        m2.stamp(
            &empty,
            CellRect {
                x: 8,
                y: 8,
                w: 8,
                h: 8,
            },
        );
        m2.composite_onto(&mut fb, dims);
        assert_eq!(fb, before, "empty glyph must not touch the framebuffer");
    }

    #[test]
    fn halo_mask_far_pixel_beyond_spread_is_untouched() {
        // A 3×3 glyph near one corner; a pixel well beyond the mask plus
        // the blur spread must stay pristine.
        let dims = FramebufferDims {
            w: 64,
            h: 64,
            stride: 256,
        };
        let (mut fb, _) = grey_fb(64, 200);
        let glyph = solid_glyph(3);
        let mut m = HaloMask::new(dims);
        m.stamp(
            &glyph,
            CellRect {
                x: 4,
                y: 4,
                w: 3,
                h: 3,
            },
        );
        m.composite_onto(&mut fb, dims);

        // Glyph occupies cols/rows 4..=6. The blur reaches HALO_SPREAD
        // beyond that. A pixel comfortably past 6 + HALO_SPREAD is safe.
        let far = 6 + HALO_SPREAD as u32 + 4;
        assert_eq!(
            pixel_r(&fb, dims, far, far),
            200,
            "pixel beyond mask + blur spread must be untouched"
        );
        // The far corner is also pristine.
        assert_eq!(
            pixel_r(&fb, dims, 63, 63),
            200,
            "distant corner must be untouched"
        );
    }

    #[test]
    fn halo_mask_less_visible_on_dark_and_monotonic() {
        // Same mask over a bright vs a dark background. The Oklab
        // multiply-toward-black means:
        //   * the bright pixel is darkened by a larger absolute amount
        //     (the haze is "less visible" on dark backgrounds), and
        //   * the darker background can never end up brighter than the
        //     lighter one (monotonic in background brightness).
        const BRIGHT: u8 = 200;
        const DARK: u8 = 40;
        let dims = FramebufferDims {
            w: 24,
            h: 24,
            stride: 96,
        };
        let glyph = solid_glyph(5);
        let rect = CellRect {
            x: 10,
            y: 10,
            w: 5,
            h: 5,
        };

        let (mut fb_bright, _) = grey_fb(24, BRIGHT);
        {
            let mut m = HaloMask::new(dims);
            m.stamp(&glyph, rect);
            m.composite_onto(&mut fb_bright, dims);
        }
        let bright_after = pixel_r(&fb_bright, dims, 12, 12);

        let (mut fb_dark, _) = grey_fb(24, DARK);
        {
            let mut m = HaloMask::new(dims);
            m.stamp(&glyph, rect);
            m.composite_onto(&mut fb_dark, dims);
        }
        let dark_after = pixel_r(&fb_dark, dims, 12, 12);

        // Both darkened.
        assert!(bright_after < BRIGHT, "bright bg must darken");
        assert!(dark_after <= DARK, "dark bg must not brighten");

        // Less visible on dark: absolute darkening is smaller.
        let bright_drop = u32::from(BRIGHT) - u32::from(bright_after);
        let dark_drop = u32::from(DARK) - u32::from(dark_after);
        assert!(
            bright_drop > dark_drop,
            "haze must darken the bright bg more in absolute terms \
             (bright_drop={bright_drop}, dark_drop={dark_drop})"
        );

        // Monotonic: darker bg stays no brighter than the lighter one.
        assert!(
            dark_after <= bright_after,
            "result on dark bg ({dark_after}) must be <= result on bright bg ({bright_after})"
        );
    }

    /// Regression for the "white dots along borders" bug: two identical
    /// semi-transparent-white glyphs that overlap one pixel must, after
    /// the MAX-alpha layer combine, composite that pixel exactly ONCE —
    /// i.e. land on the same value as a single 60%-white src_over, not a
    /// doubled (brighter) one.
    #[test]
    fn text_layer_overlap_composites_once_not_doubled() {
        let dims = FramebufferDims {
            w: 2,
            h: 1,
            stride: 8,
        };
        // dst = (10, 20, 30) → BGRX (30, 20, 10, 0) at both pixels.
        let mk_fb = || {
            let mut fb = vec![0u8; (dims.stride * dims.h) as usize];
            for x in 0..2usize {
                let o = x * 4;
                fb[o] = 30;
                fb[o + 1] = 20;
                fb[o + 2] = 10;
            }
            fb
        };

        // 60% white, the NamedColor::Foreground tone.
        let fg = RgbaColor(0xFF, 0xFF, 0xFF, 0x99);
        // Single 1×1 full-coverage glyph at pixel (0,0).
        let g = GlyphBitmap {
            width: 1,
            height: 1,
            coverage: vec![255],
            offset_x: 0,
            offset_y: 0,
        };
        let rect = CellRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };

        // Reference: a SINGLE stamp+composite at pixel (0,0).
        let mut single = mk_fb();
        {
            let mut layer = TextLayer::new(dims);
            layer.stamp(&g, rect, fg);
            layer.composite_onto(&mut single, dims);
        }

        // Doubled scenario: TWO identical glyphs both hitting pixel (0,0)
        // (the cell-join overlap). MAX-combine must collapse them to one.
        let mut overlap = mk_fb();
        {
            let mut layer = TextLayer::new(dims);
            layer.stamp(&g, rect, fg);
            layer.stamp(&g, rect, fg);
            layer.composite_onto(&mut overlap, dims);
        }

        assert_eq!(
            &overlap[0..3],
            &single[0..3],
            "overlapping identical 60%-white glyphs must composite once, not doubled"
        );
    }

    /// A non-overlapping glyph pixel routed through the TextLayer must
    /// produce the same framebuffer value as the old per-glyph path (a
    /// single `src_over` at `round(coverage * fg.alpha / 255)`).
    #[test]
    fn text_layer_non_overlap_matches_per_glyph() {
        let dims = FramebufferDims {
            w: 2,
            h: 2,
            stride: 8,
        };
        let mk_fb = || {
            let mut fb = vec![0u8; (dims.stride * dims.h) as usize];
            for i in 0..fb.len() / 4 {
                let o = i * 4;
                fb[o] = 70;
                fb[o + 1] = 90;
                fb[o + 2] = 110;
            }
            fb
        };
        let fg = RgbaColor(0xFF, 0xFF, 0xFF, 0x99);
        let g = GlyphBitmap {
            width: 1,
            height: 1,
            coverage: vec![200],
            offset_x: 0,
            offset_y: 0,
        };
        let rect = CellRect {
            x: 1,
            y: 1,
            w: 1,
            h: 1,
        };

        // Layered path.
        let mut layered = mk_fb();
        {
            let mut layer = TextLayer::new(dims);
            layer.stamp(&g, rect, fg);
            layer.composite_onto(&mut layered, dims);
        }

        // Direct src_over with the same effective alpha the stamp uses:
        // round(200 * 0x99 / 255).
        let mut direct = mk_fb();
        {
            let eff = ((u16::from(200u8) * u16::from(0x99u8)) + 127) / 255;
            let eff = eff as u8;
            let off = 8 + 4; // pixel (1,1)
            let (dr, dg, db) = read_bgrx(&direct[off..off + 4]);
            let (nr, ng, nb) = src_over(0xFF, 0xFF, 0xFF, eff, dr, dg, db);
            write_bgrx(&mut direct[off..off + 4], nr, ng, nb);
        }

        assert_eq!(
            layered, direct,
            "non-overlapping pixel must match the direct per-glyph composite"
        );
    }

    /// Selection: a cell with an opaque-ish bg fill plus a glyph. The bg
    /// fill must be present where the glyph has no ink, and the text
    /// must sit on top of the fill where it does (bg drawn first, text
    /// layer last).
    #[test]
    fn fill_bg_then_text_layer_ordering() {
        let dims = FramebufferDims {
            w: 2,
            h: 1,
            stride: 8,
        };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];

        let bg = RgbaColor(0xD3, 0xD7, 0xCF, 0xFF); // opaque selection-ish fill
        // Glyph inks only pixel (0,0); pixel (1,0) is bare bg.
        let glyph = GlyphBitmap {
            width: 2,
            height: 1,
            coverage: vec![255, 0],
            offset_x: 0,
            offset_y: 0,
        };
        let fg = RgbaColor(0x10, 0x10, 0x10, 0xFF); // dark text
        let rect = CellRect {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
        };

        // bg fills first...
        fill_cell_bg(&mut fb, dims, rect, bg);
        // ...then text layer on top.
        let mut layer = TextLayer::new(dims);
        layer.stamp(&glyph, rect, fg);
        layer.composite_onto(&mut fb, dims);

        // Pixel (0,0): full-coverage dark text overwrites the fill.
        assert_eq!(&fb[0..3], &[0x10, 0x10, 0x10], "text over fill at (0,0)");
        // Pixel (1,0): no ink → the bg fill shows (BGRX = CF D7 D3).
        assert_eq!(&fb[4..7], &[0xCF, 0xD7, 0xD3], "bg fill at (1,0)");
    }

    /// Multi-colour overlap: when two differently-coloured glyphs touch
    /// the same pixel, the higher-coverage contributor's colour wins,
    /// and MAX is order-independent.
    #[test]
    fn text_layer_higher_coverage_color_wins() {
        let dims = FramebufferDims {
            w: 1,
            h: 1,
            stride: 4,
        };
        let g_low = GlyphBitmap {
            width: 1,
            height: 1,
            coverage: vec![80],
            offset_x: 0,
            offset_y: 0,
        };
        let g_high = GlyphBitmap {
            width: 1,
            height: 1,
            coverage: vec![255],
            offset_x: 0,
            offset_y: 0,
        };
        let rect = CellRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        };
        let red = RgbaColor(0xCC, 0x00, 0x00, 0xFF);
        let green = RgbaColor(0x00, 0xCC, 0x00, 0xFF);

        // Low-coverage red then high-coverage green: green (higher
        // coverage → higher alpha) must claim the pixel colour. Full
        // coverage + opaque green → src replaces dst (BGRX = 00 CC 00).
        let mut fb = vec![0u8; 4];
        let mut layer = TextLayer::new(dims);
        layer.stamp(&g_low, rect, red);
        layer.stamp(&g_high, rect, green);
        layer.composite_onto(&mut fb, dims);
        assert_eq!(&fb[0..3], &[0x00, 0xCC, 0x00], "higher-coverage green wins");

        // Reverse order → MAX keeps the same winner.
        let mut fb2 = vec![0u8; 4];
        let mut layer2 = TextLayer::new(dims);
        layer2.stamp(&g_high, rect, green);
        layer2.stamp(&g_low, rect, red);
        layer2.composite_onto(&mut fb2, dims);
        assert_eq!(&fb2[0..3], &[0x00, 0xCC, 0x00], "MAX is order-independent");
    }

    #[test]
    fn text_layer_empty_is_noop() {
        let dims = FramebufferDims {
            w: 8,
            h: 8,
            stride: 32,
        };
        let (mut fb, _) = grey_fb(8, 123);
        let before = fb.clone();
        // No stamps → bbox None → composite is a no-op.
        let layer = TextLayer::new(dims);
        layer.composite_onto(&mut fb, dims);
        assert_eq!(
            fb, before,
            "empty text layer must not touch the framebuffer"
        );
    }

    #[test]
    fn resolve_color_named_red() {
        let c = resolve_color(Color::Named(NamedColor::Red));
        assert_eq!(c, RgbaColor(0xCC, 0x00, 0x00, 0xFF));
    }

    #[test]
    fn resolve_color_named_bright_red() {
        let c = resolve_color(Color::Named(NamedColor::BrightRed));
        assert_eq!(c, RgbaColor(0xEF, 0x29, 0x29, 0xFF));
    }

    #[test]
    fn resolve_color_named_background_is_transparent() {
        let c = resolve_color(Color::Named(NamedColor::Background));
        assert_eq!(
            c.3, 0x00,
            "default background must be transparent so PNG shows"
        );
    }

    #[test]
    fn resolve_color_indexed_cube() {
        // Index 16 = cube (0,0,0) = pure black.
        assert_eq!(resolve_color(Color::Indexed(16)), RgbaColor(0, 0, 0, 0xFF));
        // Index 231 = cube (5,5,5) = pure white (55 + 40*5 = 255).
        assert_eq!(
            resolve_color(Color::Indexed(231)),
            RgbaColor(255, 255, 255, 0xFF)
        );
        // Index 196 = cube r=5,g=0,b=0 = (255, 0, 0).
        // n = 196 - 16 = 180; r = 180/36 = 5; g = (180/6)%6 = 30%6 = 0; b = 180%6 = 0.
        assert_eq!(
            resolve_color(Color::Indexed(196)),
            RgbaColor(255, 0, 0, 0xFF)
        );
        // Index 46 = cube (0,5,0) = (0, 255, 0).
        // n = 30; r = 0; g = (30/6)%6 = 5; b = 30%6 = 0.
        assert_eq!(
            resolve_color(Color::Indexed(46)),
            RgbaColor(0, 255, 0, 0xFF)
        );
    }

    #[test]
    fn resolve_color_indexed_greyscale() {
        // Index 232 = level 8.
        assert_eq!(resolve_color(Color::Indexed(232)), RgbaColor(8, 8, 8, 0xFF));
        // Index 255 = level 8 + 10 * 23 = 238.
        assert_eq!(
            resolve_color(Color::Indexed(255)),
            RgbaColor(238, 238, 238, 0xFF)
        );
    }

    #[test]
    fn resolve_color_indexed_low_matches_ansi() {
        // Indexed(1) must equal Named(Red).
        assert_eq!(
            resolve_color(Color::Indexed(1)),
            resolve_color(Color::Named(NamedColor::Red))
        );
        // Indexed(9) must equal Named(BrightRed).
        assert_eq!(
            resolve_color(Color::Indexed(9)),
            resolve_color(Color::Named(NamedColor::BrightRed))
        );
    }

    #[test]
    fn resolve_bg_color_selection_is_more_transparent() {
        // ratatui Color::Gray selection bg → NamedColor::White. In the
        // background role it must be the faint selection alpha.
        let bg = resolve_bg_color(Color::Named(NamedColor::White));
        assert_eq!(bg, RgbaColor(0xD3, 0xD7, 0xCF, SELECTION_BG_ALPHA));
        assert_eq!(SELECTION_BG_ALPHA, 0x40, "selection bg alpha pinned");
        // More transparent than the shared/foreground White mapping.
        let fg = resolve_color(Color::Named(NamedColor::White));
        assert!(
            bg.3 < fg.3,
            "selection bg ({}) must be more transparent than white fg ({})",
            bg.3,
            fg.3
        );
    }

    #[test]
    fn resolve_color_white_fg_unchanged() {
        // The foreground/shared path must NOT lose opacity — white text
        // (and indexed 7) stay at the original 0x80 alpha.
        assert_eq!(
            resolve_color(Color::Named(NamedColor::White)),
            RgbaColor(0xD3, 0xD7, 0xCF, 0x80)
        );
        // Non-white backgrounds resolve identically through both paths.
        assert_eq!(
            resolve_bg_color(Color::Named(NamedColor::Red)),
            resolve_color(Color::Named(NamedColor::Red))
        );
        assert_eq!(
            resolve_bg_color(Color::Named(NamedColor::Background)),
            resolve_color(Color::Named(NamedColor::Background))
        );
    }

    #[test]
    fn resolve_color_spec() {
        let c = resolve_color(Color::Spec(Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        }));
        assert_eq!(c, RgbaColor(0x12, 0x34, 0x56, 0xFF));
    }
}
