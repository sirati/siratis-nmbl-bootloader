//! Render primitives for the graphical boot splash.
//!
//! Gated behind the `image-splash` Cargo feature. The orchestration —
//! opening DRM, polling input, running the boot menu — lives in
//! [`crate::ui::run_selector`]; this module only owns the rendering
//! and input primitives the orchestrator stitches together.
//!
//! Submodule layout: [`drm`] owns the framebuffer, [`png`] decodes the
//! background image, [`scale`] cover-scales it to the framebuffer,
//! [`glyph_cache`] pre-rasterises the font, [`terminal`] runs an
//! `alacritty_terminal::Term` over the ratatui-rendered ANSI bytes,
//! [`compositor`] paints cells into the framebuffer, [`input`]
//! provides keyboard input via `/dev/tty1`, and [`passphrase_demo`]
//! is a UI-only demo of the LUKS passphrase prompt.

pub mod compositor;
pub mod drm;
pub mod glyph_cache;
pub mod input;
pub mod passphrase_demo;
pub mod png;
pub mod scale;
pub mod terminal;
pub mod types;

// `run_selector` no longer opens its own splash console — the
// orchestrator brings up the boot console once (via
// `crate::ui::console::open_console`) before phase 1 and passes the
// resulting `&mut dyn Console` through every subsequent phase. The
// "missing DRI falls back to serial" decision now lives in
// `console::decide_backend` / `console::open_console`, both of which
// have their own pure-decision tests under `src/ui/console/mod.rs`.
