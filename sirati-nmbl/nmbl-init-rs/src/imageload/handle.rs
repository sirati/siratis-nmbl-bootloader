//! Loaded-image bookkeeping + the normal-path teardown (#23).
//!
//! A [`DriverImageHandle`] records what one successfully loaded driver image
//! left behind that teardown must undo: the loop minor it bound and the
//! mountpoint the squashfs is mounted at. [`DriverImagesHandle`] is the ordered
//! collection the loader returns, and [`detach_all_driver_images`] unwinds them.
//!
//! These types are ALWAYS compiled (the loader's public surface is ungated so
//! the #24 call site links in every build); the privileged unmount/detach work
//! only runs when there are handles to tear down, which a non-secure-boot build
//! never produces (FIX-05).

use std::path::PathBuf;

use crate::error::Result;

/// What one loaded driver image left mounted, for teardown.
///
/// Records the source image path (for logging), the bound loop minor, and the
/// mountpoint the squashfs sits at. Dropping the handle does NOT auto-detach —
/// teardown is explicit via [`detach_all_driver_images`] so the FIX-55
/// leave-mounted-into-the-capped-shell path can simply skip the call.
#[derive(Debug, Clone)]
pub struct DriverImageHandle {
    /// Absolute on-disk path of the driver squashfs (for log/diagnostics).
    image_path: PathBuf,
    /// The `/dev/loopN` minor bound to the image.
    loop_index: u32,
    /// Where the squashfs is mounted read-only.
    mountpoint: PathBuf,
}

impl DriverImageHandle {
    /// Construct a handle from the loop index + mountpoint a successful load
    /// produced.
    #[must_use]
    pub fn new(image_path: PathBuf, loop_index: u32, mountpoint: PathBuf) -> Self {
        Self {
            image_path,
            loop_index,
            mountpoint,
        }
    }

    /// The bound loop minor (`/dev/loop{N}`).
    #[must_use]
    pub fn loop_index(&self) -> u32 {
        self.loop_index
    }

    /// The read-only mountpoint of the image.
    #[must_use]
    pub fn mountpoint(&self) -> &std::path::Path {
        &self.mountpoint
    }

    /// The source squashfs path the handle was loaded from.
    #[must_use]
    pub fn image_path(&self) -> &std::path::Path {
        &self.image_path
    }
}

/// The ordered set of loaded driver images returned by
/// [`super::load_driver_images`].
///
/// Order matches the operator's declared image order so teardown (and any
/// downstream measurement of the loaded set) is deterministic.
#[derive(Debug, Clone, Default)]
pub struct DriverImagesHandle {
    images: Vec<DriverImageHandle>,
}

impl DriverImagesHandle {
    /// An empty handle — the feature-off / no-images / pre-load state.
    #[must_use]
    pub fn empty() -> Self {
        Self { images: Vec::new() }
    }

    /// Append a freshly loaded image's handle (load order preserved).
    pub fn push(&mut self, handle: DriverImageHandle) {
        self.images.push(handle);
    }

    /// The loaded images, in load order.
    #[must_use]
    pub fn images(&self) -> &[DriverImageHandle] {
        &self.images
    }

    /// `true` when no images were loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Number of loaded images.
    #[must_use]
    pub fn len(&self) -> usize {
        self.images.len()
    }
}

/// Tear down every loaded driver image: lazily unmount the squashfs, then
/// detach the loop device (#23 teardown for the NORMAL, non-shell path).
///
/// Images are torn down in REVERSE load order (last mounted first) so a later
/// image that was mounted under an earlier one's tree — not the case today, but
/// cheap insurance — unwinds cleanly. Each step is best-effort: a failure on
/// one image is logged and the rest still tear down, because teardown runs on
/// the success path right before kexec where a stuck unmount must not strand
/// the boot.
///
/// FIX-55: this is the loader's NORMAL-path cleanup. On the diverted
/// capped-shell path the caller (#24) deliberately does NOT call this — the
/// images stay mounted for inspection, which is safe because they carry no
/// secrets.
///
/// # Errors
/// Never returns `Err` for an individual image (failures are logged); the
/// `Result` shape is kept so the call site can stay uniform with the other
/// teardown helpers and a future hard-fail policy can be slotted in.
pub fn detach_all_driver_images(handle: &DriverImagesHandle) -> Result<()> {
    for image in handle.images().iter().rev() {
        detach_one(image);
    }
    Ok(())
}

/// Best-effort teardown of a single image: lazy-unmount then `LOOP_CLR_FD`.
fn detach_one(image: &DriverImageHandle) {
    // Only the secure-boot build can have produced a handle (FIX-05), so the
    // privileged teardown body lives behind the feature; a non-secure-boot
    // build never reaches here with a non-empty handle.
    #[cfg(feature = "secure-boot")]
    super::mount::teardown_image(image.loop_index(), image.mountpoint());

    // Silence the unused-arg warning on the (unreachable) non-secure-boot path.
    #[cfg(not(feature = "secure-boot"))]
    let _ = image;
}
