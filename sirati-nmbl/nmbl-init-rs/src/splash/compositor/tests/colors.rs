#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "test assertions"
)]

use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::splash::types::RgbaColor;

use super::super::colors::SELECTION_BG_ALPHA;
use super::super::{resolve_bg_color, resolve_color};

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
