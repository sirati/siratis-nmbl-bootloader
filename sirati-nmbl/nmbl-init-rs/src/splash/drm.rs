//! DRM / simpledrm bring-up.
//!
//! Opens `/dev/dri/card0`, picks the preferred mode of the first
//! connected connector, allocates an XRGB8888 dumb buffer, mmaps it,
//! and exposes a page-flip primitive. `SplashDrm`'s `Drop` impl
//! munmaps and restores the original CRTC without panicking.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use drm::Device as BasicDevice;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{
    Device as ControlDevice, Mode, ModeTypeFlags, connector, crtc, dumbbuffer::DumbBuffer,
    dumbbuffer::DumbMapping, framebuffer,
};

use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::splash::types::FramebufferDims;

/// Thin newtype wrapper around an `OwnedFd` so we can implement the
/// `drm` crate's `Device` traits on it. The fd is closed automatically
/// when this value drops.
struct Card(OwnedFd);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl BasicDevice for Card {}
impl ControlDevice for Card {}

/// RAII handle to the open DRM device + active dumb buffer.
///
/// On `Drop` the buffer is unmapped (by `DumbMapping`'s own `Drop`),
/// the original CRTC mode is restored, and the framebuffer + dumb
/// buffer are destroyed. The fd is then closed by `OwnedFd`'s `Drop`.
/// None of these steps panic; cleanup failures are logged through
/// `nmbl_warn!` and execution continues.
pub struct SplashDrm {
    card: Card,
    dims: FramebufferDims,

    // Active mode-set state.
    connector: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    fb: framebuffer::Handle,
    buffer: DumbBuffer,

    // Mapping must outlive `buffer` (it borrows `card`) but be dropped
    // before `card` so the munmap ioctl still has a valid fd. `Option`
    // is the standard trick: take it in `Drop` so the mapping's own
    // destructor runs first.
    mapping: Option<DumbMapping<'static>>,

    // Saved state to restore on drop. simpledrm boots with a CRTC
    // driving the firmware framebuffer; if we don't restore the
    // original mode the kernel console is left pointing at our soon-
    // to-be-destroyed buffer.
    original_crtc: crtc::Info,
}

/// Try to open the DRM card.
///
/// - `Ok(Some(drm))`: opened, mode-set succeeded.
/// - `Ok(None)`: device missing (`ENOENT`) or unavailable to us
///   (`EACCES`). Common on headless and pre-`sysfb` setups; the caller
///   falls back to the tty UI without surfacing this as an error.
/// - `Err(_)`: device exists but bring-up failed.
pub fn open_card(path: &Path) -> Result<Option<SplashDrm>> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)
    {
        Ok(f) => f,
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => return Ok(None),
            _ => return Err(NmblError::Tui { source: e }),
        },
    };
    let card = Card(OwnedFd::from(file));

    bring_up(card).map(Some)
}

/// Wrap an `io::Error` from a `drm` crate call into the project's
/// `NmblError::Tui` variant. Centralised so the call sites stay tidy.
fn tui_err<E: Into<io::Error>>(e: E) -> NmblError {
    NmblError::Tui { source: e.into() }
}

fn io_other(msg: &'static str) -> NmblError {
    NmblError::Tui {
        source: io::Error::other(msg),
    }
}

