//! Unit tests for the splash background loading helpers.

use std::path::PathBuf;

use crate::config::SplashBackgroundLocation;
use crate::splash::types::FramebufferDims;
use crate::sys::ops::RealSys;

use super::background::{
    FALLBACK_BG_RGBA, SIDECAR_SPLASH_BG_BASENAME, load_sidecar_background_or_fallback,
    locate_sidecar_background, solid_background,
};

/// Smallest valid RGBA8 PNG: a 1x1 opaque-red pixel. Reused from
/// the `splash::png` decode tests so the sidecar loader exercises
/// the real decode path without touching a build asset.
const ONE_BY_ONE_RED_RGBA: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8, 0xcf, 0xc0, 0xf0,
    0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn dims(w: u32, h: u32) -> FramebufferDims {
    FramebufferDims {
        w,
        h,
        stride: w.saturating_mul(4),
    }
}

fn cfg_with_mountpoint(mp: Option<PathBuf>) -> crate::config::Config {
    let mut c = crate::config::Config::recovery_default();
    c.splash.background_location = SplashBackgroundLocation::BootPartition;
    c.runtime_boot_mountpoint = mp;
    c
}

#[test]
fn locate_sidecar_joins_fixed_basename_under_mountpoint() {
    let c = cfg_with_mountpoint(Some(PathBuf::from("/mnt/boot")));
    assert_eq!(
        locate_sidecar_background(&c).expect("mountpoint present resolves a path"),
        PathBuf::from("/mnt/boot/nmblsplash.png"),
    );
}

#[test]
fn locate_sidecar_is_none_without_mountpoint() {
    // Legacy embedded-config mode: no NMBL-mounted boot partition,
    // so the sidecar cannot be resolved and the loader must fall
    // back to the solid background.
    let c = cfg_with_mountpoint(None);
    assert!(locate_sidecar_background(&c).is_none());
}

#[test]
fn solid_background_fills_every_pixel() {
    let buf = solid_background(dims(2, 3), FALLBACK_BG_RGBA);
    assert_eq!(buf.len(), 2 * 3 * 4);
    for px in buf.chunks_exact(4) {
        assert_eq!(px, FALLBACK_BG_RGBA);
    }
}

#[test]
fn sidecar_loader_reads_png_from_boot_partition() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(SIDECAR_SPLASH_BG_BASENAME),
        ONE_BY_ONE_RED_RGBA,
    )
    .expect("stage sidecar png");
    let c = cfg_with_mountpoint(Some(dir.path().to_path_buf()));

    let fb = dims(4, 4);
    let scaled = load_sidecar_background_or_fallback(&mut RealSys::sync_only(), &c, fb);
    // A real decode+scale of the 1x1 red PNG to 4x4 yields a tight
    // 4*4*4 buffer of opaque-red pixels — distinct from the solid
    // fallback colour, proving the sidecar path was taken.
    assert_eq!(scaled.len(), 4 * 4 * 4);
    assert_eq!(
        scaled.get(0..4),
        Some([0xff, 0x00, 0x00, 0xff].as_slice()),
        "first pixel must be the decoded red, not the fallback",
    );
}

#[test]
fn sidecar_loader_falls_back_when_file_missing() {
    // Mountpoint is set but the sidecar file is absent: the loader
    // must degrade to the solid fallback, never error.
    let dir = tempfile::tempdir().expect("tempdir");
    let c = cfg_with_mountpoint(Some(dir.path().to_path_buf()));

    let fb = dims(4, 4);
    let scaled = load_sidecar_background_or_fallback(&mut RealSys::sync_only(), &c, fb);
    assert_eq!(scaled.len(), 4 * 4 * 4);
    assert_eq!(
        scaled.get(0..4),
        Some(FALLBACK_BG_RGBA.as_slice()),
        "missing sidecar must yield the solid fallback colour",
    );
}

#[test]
fn sidecar_loader_falls_back_on_corrupt_png() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join(SIDECAR_SPLASH_BG_BASENAME),
        b"not a png at all",
    )
    .expect("stage corrupt sidecar");
    let c = cfg_with_mountpoint(Some(dir.path().to_path_buf()));

    let fb = dims(4, 4);
    let scaled = load_sidecar_background_or_fallback(&mut RealSys::sync_only(), &c, fb);
    assert_eq!(scaled.len(), 4 * 4 * 4);
    assert_eq!(scaled.get(0..4), Some(FALLBACK_BG_RGBA.as_slice()));
}

#[test]
fn sidecar_loader_falls_back_without_mountpoint() {
    let c = cfg_with_mountpoint(None);
    let fb = dims(4, 4);
    let scaled = load_sidecar_background_or_fallback(&mut RealSys::sync_only(), &c, fb);
    assert_eq!(scaled.len(), 4 * 4 * 4);
    assert_eq!(scaled.get(0..4), Some(FALLBACK_BG_RGBA.as_slice()));
}
