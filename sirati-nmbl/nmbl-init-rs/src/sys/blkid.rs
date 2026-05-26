//! Udev-less population of /dev/disk/by-{partlabel,label,uuid,partuuid}/
//! symlinks. We walk /sys/class/block, run blkid -o export on each
//! resulting /dev/<name>, parse the KEY=VALUE output, and create the
//! corresponding by-* symlinks. Mirrors what udev would do but
//! without dragging in udev itself (which would balloon the
//! initramfs by an order of magnitude).
//!
//! Replaces the bash loop that lived in `mount-and-kernel.sh.nix`
//! (commit 534fe5d, "sirati-nmbl: udev-less stage-0 …").

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{NmblError, Result};
use crate::sys::activation::run_capture;
use crate::{nmbl_info, nmbl_verbose, nmbl_warn};

/// Filesystem locations + blkid attribute keys we care about. Kept as
/// a slice constant so tests can iterate the same set the production
/// path uses, with no chance of drift.
const CATEGORIES: &[(&str, &str)] = &[
    ("by-partlabel", "PARTLABEL"),
    ("by-label", "LABEL"),
    ("by-uuid", "UUID"),
    ("by-partuuid", "PARTUUID"),
];

/// Absolute path to the `blkid` binary in the NMBL initramfs.
///
/// The Nix side wires `pkgs.util-linux`'s `bin/blkid` into `/bin/blkid`
/// inside the initrd (see `lib/config.nix` baseContents). Production
/// always invokes that path; tests skip when it is missing.
const BLKID_BINARY: &str = "/bin/blkid";

/// Where /sys exposes the kernel-known block devices.
const SYSFS_BLOCK_DIR: &str = "/sys/class/block";

/// Where the by-* symlink tree lives.
const DISK_DIR: &str = "/dev/disk";

/// Exit code blkid uses for "no superblock found" — common for
/// unformatted partitions and raw whole-disk nodes. Treat as "no
/// attributes", not a failure.
const BLKID_EXIT_NO_SUPERBLOCK: i32 = 2;

/// Populate /dev/disk/by-{partlabel,label,uuid,partuuid}/ symlinks
/// for every block device in /sys/class/block. Idempotent — re-runs
/// just overwrite the same target. Errors from individual devices
/// are logged via `nmbl_warn!` and do not fail the whole call; only
/// catastrophic errors (e.g. /sys/class/block not readable) bubble.
pub fn populate_disk_by_symlinks() -> Result<()> {
    let sysfs = Path::new(SYSFS_BLOCK_DIR);
    let entries = std::fs::read_dir(sysfs).map_err(|source| NmblError::Io {
        source,
        context: format!("reading {}", sysfs.display()),
    })?;

    // Pre-create the four target directories once. `create_dir_all`
    // already treats AlreadyExists as success.
    for (dir_name, _) in CATEGORIES {
        let dir = Path::new(DISK_DIR).join(dir_name);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            nmbl_warn!(
                "blkid: could not create {}: {} — symlinks for this category will be skipped",
                dir.display(),
                e,
            );
        }
    }

    let mut device_count: usize = 0;
    let mut link_count: usize = 0;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                nmbl_warn!("blkid: dir entry in {} unreadable: {}", sysfs.display(), e);
                continue;
            }
        };

        let name = entry.file_name();
        let dev_path = Path::new("/dev").join(&name);

        // Skip entries the kernel exposes in /sys but for which no
        // /dev/<name> node has materialised (device-mapper aliases,
        // partitions without nodes, …).
        match std::fs::symlink_metadata(&dev_path) {
            Ok(_) => {}
            Err(_) => continue,
        }

        device_count = device_count.saturating_add(1);

        let attrs = match blkid_for(&dev_path) {
            Ok(map) => map,
            Err(e) => {
                nmbl_warn!("blkid: scanning {} failed: {}", dev_path.display(), e);
                continue;
            }
        };

        link_count = link_count.saturating_add(create_links_for(&dev_path, &attrs));
    }

    nmbl_info!(
        "blkid: scanned {} block device(s), created/updated {} by-* symlink(s)",
        device_count,
        link_count,
    );
    Ok(())
}

