//! Orchestration-level tests for the driver-image loader (#23).
//!
//! The privileged loop/mount/`init_module` steps need root + a loop-control
//! node, so they are exercised at the unit level inside each submodule (with a
//! skip-when-unprivileged guard). HERE we pin the orchestration contract that
//! does not need privilege: the feature-off no-op, the ordered handle
//! bookkeeping + teardown, and — the security keystone — that an enforce-mode
//! verify failure returns BEFORE any mount is attempted (single-fd
//! verify-first).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]

use std::path::PathBuf;

use super::*;
use crate::config::Config;
#[cfg(feature = "secure-boot")]
use crate::error::NmblError;

/// Parse a config from TOML, panicking on failure (test-only).
fn cfg_from(toml_text: &str) -> Config {
    toml::from_str::<Config>(toml_text).expect("test config parses")
}

#[test]
fn disabled_feature_is_a_noop_empty_handle() {
    // The default config never opted in: no work, empty handle.
    let cfg = Config::recovery_default();
    let handle = load_driver_images(&cfg).expect("disabled load is a no-op");
    assert!(handle.is_empty());
    assert_eq!(handle.len(), 0);
}

#[test]
fn enabled_but_no_images_is_a_noop_empty_handle() {
    let cfg = cfg_from("[driver_images]\nenable = true\n");
    let handle = load_driver_images(&cfg).expect("no images is a no-op");
    assert!(handle.is_empty());
}

#[test]
fn teardown_of_empty_handle_is_ok() {
    let handle = DriverImagesHandle::empty();
    detach_all_driver_images(&handle).expect("empty teardown is a no-op Ok");
}

#[test]
fn handle_preserves_load_order() {
    // The handle is the ordered record teardown + downstream measurement walk;
    // pin that push() preserves declared order.
    let mut handle = DriverImagesHandle::empty();
    handle.push(DriverImageHandle::new(
        "a.sfs".to_string(),
        PathBuf::from("/run/nmbl-boot/a.sfs"),
        [0xaau8; 64],
        7,
        PathBuf::from("/run/nmbl-driver-images/0"),
    ));
    handle.push(DriverImageHandle::new(
        "b.sfs".to_string(),
        PathBuf::from("/run/nmbl-boot/b.sfs"),
        [0xbbu8; 64],
        9,
        PathBuf::from("/run/nmbl-driver-images/1"),
    ));
    let imgs = handle.images();
    assert_eq!(imgs.len(), 2);
    let first = imgs.first().expect("first image");
    let second = imgs.get(1).expect("second image");
    assert_eq!(first.loop_index(), 7);
    assert_eq!(first.name(), "a.sfs");
    assert_eq!(first.digest(), &[0xaau8; 64]);
    assert_eq!(first.image_path(), PathBuf::from("/run/nmbl-boot/a.sfs"));
    assert_eq!(second.loop_index(), 9);
    assert_eq!(
        second.mountpoint(),
        PathBuf::from("/run/nmbl-driver-images/1")
    );
}

/// #28: `measure_refs()` projects the ordered handle into the PCR-11 measure
/// refs — one `{name, digest}` per image, in LOAD order, reusing the verified
/// digest (no re-hash). The name is the declared boot-relative path, not the
/// runtime-prefixed absolute path.
#[cfg(feature = "secure-boot")]
#[test]
fn measure_refs_carry_name_and_digest_in_order() {
    let mut handle = DriverImagesHandle::empty();
    handle.push(DriverImageHandle::new(
        "nic.sfs".to_string(),
        PathBuf::from("/run/nmbl-boot/nic.sfs"),
        [0x11u8; 64],
        7,
        PathBuf::from("/run/nmbl-driver-images/0"),
    ));
    handle.push(DriverImageHandle::new(
        "gpu.sfs".to_string(),
        PathBuf::from("/run/nmbl-boot/gpu.sfs"),
        [0x22u8; 64],
        9,
        PathBuf::from("/run/nmbl-driver-images/1"),
    ));
    let refs = handle.measure_refs();
    assert_eq!(refs.len(), 2);
    let first = refs.first().expect("first ref");
    let second = refs.get(1).expect("second ref");
    assert_eq!(first.name, "nic.sfs");
    assert_eq!(first.digest, [0x11u8; 64]);
    assert_eq!(second.name, "gpu.sfs");
    assert_eq!(second.digest, [0x22u8; 64]);
}

