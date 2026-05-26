//! PNG decode wrapper around the `png` crate.
//!
//! Skeleton only — Phase 4a fills in the body.

#![allow(dead_code, unused_variables)]

use std::path::Path;

use crate::error::{NmblError, Result};

/// Decoded RGBA8 image. Pixels are row-major; no stride padding.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

pub fn decode_rgba(_path: &Path) -> Result<Image> {
    Err(NmblError::Tui {
        source: std::io::Error::other("splash::png not implemented"),
    })
}
