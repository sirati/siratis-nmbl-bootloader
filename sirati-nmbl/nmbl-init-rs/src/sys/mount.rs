//! Thin wrappers around `mount(2)` and `umount2(2)`.
//!
//! The option string accepted by [`mount_fs`] follows the same comma-separated
//! grammar as `mount -o`: tokens that map to a kernel `MS_*` flag are pulled
//! out of the string, the remainder is forwarded verbatim as the filesystem
//! `data` argument. Mirrors the classification table in util-linux's
//! `libmount/src/optmap.c`.

use std::path::{Path, PathBuf};

use nix::mount::{MntFlags, MsFlags};

use crate::error::{NmblError, Result};

/// One entry in the option translation table: the literal token, the flags it
/// sets, and the flags it clears (for negative forms like `rw` clearing
/// `MS_RDONLY`). If the same token appears twice the last entry wins.
struct OptionEntry {
    name: &'static str,
    set: MsFlags,
    clear: MsFlags,
}

const fn entry(name: &'static str, set: MsFlags, clear: MsFlags) -> OptionEntry {
    OptionEntry { name, set, clear }
}

// Token → (set, clear). Order matters only for duplicate names: the last
// matching row wins (`fold_options` walks the table back-to-front).
const OPTION_TABLE: &[OptionEntry] = &[
    entry("defaults", MsFlags::empty(), MsFlags::empty()),
    entry("ro", MsFlags::MS_RDONLY, MsFlags::empty()),
    entry("rw", MsFlags::empty(), MsFlags::MS_RDONLY),
    entry("nosuid", MsFlags::MS_NOSUID, MsFlags::empty()),
    entry("suid", MsFlags::empty(), MsFlags::MS_NOSUID),
    entry("nodev", MsFlags::MS_NODEV, MsFlags::empty()),
    entry("dev", MsFlags::empty(), MsFlags::MS_NODEV),
    entry("noexec", MsFlags::MS_NOEXEC, MsFlags::empty()),
    entry("exec", MsFlags::empty(), MsFlags::MS_NOEXEC),
    entry("sync", MsFlags::MS_SYNCHRONOUS, MsFlags::empty()),
    entry("async", MsFlags::empty(), MsFlags::MS_SYNCHRONOUS),
    entry("dirsync", MsFlags::MS_DIRSYNC, MsFlags::empty()),
    entry("remount", MsFlags::MS_REMOUNT, MsFlags::empty()),
    entry("bind", MsFlags::MS_BIND, MsFlags::empty()),
    entry(
        "rbind",
        MsFlags::MS_BIND.union(MsFlags::MS_REC),
        MsFlags::empty(),
    ),
    entry("shared", MsFlags::MS_SHARED, MsFlags::empty()),
    entry(
        "rshared",
        MsFlags::MS_SHARED.union(MsFlags::MS_REC),
        MsFlags::empty(),
    ),
    entry("private", MsFlags::MS_PRIVATE, MsFlags::empty()),
    entry(
        "rprivate",
        MsFlags::MS_PRIVATE.union(MsFlags::MS_REC),
        MsFlags::empty(),
    ),
    entry("slave", MsFlags::MS_SLAVE, MsFlags::empty()),
    entry(
        "rslave",
        MsFlags::MS_SLAVE.union(MsFlags::MS_REC),
        MsFlags::empty(),
    ),
    entry("noatime", MsFlags::MS_NOATIME, MsFlags::empty()),
    entry("atime", MsFlags::empty(), MsFlags::MS_NOATIME),
    entry("nodiratime", MsFlags::MS_NODIRATIME, MsFlags::empty()),
    entry("diratime", MsFlags::empty(), MsFlags::MS_NODIRATIME),
    entry("relatime", MsFlags::MS_RELATIME, MsFlags::empty()),
    entry("norelatime", MsFlags::empty(), MsFlags::MS_RELATIME),
    entry("strictatime", MsFlags::MS_STRICTATIME, MsFlags::empty()),
    entry("nostrictatime", MsFlags::empty(), MsFlags::MS_STRICTATIME),
    entry("mand", MsFlags::MS_MANDLOCK, MsFlags::empty()),
    entry("lazytime", MsFlags::MS_LAZYTIME, MsFlags::empty()),
    entry("nolazytime", MsFlags::empty(), MsFlags::MS_LAZYTIME),
    entry("iversion", MsFlags::MS_I_VERSION, MsFlags::empty()),
    entry("noiversion", MsFlags::empty(), MsFlags::MS_I_VERSION),
    entry("silent", MsFlags::MS_SILENT, MsFlags::empty()),
    entry("loud", MsFlags::empty(), MsFlags::MS_SILENT),
];

