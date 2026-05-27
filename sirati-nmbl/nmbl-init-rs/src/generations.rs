//! Scan `/nix/var/nix/profiles` for NixOS system generations.
//!
//! Replaces `scripts/find-generations.sh.nix`. Each `system-<N>-link` symlink
//! describes one bootable generation; we resolve its kernel/initrd targets,
//! read its kernel-params file, and surface the result as [`Generation`].

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::ui::BootReporter;
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
    /// Path to the NixOS stage-2 `init` script as referenced from
    /// `<profile_link>/init`. Intentionally NOT canonicalized: the chained
    /// kernel needs the path through the profile symlink so the store path
    /// it executes matches what we hand it on the cmdline.
    pub init_path: PathBuf,
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

/// Single-level symlink resolution that rewrites absolute targets to be
/// reachable from NMBL's namespace.
///
/// The system disk's profile symlinks point at absolute store paths like
/// `/nix/store/<hash>/...`, but NMBL has the system root mounted under
/// `mount_prefix` (typically `/mnt/system`), so those targets don't exist
/// from NMBL's view. Mirroring the bash bootloader's `resolve_*_path`
/// helpers (commit e310b67), absolute targets are prefixed and relative
/// targets are joined against the link's parent directory. Non-symlinks
/// pass through unchanged.
fn mount_aware_resolve(path: &Path, mount_prefix: &Path) -> std::io::Result<PathBuf> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_symlink() {
        return Ok(path.to_path_buf());
    }
    let target = std::fs::read_link(path)?;
    if target.is_absolute() {
        let rel = target.strip_prefix("/").unwrap_or(&target);
        Ok(mount_prefix.join(rel))
    } else {
        let parent = path.parent().unwrap_or_else(|| Path::new("/"));
        Ok(parent.join(target))
    }
}

/// Read `<toplevel>/kernel-params` and split on whitespace. IO failures
/// degrade to an empty Vec with a warning — params are nice-to-have, not
/// fatal.
fn read_kernel_params(toplevel: &Path) -> Vec<String> {
    let path = toplevel.join("kernel-params");
    match std::fs::read_to_string(&path) {
        Ok(text) => text.split_ascii_whitespace().map(String::from).collect(),
        Err(err) => {
            nmbl_warn!("kernel-params unreadable at {}: {err}", path.display());
            Vec::new()
        }
    }
}

/// Best-effort: read `<toplevel>/nixos-version` for a human label. Missing
/// file → empty string (logged at verbose only).
fn read_label(toplevel: &Path) -> String {
    let path = toplevel.join("nixos-version");
    match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().to_string(),
        Err(err) => {
            nmbl_verbose!("no nixos-version at {}: {err}", path.display());
            String::new()
        }
    }
}

/// Resolve `<toplevel>/kernel` and `<toplevel>/initrd` through
/// [`mount_aware_resolve`]. Either failing means the generation is broken
/// and the caller should skip it.
fn resolve_kernel_initrd(toplevel: &Path, mount_prefix: &Path) -> Result<(PathBuf, PathBuf)> {
    let resolve = |name: &str| -> Result<PathBuf> {
        let p = toplevel.join(name);
        mount_aware_resolve(&p, mount_prefix).map_err(|source| NmblError::Io {
            source,
            context: format!("resolving {}", p.display()),
        })
    };
    Ok((resolve("kernel")?, resolve("initrd")?))
}

/// Probe `<profile_link>/init` WITHOUT following symlinks. We want the
/// un-resolved path because the chained kernel boots its own initrd which
/// will mount the store and execute exactly the string we hand it on the
/// cmdline; resolving here would replace `<profile_link>/init` with the
/// underlying store path, which is fine on disk but defeats the symlink
/// indirection that lets rollbacks point a fixed cmdline at a moving
/// target. We stat through the mount-aware toplevel (since accessing
/// anything under the raw profile link would walk an absolute store path)
/// but return the un-resolved profile-link path. Missing or unreadable →
/// `Err`, caller skips the generation.
fn resolve_init_path(profile_link: &Path, toplevel: &Path) -> Result<PathBuf> {
    let probe = toplevel.join("init");
    std::fs::symlink_metadata(&probe).map_err(|source| NmblError::Io {
        source,
        context: format!("stat {}", probe.display()),
    })?;
    Ok(profile_link.join("init"))
}

