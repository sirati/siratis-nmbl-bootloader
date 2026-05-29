//! DRM-framebuffer backend for the [`Console`] abstraction.
//!
//! Owns: a [`SplashDrm`] for mode-set + flip, the pre-scaled
//! background, the [`GlyphCache`] for font rasterisation, [`CellDims`]
//! for the cell grid, and a [`SplashInput`] for raw-mode key reads via
//! `/dev/tty1`. Rendering goes through the pre-existing
//! [`crate::ui::render_splash_frame`] pipeline — ratatui-draw → vte
//! parse → cell-walk → blit — so the splash-side `run_splash_selector`
//! and this trait impl stay byte-identical at the framebuffer level.
//!
//! No new `unsafe` is introduced; all syscalls flow through the splash
//! primitives' existing rustix-based wrappers.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use crate::config::{Config, SplashBackgroundLocation};
use crate::error::{NmblError, Result};
use crate::log;
use crate::nmbl_warn;
use crate::splash::drm::{SplashDrm, open_card_with_fallback};
use crate::splash::glyph_cache::{self, GlyphCache};
use crate::splash::input::SplashInput;
use crate::splash::png;
use crate::splash::scale;
use crate::splash::types::{CellDims, FramebufferDims};
use crate::ui::POLL_SLICE;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::{render_splash_frame, render_splash_frame_with};

/// Tty node opened for raw-mode keyboard input alongside the DRM
/// framebuffer output. See [`crate::ui::INPUT_TTY_PATH`] for the
/// rationale; we mirror that constant here so this module is
/// self-contained.
const INPUT_TTY_PATH: &str = "/dev/tty1";

