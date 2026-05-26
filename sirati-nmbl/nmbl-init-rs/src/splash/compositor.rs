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

/// Fill the cell rectangle at `(cell_x, cell_y)` pixels with `bg` (if
/// `bg.3 > 0`), then alpha-blend the glyph in `fg` color over it. Any
/// pixel that would fall outside the framebuffer is silently clipped.
pub fn blit_cell(
    fb: &mut [u8],
    fb_dims: FramebufferDims,
    glyph: &GlyphBitmap,
    cell_x: u32,
    cell_y: u32,
    fg: RgbaColor,
    bg: RgbaColor,
) {
    let stride = fb_dims.stride as usize;
    let fb_w = fb_dims.w;
    let fb_h = fb_dims.h;

    for gy in 0..glyph.height {
        let py = cell_y.saturating_add(gy);
        if py >= fb_h {
            break;
        }
        let row_off = (py as usize).saturating_mul(stride);

        for gx in 0..glyph.width {
            let px = cell_x.saturating_add(gx);
            if px >= fb_w {
                break;
            }
            let pix_off = row_off.saturating_add((px as usize).saturating_mul(4));
            let Some(dst) = fb.get_mut(pix_off..pix_off.saturating_add(4)) else {
                continue;
            };

            // Stage 1: optional background fill.
            if bg.3 > 0 {
                let RgbaColor(br, bg_g, bb, ba) = bg;
                let (dr, dg, db) = read_bgrx(dst);
                let (nr, ng, nb) = src_over(br, bg_g, bb, ba, dr, dg, db);
                write_bgrx(dst, nr, ng, nb);
            }

            // Stage 2: alpha-blend the glyph coverage with `fg` over
            // whatever currently sits in the pixel.
            let cov_idx = (gy as usize)
                .saturating_mul(glyph.width as usize)
                .saturating_add(gx as usize);
            let coverage = glyph.coverage.get(cov_idx).copied().unwrap_or(0);
            if coverage == 0 {
                continue;
            }

            let RgbaColor(fr, fg_g, fb_c, _) = fg;
            let (dr, dg, db) = read_bgrx(dst);
            let (nr, ng, nb) = src_over(fr, fg_g, fb_c, coverage, dr, dg, db);
            write_bgrx(dst, nr, ng, nb);
        }
    }
}