/// Run `blkid -o export <dev>` and parse the result. Exit code 2 is
/// remapped to "empty attributes" — see [`BLKID_EXIT_NO_SUPERBLOCK`].
fn blkid_for(dev: &Path) -> Result<HashMap<String, String>> {
    let argv = vec![
        "-o".to_string(),
        "export".to_string(),
        dev.display().to_string(),
    ];
    let (outcome, captured) = run_capture(Path::new(BLKID_BINARY), &argv)?;

    if !outcome.normal_exit {
        nmbl_warn!(
            "blkid: {} killed by signal (exit_code={}); treating as empty",
            dev.display(),
            outcome.exit_code,
        );
        return Ok(HashMap::new());
    }

    match outcome.exit_code {
        0 => {}
        BLKID_EXIT_NO_SUPERBLOCK => return Ok(HashMap::new()),
        other => {
            nmbl_warn!(
                "blkid: {} exited {} (not 0/2); treating as empty",
                dev.display(),
                other,
            );
            return Ok(HashMap::new());
        }
    }

    let text = match std::str::from_utf8(&captured) {
        Ok(s) => s,
        Err(e) => {
            nmbl_warn!(
                "blkid: {} produced non-UTF8 stdout ({}); skipping",
                dev.display(),
                e,
            );
            return Ok(HashMap::new());
        }
    };

    Ok(parse_blkid_export(text))
}

/// For each category that has an attribute in `attrs`, ensure the
/// symlink points at `dev`. Returns the number of links touched.
fn create_links_for(dev: &Path, attrs: &HashMap<String, String>) -> usize {
    let mut count: usize = 0;
    for (dir_name, attr_key) in CATEGORIES {
        let Some(value) = attrs.get(*attr_key) else {
            continue;
        };
        if !is_safe_symlink_name(value) {
            nmbl_warn!(
                "blkid: refusing to create {}/{:?} for {} — contains '/' or NUL",
                dir_name,
                value,
                dev.display(),
            );
            continue;
        }
        let link_path = Path::new(DISK_DIR).join(dir_name).join(value);
        match ensure_symlink(&link_path, dev) {
            Ok(true) => {
                count = count.saturating_add(1);
                nmbl_verbose!("blkid: {}/{} -> {}", dir_name, value, dev.display(),);
            }
            Ok(false) => {
                // Symlink already pointed at the right place — nothing
                // to do, don't count toward "created/updated".
            }
            Err(e) => {
                nmbl_warn!(
                    "blkid: failed to symlink {} -> {}: {}",
                    link_path.display(),
                    dev.display(),
                    e,
                );
            }
        }
    }
    count
}

/// Ensure `link` is a symlink that points at `target`. Returns `Ok(true)`
/// when a new link was written (created or replaced), `Ok(false)` when
/// it already matched.
fn ensure_symlink(link: &Path, target: &Path) -> Result<bool> {
    match std::fs::read_link(link) {
        Ok(existing) => {
            if existing == target {
                return Ok(false);
            }
            nmbl_warn!(
                "blkid: replacing stale symlink {} -> {} (was {})",
                link.display(),
                target.display(),
                existing.display(),
            );
            std::fs::remove_file(link).map_err(|source| NmblError::Io {
                source,
                context: format!("removing stale symlink {}", link.display()),
            })?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Fall through to create.
        }
        Err(e) => {
            // The path exists but is not a readable symlink (perhaps
            // it is a regular file). Try to remove it so we can place
            // our symlink; if that fails, surface the error.
            nmbl_warn!(
                "blkid: {} exists but is not a symlink ({}); replacing",
                link.display(),
                e,
            );
            std::fs::remove_file(link).map_err(|source| NmblError::Io {
                source,
                context: format!("removing non-symlink at {}", link.display()),
            })?;
        }
    }

    std::os::unix::fs::symlink(target, link).map_err(|source| NmblError::Io {
        source,
        context: format!(
            "creating symlink {} -> {}",
            link.display(),
            target.display()
        ),
    })?;
    Ok(true)
}