/// Enumerate resources, pick a connected connector + mode + CRTC,
/// allocate a dumb buffer, map it, and set the mode. Splitting this
/// out of [`open_card`] keeps the error-conversion surface small.
fn bring_up(card: Card) -> Result<SplashDrm> {
    let resources = card.resource_handles().map_err(tui_err)?;

    // First connected connector with at least one mode.
    let (connector_handle, connector_info) = resources
        .connectors()
        .iter()
        .find_map(|h| {
            let info = card.get_connector(*h, false).ok()?;
            if info.state() == connector::State::Connected && !info.modes().is_empty() {
                Some((*h, info))
            } else {
                None
            }
        })
        .ok_or_else(|| io_other("no connected DRM connector with usable modes"))?;

    // Prefer a PREFERRED-flagged mode, otherwise take the first.
    let mode = connector_info
        .modes()
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .copied()
        .or_else(|| connector_info.modes().first().copied())
        .ok_or_else(|| io_other("connector reported no modes"))?;

    let (width, height) = mode.size();
    let width = u32::from(width);
    let height = u32::from(height);

    // Pick a CRTC that can drive this connector. The connector's
    // current encoder, if any, points at the simplest correct choice;
    // otherwise try every encoder and intersect with the resource
    // list's CRTC set.
    let crtc_handle = pick_crtc(&card, &resources, &connector_info)?;

    // Snapshot the original CRTC so we can restore it on Drop.
    let original_crtc = card.get_crtc(crtc_handle).map_err(tui_err)?;

    // XRGB8888: 32 bpp, 4 bytes per pixel. simpledrm exposes only this
    // format across every NixOS-shipped firmware framebuffer path.
    let mut buffer = card
        .create_dumb_buffer((width, height), DrmFourcc::Xrgb8888, 32)
        .map_err(tui_err)?;
    let pitch = buffer.pitch();

    let fb = match card.add_framebuffer(&buffer, 24, 32) {
        Ok(fb) => fb,
        Err(e) => {
            // Best effort: free the dumb buffer if framebuffer creation
            // fails. `destroy_dumb_buffer` consumes the handle.
            if let Err(de) = card.destroy_dumb_buffer(buffer) {
                nmbl_warn!("splash::drm: destroy_dumb_buffer after add_framebuffer error: {de}");
            }
            return Err(tui_err(e));
        }
    };

    // Map the buffer. `DumbMapping<'a>` borrows `&'a mut DumbBuffer`,
    // but the mmap region itself outlives that borrow — it stays valid
    // until `DumbMapping`'s own `Drop` calls `munmap`. We need to widen
    // the lifetime to `'static` so the struct can store it alongside
    // `buffer`. The `Result` is mapped in one shot to release the
    // mutable borrow on `buffer` immediately, freeing up cleanup paths.
    //
    // SAFETY: the mmap region is independent of `&mut buffer`; it
    // persists until munmap. We store both `mapping` and `card` inside
    // `SplashDrm`, and `Drop` explicitly `.take()`s the mapping before
    // any field — including `card` — runs its own destructor, so the
    // munmap ioctl in `DumbMapping::drop` always sees a live fd. The
    // mapping is never exposed by reference outside `&mut self`, so the
    // widened lifetime never escapes.
    let map_res: io::Result<DumbMapping<'static>> = card
        .map_dumb_buffer(&mut buffer)
        .map(|m| unsafe { std::mem::transmute::<DumbMapping<'_>, DumbMapping<'static>>(m) });
    let mapping = match map_res {
        Ok(m) => m,
        Err(e) => {
            if let Err(fe) = card.destroy_framebuffer(fb) {
                nmbl_warn!("splash::drm: destroy_framebuffer after map error: {fe}");
            }
            if let Err(de) = card.destroy_dumb_buffer(buffer) {
                nmbl_warn!("splash::drm: destroy_dumb_buffer after map error: {de}");
            }
            return Err(tui_err(e));
        }
    };

    // Commit the mode.
    if let Err(e) = card.set_crtc(crtc_handle, Some(fb), (0, 0), &[connector_handle], Some(mode)) {
        // Tear down in reverse order. `mapping` is dropped here, which
        // munmaps; then the framebuffer and dumb buffer.
        drop(mapping);
        if let Err(fe) = card.destroy_framebuffer(fb) {
            nmbl_warn!("splash::drm: destroy_framebuffer after set_crtc error: {fe}");
        }
        if let Err(de) = card.destroy_dumb_buffer(buffer) {
            nmbl_warn!("splash::drm: destroy_dumb_buffer after set_crtc error: {de}");
        }
        return Err(tui_err(e));
    }

    Ok(SplashDrm {
        card,
        dims: FramebufferDims {
            w: width,
            h: height,
            stride: pitch,
        },
        connector: connector_handle,
        crtc: crtc_handle,
        mode,
        fb,
        buffer,
        mapping: Some(mapping),
        original_crtc,
    })
}

