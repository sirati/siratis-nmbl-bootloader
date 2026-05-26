//! Phase 6: load the chosen image, tear down initramfs mounts, kexec.
//! Replaces `sirati-nmbl/scripts/kexec-boot.sh.nix`. Load MUST happen
//! before any unmount — [`sys::kexec::load`] reads kernel+initrd from
//! the still-mounted `/mnt/system`. Anything that fails after the image
//! is loaded is logged + swallowed; we're about to replace this kernel.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::mount::MntFlags;

use crate::config::Config;
use crate::devices::resolve_mountpoint;
use crate::error::Result;
use crate::generations::Generation;
use crate::sys;
use crate::{nmbl_info, nmbl_warn};

/// Pseudo-filesystems from phase 1, mount order. Reversed for teardown.
const PSEUDO_FS: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp"];

/// Post-`sync(2)` settle window. `sync` only schedules writeback; real
/// hardware needs a beat to commit before we cut the mounts out.
const POST_SYNC_FLUSH: Duration = Duration::from_millis(50);

/// Final cmdline. `cmdline_override` wins verbatim (TUI editor path);
/// otherwise the generation's own params are space-joined.
fn build_cmdline(generation: &Generation, cmdline_override: Option<&str>) -> String {
    match cmdline_override {
        Some(s) => s.to_string(),
        None => generation.kernel_params.join(" "),
    }
}

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
/// (reverse), then `reboot(LINUX_REBOOT_CMD_KEXEC)`. `Infallible`
/// encodes that success is the noreturn path.
pub fn kexec_into(
    config: &Config,
    generation: &Generation,
    cmdline_override: Option<&str>,
) -> Result<Infallible> {
    let cmdline = build_cmdline(generation, cmdline_override);
    nmbl_info!(
        "kexec: loading generation {} (kernel={}, initrd={})",
        generation.number,
        generation.kernel.display(),
        generation.initrd.display()
    );
    sys::kexec::load(&generation.kernel, Some(&generation.initrd), &cmdline, 0)?;
    nmbl_info!("kexec: image loaded ({} bytes cmdline)", cmdline.len());

    nix::unistd::sync();
    std::thread::sleep(POST_SYNC_FLUSH);

    for target in reverse_mount_targets(config) {
        detach(&target);
    }
    detach(config.paths.system_root.as_path());
    for pseudo in PSEUDO_FS.iter().rev() {
        detach(Path::new(pseudo));
    }

    nmbl_info!("kexec: handing off to new kernel");
    sys::kexec::execute()
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

    fn gen_for(params: &[&str]) -> Generation {
        Generation {
            number: 42,
            profile_link: PathBuf::from("/mnt/system/nix/var/nix/profiles/system-42-link"),
            kernel: PathBuf::from("/mnt/system/boot/vmlinuz"),
            initrd: PathBuf::from("/mnt/system/boot/initrd"),
            kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
            label: String::new(),
        }
    }

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
    fn build_cmdline_override_used_verbatim() {
        let g = gen_for(&["root=/dev/sda1", "quiet"]);
        let s = "init=/sbin/init debug";
        assert_eq!(build_cmdline(&g, Some(s)), s);
    }

    #[test]
    fn build_cmdline_no_override_joins_params() {
        let g = gen_for(&["root=/dev/sda1", "ro", "quiet"]);
        assert_eq!(build_cmdline(&g, None), "root=/dev/sda1 ro quiet");
    }

    #[test]
    fn build_cmdline_empty_override_yields_empty() {
        let g = gen_for(&["root=/dev/sda1"]);
        assert_eq!(build_cmdline(&g, Some("")), "");
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
