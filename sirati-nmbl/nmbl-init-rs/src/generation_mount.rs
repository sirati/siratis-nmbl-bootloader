//! Target-initrd mount of a signed EROFS generation image.
//!
//! The backing path is opened once. Its signature is checked over that pinned
//! descriptor, which is then handed directly to `LOOP_CONFIGURE` before the
//! resulting read-only loop device is mounted. The pathname is never reopened.

use std::fs::{self, File};
use std::os::fd::AsFd;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sig::{DOMAIN_GENERATION_IMAGE, verify_image_fd};
use crate::sys::loopdev::{detach_loop_device, loop_bind_ro, open_loop_device};
use crate::sys::mount::mount_fs;

/// Open, verify, attach, and mount one signed EROFS image.
///
/// `device_link` is published only after verification and attachment. It lets
/// systemd associate the already-created mount with a fail-closed static mount
/// unit whose source cannot exist when verification fails.
pub fn mount_verified_generation(
    config: &Config,
    image: &Path,
    signature: &Path,
    target: &Path,
    device_link: &Path,
) -> Result<()> {
    ensure_mount_target(target)?;
    let pinned = File::open(image).map_err(|source| NmblError::Io {
        source,
        context: format!("opening generation image {}", image.display()),
    })?;
    verify_image_fd(
        pinned.as_fd(),
        "target generation image",
        Some(signature),
        DOMAIN_GENERATION_IMAGE,
        config,
    )?;

    let index = loop_bind_ro(&pinned).map_err(|error| *error.source)?;
    let loop_path = PathBuf::from(format!("/dev/loop{index}"));
    if let Err(error) = publish_device_link(device_link, &loop_path)
        .and_then(|()| mount_fs(Some(&loop_path), target, "erofs", "ro,nodev,nosuid"))
    {
        let _ = fs::remove_file(device_link);
        if let Ok(loop_fd) = open_loop_device(index, true) {
            let _ = detach_loop_device(&loop_fd);
        }
        return Err(error);
    }
    Ok(())
}

fn ensure_mount_target(target: &Path) -> Result<()> {
    fs::create_dir_all(target).map_err(|source| NmblError::Io {
        source,
        context: format!("creating generation mountpoint {}", target.display()),
    })?;
    let metadata = fs::symlink_metadata(target).map_err(|source| NmblError::Io {
        source,
        context: format!("inspecting generation mountpoint {}", target.display()),
    })?;
    if !metadata.file_type().is_dir() {
        return Err(NmblError::Io {
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "generation mountpoint is not a directory",
            ),
            context: target.display().to_string(),
        });
    }
    Ok(())
}

fn publish_device_link(link: &Path, loop_path: &Path) -> Result<()> {
    let parent = link.parent().ok_or_else(|| NmblError::Io {
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "verified loop-device link has no parent",
        ),
        context: link.display().to_string(),
    })?;
    fs::create_dir_all(parent).map_err(|source| NmblError::Io {
        source,
        context: format!("creating verified-device directory {}", parent.display()),
    })?;
    let temporary = parent.join(format!(".nmbl-generation-mount.{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    symlink(loop_path, &temporary).map_err(|source| NmblError::Io {
        source,
        context: format!(
            "creating temporary verified-device link {}",
            temporary.display()
        ),
    })?;
    fs::rename(&temporary, link).map_err(|source| NmblError::Io {
        source,
        context: format!("publishing verified-device link {}", link.display()),
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests may fail immediately while arranging temporary paths"
)]
mod tests {
    use super::*;

    #[test]
    fn mount_target_must_be_a_real_directory() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let real = temp.path().join("real");
        let link = temp.path().join("link");
        fs::create_dir(&real).expect("real mountpoint");
        symlink(&real, &link).expect("mountpoint symlink");

        assert!(ensure_mount_target(&real).is_ok());
        assert!(ensure_mount_target(&link).is_err());
    }

    #[test]
    fn verified_device_link_is_replaced_atomically() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let link = temp.path().join("verified");
        symlink("/dev/loop1", &link).expect("old device link");

        publish_device_link(&link, Path::new("/dev/loop2")).expect("replace device link");

        assert_eq!(
            fs::read_link(link).expect("read device link"),
            Path::new("/dev/loop2")
        );
    }
}