/// The security keystone for #23: an enforce-mode image with a bad/absent
/// signature must abort at the VERIFY stage — never reaching the mount. We can
/// assert this without privilege because verify runs first over the pinned fd;
/// a `verify`-stage error proves no loop/mount was attempted (a mount attempt
/// would surface a `loop-*` or `mount` stage instead).
#[cfg(feature = "secure-boot")]
#[test]
fn enforce_bad_signature_refuses_before_mount() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Stage a bogus image with no sidecar on the "boot partition".
    let img = dir.path().join("d.sfs");
    std::fs::write(&img, b"not-a-real-squashfs").expect("write image");

    let mut cfg = cfg_from(
        "[paths]\nshell = \"/bin/sh\"\n\
         [signing]\nenable = true\nenforce = true\n\
         [driver_images]\nenable = true\n\
         [[driver_images.images]]\n\
         path = \"d.sfs\"\nsig_path = \"d.sfs.sig\"\nmodules = []\n",
    );
    // The boot mountpoint is the staging dir, so `d.sfs` resolves to our bogus
    // file and `d.sfs.sig` to a missing sidecar.
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());

    let err = load_driver_images(&cfg).expect_err("enforce + bad sig must refuse");
    match err {
        NmblError::DriverImage { stage, .. } => assert_eq!(
            stage, "verify",
            "must refuse at verify, before any mount (got stage {stage})"
        ),
        other => panic!("expected DriverImage(verify), got {other:?}"),
    }
}

/// Multiple images: the FIRST failing image aborts the whole run and the
/// returned error names that image's failing stage — confirming images are
/// processed in order and a refusal short-circuits the rest.
#[cfg(feature = "secure-boot")]
#[test]
fn multiple_images_first_refusal_short_circuits() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("first.sfs"), b"bogus-1").expect("write first");
    std::fs::write(dir.path().join("second.sfs"), b"bogus-2").expect("write second");

    let mut cfg = cfg_from(
        "[paths]\nshell = \"/bin/sh\"\n\
         [signing]\nenable = true\nenforce = true\n\
         [driver_images]\nenable = true\n\
         [[driver_images.images]]\n\
         path = \"first.sfs\"\nsig_path = \"first.sfs.sig\"\nmodules = []\n\
         [[driver_images.images]]\n\
         path = \"second.sfs\"\nsig_path = \"second.sfs.sig\"\nmodules = []\n",
    );
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());

    // The first image fails verify; the run aborts there (the second image's
    // missing sidecar is never reached).
    let err = load_driver_images(&cfg).expect_err("first bad image aborts the run");
    assert!(
        matches!(
            err,
            NmblError::DriverImage {
                stage: "verify",
                ..
            }
        ),
        "first image must refuse at verify and short-circuit the rest",
    );
}

/// A declared image whose file is absent fails at the `open` stage — before any
/// verify/mount — so a missing image is a clean, well-tagged refusal.
#[cfg(feature = "secure-boot")]
#[test]
fn missing_image_file_is_open_stage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = cfg_from(
        "[paths]\nshell = \"/bin/sh\"\n\
         [signing]\nenable = true\nenforce = true\n\
         [driver_images]\nenable = true\n\
         [[driver_images.images]]\n\
         path = \"absent.sfs\"\nsig_path = \"absent.sfs.sig\"\nmodules = []\n",
    );
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());

    let err = load_driver_images(&cfg).expect_err("absent image must error");
    assert!(matches!(err, NmblError::DriverImage { stage: "open", .. }));
}
