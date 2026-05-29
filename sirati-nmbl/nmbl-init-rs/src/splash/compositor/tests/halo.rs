#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::splash::types::{FramebufferDims, GlyphBitmap};

use super::super::halo::HALO_PAD;
use super::super::{CellRect, blit_halo, wants_halo};

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
fn blit_halo_darkens_glyph_and_leaves_distant_pixels() {
    // 64×64 bright-grey fb, 5×5 solid glyph at cell (28, 28). The fb
    // is sized so the whole padded halo canvas (glyph bbox ± HALO_PAD)
    // fits with room to spare, leaving genuinely distant pixels.
    let (mut fb, dims) = grey_fb(64, 200);
    let glyph = solid_glyph(5);
    let rect = CellRect {
        x: 28,
        y: 28,
        w: 5,
        h: 5,
    };
    blit_halo(&mut fb, dims, &glyph, rect);

    // Glyph core (30, 30) sits inside the solid ink → darkened.
    assert!(
        pixel_r(&fb, dims, 30, 30) < 200,
        "halo must darken the glyph core"
    );

    // The wider, concave falloff still has a finite reach: the canvas
    // spans the glyph bbox padded by HALO_PAD on each side. A pixel
    // beyond that pad can never be touched.
    let pad = HALO_PAD;
    let glyph_max = 28 + 5; // exclusive right/bottom edge of the glyph
    let tail_end = glyph_max + pad; // last column/row the canvas can reach
    let far = tail_end + 2; // safely beyond the tail
    assert_eq!(
        pixel_r(&fb, dims, far, far),
        200,
        "pixel beyond the haze tail must be untouched"
    );
    // Corner (0, 0) is far outside the padded halo canvas → pristine.
    assert_eq!(
        pixel_r(&fb, dims, 0, 0),
        200,
        "distant corner pixel must be untouched"
    );
}

#[test]
fn blit_halo_empty_glyph_is_noop() {
    let (mut fb, dims) = grey_fb(16, 123);
    let before = fb.clone();
    let glyph = GlyphBitmap {
        width: 0,
        height: 0,
        coverage: Vec::new(),
        offset_x: 0,
        offset_y: 0,
    };
    let rect = CellRect {
        x: 8,
        y: 8,
        w: 8,
        h: 8,
    };
    blit_halo(&mut fb, dims, &glyph, rect);
    assert_eq!(fb, before, "empty glyph must not touch the framebuffer");
}

#[test]
fn blit_halo_less_visible_on_dark_and_monotonic() {
    // Same glyph + cell + halo strength over a bright vs a dark
    // background. The Oklab multiply-toward-black means:
    //   * the bright pixel is darkened by a larger absolute amount
    //     (the haze is "less visible" on dark backgrounds), and
    //   * the darker background can never end up brighter than the
    //     lighter one (monotonic in background brightness).
    const BRIGHT: u8 = 200;
    const DARK: u8 = 40;
    let glyph = solid_glyph(5);
    let rect = CellRect {
        x: 10,
        y: 10,
        w: 5,
        h: 5,
    };

    let (mut fb_bright, dims) = grey_fb(24, BRIGHT);
    blit_halo(&mut fb_bright, dims, &glyph, rect);
    let bright_after = pixel_r(&fb_bright, dims, 12, 12);

    let (mut fb_dark, _) = grey_fb(24, DARK);
    blit_halo(&mut fb_dark, dims, &glyph, rect);
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
