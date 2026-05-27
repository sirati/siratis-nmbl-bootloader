//! Phase 6: load the chosen image, tear down initramfs mounts, kexec.
//! Replaces `sirati-nmbl/scripts/kexec-boot.sh.nix`. Load MUST happen
//! before any unmount — [`sys::kexec::load`] reads kernel+initrd from
//! the still-mounted `/mnt/system`. Anything that fails after the image
//! is loaded is logged + swallowed; we're about to replace this kernel.

use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::mount::MntFlags;

use crate::activation::KeyInjection;
use crate::config::Config;
use crate::devices::resolve_mountpoint;
use crate::error::Result;
use crate::generations::Generation;
use crate::sys;
use crate::sys::cpio::{InjectionEntry, build_fragment};
use crate::{nmbl_info, nmbl_warn};

/// Pseudo-filesystems from phase 1, mount order. Reversed for teardown.
const PSEUDO_FS: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp"];

/// Post-`sync(2)` settle window. `sync` only schedules writeback; real
/// hardware needs a beat to commit before we cut the mounts out.
const POST_SYNC_FLUSH: Duration = Duration::from_millis(50);

/// Final cmdline.
///
/// * `cmdline_override` (TUI editor path) wins verbatim — an operator who has
///   hand-edited the line must not have their text silently mutated. No
///   `init=` injection happens in this branch.
/// * Otherwise the generation's own `kernel_params` are space-joined, and
///   `init=<stage2>` is appended unless the joined string already carries an
///   `init=` token (split on whitespace). The init value is the generation's
///   `init_path` stripped of `system_root`, with a leading `/` re-prepended so
///   the chained kernel — which mounts the store at `/`, not under our
///   `/mnt/system` prefix — sees a path that exists in its own namespace. If
///   `init_path` is somehow outside `system_root`, fall back to the raw path
///   with a warning rather than producing a broken cmdline.
fn build_cmdline(
    generation: &Generation,
    cmdline_override: Option<&str>,
    system_root: &Path,
) -> String {
    if let Some(s) = cmdline_override {
        return s.to_string();
    }

    let joined = generation.kernel_params.join(" ");
    if joined
        .split_ascii_whitespace()
        .any(|t| t.starts_with("init="))
    {
        return joined;
    }

    let init_arg = match generation.init_path.strip_prefix(system_root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => {
            nmbl_warn!(
                "init path {} is not under system_root {}; passing through unchanged",
                generation.init_path.display(),
                system_root.display(),
            );
            generation.init_path.display().to_string()
        }
    };

    if joined.is_empty() {
        format!("init={init_arg}")
    } else {
        format!("{joined} init={init_arg}")
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
) -> Result<Infallible> {
    let cmdline = build_cmdline(generation, cmdline_override, &config.paths.system_root);
    nmbl_info!(
        "kexec: loading generation {} (kernel={}, initrd={})",
        generation.number,
        generation.kernel.display(),
        generation.initrd.display()
    );
    if key_injections.is_empty() {
        sys::kexec::load(&generation.kernel, Some(&generation.initrd), &cmdline, 0)?;
    } else {
        let entries: Vec<InjectionEntry<'_>> = key_injections
            .iter()
            .map(|inj| InjectionEntry {
                path: inj.path.as_path(),
                content: inj.secret.as_slice(),
            })
            .collect();
        let fragment = build_fragment(&entries);
        nmbl_info!(
            "kexec: injecting {} keyfile(s) into initrd via memfd ({} bytes)",
            key_injections.len(),
            fragment.len()
        );
        sys::kexec::load_with_extra_initrd_cpio(
            &generation.kernel,
            &generation.initrd,
            fragment.as_slice(),
            &cmdline,
            0,
        )?;
    }
    nmbl_info!("kexec: image loaded ({} bytes cmdline)", cmdline.len());

    nix::unistd::sync();
    std::thread::sleep(POST_SYNC_FLUSH);

    // Dedupe: the root filesystem's mountpoint resolves to system_root,
    // so reverse_mount_targets already covers it. detaching twice gets
    // EINVAL on the second call ("not a mount point").
    let mut already: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
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
            init_path: PathBuf::from("/mnt/system/nix/var/nix/profiles/system-42-link/init"),
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

    fn root() -> PathBuf {
        PathBuf::from("/mnt/system")
    }

    #[test]
    fn build_cmdline_override_used_verbatim() {
        let g = gen_for(&["root=/dev/sda1", "quiet"]);
        let s = "init=/sbin/init debug";
        assert_eq!(build_cmdline(&g, Some(s), &root()), s);
    }

    #[test]
    fn build_cmdline_no_override_joins_params_and_appends_init() {
        let g = gen_for(&["root=/dev/sda1", "ro", "quiet"]);
        assert_eq!(
            build_cmdline(&g, None, &root()),
            "root=/dev/sda1 ro quiet init=/nix/var/nix/profiles/system-42-link/init",
        );
    }

    #[test]
    fn build_cmdline_empty_override_yields_empty() {
        let g = gen_for(&["root=/dev/sda1"]);
        assert_eq!(build_cmdline(&g, Some(""), &root()), "");
    }

    #[test]
    fn injects_init_when_missing() {
        let mut g = gen_for(&["root=fstab"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
        let out = build_cmdline(&g, None, &root());
        assert!(
            out.ends_with(" init=/nix/store/abc/init"),
            "unexpected cmdline: {out}",
        );
    }

    #[test]
    fn respects_existing_init_in_params() {
        let mut g = gen_for(&["init=/explicit"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/xyz/init");
        assert_eq!(build_cmdline(&g, None, &root()), "init=/explicit");
    }

    #[test]
    fn override_passes_through() {
        let mut g = gen_for(&["root=fstab"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/xyz/init");
        assert_eq!(build_cmdline(&g, Some("foo bar"), &root()), "foo bar");
    }

    #[test]
    fn init_outside_system_root_warns_but_uses_raw() {
        let mut g = gen_for(&["root=fstab"]);
        g.init_path = PathBuf::from("/elsewhere/init");
        let out = build_cmdline(&g, None, &root());
        assert!(
            out.ends_with(" init=/elsewhere/init"),
            "unexpected cmdline: {out}",
        );
    }

    #[test]
    fn empty_params_still_inject_init() {
        let mut g = gen_for(&[]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
        assert_eq!(build_cmdline(&g, None, &root()), "init=/nix/store/abc/init");
    }

    #[test]
    fn init_token_matched_only_at_token_start() {
        // A param ending in "init=" must NOT short-circuit injection — the
        // check looks at whole whitespace tokens, not substrings.
        let mut g = gen_for(&["weird_suffix_init=foo"]);
        g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
        let out = build_cmdline(&g, None, &root());
        assert!(out.contains(" init=/nix/store/abc/init"), "got: {out}");
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
