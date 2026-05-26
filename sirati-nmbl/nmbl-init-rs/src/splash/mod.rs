//! Optional graphical boot splash.
//!
//! Gated behind the `image-splash` Cargo feature. When the feature is
//! enabled and `config.splash.enable == true`, [`try_run_selector`]
//! drives the boot menu through a DRM framebuffer instead of through
//! `/dev/console`. Every failure path returns `Ok(None)` or `Err(_)`
//! so the caller can fall back to today's tty UI.
//!
//! The submodules are scaffolding: each holds the public signatures
//! the subsequent splash phases fill in. Keeping the tree here in one
//! commit lets later work proceed on disjoint files without
//! redefining shared types.

pub mod compositor;
pub mod drm;
pub mod glyph_cache;
pub mod png;
pub mod scale;
pub mod terminal;
pub mod types;

use crate::config::Config;
use crate::error::Result;
use crate::generations::Generation;
use crate::ui::Decision;

/// Attempt to drive the boot menu through the splash renderer.
///
/// - `Ok(Some(decision))`: splash rendered and the operator chose.
/// - `Ok(None)`: splash is unavailable (no DRM device, no assets);
///   caller should fall back to the tty UI without surfacing an error.
/// - `Err(_)`: splash was attempted and failed mid-flight; caller logs
///   and falls back to the tty UI.
pub fn try_run_selector(
    _config: &Config,
    _generations: &[Generation],
) -> Result<Option<Decision>> {
    Ok(None)
}
