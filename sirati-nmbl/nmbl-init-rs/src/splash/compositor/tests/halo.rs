#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::splash::types::{FramebufferDims, GlyphBitmap};

use super::super::halo::HALO_SPREAD;
use super::super::{CellRect, HaloMask, wants_halo};

/// Build a `dim × dim` framebuffer (stride = dim*4) filled with an
/// opaque grey `v` in BGRX order, plus its dims.
pub(super) fn grey_fb(dim: u32, v: u8) -> (Vec<u8>, FramebufferDims) {
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
pub(super) fn pixel_r(fb: &[u8], dims: FramebufferDims, x: u32, y: u32) -> u8 {
    let off = (y as usize) * (dims.stride as usize) + (x as usize) * 4;
    fb.get(off + 2).copied().unwrap_or(0)
}

/// A solid `n × n` fully-opaque glyph at cell-relative offset (0, 0).
pub(super) fn solid_glyph(n: u32) -> GlyphBitmap {
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
