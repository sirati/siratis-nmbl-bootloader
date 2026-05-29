#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]

use crate::splash::types::{FramebufferDims, GlyphBitmap, RgbaColor};

use super::super::blit::{read_bgrx, src_over, write_bgrx};
use super::super::{CellRect, TextLayer, fill_cell_bg};
use super::halo::grey_fb;

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
