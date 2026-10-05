//! Mount only the declared plaintext identity volume before external rescue.
use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::sys::poller::LocalSender;
use std::path::{Component, Path};
use std::time::Duration;

const TARGET: &str = "/nmbl-identity";

fn options(fstype: &str, supplied: &[String]) -> Result<String> {
    if !matches!(fstype, "btrfs" | "ext4" | "xfs") {
        return Err(invalid("unsupported identity filesystem"));
    }
    // Only the subvolume selector is inherited. Performance/write options from
    // the normal writable filesystem must not override rescue mount policy.
    let mut out = vec![
        "ro".to_string(),
        "nodev".into(),
        "nosuid".into(),
        "noexec".into(),
    ];
    if supplied.len() > 1 {
        return Err(invalid("ambiguous identity subvolume selector"));
    }
    for option in supplied {
        if let Some(value) = option.strip_prefix("subvol=") {
            if fstype != "btrfs"
                || value.is_empty()
                || value.contains(',')
                || Path::new(value)
                    .components()
                    .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
            {
                return Err(invalid("unsafe identity subvolume"));
            }
            out.push(option.clone());
        } else if let Some(value) = option.strip_prefix("subvolid=") {
            if fstype != "btrfs" || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err(invalid("unsafe identity subvolume ID"));
            }
            out.push(option.clone());
        } else {
            return Err(invalid("unsupported identity mount option"));
        }
    }
    out.push(
        match fstype {
            "btrfs" => "rescue=nologreplay",
            "ext4" => "noload",
            _ => "norecovery",
        }
        .into(),
    );
    Ok(out.join(","))
}
fn invalid(reason: &str) -> NmblError {
    NmblError::ConfigInvalid {
        reason: reason.into(),
        context: "rescue identity volume".into(),
    }
}

pub(super) async fn mount(config: &Config, sender: &LocalSender) -> Result<()> {
    let Some(volume) = &config.rescue.identity_volume else {
        return Ok(());
    };
    if !volume.device.starts_with("/dev")
        || volume.device.starts_with("/dev/mapper")
        || volume
            .device
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid("identity device must be an absolute /dev path"));
    }
    let opts = options(&volume.fstype, &volume.options)?;
    crate::modules::load_modules(
        &config.kernel_modules.modules_dir,
        std::slice::from_ref(&volume.fstype),
        &config.kernel_modules.blacklist,
    )?;
    crate::modules::load_modules(
        &config.kernel_modules.modules_dir,
        &volume.required_modules,
        &config.kernel_modules.blacklist,
    )?;
    let devices = crate::sys::blkid::populate_disk_by_symlinks(sender).await?;
    if !devices.is_empty() {
        crate::sys::btrfs::scan_devices(&devices)?;
    }
    crate::devices::wait_for(
        &volume.device,
        Duration::from_secs(config.general.device_timeout_secs.min(30)),
        "rescue identity device",
        None,
    )
    .await?;
    std::fs::create_dir_all(TARGET).map_err(|source| NmblError::Io {
        source,
        context: "creating rescue identity mountpoint".into(),
    })?;
    // No activation, mapper unlock, root/store mount, or generation lookup.
    crate::sys::mount::mount_fs(
        Some(&volume.device),
        Path::new(TARGET),
        &volume.fstype,
        &opts,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn read_only_policy_cannot_be_overridden() {
        assert_eq!(
            options("btrfs", &["subvol=@persistent".into()]).unwrap(),
            "ro,nodev,nosuid,noexec,subvol=@persistent,rescue=nologreplay"
        );
        for bad in [
            "rw",
            "exec",
            "subvol=../secret",
            "subvol=@persistent,rw",
            "subvolid=1,rw",
        ] {
            assert!(options("btrfs", &[bad.into()]).is_err());
        }
        assert!(options("crypto_LUKS", &[]).is_err());
        assert!(options("btrfs", &["subvol=a".into(), "subvolid=5".into()]).is_err());
        assert!(options("ext4", &["subvol=foo".into()]).is_err());
    }
}