/// Standard src-over blend: `out = (src * a + dst * (255 - a) + 127) / 255`
/// per channel, with the +127 rounding to nearest. Inputs are channel
/// values in 0..=255 and an alpha in 0..=255; output channels stay in
/// the same range.
#[inline]
fn src_over(sr: u8, sg: u8, sb: u8, a: u8, dr: u8, dg: u8, db: u8) -> (u8, u8, u8) {
    let a = u16::from(a);
    let inv = 255u16.saturating_sub(a);
    let blend = |s: u8, d: u8| -> u8 {
        let s = u16::from(s).saturating_mul(a);
        let d = u16::from(d).saturating_mul(inv);
        // +127 for round-to-nearest; the sum fits in u16 since
        // s + d ≤ 255 * 255 + 255 * 255 = 130_050 ≤ u16::MAX.
        let sum = s.saturating_add(d).saturating_add(127);
        let q = sum / 255;
        if q > 255 { 255 } else { q as u8 }
    };
    (blend(sr, dr), blend(sg, dg), blend(sb, db))
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
        NamedColor::White | NamedColor::DimWhite => RgbaColor(0xD3, 0xD7, 0xCF, 0xFF),
        NamedColor::BrightBlack => RgbaColor(0x55, 0x57, 0x53, 0xFF),
        NamedColor::BrightRed => RgbaColor(0xEF, 0x29, 0x29, 0xFF),
        NamedColor::BrightGreen => RgbaColor(0x8A, 0xE2, 0x34, 0xFF),
        NamedColor::BrightYellow => RgbaColor(0xFC, 0xE9, 0x4F, 0xFF),
        NamedColor::BrightBlue => RgbaColor(0x72, 0x9F, 0xCF, 0xFF),
        NamedColor::BrightMagenta => RgbaColor(0xAD, 0x7F, 0xA8, 0xFF),
        NamedColor::BrightCyan => RgbaColor(0x34, 0xE2, 0xE2, 0xFF),
        NamedColor::BrightWhite | NamedColor::BrightForeground => {
            RgbaColor(0xEE, 0xEE, 0xEC, 0xFF)
        }
        // Foreground: opaque white; the overlay sits on top of the PNG.
        NamedColor::Foreground => RgbaColor(0xFF, 0xFF, 0xFF, 0xFF),
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
            RgbaColor(cube_channel(r_idx), cube_channel(g_idx), cube_channel(b_idx), 0xFF)
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
#[allow(clippy::expect_used, clippy::indexing_slicing, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn blit_background_byte_order() {
        // 2×2 framebuffer, stride = 2*4 = 8 bytes.
        let dims = FramebufferDims { w: 2, h: 2, stride: 8 };
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
        let dims = FramebufferDims { w: 1, h: 2, stride: 8 };
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
        let dims = FramebufferDims { w: 1, h: 2, stride: 4 };
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
        let dims = FramebufferDims { w: 4, h: 4, stride: 16 };
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
        };
        let fg = RgbaColor(200, 100, 50, 0xFF);
        // Transparent bg → only the glyph blend stage runs.
        let bg = RgbaColor(0, 0, 0, 0);

        // Place the glyph at (1, 1) so the top-left lands on pixel (1, 1).
        blit_cell(&mut fb, dims, &glyph, 1, 1, fg, bg);

        // Pixel (1, 1): full coverage → fg overwrites dst exactly.
        let p11: usize = 16 + 4;
        assert_eq!(fb[p11], 50); // B = fg.b
        assert_eq!(fb[p11 + 1], 100); // G = fg.g
        assert_eq!(fb[p11 + 2], 200); // R = fg.r
        assert_eq!(fb[p11 + 3], 0); // X always 0

        // Pixel (2, 1): coverage = 128. Compute expected channel value
        // exactly the way the implementation does.
        let blend = |s: u8, d: u8, a: u8| -> u8 {
            let s = u16::from(s) * u16::from(a);
            let d = u16::from(d) * (255u16 - u16::from(a));
            ((s + d + 127) / 255) as u8
        };
        let p21: usize = 16 + 2 * 4;
        let exp_r = blend(200, 10, 128);
        let exp_g = blend(100, 20, 128);
        let exp_b = blend(50, 30, 128);
        assert_eq!(fb[p21], exp_b, "B at (2,1)");
        assert_eq!(fb[p21 + 1], exp_g, "G at (2,1)");
        assert_eq!(fb[p21 + 2], exp_r, "R at (2,1)");

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
        let dims = FramebufferDims { w: 2, h: 2, stride: 8 };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];

        let glyph = GlyphBitmap {
            width: 2,
            height: 2,
            coverage: vec![0, 0, 0, 0], // glyph contributes nothing
        };
        let fg = RgbaColor(255, 255, 255, 0xFF);
        let bg = RgbaColor(0x40, 0x60, 0x80, 0xFF);

        blit_cell(&mut fb, dims, &glyph, 0, 0, fg, bg);

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
        let dims = FramebufferDims { w: 2, h: 2, stride: 8 };
        let mut fb = vec![0u8; (dims.stride * dims.h) as usize];
        let glyph = GlyphBitmap {
            width: 2,
            height: 2,
            coverage: vec![255, 255, 255, 255],
        };
        let fg = RgbaColor(10, 20, 30, 0xFF);
        let bg = RgbaColor(0, 0, 0, 0);

        blit_cell(&mut fb, dims, &glyph, 1, 1, fg, bg);

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
        assert_eq!(c.3, 0x00, "default background must be transparent so PNG shows");
    }

    #[test]
    fn resolve_color_indexed_cube() {
        // Index 16 = cube (0,0,0) = pure black.
        assert_eq!(
            resolve_color(Color::Indexed(16)),
            RgbaColor(0, 0, 0, 0xFF)
        );
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
        assert_eq!(
            resolve_color(Color::Indexed(232)),
            RgbaColor(8, 8, 8, 0xFF)
        );
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
    fn resolve_color_spec() {
        let c = resolve_color(Color::Spec(Rgb { r: 0x12, g: 0x34, b: 0x56 }));
        assert_eq!(c, RgbaColor(0x12, 0x34, 0x56, 0xFF));
    }
}