/// `false` if `value` contains a `/` (would escape the by-* directory)
/// or an embedded NUL (would terminate the C path early). Walks via
/// `chars().any()` so we never raw-index into a `&str`.
fn is_safe_symlink_name(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value == "." || value == ".." {
        return false;
    }
    !value.chars().any(|c| c == '/' || c == '\0')
}

/// Parse one `blkid -o export` payload into a `HashMap`.
///
/// Format (per `blkid(8)` OUTPUT FORMAT, "export" mode): one
/// `KEY=VALUE` per line, blank lines separate device records (we
/// always call with a single device, so we just merge keys). VALUEs
/// are unquoted. Lines without `=` are ignored. Whitespace is
/// trimmed off the KEY side; the VALUE is taken verbatim except for
/// trailing CR / LF.
pub fn parse_blkid_export(text: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for raw_line in text.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let Some(eq_idx) = line.find('=') else {
            continue;
        };
        // `split_at` is total here because `eq_idx` came from `find`.
        let (key, value_with_eq) = line.split_at(eq_idx);
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        // Strip the leading '='. `value_with_eq` starts with '=' so
        // `get(1..)` is total too; `unwrap_or_default` collapses the
        // would-never-fire None branch into an empty `&str`.
        let value = value_with_eq.get(1..).unwrap_or_default();
        out.insert(key.to_string(), value.to_string());
    }
    out
}

