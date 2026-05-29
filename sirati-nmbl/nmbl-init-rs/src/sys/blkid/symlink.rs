//! Symlink creation helpers for /dev/disk/by-* population.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{NmblError, Result};
use crate::{nmbl_verbose, nmbl_warn};

use super::{CATEGORIES, DISK_DIR};

/// For each category that has an attribute in `attrs`, ensure the
/// symlink points at `dev`. Returns the number of links touched.
pub(super) fn create_links_for(dev: &Path, attrs: &HashMap<String, String>) -> usize {
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
pub(super) fn ensure_symlink(link: &Path, target: &Path) -> Result<bool> {
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
pub(super) fn is_safe_symlink_name(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value == "." || value == ".." {
        return false;
    }
    !value.chars().any(|c| c == '/' || c == '\0')
}

/// Public for the unit tests below; allows them to assert what we'd
/// _try_ to create without actually touching `/dev/disk/`.
#[allow(dead_code)]
pub(super) fn link_targets_for(
    dev: &Path,
    attrs: &HashMap<String, String>,
) -> Vec<(PathBuf, PathBuf)> {
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