/// Split `options` on commas and partition each token into either a flag
/// update or a passthrough data fragment. Empty tokens are skipped.
fn fold_options(options: &str) -> (MsFlags, String) {
    let mut flags = MsFlags::empty();
    let mut data_parts: Vec<&str> = Vec::new();

    for token in options.split(',') {
        if token.is_empty() {
            continue;
        }
        if let Some(e) = OPTION_TABLE.iter().rev().find(|e| e.name == token) {
            flags.remove(e.clear);
            flags.insert(e.set);
        } else {
            data_parts.push(token);
        }
    }

    (flags, data_parts.join(","))
}

/// Mount a filesystem. `source` is `None` for pseudo-filesystems (proc, sysfs,
/// tmpfs, …) that ignore it. The `options` string is translated to mount flags
/// + a data string the same way `mount -o` does it.
pub fn mount_fs(source: Option<&Path>, target: &Path, fstype: &str, options: &str) -> Result<()> {
    let (flags, data) = fold_options(options);
    let data_opt: Option<&str> = if data.is_empty() { None } else { Some(&data) };

    nix::mount::mount(source, target, Some(fstype), flags, data_opt).map_err(|e| NmblError::Mount {
        src: source.map(PathBuf::from),
        dst: PathBuf::from(target),
        fstype: fstype.to_owned(),
        source: e,
    })
}

/// Unmount a target with the given flags. Use [`MntFlags::MNT_DETACH`] for the
/// lazy unmount the pre-kexec tear-down wants.
pub fn umount(target: &Path, flags: MntFlags) -> Result<()> {
    nix::mount::umount2(target, flags).map_err(|e| NmblError::Umount {
        dst: PathBuf::from(target),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ro_noatime_user_xattr() {
        let (flags, data) = fold_options("ro,noatime,user_xattr");
        assert_eq!(flags, MsFlags::MS_RDONLY | MsFlags::MS_NOATIME);
        assert_eq!(data, "user_xattr");
    }

    #[test]
    fn rw_relatime_defaults() {
        let (flags, data) = fold_options("rw,relatime,defaults");
        assert_eq!(flags, MsFlags::MS_RELATIME);
        assert_eq!(data, "");
    }

    #[test]
    fn empty_input() {
        let (flags, data) = fold_options("");
        assert_eq!(flags, MsFlags::empty());
        assert_eq!(data, "");
    }

    #[test]
    fn bind_rw() {
        let (flags, data) = fold_options("bind,rw");
        assert_eq!(flags, MsFlags::MS_BIND);
        assert_eq!(data, "");
    }

    #[test]
    fn ro_with_passthrough() {
        let (flags, data) = fold_options("ro,subvol=@root");
        assert_eq!(flags, MsFlags::MS_RDONLY);
        assert_eq!(data, "subvol=@root");
    }

    #[test]
    fn rw_clears_earlier_ro() {
        // The classic "ro,rw" sequence — second wins.
        let (flags, _) = fold_options("ro,rw");
        assert_eq!(flags, MsFlags::empty());
    }

    #[test]
    fn rbind_sets_bind_and_rec() {
        let (flags, _) = fold_options("rbind");
        assert_eq!(flags, MsFlags::MS_BIND | MsFlags::MS_REC);
    }

    #[test]
    fn skips_empty_tokens() {
        let (flags, data) = fold_options(",,ro,,subvol=@,,");
        assert_eq!(flags, MsFlags::MS_RDONLY);
        assert_eq!(data, "subvol=@");
    }
}
