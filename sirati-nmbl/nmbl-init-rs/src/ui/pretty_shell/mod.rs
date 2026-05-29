//! Pretty-shell screen: host an in-process terminal emulator inside the
//! existing bordered TUI box so the operator can poke around without
//! NMBL `execve(2)`ing the shell as PID 1.
//!
//! The architecture is the same triangle the rest of NMBL uses: state
//! lives in [`crate::ui::app::Screen::PtyShell`], events go through
//! [`PtyShellState::on_key`], and the renderer in
//! [`crate::ui::view::render_pty_shell`] paints the alacritty grid into
//! a ratatui [`ratatui::widgets::Block`] with the rest of the screen
//! chrome (header / footer). The driver loop here glues the three
//! together.
//!
//! ## Why not Console::draw_with?
//!
//! Both shapes would compile. Picking the [`Screen::PtyShell`] variant
//! keeps the "all interactive UI is App+Screen" rule intact and makes
//! the lifecycle (entry, render, key handling, exit) discoverable from
//! [`crate::ui::view::render_current_screen`] alongside every other
//! screen. `draw_with` is reserved for dynamic widgets that don't fit
//! the state machine (download gauges, in-flight editors).
//!
//! ## Scrolling
//!
//! `alacritty_terminal`'s `Grid` carries a `display_offset` for
//! scrollback view. Ctrl+Shift+Up/Down step the offset one row at a
//! time; Ctrl+Shift+PageUp/PageDown jump a screenful; Ctrl+Shift+End
//! snaps back to the live tail. Any keystroke that is not a scroll
//! shortcut implicitly snaps the view to the bottom and is forwarded
//! to the child via the master fd.
//!
//! ## Quitting
//!
//! `Ctrl+Shift+<letter>` is not encodable over a legacy serial/xterm
//! line, so quit uses the OpenSSH-style escape instead: at the start of
//! a line, type `~.` to return to the emergency menu. The `~` is only
//! honoured immediately after a newline; a mid-line `~` is an ordinary
//! character, and `~~` sends a literal tilde.

/// Minimum grid dimensions used by the pretty-shell box. The runtime
/// size is derived from [`crate::ui::console::Console::size`] minus the
/// chrome the renderer paints (3-row header, 1-row footer, 2-row +
/// 2-col bordered block), so a 1920x1080 splash gets a much larger PTY
/// than the 80x24-derived floor below.
///
/// The floor exists for tiny consoles (degraded VGA, recovery serial
/// shim) so the alacritty grid never collapses to zero cells. On those
/// hosts the renderer still clips to the actual frame; the larger grid
/// just keeps the VT parser happy.
pub(super) const PRETTY_SHELL_MIN_COLS: u16 = 40;
pub(super) const PRETTY_SHELL_MIN_ROWS: u16 = 10;

/// Chrome rows the renderer consumes around the pretty-shell grid:
/// 3-row header, 1-row footer, and the bordered block eats 1 row on
/// top + 1 row on bottom (= 2). Total = 6.
pub(super) const CHROME_ROWS: u16 = 6;
/// Chrome columns the bordered block consumes: 1 col left + 1 col
/// right (= 2). The header/footer don't add side chrome.
pub(super) const CHROME_COLS: u16 = 2;

mod driver;
mod keys;
mod pump;
mod render;
mod state;

#[cfg(test)]
mod tests;

// Re-export the public API at the original module path so external
// callers using `crate::ui::pretty_shell::*` are unchanged.
pub use driver::run_pretty_shell;
pub use keys::key_to_bytes;
pub use state::PtyShellState;
