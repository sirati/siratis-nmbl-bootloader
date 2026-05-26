//! DRM / simpledrm bring-up.
//!
//! Opens `/dev/dri/card0`, picks the preferred mode of the first
//! connected connector, allocates an XRGB8888 dumb buffer, mmaps it,
//! and exposes a page-flip primitive. `SplashDrm`'s `Drop` impl
//! munmaps and restores the original CRTC without panicking.
//!
//! Skeleton only — Phase 3 fills in the body.

#![allow(dead_code, unused_variables)]

use std::path::Path;

use crate::error::{NmblError, Result};
use crate::splash::types::FramebufferDims;

/// RAII handle to the open DRM device + active dumb buffer.
pub struct SplashDrm {
    _private: (),
}

/// Try to open the DRM card.
///
/// - `Ok(Some(drm))`: opened, mode-set succeeded.
/// - `Ok(None)`: device missing (ENOENT). Common on headless and
///   pre-`sysfb` setups; the caller falls back to the tty UI without
///   surfacing this as an error.
/// - `Err(_)`: device exists but bring-up failed.
pub fn open_card(_path: &Path) -> Result<Option<SplashDrm>> {
    Ok(None)
}

impl SplashDrm {
    pub fn dims(&self) -> FramebufferDims {
        FramebufferDims {
            w: 0,
            h: 0,
            stride: 0,
        }
    }

    /// Mutable view over the active dumb buffer.
    pub fn buffer_mut(&mut self) -> Result<&mut [u8]> {
        Err(NmblError::Tui {
            source: std::io::Error::other("splash::drm not implemented"),
        })
    }

    /// Atomically present the current buffer.
    pub fn flip(&mut self) -> Result<()> {
        Err(NmblError::Tui {
            source: std::io::Error::other("splash::drm not implemented"),
        })
    }
}