/// Font size, in pixels, used to rasterise the splash glyph cache.
/// Same value as the existing `crate::ui::SPLASH_FONT_PX`.
const SPLASH_FONT_PX: f32 = 16.0;

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
const FALLBACK_BG_RGBA: [u8; 4] = [0x1e, 0x1e, 0x2e, 0xff];

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
fn locate_sidecar_background(config: &Config) -> Option<PathBuf> {
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
fn solid_background(dims: FramebufferDims, rgba: [u8; 4]) -> Vec<u8> {
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
fn load_sidecar_background_or_fallback(config: &Config, fb_dims: FramebufferDims) -> Vec<u8> {
    let Some(path) = locate_sidecar_background(config) else {
        nmbl_warn!(
            "splash: background_location=boot-partition but the boot partition mountpoint is \
             unknown (legacy embedded-config mode); using solid fallback background"
        );
        return solid_background(fb_dims, FALLBACK_BG_RGBA);
    };

    let image = match png::decode_rgba(&path) {
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

/// DRM-backed console. Constructed via [`SplashConsole::open`].
pub struct SplashConsole {
    drm: SplashDrm,
    bg_scaled: Vec<u8>,
    cache: GlyphCache,
    cell_dims: CellDims,
    input: SplashInput,
}

impl SplashConsole {
    /// Bring up the splash backend.
    ///
    /// Returns `Ok(Some(_))` on a clean bring-up, `Ok(None)` when the
    /// backend is unavailable (no DRM device, no font, etc.; the
    /// orchestrator falls back to tty), and `Err(_)` only when a
    /// real, surfaced bring-up error occurred mid-flight.
    pub fn open(config: &Config) -> Result<Option<SplashConsole>> {
        // 1. Open the DRM card. Missing / inaccessible nodes map to
        //    `Ok(None)` inside `open_card_with_fallback`, so this
        //    propagates only real bring-up errors.
        let drm = match open_card_with_fallback(&config.splash.dri_path)? {
            Some(d) => d,
            None => return Ok(None),
        };
        let fb_dims = drm.dims();

        // 2. Load the background PNG and cover-scale it to the framebuffer.
        //
        //    Two sources, selected by `splash.background_location`:
        //    * `Initrd` (default): decode the embedded PNG at
        //      `splash.background_image`. A decode failure here is a
        //      real bring-up error (the asset is baked into the
        //      initramfs and must be present) and propagates as today.
        //    * `BootPartition`: decode the sidecar PNG staged next to
        //      the initrd on the boot partition, resolved against the
        //      Phase-0.5 mountpoint. Phase ordering: in bootstrap mode
        //      `run_bootstrap_phase` mounts the boot partition and sets
        //      `runtime_boot_mountpoint` BEFORE `open_console` runs, so
        //      the file is reachable here. If the mountpoint is unknown
        //      (legacy embedded-config mode) or the PNG is
        //      missing/unreadable/corrupt, we WARN and fall back to a
        //      solid background — never panic, never block boot. This
        //      mirrors how `rescue::disk` treats a missing
        //      `nmbl-rescue.sfs` on the boot partition.
        let bg_scaled = match config.splash.background_location {
            SplashBackgroundLocation::Initrd => {
                let bg_image = png::decode_rgba(&config.splash.background_image)?;
                scale::cover_scale_nearest(&bg_image.rgba, bg_image.width, bg_image.height, fb_dims)
            }
            SplashBackgroundLocation::BootPartition => {
                load_sidecar_background_or_fallback(config, fb_dims)
            }
        };

        // 3. Load the font and derive grid dimensions from the cell size.
        //
        //    Try the configured on-disk font first. On ANY load error
        //    (missing file, unreadable, corrupt/unsupported face) WARN
        //    and fall back to the DejaVu Sans Mono baked into the binary
        //    so a bad operator font degrades gracefully instead of
        //    dropping splash entirely. Mirrors how the sidecar
        //    background falls back to a solid fill.
        let cache = match glyph_cache::load(&config.splash.font_path, SPLASH_FONT_PX) {
            Ok(cache) => cache,
            Err(e) => {
                nmbl_warn!(
                    "splash: failed to load font {} ({e}); using embedded fallback",
                    config.splash.font_path.display()
                );
                glyph_cache::load_embedded_fallback(SPLASH_FONT_PX)?
            }
        };
        let cell_size = cache.cell_size();
        let cell_w = cell_size.w.max(1);
        let cell_h = cell_size.h.max(1);
        let cols = (fb_dims.w / cell_w).min(u32::from(u16::MAX)) as u16;
        let rows = (fb_dims.h / cell_h).min(u32::from(u16::MAX)) as u16;
        if cols == 0 || rows == 0 {
            return Err(NmblError::Tui {
                source: std::io::Error::other("splash framebuffer too small for one cell"),
            });
        }
        let cell_dims = CellDims {
            cols,
            rows,
            cell_w,
            cell_h,
        };

        // 4. Open /dev/tty1 for raw-mode keyboard input.
        let input = SplashInput::open(Path::new(INPUT_TTY_PATH))?;

        // The splash bring-up sequence already calls KDSETMODE(KD_GRAPHICS)
        // on /dev/tty1, which suppresses kernel printk to that VT. We
        // still flip the macro gate so `nmbl_*!` stops writing to
        // stderr (which would race the ratatui repaint on the splash
        // framebuffer and also leak to any serial line registered as a
        // secondary console).
        log::set_tui_active();

        Ok(Some(SplashConsole {
            drm,
            bg_scaled,
            cache,
            cell_dims,
            input,
        }))
    }

    /// Borrow the cell-grid dimensions. Useful for callers that need
    /// to lay out modals against the grid without re-querying through
    /// the trait.
    pub fn cell_dims(&self) -> CellDims {
        self.cell_dims
    }
}

impl Console for SplashConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        render_splash_frame(
            &mut self.drm,
            &self.bg_scaled,
            &self.cache,
            self.cell_dims,
            app,
        )
    }

    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move {
            // A key buffered by a prior poll is ready now; skip the
            // reactor and drain it.
            if self.input.has_pending() {
                return Ok(self
                    .input
                    .poll(Duration::from_millis(0))?
                    .map(ConsoleEvent::Key));
            }
            // Await readability on /dev/tty1 through tokio's reactor,
            // then run the identical synchronous drain (which keeps the
            // bare-Esc 10ms follow-up disambiguation). No borrow held
            // across the await.
            let slice = timeout.min(POLL_SLICE);
            super::await_fd_readable(self.input.input_fd(), slice).await?;
            Ok(self
                .input
                .poll(Duration::from_millis(0))?
                .map(ConsoleEvent::Key))
        })
    }

    fn poll_event_blocking(&mut self, timeout: Duration) -> Result<Option<ConsoleEvent>> {
        // Cap the effective wait the same way [`TtyConsole`] does so
        // backends are uniformly responsive to ticking countdowns and
        // spinner animations. The caller-supplied timeout is honoured
        // but never longer than POLL_SLICE per call; the trait doc
        // pins this contract for both backends.
        //
        // The splash framebuffer has a fixed cell grid derived at
        // bring-up from the DRM mode, so this backend never emits
        // resize events — only keys.
        let slice = timeout.min(POLL_SLICE);
        Ok(self.input.poll(slice)?.map(ConsoleEvent::Key))
    }

    fn size(&self) -> (u16, u16) {
        (self.cell_dims.cols, self.cell_dims.rows)
    }

    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Splash
    }

    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        render_splash_frame_with(
            &mut self.drm,
            &self.bg_scaled,
            &self.cache,
            self.cell_dims,
            body,
        )
    }

    /// Hand the framebuffer back to the kernel-elected VT so the
    /// kernel resumes painting printk + the active VT renders the
    /// multiplexed shell output. We release DRM master and restore
    /// the input tty's termios so the foreign writer can pass bytes
    /// through `/dev/tty1` without our raw-mode flags eating them.
    ///
    /// The mode-set state is preserved — `resume` re-acquires master
    /// and re-renders the splash composite without re-running the
    /// font load / cover-scale pipeline.
    fn suspend(&mut self) -> Result<()> {
        // Re-enable eprintln in the `nmbl_*!` macros so any warning
        // emitted by the rest of the suspend / relay path reaches the
        // operator's pre-shell screen. Re-armed on `resume`.
        log::clear_tui_active();
        // DRM master FIRST: doing it before termios restore minimises
        // the window where the kernel could paint printk while
        // userspace still has raw-mode termios.
        self.drm.drop_master();
        if let Err(e) = self.input.suspend() {
            nmbl_warn!("SplashConsole::suspend: input suspend failed: {e}");
        }
        Ok(())
    }

    /// Re-acquire the framebuffer + raw-mode input tty. The render
    /// pipeline is unchanged; the next [`render`] call will flush a
    /// full frame because each splash frame redoes the composite +
    /// page-flip from scratch (no incremental updates).
    fn resume(&mut self) -> Result<()> {
        if let Err(e) = self.input.resume() {
            nmbl_warn!("SplashConsole::resume: input resume failed: {e}");
        }
        self.drm.acquire_master();
        // Re-arm the macro gate so the post-shell render path doesn't
        // leak eprintln smear over the splash framebuffer.
        log::set_tui_active();
        Ok(())
    }

    fn caps_lock_active(&self) -> Option<bool> {
        // `/dev/tty1` is always a kernel VT, so KDGKBLED works here and
        // reports the live Caps-Lock state of the framebuffer keyboard.
        self.input.caps_lock_active()
    }
}

