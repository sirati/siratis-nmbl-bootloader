//! Phase 6: load the chosen image, tear down initramfs mounts, kexec.
//! Replaces `sirati-nmbl/scripts/kexec-boot.sh.nix`. Load MUST happen
//! before any unmount — [`sys::kexec::load`] reads kernel+initrd from
//! the still-mounted `/mnt/system`. Anything that fails after the image
//! is loaded is logged + swallowed; we're about to replace this kernel.

mod handoff;

use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::mount::MntFlags;

use crate::activation::KeyInjection;
use crate::config::Config;
use crate::devices::resolve_mountpoint;
use crate::error::Result;
use crate::generations::Generation;
use crate::sys;
use crate::terminal::TerminalAction;
use crate::{nmbl_info, nmbl_warn};

/// Pseudo-filesystems from phase 1, mount order. Reversed for teardown.
const PSEUDO_FS: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp"];

/// Post-`sync(2)` settle window. `sync` only schedules writeback; real
/// hardware needs a beat to commit before we cut the mounts out.
const POST_SYNC_FLUSH: Duration = Duration::from_millis(50);

/// Filesystems in REVERSE declaration order, paths resolved against
/// `system_root`. Children come before parents.
fn reverse_mount_targets(config: &Config) -> Vec<PathBuf> {
    let root = config.paths.system_root.as_path();
    config
        .filesystems
        .iter()
        .rev()
        .map(|fs| resolve_mountpoint(root, fs))
        .collect()
}

/// Lazy-unmount `target`; log + swallow any failure.
fn detach(target: &Path) {
    if let Err(err) = sys::mount::umount(target, MntFlags::MNT_DETACH) {
        nmbl_warn!("umount({}) failed: {err}", target.display());
    }
}

/// Kexec into `generation` (PLAN.md §7 phase 6): load image, sync +
/// settle, lazy-unmount config fs (reverse) + system_root + pseudo-fs
/// (reverse), then return [`TerminalAction::Kexec`] so the dispatcher
/// in `main` fires `reboot(LINUX_REBOOT_CMD_KEXEC)` after every
/// stack-allocated resource has been dropped via normal unwinding.
///
/// The image itself is loaded eagerly here — the kernel holds it in
/// the kexec image slot — but the cutover syscall is deferred to the
/// dispatcher so a stale console handle on the caller's stack cannot
/// leak its `Drop` side effects past the reboot.
///
/// When `key_injections` is non-empty, an in-memory cpio fragment
/// containing those files is appended to the system initrd via
/// `memfd_create(2)` before `kexec_file_load(2)` — the typed
/// passphrases never touch disk.
pub fn kexec_into(
    config: &Config,
    generation: &Generation,
    cmdline_override: Option<&str>,
    key_injections: &[KeyInjection],
) -> Result<TerminalAction> {
    // Build the cmdline, stage the log + key injections, then (in F4)
    // verify + measure the generation before filling the kexec image
    // slot. Behaviour-preserving wrapper around the load sequence.
    let cmdline =
        handoff::verify_measure_then_load(config, generation, cmdline_override, key_injections)?;
    nmbl_info!("kexec: image loaded ({} bytes cmdline)", cmdline.len());

    nix::unistd::sync();
    std::thread::sleep(POST_SYNC_FLUSH);

    // Dedupe: the root filesystem's mountpoint resolves to system_root,
    // so reverse_mount_targets already covers it. detaching twice gets
    // EINVAL on the second call ("not a mount point").
    let mut already: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    // Detach the stateful RW twin first when present — it sits "above"
    // the operator's filesystems in mount order (mounted in phase 0.5
    // after boot_fs but before phases 1+), so it must come off before
    // anything else in the reverse-mount sweep below.
    #[cfg(feature = "stateful")]
    if let Some(state_mp) = config.runtime_state_mountpoint.as_deref()
        && already.insert(state_mp.to_path_buf())
    {
        detach(state_mp);
    }
    for target in reverse_mount_targets(config) {
        if already.insert(target.clone()) {
            detach(&target);
        }
    }
    let system_root = config.paths.system_root.clone();
    if already.insert(system_root.clone()) {
        detach(&system_root);
    }
    for pseudo in PSEUDO_FS.iter().rev() {
        detach(Path::new(pseudo));
    }

    nmbl_info!("kexec: image staged; dispatcher will hand off to new kernel");
    Ok(TerminalAction::Kexec)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::*;
    use crate::config::FilesystemEntry;

    fn fs(device: &str, mountpoint: &str, is_root: bool) -> FilesystemEntry {
        FilesystemEntry {
            device: device.to_string(),
            mountpoint: PathBuf::from(mountpoint),
            fstype: "ext4".to_string(),
            options: String::new(),
            is_root,
        }
    }

    fn config_with(fs_list: Vec<FilesystemEntry>) -> Config {
        let mut cfg: Config =
            toml::from_str("[paths]\nsystem_root = \"/mnt/system\"\n").expect("base config");
        cfg.filesystems = fs_list;
        cfg
    }

    #[test]
    fn reverse_mount_targets_orders_children_first() {
        let cfg = config_with(vec![
            fs("/dev/sda2", "/", true),
            fs("/dev/sda1", "/boot", false),
            fs("/dev/sda3", "/boot/efi", false),
        ]);
        assert_eq!(
            reverse_mount_targets(&cfg),
            vec![
                PathBuf::from("/mnt/system/boot/efi"),
                PathBuf::from("/mnt/system/boot"),
                PathBuf::from("/mnt/system"),
            ],
        );
        assert!(reverse_mount_targets(&config_with(vec![])).is_empty());
    }
}
