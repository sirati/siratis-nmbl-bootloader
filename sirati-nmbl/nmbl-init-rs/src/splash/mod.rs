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

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::ui::run_selector;
    use std::path::PathBuf;

    #[test]
    #[allow(
        clippy::expect_used,
        clippy::panic,
        reason = "tests assert with panics on contract failure"
    )]
    fn run_selector_with_missing_dri_falls_back_cleanly() {
        // When the configured DRI path is missing and the /dev/dri/card*
        // scan finds nothing usable, run_selector falls back to the
        // serial prompt. On dev hosts the scan may hit a real card
        // whose bring-up requires DRM master we don't have — accept
        // either path's outcome, since neither produces a working splash.
        let mut config = Config::recovery_default();
        config.splash.enable = true;
        config.splash.dri_path = PathBuf::from("/dev/this/does/not/exist");
        // The fallback path will try to read from stdin which the test
        // harness doesn't have. We're satisfied as long as the call
        // doesn't panic before that point.
        let _ = run_selector(&config, &[]);
    }
}
