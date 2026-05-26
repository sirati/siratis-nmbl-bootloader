//! Scan `/nix/var/nix/profiles` for NixOS system generations.
//!
//! Replaces `scripts/find-generations.sh.nix`. Each `system-<N>-link` symlink
//! describes one bootable generation; we resolve its kernel/initrd targets,
//! read its kernel-params file, and surface the result as [`Generation`].

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::{nmbl_verbose, nmbl_warn};

/// Single NixOS system generation discovered under
/// `Config::paths::nix_profiles_dir`.
#[derive(Debug, Clone)]
pub struct Generation {
    /// Generation number parsed from `system-<N>-link`.
    pub number: u32,
    /// Full path to the profile symlink itself
    /// (e.g. `/mnt/system/nix/var/nix/profiles/system-42-link`).
    pub profile_link: PathBuf,
    /// Resolved path to the kernel image.
    pub kernel: PathBuf,
    /// Resolved path to the initrd.
    pub initrd: PathBuf,
    /// Contents of `profile_link/kernel-params`, split on whitespace.
    pub kernel_params: Vec<String>,
    /// Best-effort label from `profile_link/nixos-version`. Empty when the
    /// file is missing or unreadable.
    pub label: String,
}

/// Parse `system-<N>-link` filenames into `N`. Returns `None` for anything
/// that doesn't match exactly — that directory hosts other entries too.
fn parse_generation_number(name: &str) -> Option<u32> {
    name.strip_prefix("system-")?
        .strip_suffix("-link")?
        .parse::<u32>()
        .ok()
}

/// Read `<link>/kernel-params` and split on whitespace. IO failures degrade
/// to an empty Vec with a warning — params are nice-to-have, not fatal.
fn read_kernel_params(link: &Path) -> Vec<String> {
    let path = link.join("kernel-params");
    match std::fs::read_to_string(&path) {
        Ok(text) => text.split_ascii_whitespace().map(String::from).collect(),
        Err(err) => {
            nmbl_warn!("kernel-params unreadable at {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// Best-effort: read `<link>/nixos-version` for a human label. Missing file
/// → empty string (logged at verbose only).
fn read_label(link: &Path) -> String {
    let path = link.join("nixos-version");
    match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().to_string(),
        Err(err) => {
            nmbl_verbose!("no nixos-version at {}: {err}", path.display());
            String::new()
        }
    }
}

/// Canonicalize `<link>/kernel` and `<link>/initrd`. Either failing means the
/// generation is broken and the caller should skip it.
fn resolve_kernel_initrd(link: &Path) -> Result<(PathBuf, PathBuf)> {
    let resolve = |name: &str| -> Result<PathBuf> {
        let p = link.join(name);
        std::fs::canonicalize(&p).map_err(|source| NmblError::Io {
            source,
            context: format!("canonicalizing {}", p.display()),
        })
    };
    Ok((resolve("kernel")?, resolve("initrd")?))
}

/// Scan `config.paths.nix_profiles_dir` for `system-*-link` entries and return
/// the matching generations sorted by `number` DESCENDING (newest first).
///
/// Returns [`NmblError::NoGenerations`] when the directory cannot be read or
/// has no usable entries.
pub fn scan_generations(config: &Config) -> Result<Vec<Generation>> {
    let dir = config.paths.nix_profiles_dir.clone();
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(err) => {
            nmbl_warn!("cannot read {}: {err}", dir.display());
            return Err(NmblError::NoGenerations { searched: dir });
        }
    };

    let mut generations: Vec<Generation> = Vec::new();
    for entry in entries.flatten() {
        let file_name_os = entry.file_name();
        let Some(name) = file_name_os.to_str() else {
            continue;
        };
        let Some(number) = parse_generation_number(name) else {
            continue;
        };

        let profile_link = entry.path();
        let (kernel, initrd) = match resolve_kernel_initrd(&profile_link) {
            Ok(pair) => pair,
            Err(err) => {
                nmbl_warn!(
                    "skipping generation {number} at {}: {err}",
                    profile_link.display()
                );
                continue;
            }
        };

        generations.push(Generation {
            number,
            kernel_params: read_kernel_params(&profile_link),
            label: read_label(&profile_link),
            profile_link,
            kernel,
            initrd,
        });
    }

    if generations.is_empty() {
        return Err(NmblError::NoGenerations { searched: dir });
    }

    // Newest first — the TUI selects index 0 as the default boot entry.
    generations.sort_by_key(|g| std::cmp::Reverse(g.number));
    Ok(generations)
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
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Scratch directory; deletes itself on Drop. Avoids the `tempfile` crate.
    struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "nmbl-gen-{tag}-{pid}-{nanos}-{seq}",
                pid = std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self { path }
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn config_for(dir: &Path) -> Config {
        let text = format!(
            "[paths]\nnix_profiles_dir = {dir:?}\nsystem_root = {dir:?}\nshell = \"/bin/sh\"\n",
        );
        toml::from_str::<Config>(&text).expect("config parses")
    }

    /// Build a fake profile dir; canonicalize on a regular file resolves to
    /// the file's own absolute path, which is all the scanner needs.
    fn make_profile(root: &Path, n: u32, params: &str) -> PathBuf {
        let p = root.join(format!("profile-{n}"));
        std::fs::create_dir_all(&p).expect("profile dir");
        std::fs::write(p.join("kernel"), b"k").expect("kernel");
        std::fs::write(p.join("initrd"), b"i").expect("initrd");
        std::fs::write(p.join("kernel-params"), params).expect("params");
        p
    }

    #[test]
    fn empty_dir_yields_no_generations() {
        let tmp = TempDir::new("empty");
        let err = scan_generations(&config_for(&tmp.path)).expect_err("must error");
        match err {
            NmblError::NoGenerations { searched } => assert_eq!(searched, tmp.path),
            other => panic!("expected NoGenerations, got {other:?}"),
        }
    }

    #[test]
    fn descending_order_by_number() {
        let tmp = TempDir::new("desc");
        let profiles = tmp.path.join("profiles");
        let backing = tmp.path.join("backing");
        std::fs::create_dir_all(&profiles).expect("profiles");
        std::fs::create_dir_all(&backing).expect("backing");
        for n in [1u32, 10, 42] {
            let p = make_profile(&backing, n, &format!("root=/dev/sda{n}"));
            symlink(&p, profiles.join(format!("system-{n}-link"))).expect("symlink");
        }
        let gens = scan_generations(&config_for(&profiles)).expect("scan ok");
        assert_eq!(
            gens.iter().map(|g| g.number).collect::<Vec<_>>(),
            [42, 10, 1]
        );
        assert_eq!(gens[0].kernel_params, vec!["root=/dev/sda42".to_string()]);
    }

    #[test]
    fn ignores_garbage_entries() {
        let tmp = TempDir::new("garbage");
        let profiles = tmp.path.join("profiles");
        let backing = tmp.path.join("backing");
        std::fs::create_dir_all(&profiles).expect("profiles");
        std::fs::create_dir_all(&backing).expect("backing");
        let p = make_profile(&backing, 7, "quiet");
        symlink(&p, profiles.join("system-7-link")).expect("symlink");
        std::fs::write(profiles.join("system-bogus-link"), b"x").expect("bogus");
        std::fs::write(profiles.join("random_file"), b"x").expect("random");
        let gens = scan_generations(&config_for(&profiles)).expect("scan ok");
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].number, 7);
        assert_eq!(gens[0].kernel_params, vec!["quiet".to_string()]);
    }
}
