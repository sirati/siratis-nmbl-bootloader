use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

use crate::splash::types::RgbaColor;

/// Alpha (0..255) of the selection-highlight background. ratatui's
/// `Color::Gray` highlight maps through SGR 37 to [`NamedColor::White`];
/// rendered in the *background* role it should read as a faint tint so
/// the photo stays visible. Kept separate from the foreground mapping so
/// white *text* (which also resolves through the White slot) is never
/// made fainter — see [`resolve_bg_color`].
pub(crate) const SELECTION_BG_ALPHA: u8 = 0x40;

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
