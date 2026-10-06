//! The rescue tools EROFS: `nmblctl` and its runtime closure, mounted at
//! `/nmbl-tools` inside the rescue.
//!
//! `nmblctl` is built with this host's signing public keys baked in, so it
//! cannot live in the host-independent stage-2 image. NMBL's runtime config
//! names this second image and pins its SHA-512 (`[rescue.tools]`, rendered
//! next to the image it belongs to). Like the stage-2 image and the networking
//! stage, it is opened once, its signature (when signing is enabled) and pin
//! are checked over that descriptor, and the same descriptor is loop-bound.
//!
//! A tools image that fails any check is never mounted. The rescue then runs
//! without `nmblctl`: the operator keeps the console and SSH recovery, and the
//! rescue `/init` reports why from `/etc/nmbl-tools-disabled`. This matches the
//! networking stage, whose rejection removes only what that stage provides.
//! Audit mode does not relax the signature check here.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::loopdev::loop_bind_ro;
use crate::sys::mount::mount_fs;

const TARGET: &str = "/rescue/nmbl-tools";
const DISABLED_MARKER: &str = "/rescue/etc/nmbl-tools-disabled";
const WHAT: &str = "rescue tools image";

/// Verify and mount the configured tools image at `/rescue/nmbl-tools`.
/// `Ok(())` when none is configured.
pub(super) fn prepare(config: &Config) -> Result<()> {
    let Some(tools) = &config.rescue.tools else {
        return Ok(());
    };
    let image = locate(config, &tools.path)?;
    let file = File::open(&image).map_err(|source| {
        wrap(
            "tools-open",
            NmblError::Io {
                source,
                context: format!("opening {}", image.display()),
            },
        )
    })?;

    let signed_digest = verify_signature(config, &file, &image)?;
    super::image::check_pin(Some(&tools.sha512), &file, signed_digest, WHAT)?;

    crate::modules::load_modules(
        &config.kernel_modules.modules_dir,
        &["erofs".to_string()],
        &config.kernel_modules.blacklist,
    )
    .map_err(|source| wrap("tools-load-erofs", source))?;
    let index = loop_bind_ro(&file).map_err(|error| wrap("tools-loop-bind", *error.source))?;
    std::fs::create_dir_all(TARGET).map_err(|source| {
        wrap(
            "tools-mkdir",
            NmblError::Io {
                source,
                context: format!("creating {TARGET}"),
            },
        )
    })?;
    let loop_device = PathBuf::from(format!("/dev/loop{index}"));
    mount_fs(
        Some(&loop_device),
        Path::new(TARGET),
        "erofs",
        "ro,nodev,nosuid",
    )
    .map_err(|source| wrap("tools-mount", source))
}

/// Record why the tools image was refused, for the rescue `/init`. Nothing
/// was mounted, so a failure to write the note only loses the explanation.
pub(super) fn disable(error: &NmblError) {
    eprintln!("[nmbl] rescue tools image refused; the rescue runs without nmblctl: {error}");
    if let Err(write_error) = std::fs::write(DISABLED_MARKER, format!("{error}\n")) {
        eprintln!("[nmbl] could not record the refusal in {DISABLED_MARKER}: {write_error}");
    }
}

fn locate(config: &Config, configured: &Path) -> Result<PathBuf> {
    let raw = configured.to_string_lossy();
    let relative = super::image::boot_relative(&raw).ok_or_else(|| {
        wrap(
            "tools-path",
            NmblError::ConfigInvalid {
                reason: format!("unsafe rescue tools path `{}`", raw.trim()),
                context: "[rescue.tools].path".into(),
            },
        )
    })?;
    let boot = config.runtime_boot_mountpoint.as_deref().ok_or_else(|| {
        wrap(
            "tools-locate",
            NmblError::ConfigInvalid {
                reason: "the rescue tools image needs the mounted boot filesystem".into(),
                context: "[rescue.tools]".into(),
            },
        )
    })?;
    Ok(boot.join(relative))
}

/// With signing enabled the image must carry a valid signature under the
/// rescue-tools domain; returns the digest streamed over `file` so the pin
/// does not read the image twice.
#[cfg(feature = "secure-boot")]
fn verify_signature(config: &Config, file: &File, image: &Path) -> Result<Option<[u8; 64]>> {
    use std::os::fd::AsFd;
    // signing safety: signing disabled is the operator declining the feature
    // (as for the stage-2 image, FIX-04); the required SHA-512 pin still binds
    // the image to this config.
    if !config.signing.enable {
        return Ok(None);
    }
    let signature = super::image::sidecar_path(image, &config.signing.sig_path_suffix);
    crate::sig::verify_image_fd_digest(
        file.as_fd(),
        WHAT,
        Some(&signature),
        crate::sig::DOMAIN_RESCUE_TOOLS,
        config,
    )
    .map(Some)
    .map_err(|source| wrap("tools-verify", source))
}

#[cfg(not(feature = "secure-boot"))]
fn verify_signature(_config: &Config, _file: &File, _image: &Path) -> Result<Option<[u8; 64]>> {
    Ok(None)
}

fn wrap(stage: &'static str, source: NmblError) -> NmblError {
    NmblError::Rescue {
        stage,
        source: Box::new(source),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::{RescueConfig, RescueTools};

    fn config(path: &str, sha512: String, boot: Option<PathBuf>) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = RescueConfig {
            tools: Some(RescueTools {
                path: PathBuf::from(path),
                sha512,
            }),
            ..RescueConfig::default()
        };
        c.runtime_boot_mountpoint = boot;
        c
    }

    fn stage(result: Result<()>) -> &'static str {
        match result {
            Err(NmblError::Rescue { stage, .. }) => stage,
            other => panic!("expected a rescue-stage error, got {other:?}"),
        }
    }

    #[test]
    fn nothing_configured_is_a_no_op() {
        prepare(&Config::recovery_default()).expect("no tools image configured");
    }

    #[test]
    fn unsafe_paths_are_refused_before_any_io() {
        let dir = tempfile::tempdir().expect("tempdir");
        for path in ["../tools.erofs", "", "/", "a/../b"] {
            let c = config(path, "0".repeat(128), Some(dir.path().to_path_buf()));
            assert_eq!(stage(prepare(&c)), "tools-path", "{path:?}");
        }
    }

    #[test]
    fn missing_image_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let c = config(
            "nmbl/rescue-tools.erofs",
            "0".repeat(128),
            Some(dir.path().to_path_buf()),
        );
        assert_eq!(stage(prepare(&c)), "tools-open");
        let c = config("nmbl/rescue-tools.erofs", "0".repeat(128), None);
        assert_eq!(stage(prepare(&c)), "tools-locate");
    }

    #[test]
    fn an_image_other_than_the_pinned_one_is_refused_before_binding() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("nmbl")).expect("mkdir");
        std::fs::write(dir.path().join("nmbl/rescue-tools.erofs"), b"tampered").expect("write");
        let c = config(
            "/nmbl/rescue-tools.erofs",
            "0".repeat(128),
            Some(dir.path().to_path_buf()),
        );
        // Signing is off in the recovery default, so the pin is the check
        // that refuses it (or, without digest support, fails closed).
        assert_eq!(stage(prepare(&c)), "image-digest");
    }
}
