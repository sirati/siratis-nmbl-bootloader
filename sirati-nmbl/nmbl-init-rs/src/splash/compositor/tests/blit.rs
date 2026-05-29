#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]

use crate::splash::types::{FramebufferDims, GlyphBitmap, RgbaColor};

use super::super::{CellRect, blit_background, blit_cell};

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