/// Public for the unit tests below; allows them to assert what we'd
/// _try_ to create without actually touching `/dev/disk/`.
#[allow(dead_code)]
fn link_targets_for(dev: &Path, attrs: &HashMap<String, String>) -> Vec<(PathBuf, PathBuf)> {
    let mut links = Vec::new();
    for (dir_name, attr_key) in CATEGORIES {
        if let Some(value) = attrs.get(*attr_key)
            && is_safe_symlink_name(value)
        {
            links.push((
                Path::new(DISK_DIR).join(dir_name).join(value),
                dev.to_path_buf(),
            ));
        }
    }
    links
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_blkid_output() {
        let text = "\
DEVNAME=/dev/sda1
LABEL=boot
UUID=1234-ABCD
TYPE=vfat
PARTLABEL=disk-main-ESP
PARTUUID=abcdef01-1234-5678-9abc-def012345678
";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("LABEL"), Some(&"boot".to_string()));
        assert_eq!(map.get("UUID"), Some(&"1234-ABCD".to_string()));
        assert_eq!(map.get("PARTLABEL"), Some(&"disk-main-ESP".to_string()));
        assert_eq!(
            map.get("PARTUUID"),
            Some(&"abcdef01-1234-5678-9abc-def012345678".to_string()),
        );
        assert_eq!(map.get("DEVNAME"), Some(&"/dev/sda1".to_string()));
        assert_eq!(map.get("TYPE"), Some(&"vfat".to_string()));
        assert_eq!(map.len(), 6);
    }

    #[test]
    fn parse_empty_input_returns_empty_map() {
        assert!(parse_blkid_export("").is_empty());
    }

    #[test]
    fn parse_skips_blank_lines_and_lines_without_eq() {
        let text = "\n\nUUID=abc\n\njust-a-comment-line\nLABEL=root\n\n";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("UUID"), Some(&"abc".to_string()));
        assert_eq!(map.get("LABEL"), Some(&"root".to_string()));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn parse_handles_crlf_line_endings() {
        let text = "UUID=abc\r\nLABEL=root\r\n";
        let map = parse_blkid_export(text);
        // CR is stripped from the end of the value.
        assert_eq!(map.get("UUID"), Some(&"abc".to_string()));
        assert_eq!(map.get("LABEL"), Some(&"root".to_string()));
    }

    #[test]
    fn parse_value_with_embedded_equals_keeps_full_value() {
        // blkid never emits such values for the keys we care about,
        // but the parser shouldn't truncate at the second '='.
        let text = "WEIRD=foo=bar=baz";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("WEIRD"), Some(&"foo=bar=baz".to_string()));
    }

    #[test]
    fn parse_value_with_trailing_whitespace_kept() {
        // We do NOT trim values — blkid's export format never quotes
        // them, so trailing whitespace would be meaningful.
        let text = "LABEL=root \n";
        let map = parse_blkid_export(text);
        assert_eq!(map.get("LABEL"), Some(&"root ".to_string()));
    }

    #[test]
    fn safe_symlink_name_rejects_slash_nul_and_dots() {
        assert!(!is_safe_symlink_name(""));
        assert!(!is_safe_symlink_name("foo/bar"));
        assert!(!is_safe_symlink_name("/etc/passwd"));
        assert!(!is_safe_symlink_name("ok\0bad"));
        assert!(!is_safe_symlink_name("."));
        assert!(!is_safe_symlink_name(".."));
    }

    #[test]
    fn safe_symlink_name_accepts_typical_disko_values() {
        assert!(is_safe_symlink_name("disk-main-ESP"));
        assert!(is_safe_symlink_name("root"));
        assert!(is_safe_symlink_name("1234-ABCD"));
        assert!(is_safe_symlink_name("abcdef01-1234-5678-9abc-def012345678",));
        // Spaces and dots are legal in filesystem labels.
        assert!(is_safe_symlink_name("My Disk"));
        assert!(is_safe_symlink_name("v1.0.partition"));
    }

    #[test]
    fn link_targets_uses_all_four_categories_when_present() {
        let mut attrs = HashMap::new();
        attrs.insert("PARTLABEL".to_string(), "disk-main-ESP".to_string());
        attrs.insert("LABEL".to_string(), "boot".to_string());
        attrs.insert("UUID".to_string(), "1234-ABCD".to_string());
        attrs.insert(
            "PARTUUID".to_string(),
            "abcdef01-1234-5678-9abc-def012345678".to_string(),
        );
        let dev = Path::new("/dev/sda1");
        let links = link_targets_for(dev, &attrs);
        assert_eq!(links.len(), 4);
        // Each link must point back at /dev/sda1.
        for (_, target) in &links {
            assert_eq!(target.as_path(), dev);
        }
        // Spot-check one of the link paths.
        let partlabel_link = PathBuf::from("/dev/disk/by-partlabel/disk-main-ESP");
        assert!(links.iter().any(|(l, _)| l == &partlabel_link));
    }

    #[test]
    fn link_targets_skips_unsafe_values() {
        let mut attrs = HashMap::new();
        attrs.insert("LABEL".to_string(), "../escape".to_string());
        attrs.insert("UUID".to_string(), "safe".to_string());
        let dev = Path::new("/dev/sda1");
        let links = link_targets_for(dev, &attrs);
        assert_eq!(links.len(), 1, "the unsafe LABEL must have been dropped");
        let (link, _) = &links[0];
        assert_eq!(link, &PathBuf::from("/dev/disk/by-uuid/safe"));
    }

    #[test]
    fn categories_constant_lists_all_four_dirs() {
        // Sanity-check the constant so a future edit doesn't silently
        // forget a category. If you intend to add a fifth, update
        // this test deliberately.
        let names: Vec<&str> = CATEGORIES.iter().map(|(d, _)| *d).collect();
        assert_eq!(
            names,
            vec!["by-partlabel", "by-label", "by-uuid", "by-partuuid"],
        );
    }
}
