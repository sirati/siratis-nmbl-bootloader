//! Runtime loader for signed driver images (FEATURE-#1, task #23).
//!
//! NMBL can load out-of-tree kernel drivers from one or more *detached, signed*
//! squashfs blobs on the boot partition before kexec. Each blob is a
//! `makeModulesClosure`-style tree (`lib/modules/<release>/…` + `modules.dep`)
//! plus a `lib/firmware/` directory, signed with the host's `driver-image`
//! role key. The operator declares the blobs in `[driver_images]`
//! ([`crate::config::DriverImagesConfig`]); this module turns that declaration
//! into loaded modules.
//!
//! ## The per-image pipeline (fixed order, single fd — FIX-02)
//!
//! For each declared image, [`load_driver_images`] performs — in this exact
//! order, over a SINGLE pinned fd of the squashfs:
//!
//! 1. **Verify** ([`verify::verify_driver_image`]): stream the fd through the
//!    frozen ML-DSA pipeline ([`crate::sig::verify_image_fd_digest`]) under the
//!    [`crate::sig::DOMAIN_DRIVER_IMAGE`] role and apply the operator's signing
//!    posture via [`crate::sig::apply_policy`]. An enforce-mode failure returns
//!    a [`crate::error::NmblError::DriverImage`] the caller routes to
//!    `policy::refuse_unsigned` (FIX-05 / R-1). The image is NEVER mounted on a
//!    verify failure. The verified SHA-512 digest is captured on the handle for
//!    the PCR-11 measure (#28 — no re-hash, FIX-02).
//! 2. **Mount read-only** ([`mount::mount_squashfs_ro`]): bind the SAME fd to a
//!    loop device read-only via [`crate::sys::loopdev::loop_bind_ro`] (#22) and
//!    mount the squashfs `ro` at a per-image mountpoint. No reopen — the fd
//!    verified in step 1 is the fd the kernel reads through the loop device.
//! 3. **Firmware** ([`firmware::add_firmware_search_path`]): register the
//!    image's `lib/firmware/` with the kernel's `firmware_class` search path so
//!    `init_module` can satisfy a driver's firmware request.
//! 4. **Load modules** ([`modules::load_image_modules`]): `init_module` the
//!    declared modules (in order, honouring the per-image blacklist) by REUSING
//!    [`crate::modules::load_modules`] against the modules tree inside the
//!    mountpoint — no duplicate module-loader logic lives here.
//!
//! Each loaded image records its loop index + mountpoint in a
//! [`DriverImageHandle`]; the whole run returns a [`DriverImagesHandle`].
//!
//! ## Teardown + FIX-55
//!
//! [`detach_all_driver_images`] lazily unmounts each image and detaches its
//! loop device — the NORMAL (non-shell) path's cleanup before kexec. Note,
//! per FIX-55, that on the diverted *capped-shell* path the driver images are
//! deliberately left MOUNTED so the operator can inspect them; that is safe
//! because **driver images carry NO secret material** (they are public,
//! signed driver closures). Teardown is therefore only invoked on the normal
//! load-then-kexec path, never before dropping into the capped shell.
//!
//! ## Where the boot-runtime hook lands (#24)
//!
//! This module exposes ONLY the loader + its teardown. The call site that runs
//! [`load_driver_images`] during boot lives in `src/main_parts/boot_runtime.rs`
//! (`run_boot_inside_runtime`, after the early-module load and before the
//! generation kexec). On a verify/load failure it routes the
//! [`crate::error::NmblError::DriverImage`] through `policy::refuse_unsigned`
//! → `RebootIntoRescue` (R-1), and on the normal path it decides
//! teardown-vs-leave-mounted per the FIX-55 note above. The ordered image refs
//! the loader returns are the seam #28 will thread into the TPM measure
//! handoff (`tpm::measure::extend_handoff`); that threading is not done here.

mod handle;

#[cfg(feature = "secure-boot")]
mod firmware;
#[cfg(feature = "secure-boot")]
mod locate;
#[cfg(feature = "secure-boot")]
mod modules;
#[cfg(feature = "secure-boot")]
mod mount;
#[cfg(feature = "secure-boot")]
mod verify;

#[cfg(test)]
mod tests;

pub use handle::{DriverImageHandle, DriverImagesHandle, detach_all_driver_images};

use crate::config::Config;
use crate::error::Result;
use crate::sys::ops::{FsOps, ModuleOps};