impl Drop for SplashConsole {
    fn drop(&mut self) {
        // Final handover (kexec / emergency execve): re-enable
        // eprintln in `nmbl_*!`. The splash backend's other Drop
        // chains (SplashDrm, SplashInput) handle KD mode and termios
        // restoration on their own.
        log::clear_tui_active();
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;
    use crate::config::SplashBackgroundLocation;

    /// Smallest valid RGBA8 PNG: a 1x1 opaque-red pixel. Reused from
    /// the `splash::png` decode tests so the sidecar loader exercises
    /// the real decode path without touching a build asset.
    const ONE_BY_ONE_RED_RGBA: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn dims(w: u32, h: u32) -> FramebufferDims {
        FramebufferDims {
            w,
            h,
            stride: w.saturating_mul(4),
        }
    }

    fn cfg_with_mountpoint(mp: Option<PathBuf>) -> Config {
        let mut c = Config::recovery_default();
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
        let scaled = load_sidecar_background_or_fallback(&c, fb);
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
        let scaled = load_sidecar_background_or_fallback(&c, fb);
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
        let scaled = load_sidecar_background_or_fallback(&c, fb);
        assert_eq!(scaled.len(), 4 * 4 * 4);
        assert_eq!(scaled.get(0..4), Some(FALLBACK_BG_RGBA.as_slice()));
    }

    #[test]
    fn sidecar_loader_falls_back_without_mountpoint() {
        let c = cfg_with_mountpoint(None);
        let fb = dims(4, 4);
        let scaled = load_sidecar_background_or_fallback(&c, fb);
        assert_eq!(scaled.len(), 4 * 4 * 4);
        assert_eq!(scaled.get(0..4), Some(FALLBACK_BG_RGBA.as_slice()));
    }
}
