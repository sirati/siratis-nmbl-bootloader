//! Register a driver image's firmware directory with the kernel (#23, step 3).
//!
//! A driver `.ko` may `request_firmware()` a blob during `init_module`. Without
//! udev (NMBL ships none) the kernel's `firmware_class` falls back to the
//! built-in search paths plus an operator-settable *custom* path exposed at
//! `/sys/module/firmware_class/parameters/path`. Writing the image's
//! `lib/firmware` there before loading its modules lets the kernel satisfy a
//! firmware request synchronously from the mounted squashfs.
//!
//! This is BEST-EFFORT and side-effecting only: it never fails the load. If the
//! sysfs knob is absent (kernel built without the custom-path option) or the
//! image ships no `lib/firmware`, the modules that need no firmware still load;
//! a module that genuinely needs a missing blob surfaces its own, clearer error
//! at `init_module` time.
//!
//! Caveat: the kernel honours a SINGLE custom firmware path, so the last write
//! wins. With multiple driver images each is given the path immediately before
//! ITS modules load (the per-image pipeline interleaves
//! firmware-then-load-then-next-image), so each image's firmware is in place
//! for exactly its own module loads.

use std::path::Path;

use crate::nmbl_verbose;

/// The sysfs knob the kernel reads for an extra firmware search directory.
const FIRMWARE_CLASS_PATH: &str = "/sys/module/firmware_class/parameters/path";

/// Point the kernel's `firmware_class` custom search path at `<mountpoint>/lib/firmware`.
///
/// Best-effort: a missing sysfs knob, a missing `lib/firmware` in the image, or
/// a non-UTF-8 path are all logged at verbose level and ignored. Never fails the
/// driver-image load.
pub(super) fn add_firmware_search_path(mountpoint: &Path) {
    let fw_dir = mountpoint.join("lib/firmware");
    if !fw_dir.is_dir() {
        nmbl_verbose!(
            "driver-image: no lib/firmware in {}; skipping firmware path registration",
            mountpoint.display()
        );
        return;
    }

    let Some(fw_str) = fw_dir.to_str() else {
        nmbl_verbose!(
            "driver-image: firmware dir {} is not valid UTF-8; skipping",
            fw_dir.display()
        );
        return;
    };

    match std::fs::write(FIRMWARE_CLASS_PATH, fw_str) {
        Ok(()) => nmbl_verbose!("driver-image: registered firmware search path {}", fw_str),
        Err(e) => nmbl_verbose!(
            "driver-image: could not write firmware search path to {} ({}); \
             modules needing firmware may fail to load",
            FIRMWARE_CLASS_PATH,
            e
        ),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert on contract failures")]
mod tests {
    use super::*;

    #[test]
    fn missing_firmware_dir_is_a_noop() {
        // An image with no lib/firmware must not panic or touch sysfs — it just
        // returns. (We can't assert on the sysfs write here without root, so we
        // exercise the no-firmware-dir early return, which never writes.)
        let dir = tempfile::tempdir().expect("tempdir");
        add_firmware_search_path(dir.path());
    }

    #[test]
    fn present_firmware_dir_attempts_registration() {
        // With a lib/firmware present the helper proceeds to the sysfs write,
        // which on a non-root/CI host fails and is swallowed. The point is that
        // it does not panic and the early return is NOT taken.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("lib/firmware")).expect("mk firmware dir");
        add_firmware_search_path(dir.path());
    }
}
