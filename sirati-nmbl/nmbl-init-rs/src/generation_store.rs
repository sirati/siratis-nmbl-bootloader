//! Early mount for generation images stored outside the boot partition.

use std::path::{Component, Path, PathBuf};

use crate::config::GenerationImageConfig;
use crate::error::{NmblError, Result};

/// Mount the stage-1 generation store privately and return the state root in
/// it. When the store's device is the bootstrap boot filesystem (a single
/// persistent partition holding both the external config and the generations,
/// as on a DNS-VPS layout), the kernel refuses a second mount of the same ext4
/// block device with `EBUSY` when the read-only flags differ; the mounted
/// `boot_mountpoint` is then bind-mounted and the shared superblock remounted
/// with the store options, leaving the bootstrap mount read-only.
pub fn mount_and_resolve(
    policy: &GenerationImageConfig,
    boot_mountpoint: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let Some(store) = policy.stage1_store.as_ref() else {
        return Ok(None);
    };
    validate(
        store.mountpoint.as_path(),
        store.relative_state_root.as_path(),
    )?;
    std::fs::create_dir_all(&store.mountpoint).map_err(|source| NmblError::Io {
        source,
        context: format!("creating generation store {}", store.mountpoint.display()),
    })?;
    match crate::sys::mount::mount_fs(
        Some(store.device.as_path()),
        store.mountpoint.as_path(),
        &store.fstype,
        &store.options,
    ) {
        Ok(()) => {}
        Err(NmblError::Mount {
            source: nix::errno::Errno::EBUSY,
            ..
        }) if boot_mountpoint.is_some() => {
            let boot = boot_mountpoint.unwrap_or(Path::new("/"));
            crate::nmbl_info!(
                "generation store {} already mounted as the boot filesystem; bind-mounting {}",
                store.device.display(),
                boot.display(),
            );
            crate::sys::mount::mount_fs(Some(boot), store.mountpoint.as_path(), &store.fstype, "bind")?;
            // The bootstrap may have mounted the shared superblock read-only.
            // Generation state is written here, so remount the superblock with
            // the store options through the private mount; the bootstrap mount
            // keeps its own read-only mount flag.
            crate::sys::mount::remount_superblock(store.mountpoint.as_path(), &store.options)?;
        }
        Err(err) => return Err(err),
    }
    Ok(Some(store.mountpoint.join(&store.relative_state_root)))
}

fn validate(mountpoint: &Path, relative: &Path) -> Result<()> {
    let safe_mountpoint = mountpoint.is_absolute()
        && mountpoint != Path::new("/")
        && mountpoint
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
    let safe_relative = !relative.as_os_str().is_empty()
        && !relative.is_absolute()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_)));
    if safe_mountpoint && safe_relative {
        Ok(())
    } else {
        Err(NmblError::ConfigInvalid {
            reason: "generation stage1 store paths are unsafe".into(),
            context: "generation-image stage1 store".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_private_mount_and_relative_tree() {
        assert!(validate(Path::new("/mnt/nmbl-store"), Path::new("nmbl-generations")).is_ok());
    }

    #[test]
    fn rejects_root_absolute_or_parent_paths() {
        for (mountpoint, relative) in [
            ("/", "state"),
            ("relative", "state"),
            ("/mnt/store", "/state"),
            ("/mnt/store", "../state"),
        ] {
            assert!(validate(Path::new(mountpoint), Path::new(relative)).is_err());
        }
    }
}