/// Load every declared, signed driver image in order (the #23 entry point).
///
/// Returns a [`DriverImagesHandle`] recording the loop device + mountpoint of
/// each successfully loaded image so the caller (#24) can tear them down. The
/// first image that fails to verify, mount, or load aborts the run with a
/// [`crate::error::NmblError::DriverImage`]; the partial handle is dropped, so
/// #24's `refuse_unsigned` path (which reboots into rescue) does not need to
/// unwind any earlier images. (#24 owns the policy decision; this loader stays
/// pure.)
///
/// A no-op returning an empty handle when the feature is disabled
/// (`!driver_images.enable`) or no images are declared. On a build WITHOUT the
/// `secure-boot` feature this is *always* a no-op: `driver_images.enable` can
/// never be `true` there (the Nix module rejects it at config time — FIX-05),
/// so an unverified image can never reach a mount/load step.
///
/// # Errors
/// Returns [`crate::error::NmblError::DriverImage`] when an image fails to
/// verify (enforce mode), loop-bind, mount, or load its modules.
pub fn load_driver_images<S: FsOps + ModuleOps>(
    ops: &mut S,
    config: &Config,
) -> Result<DriverImagesHandle> {
    if !config.driver_images.enable || config.driver_images.images.is_empty() {
        return Ok(DriverImagesHandle::empty());
    }

    // Unreachable in practice on a non-secure-boot build: `driver_images.enable`
    // can never be `true` there (FIX-05 rejects it at config time). Fail closed
    // rather than load an image we have no verify pipeline for.
    #[cfg(not(feature = "secure-boot"))]
    {
        let _ = ops;
        Err(crate::error::NmblError::DriverImage {
            stage: "no-verify-feature",
            source: Box::new(crate::error::NmblError::ConfigInvalid {
                reason: "driver_images.enable requires a secure-boot build to verify images"
                    .to_string(),
                context: "load_driver_images without the secure-boot feature".to_string(),
            }),
        })
    }

    #[cfg(feature = "secure-boot")]
    {
        let mut handle = DriverImagesHandle::empty();
        for (index, spec) in config.driver_images.images.iter().enumerate() {
            let loaded = self::load_one(ops, config, spec, index)?;
            handle.push(loaded);
        }
        Ok(handle)
    }
}

/// Verify → mount-ro → firmware → load ONE image over a single pinned fd.
///
/// The exact-order pipeline documented at the module root. `index` only labels
/// the per-image mountpoint so concurrent images do not collide.
#[cfg(feature = "secure-boot")]
fn load_one<S: FsOps + ModuleOps>(
    ops: &mut S,
    config: &Config,
    spec: &crate::config::DriverImageSpec,
    index: usize,
) -> Result<DriverImageHandle> {
    use std::os::fd::AsFd;

    let resolved = locate::resolve_image(config, spec)?;

    // ONE fd for the whole pipeline (FIX-02): the bytes we verify are the bytes
    // the loop device serves to the kernel. Opened through the FsOps seam so
    // --validate-initrm reads the closure-mapped squashfs bytes.
    let image_fd = locate::open_image_ro(ops, &resolved)?;

    // (1) VERIFY first — never touch the loop/mount layer on a refusal. The
    // verify returns the SHA-512 it streamed over the pinned fd, which the
    // handle carries for the PCR-11 measure (#28 — no re-hash, FIX-02).
    let digest = verify::verify_driver_image(image_fd.as_fd(), &resolved, config)?;

    // (2) MOUNT the SAME fd read-only.
    let mounted = mount::mount_squashfs_ro(ops, image_fd.as_fd(), index)?;

    // (3) FIRMWARE search path for this image's lib/firmware.
    firmware::add_firmware_search_path(ops, &mounted.mountpoint);

    // (4) LOAD the declared modules (reuses crate::modules::load_modules).
    modules::load_image_modules(ops, config, spec, &mounted.mountpoint)?;

    // The measure event #4 name is the operator-declared boot-relative path
    // (`spec.path`): a STABLE, off-box-reproducible identifier (the absolute
    // `resolved.image_path` carries the runtime mountpoint prefix, so it is NOT
    // used as the measured name). A host predictor folds the same string.
    let name = spec.path.to_string_lossy().into_owned();
    Ok(DriverImageHandle::new(
        name,
        resolved.image_path,
        digest,
        mounted.loop_index,
        mounted.mountpoint,
    ))
}
