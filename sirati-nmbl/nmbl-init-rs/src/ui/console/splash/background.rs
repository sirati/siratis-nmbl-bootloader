//! Sidecar splash background loading helpers.
//!
//! Resolves, decodes, and cover-scales the boot-partition background PNG,
//! with graceful degradation to a solid fallback colour on any failure.

use std::path::PathBuf;

use crate::config::Config;
use crate::nmbl_warn;
use crate::splash::png;
use crate::splash::scale;
use crate::splash::types::FramebufferDims;
use crate::sys::ops::FsOps;

/// FIXED basename of the sidecar splash background on the boot
/// partition, used when
/// `splash.background_location = "boot-partition"`. Deliberately NOT
/// configurable — the file is always staged next to the initrd
/// (`nmbl-initrd`) at the boot-partition root. The name omits a dash
/// to stay FAT-friendly. Mirrors the rescue-sfs sidecar precedent,
/// which keys off [`crate::config::Config::runtime_boot_mountpoint`].
pub const SIDECAR_SPLASH_BG_BASENAME: &str = "nmblsplash.png";

/// Solid fallback background colour (RGBA8) painted across the whole
/// framebuffer when the sidecar background cannot be loaded. A dark
/// slate so the menu chrome stays legible without an image. Matches
/// the "render splash with a solid background" graceful-degradation
/// contract.
pub(super) const FALLBACK_BG_RGBA: [u8; 4] = [0x1e, 0x1e, 0x2e, 0xff];

/// Resolve the on-disk path of the sidecar splash background.
///
/// The background lives at the FIXED basename
/// [`SIDECAR_SPLASH_BG_BASENAME`] under the boot-partition root, joined
/// against [`Config::runtime_boot_mountpoint`] (populated by Phase 0.5
/// after the boot partition is mounted). Returns `None` in legacy
/// embedded-config mode where no NMBL-mounted boot partition exists —
/// the caller then degrades to the solid fallback background. Mirrors
/// `rescue::locate_sfs`'s "no runtime mountpoint" handling, minus the
/// hard error: a missing splash background must never block boot.
pub(super) fn locate_sidecar_background(config: &Config) -> Option<PathBuf> {
    config
        .runtime_boot_mountpoint
        .as_deref()
        .map(|mp| mp.join(SIDECAR_SPLASH_BG_BASENAME))
}

/// Build a tight RGBA8 buffer of `dims.w * dims.h` pixels filled with a
/// solid colour. Used as the last-resort background when the sidecar
/// PNG is missing or unreadable so the whole framebuffer is painted
/// (an empty buffer would leave the dumb buffer's prior contents
/// showing through between cell fills).
pub(super) fn solid_background(dims: FramebufferDims, rgba: [u8; 4]) -> Vec<u8> {
    let pixels = (dims.w as usize).saturating_mul(dims.h as usize);
    let mut buf = Vec::with_capacity(pixels.saturating_mul(4));
    for _ in 0..pixels {
        buf.extend_from_slice(&rgba);
    }
    buf
}

/// Load the sidecar background PNG from the boot partition and
/// cover-scale it to `fb_dims`. On ANY failure — unknown mountpoint,
/// missing file, decode error, or a scaler that rejects the decoded
/// dimensions — emit a single `nmbl_warn!` and return a solid-colour
/// fallback buffer so the splash chrome still renders. Never returns
/// an error: a sidecar background is best-effort and must not block
/// boot.
pub(super) fn load_sidecar_background_or_fallback(
    fs: &mut dyn FsOps,
    config: &Config,
    fb_dims: FramebufferDims,
) -> Vec<u8> {
    let Some(path) = locate_sidecar_background(config) else {
        nmbl_warn!(
            "splash: background_location=boot-partition but the boot partition mountpoint is \
             unknown (legacy embedded-config mode); using solid fallback background"
        );
        return solid_background(fb_dims, FALLBACK_BG_RGBA);
    };

    // Read the sidecar PNG through the FsOps seam, then decode the bytes.
    // Any failure (read or decode) WARNs and degrades to the solid
    // fallback — best-effort, never blocks boot.
    let image = match fs
        .read_file(&path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| png::decode_rgba_from_bytes(&bytes).map_err(|e| e.to_string()))
    {
        Ok(img) => img,
        Err(e) => {
            nmbl_warn!(
                "splash: sidecar background {} could not be loaded ({e}); using solid fallback \
                 background",
                path.display(),
            );
            return solid_background(fb_dims, FALLBACK_BG_RGBA);
        }
    };

    let scaled = scale::cover_scale_nearest(&image.rgba, image.width, image.height, fb_dims);
    if scaled.is_empty() {
        nmbl_warn!(
            "splash: sidecar background {} decoded to unusable dimensions ({}x{}); using solid \
             fallback background",
            path.display(),
            image.width,
            image.height,
        );
        return solid_background(fb_dims, FALLBACK_BG_RGBA);
    }
    scaled
}
