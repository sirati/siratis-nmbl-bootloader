//! Verified rescue networking EROFS mounted into the rescue chroot.
//!
//! The stage is named by NMBL's own runtime config (`[rescue.network_stage]`,
//! rendered next to the rescue image it belongs to), not by a marker inside
//! the rescue image: the config is what NMBL already trusts (embedded, or
//! signed under the boot-config domain), and it pins the stage's SHA-512.

use std::ffi::OsString;
use std::io;
use std::os::fd::AsFd;
use std::path::{Component, Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::loopdev::loop_bind_ro;
use crate::sys::mount::mount_fs;

const DISABLED_MARKER: &str = "/rescue/etc/nmbl-network-disabled";
const TARGET: &str = "/rescue/nmbl-network";

pub(super) fn prepare(config: &Config) -> Result<()> {
    let Some(stage) = &config.rescue.network_stage else {
        return Ok(());
    };
    let relative = parse_stage_path(&stage.path.to_string_lossy())?;
    let boot = config.runtime_boot_mountpoint.as_deref().ok_or_else(|| {
        wrap(
            "network-stage-locate",
            NmblError::ConfigInvalid {
                reason: "network stage needs the mounted boot filesystem".into(),
                context: "rescue networking EROFS".into(),
            },
        )
    })?;
    let image = boot.join(relative);
    let signature = sibling_with_suffix(&image, &config.signing.sig_path_suffix);
    let file = std::fs::File::open(&image)
        .map_err(|source| wrap("network-stage-open", io_error(source, image.display())))?;

    let digest = crate::sig::verify_image_fd_digest(
        file.as_fd(),
        "rescue networking EROFS",
        Some(&signature),
        crate::sig::DOMAIN_NETWORK_STAGE,
        config,
    )
    .map_err(|source| wrap("network-stage-verify", source))?;
    super::image::check_pin(
        stage.sha512.as_deref(),
        &file,
        Some(digest),
        "rescue networking EROFS",
    )?;

    crate::modules::load_modules(
        &config.kernel_modules.modules_dir,
        &["erofs".to_string()],
        &config.kernel_modules.blacklist,
    )
    .map_err(|source| wrap("network-stage-load-erofs", source))?;

    let index =
        loop_bind_ro(&file).map_err(|error| wrap("network-stage-loop-bind", *error.source))?;
    std::fs::create_dir_all(TARGET)
        .map_err(|source| wrap("network-stage-mkdir", io_error(source, TARGET)))?;
    let loop_device = PathBuf::from(format!("/dev/loop{index}"));
    mount_fs(
        Some(&loop_device),
        Path::new(TARGET),
        "erofs",
        "ro,nodev,nosuid,noexec",
    )
    .map_err(|source| wrap("network-stage-mount", source))?;
    let config_path = Path::new(TARGET).join("etc/nmbl-network/network.conf");
    super::network_profile::validate_file(&config_path).map_err(|reason| {
        wrap(
            "network-stage-profile",
            NmblError::ConfigInvalid {
                reason,
                context: config_path.display().to_string(),
            },
        )
    })?;
    Ok(())
}

pub(super) fn disable(error: &NmblError) -> Result<()> {
    eprintln!("[nmbl] signed network stage rejected; preserving local rescue console: {error}");
    std::fs::write(DISABLED_MARKER, format!("{error}\n"))
        .map_err(|source| wrap("network-stage-disable", io_error(source, DISABLED_MARKER)))
}

fn parse_stage_path(marker: &str) -> Result<PathBuf> {
    // Boot-partition-relative like `[rescue].sfs_path`: one leading `/` is
    // tolerated (boot-set slots render `/nmbl-boot-sets/<slot>/network`).
    let trimmed = marker.trim();
    let path = Path::new(trimmed.strip_prefix('/').unwrap_or(trimmed));
    let safe = !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    if safe {
        Ok(path.to_path_buf())
    } else {
        Err(wrap(
            "network-stage-path",
            NmblError::ConfigInvalid {
                reason: format!("unsafe network-stage path `{}`", marker.trim()),
                context: "[rescue.network_stage].path".into(),
            },
        ))
    }
}

fn sibling_with_suffix(image: &Path, suffix: &str) -> PathBuf {
    let mut name = image.file_name().map_or_else(OsString::new, OsString::from);
    name.push(suffix);
    image.with_file_name(name)
}

fn io_error(source: io::Error, path: impl std::fmt::Display) -> NmblError {
    NmblError::Io {
        source,
        context: format!("accessing {path}"),
    }
}

fn wrap(stage: &'static str, source: NmblError) -> NmblError {
    NmblError::Rescue {
        stage,
        source: Box::new(source),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert that fixed fixtures parse")]
mod tests {
    use super::*;

    #[test]
    fn marker_accepts_safe_relative_path() {
        assert_eq!(
            parse_stage_path("nmbl/network.erofs\n").expect("safe path"),
            PathBuf::from("nmbl/network.erofs")
        );
    }

    #[test]
    fn marker_rejects_escape_and_absolute_paths() {
        assert_eq!(
            parse_stage_path("/nmbl-boot-sets/A/network").expect("slot path"),
            PathBuf::from("nmbl-boot-sets/A/network")
        );
        for marker in [
            "../network.erofs",
            "//network.erofs",
            "",
            "/",
            "a/../b",
            "/../x",
        ] {
            assert!(parse_stage_path(marker).is_err(), "accepted {marker:?}");
        }
    }

    #[test]
    fn sidecar_is_a_sibling() {
        assert_eq!(
            sibling_with_suffix(Path::new("/boot/nmbl/network.erofs"), ".sig"),
            PathBuf::from("/boot/nmbl/network.erofs.sig")
        );
    }
}
