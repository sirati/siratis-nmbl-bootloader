//! Termwiz `Capabilities` construction for the NMBL console.
//!
//! The initramfs ships no terminfo database. Without one, termwiz's
//! `Capabilities` carry no `cup` (`CursorAddress`) capability, and its
//! terminfo renderer falls back to a hand-rolled CSI cursor-address path
//! (`render/terminfo.rs::move_cursor_absolute`) that emits
//! `CSI {x+1};{y+1} H` — transposing row and column. ratatui's
//! `TermwizBackend` positions every changed cell with an absolute
//! `CursorPosition`, so on every incremental repaint that transposition
//! turns a horizontal run of cells into a vertical-down stair-step (a
//! full repaint after a resize is immune because `repaint_all` moves
//! between lines with `\r\n`, not absolute addressing).
//!
//! Bundling a terminfo entry that defines `cup` makes termwiz take the
//! correct `CursorAddress` path, which fixes the corruption. This is the
//! same byte-for-byte entry termwiz ships for its own Windows
//! `apply_builtin_terminfo` path.

use termwiz::caps::Capabilities;

use crate::error::Result;
use crate::nmbl_warn;

use super::util::tw_err;

/// A compiled `xterm-256color` terminfo entry, bundled into the binary.
pub(super) const BUNDLED_TERMINFO: &[u8] = include_bytes!("../data/xterm-256color");

/// Build a termwiz `Capabilities` set for the NMBL serial/VT console.
///
/// We deliberately do **not** trust the runtime environment: PID-1 boots
/// with no `$TERM` and no terminfo database on disk. Instead we feed
/// termwiz an explicit [`ProbeHints`] carrying:
///
/// - the bundled `xterm-256color` terminfo (for a correct `cup` —
///   see [`BUNDLED_TERMINFO`]),
/// - [`ColorLevel::TrueColor`] so 24-bit RGB is emitted directly as
///   `CSI 38;2;r;g;b m` rather than being quantised to a palette index,
/// - every optional capability enabled (hyperlinks, sixel, iTerm2 image
///   protocol, bracketed paste, mouse reporting) so the full terminal
///   feature set is available to any modern emulator on the other end of
///   the serial line,
/// - `force_terminfo_render_to_use_ansi_sgr` so SGR attributes are
///   emitted as standard ECMA-48 sequences, which render correctly even
///   through pagers and minimal emulators.
///
/// On the (extremely unlikely) failure to even parse the bundled
/// terminfo we fall back to the same hints without a database; the
/// truecolor/feature overrides still apply, only `cup` is missing.
pub(super) fn caps_from_env_with_fallback() -> Result<Capabilities> {
    use termwiz::caps::{ColorLevel, ProbeHints};

    let hints = ProbeHints::default()
        .term(Some("xterm-256color".to_owned()))
        .color_level(Some(ColorLevel::TrueColor))
        .hyperlinks(Some(true))
        .sixel(Some(true))
        .iterm2_image(Some(true))
        .bracketed_paste(Some(true))
        .mouse_reporting(Some(true))
        .force_terminfo_render_to_use_ansi_sgr(Some(true));

    let hints = match terminfo::Database::from_buffer(BUNDLED_TERMINFO) {
        Ok(db) => hints.terminfo_db(Some(db)),
        Err(e) => {
            nmbl_warn!(
                "TtyConsole: bundled terminfo failed to parse ({e}); \
                 cursor addressing may be wrong on incremental repaints"
            );
            hints
        }
    };

    Capabilities::new_with_hints(hints).map_err(tw_err)
}