/// Scan `config.paths.nix_profiles_dir` for `system-*-link` entries and return
/// the matching generations sorted by `number` DESCENDING (newest first).
///
/// Returns [`NmblError::NoGenerations`] when the directory cannot be read or
/// has no usable entries.
///
/// `reporter` carries the live boot console; we surface the scan path
/// as the boot-status phase label so the operator sees what's being
/// inspected.
pub fn scan_generations(
    config: &Config,
    reporter: &mut BootReporter<'_>,
) -> Result<Vec<Generation>> {
    let dir = config.paths.nix_profiles_dir.clone();
    let _ = reporter.set_phase(format!("phase 4: scanning generations in {}", dir.display()));
    let mount_prefix = config.paths.system_root.as_path();
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
        let toplevel = match mount_aware_resolve(&profile_link, mount_prefix) {
            Ok(p) => p,
            Err(err) => {
                nmbl_warn!(
                    "skipping generation {number} at {}: resolving profile link: {err}",
                    profile_link.display()
                );
                continue;
            }
        };
        let (kernel, initrd) = match resolve_kernel_initrd(&toplevel, mount_prefix) {
            Ok(pair) => pair,
            Err(err) => {
                nmbl_warn!(
                    "skipping generation {number} at {}: {err}",
                    profile_link.display()
                );
                continue;
            }
        };
        let init_path = match resolve_init_path(&profile_link, &toplevel) {
            Ok(p) => p,
            Err(err) => {
                nmbl_warn!(
                    "skipping generation {number} at {} (no init): {err}",
                    profile_link.display()
                );
                continue;
            }
        };

        generations.push(Generation {
            number,
            kernel_params: read_kernel_params(&toplevel),
            label: read_label(&toplevel),
            profile_link,
            kernel,
            initrd,
            init_path,
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
    use std::os::unix::fs::symlink;
    use std::time::Duration;

    use crossterm::event::KeyEvent;
    use tempfile::TempDir;

    use super::*;
    use crate::ui::app::App;
    use crate::ui::console::{Console, ConsoleKind};

    fn config_for(profiles: &Path, system_root: &Path) -> Config {
        let text = format!(
            "[paths]\nnix_profiles_dir = {profiles:?}\nsystem_root = {system_root:?}\nshell = \"/bin/sh\"\n",
        );
        toml::from_str::<Config>(&text).expect("config parses")
    }

    /// No-op [`Console`] implementation for tests that only need a live
    /// [`BootReporter`] to exercise `scan_generations`'s signature.
    /// Tests never observe the boot-status screen here; they only care
    /// that the scan returns the expected generations slice.
    struct NoopConsole;

    impl Console for NoopConsole {
        fn render(&mut self, _app: &App<'_>) -> Result<()> {
            Ok(())
        }
        fn poll_key(&mut self, _timeout: Duration) -> Result<Option<KeyEvent>> {
            Ok(None)
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(
            &mut self,
            _body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
        ) -> Result<()> {
            Ok(())
        }
    }

    /// Run a closure with a fresh [`BootReporter`] backed by a no-op
    /// console. The reporter is dropped before the closure returns, so
    /// the closure can pass it as `&mut BootReporter<'_>` without
    /// fighting lifetimes against the surrounding test body.
    fn with_reporter<R>(f: impl FnOnce(&mut BootReporter<'_>) -> R) -> R {
        let mut console = NoopConsole;
        let mut reporter = BootReporter::new(&mut console, "test");
        f(&mut reporter)
    }

    /// Build a fake profile dir; mount_aware_resolve on a regular file returns
    /// the same path, which is all the scanner needs when the kernel/initrd
    /// fixtures are plain files rather than symlinks.
    fn make_profile(root: &Path, n: u32, params: &str) -> PathBuf {
        let p = root.join(format!("profile-{n}"));
        std::fs::create_dir_all(&p).expect("profile dir");
        std::fs::write(p.join("kernel"), b"k").expect("kernel");
        std::fs::write(p.join("initrd"), b"i").expect("initrd");
        std::fs::write(p.join("init"), b"#!/bin/sh\n").expect("init");
        std::fs::write(p.join("kernel-params"), params).expect("params");
        p
    }

    /// Use a relative target for profile symlinks so mount_aware_resolve
    /// joins against the link's parent instead of rewriting under
    /// mount_prefix. Mirrors how the absolute-path code path is exercised
    /// separately in [`tests::absolute_symlink_rewritten_under_mount_prefix`].
    fn link_profile_relative(profiles: &Path, n: u32, backing_rel: &str) {
        symlink(backing_rel, profiles.join(format!("system-{n}-link"))).expect("symlink");
    }

    #[test]
    fn empty_dir_yields_no_generations() {
        let tmp = TempDir::new().expect("temp dir");
        let err = with_reporter(|r| scan_generations(&config_for(tmp.path(), tmp.path()), r))
            .expect_err("must error");
        match err {
            NmblError::NoGenerations { searched } => assert_eq!(searched, tmp.path()),
            other => panic!("expected NoGenerations, got {other:?}"),
        }
    }

    #[test]
    fn descending_order_by_number() {
        let tmp = TempDir::new().expect("temp dir");
        let profiles = tmp.path().join("profiles");
        let backing = tmp.path().join("backing");
        std::fs::create_dir_all(&profiles).expect("profiles");
        std::fs::create_dir_all(&backing).expect("backing");
        for n in [1u32, 10, 42] {
            make_profile(&backing, n, &format!("root=/dev/sda{n}"));
            link_profile_relative(&profiles, n, &format!("../backing/profile-{n}"));
        }
        let gens = with_reporter(|r| scan_generations(&config_for(&profiles, tmp.path()), r))
            .expect("scan ok");
        assert_eq!(
            gens.iter().map(|g| g.number).collect::<Vec<_>>(),
            [42, 10, 1]
        );
        assert_eq!(gens[0].kernel_params, vec!["root=/dev/sda42".to_string()]);
        // init_path must remain `<profile_link>/init` so the chained kernel
        // keeps the profile-symlink indirection.
        for g in &gens {
            assert_eq!(g.init_path, g.profile_link.join("init"));
        }
    }

    #[test]
    fn skips_generation_without_init() {
        let tmp = TempDir::new().expect("temp dir");
        let profiles = tmp.path().join("profiles");
        let backing = tmp.path().join("backing");
        std::fs::create_dir_all(&profiles).expect("profiles");
        std::fs::create_dir_all(&backing).expect("backing");
        // Good generation: has init.
        make_profile(&backing, 7, "quiet");
        link_profile_relative(&profiles, 7, "../backing/profile-7");
        // Broken generation: has kernel + initrd but no init.
        let bad = backing.join("profile-9");
        std::fs::create_dir_all(&bad).expect("bad dir");
        std::fs::write(bad.join("kernel"), b"k").expect("kernel");
        std::fs::write(bad.join("initrd"), b"i").expect("initrd");
        std::fs::write(bad.join("kernel-params"), "x").expect("params");
        link_profile_relative(&profiles, 9, "../backing/profile-9");

        let gens = with_reporter(|r| scan_generations(&config_for(&profiles, tmp.path()), r))
            .expect("scan ok");
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].number, 7);
    }

    /// The real NixOS layout: profile symlinks target absolute store paths
    /// (e.g. `/nix/store/<hash>/`). NMBL has the system root mounted under
    /// `mount_prefix`, so those absolute targets must be rewritten to live
    /// under that prefix.
    #[test]
    fn absolute_symlink_rewritten_under_mount_prefix() {
        let tmp = TempDir::new().expect("temp dir");
        let mount_prefix = tmp.path().join("mount");
        let store_rel = "nix/store/abcdef-system";
        let store_abs_on_disk = mount_prefix.join(store_rel);
        let profiles_dir = mount_prefix.join("nix/var/nix/profiles");
        std::fs::create_dir_all(&profiles_dir).expect("profiles");
        std::fs::create_dir_all(&store_abs_on_disk).expect("store");

        // The kernel/initrd inside the toplevel are themselves symlinks to
        // store paths — exercise the second resolution step too.
        let bz_store_rel = "nix/store/xyz-linux";
        let bz_store_abs = mount_prefix.join(bz_store_rel);
        std::fs::create_dir_all(&bz_store_abs).expect("kernel store");
        std::fs::write(bz_store_abs.join("bzImage"), b"k").expect("bz");
        std::fs::write(bz_store_abs.join("initrd"), b"i").expect("initrd");
        symlink(
            format!("/{bz_store_rel}/bzImage"),
            store_abs_on_disk.join("kernel"),
        )
        .expect("kernel symlink");
        symlink(
            format!("/{bz_store_rel}/initrd"),
            store_abs_on_disk.join("initrd"),
        )
        .expect("initrd symlink");
        std::fs::write(store_abs_on_disk.join("init"), b"#!/bin/sh\n").expect("init");
        std::fs::write(store_abs_on_disk.join("kernel-params"), "quiet").expect("params");

        // Profile link with an absolute target (the bash bootloader's case).
        symlink(format!("/{store_rel}"), profiles_dir.join("system-3-link"))
            .expect("profile symlink");

        let gens = with_reporter(|r| scan_generations(&config_for(&profiles_dir, &mount_prefix), r))
            .expect("scan ok");
        assert_eq!(gens.len(), 1);
        assert_eq!(
            gens[0].kernel,
            mount_prefix.join(format!("{bz_store_rel}/bzImage"))
        );
        assert_eq!(
            gens[0].initrd,
            mount_prefix.join(format!("{bz_store_rel}/initrd"))
        );
        assert_eq!(gens[0].init_path, profiles_dir.join("system-3-link/init"));
        assert_eq!(gens[0].kernel_params, vec!["quiet".to_string()]);
    }

    #[test]
    fn ignores_garbage_entries() {
        let tmp = TempDir::new().expect("temp dir");
        let profiles = tmp.path().join("profiles");
        let backing = tmp.path().join("backing");
        std::fs::create_dir_all(&profiles).expect("profiles");
        std::fs::create_dir_all(&backing).expect("backing");
        make_profile(&backing, 7, "quiet");
        link_profile_relative(&profiles, 7, "../backing/profile-7");
        std::fs::write(profiles.join("system-bogus-link"), b"x").expect("bogus");
        std::fs::write(profiles.join("random_file"), b"x").expect("random");
        let gens = with_reporter(|r| scan_generations(&config_for(&profiles, tmp.path()), r))
            .expect("scan ok");
        assert_eq!(gens.len(), 1);
        assert_eq!(gens[0].number, 7);
        assert_eq!(gens[0].kernel_params, vec!["quiet".to_string()]);
    }
}