/// Pick a CRTC for the given connector. Try the connector's current
/// encoder first (almost always the right answer on simpledrm), then
/// fall back to each candidate encoder's possible-CRTC set filtered
/// against the resource list.
fn pick_crtc(
    card: &Card,
    resources: &drm::control::ResourceHandles,
    connector_info: &connector::Info,
) -> Result<crtc::Handle> {
    if let Some(enc_h) = connector_info.current_encoder()
        && let Ok(enc) = card.get_encoder(enc_h)
        && let Some(c) = enc.crtc()
    {
        return Ok(c);
    }

    for enc_h in connector_info.encoders() {
        let Ok(enc) = card.get_encoder(*enc_h) else {
            continue;
        };
        let filtered = resources.filter_crtcs(enc.possible_crtcs());
        if let Some(first) = filtered.first() {
            return Ok(*first);
        }
    }

    Err(io_other("no CRTC available for connector"))
}

impl SplashDrm {
    /// Framebuffer dimensions and stride (in bytes per scanline).
    pub fn dims(&self) -> FramebufferDims {
        self.dims
    }

    /// Mutable view over the active dumb buffer. The compositor writes
    /// XRGB8888 pixels here; [`Self::flip`] then commits them.
    pub fn buffer_mut(&mut self) -> Result<&mut [u8]> {
        match self.mapping.as_mut() {
            Some(m) => Ok(m.as_mut()),
            None => Err(io_other("dumb buffer mapping is gone")),
        }
    }

    /// Commit the current buffer contents to the connector.
    ///
    /// We re-issue `set_crtc` per frame: synchronous and trivially
    /// correct on simpledrm, which doesn't expose page-flip events.
    /// Splash redraws are coarse (key press / dirty tick), so the
    /// vblank stall is not a concern.
    pub fn flip(&mut self) -> Result<()> {
        self.card
            .set_crtc(
                self.crtc,
                Some(self.fb),
                (0, 0),
                &[self.connector],
                Some(self.mode),
            )
            .map_err(tui_err)
    }
}

impl Drop for SplashDrm {
    fn drop(&mut self) {
        // Unmap the buffer first: `DumbMapping`'s own `Drop` calls
        // `munmap`. Taking it out of the `Option` guarantees it runs
        // before any of the destroy ioctls below.
        let _ = self.mapping.take();

        // Restore the original CRTC mode. If the firmware left the
        // CRTC in some non-mode-set state we just disable the CRTC
        // (set_crtc with None framebuffer + None mode); that's the
        // standard kernel handover dance.
        let restore = self.card.set_crtc(
            self.original_crtc.handle(),
            self.original_crtc.framebuffer(),
            self.original_crtc.position(),
            &[self.connector],
            self.original_crtc.mode(),
        );
        if let Err(e) = restore {
            nmbl_warn!("splash::drm: failed to restore original CRTC: {e}");
        }

        if let Err(e) = self.card.destroy_framebuffer(self.fb) {
            nmbl_warn!("splash::drm: destroy_framebuffer in Drop: {e}");
        }

        if let Err(e) = self.card.destroy_dumb_buffer(self.buffer) {
            nmbl_warn!("splash::drm: destroy_dumb_buffer in Drop: {e}");
        }

        // `self.card.0` (`OwnedFd`) closes when `self.card` drops.
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
    use std::path::PathBuf;

    /// Most important contract: a missing DRM node yields `Ok(None)`
    /// so the caller silently falls back to the tty UI. Other I/O
    /// errors must surface; an unreachable path under `/dev/` triggers
    /// `ENOENT` on Linux which we map to `Ok(None)`.
    #[test]
    fn open_card_missing_node_returns_ok_none() {
        let p = PathBuf::from("/dev/this/does/not/exist");
        match open_card(&p) {
            Ok(None) => {}
            Ok(Some(_)) => panic!("expected Ok(None), got Ok(Some(_))"),
            Err(e) => panic!("expected Ok(None), got Err({e})"),
        }
    }
}
