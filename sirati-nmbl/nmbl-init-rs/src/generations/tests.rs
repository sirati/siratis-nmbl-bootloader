//! Tests for generation scanning and active-generation resolution.
//!
//! This module is gated at the module level so all items in it are test-only.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::ui::BootReporter;
use crate::ui::app::App;
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};

use super::Generation;
use super::scan::{active_generation_index, scan_generations};

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
    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>>
    {
        Box::pin(async move { self.poll_event_blocking(timeout) })
    }
    fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
        Ok(None)
    }
    fn size(&self) -> (u16, u16) {
        (80, 24)
    }
    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }
    fn draw_with(&mut self, _body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        Ok(())
    }
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Run a closure with a fresh [`BootReporter`] backed by a no-op
/// console. The reporter is dropped before the closure returns, so
/// the closure can pass it as `&mut BootReporter<'_, '_>` without
/// fighting lifetimes against the surrounding test body.
fn with_reporter<R>(f: impl FnOnce(&mut BootReporter<'_, '_>) -> R) -> R {
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
/// separately in [`absolute_symlink_rewritten_under_mount_prefix`].
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
    symlink(format!("/{store_rel}"), profiles_dir.join("system-3-link")).expect("profile symlink");

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

/// Helper for the active-generation-index tests: builds a profiles
/// dir with the requested generation numbers (each backed by a
/// usable profile) and returns the resulting (profiles_dir,
/// scanned generations) pair.
fn profiles_with_gens(tmp: &Path, numbers: &[u32]) -> (PathBuf, Vec<Generation>) {
    let profiles = tmp.join("profiles");
    let backing = tmp.join("backing");
    std::fs::create_dir_all(&profiles).expect("profiles");
    std::fs::create_dir_all(&backing).expect("backing");
    for n in numbers {
        make_profile(&backing, *n, "quiet");
        link_profile_relative(&profiles, *n, &format!("../backing/profile-{n}"));
    }
    let gens =
        with_reporter(|r| scan_generations(&config_for(&profiles, tmp), r)).expect("scan ok");
    (profiles, gens)
}

#[test]
fn active_generation_index_matches_system_symlink_target() {
    let tmp = TempDir::new().expect("temp dir");
    let (profiles, gens) = profiles_with_gens(tmp.path(), &[1, 10, 42]);
    // Pin the active generation to 10, which is NOT the newest.
    symlink("system-10-link", profiles.join("system")).expect("system symlink");
    let idx = active_generation_index(&gens, &profiles);
    assert_eq!(gens[idx].number, 10);
    // Regression guard: this must not be the default (highest-number) slot.
    assert_ne!(idx, 0);
}

#[test]
fn active_generation_index_missing_symlink_returns_zero() {
    let tmp = TempDir::new().expect("temp dir");
    let (profiles, gens) = profiles_with_gens(tmp.path(), &[1, 7]);
    // No `system` symlink at all.
    assert_eq!(active_generation_index(&gens, &profiles), 0);
}

#[test]
fn active_generation_index_bogus_target_returns_zero() {
    let tmp = TempDir::new().expect("temp dir");
    let (profiles, gens) = profiles_with_gens(tmp.path(), &[3]);
    symlink("not-a-system-link", profiles.join("system")).expect("bogus symlink");
    assert_eq!(active_generation_index(&gens, &profiles), 0);
}

#[test]
fn active_generation_index_unscanned_generation_returns_zero() {
    // `system` points at generation 9, but generation 9 was filtered
    // from the scan (no `init`), so we can't honour the pointer and
    // must fall back to the highest-numbered scanned entry.
    let tmp = TempDir::new().expect("temp dir");
    let profiles = tmp.path().join("profiles");
    let backing = tmp.path().join("backing");
    std::fs::create_dir_all(&profiles).expect("profiles");
    std::fs::create_dir_all(&backing).expect("backing");
    make_profile(&backing, 7, "quiet");
    link_profile_relative(&profiles, 7, "../backing/profile-7");
    let bad = backing.join("profile-9");
    std::fs::create_dir_all(&bad).expect("bad dir");
    std::fs::write(bad.join("kernel"), b"k").expect("kernel");
    std::fs::write(bad.join("initrd"), b"i").expect("initrd");
    std::fs::write(bad.join("kernel-params"), "x").expect("params");
    link_profile_relative(&profiles, 9, "../backing/profile-9");
    symlink("system-9-link", profiles.join("system")).expect("system symlink");

    let gens = with_reporter(|r| scan_generations(&config_for(&profiles, tmp.path()), r))
        .expect("scan ok");
    // Gen 9 was filtered (missing init); only gen 7 remains.
    assert_eq!(gens.len(), 1);
    assert_eq!(active_generation_index(&gens, &profiles), 0);
}

#[test]
fn active_generation_index_regular_file_returns_zero() {
    let tmp = TempDir::new().expect("temp dir");
    let (profiles, gens) = profiles_with_gens(tmp.path(), &[5]);
    // A regular file where the `system` symlink should be — readlink
    // will refuse it. We must not panic and must fall back to 0.
    std::fs::write(profiles.join("system"), b"not a symlink").expect("regular file");
    assert_eq!(active_generation_index(&gens, &profiles), 0);
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

#[test]
fn missing_profiles_dir_on_unmounted_root_is_system_root_not_mounted() {
    // system_root points at a path that exists on the test's own
    // filesystem but is NOT a mount point, and the profiles dir under it
    // does not exist. The scan must classify this as "nothing mounted"
    // rather than the bare NoGenerations.
    let tmp = TempDir::new().expect("temp dir");
    let system_root = tmp.path().join("mnt/system");
    std::fs::create_dir_all(&system_root).expect("system root");
    let profiles = system_root.join("nix/var/nix/profiles");
    let err = with_reporter(|r| scan_generations(&config_for(&profiles, &system_root), r))
        .expect_err("must error");
    match err {
        NmblError::SystemRootNotMounted { mountpoint } => assert_eq!(mountpoint, system_root),
        other => panic!("expected SystemRootNotMounted, got {other:?}"),
    }
}

#[test]
fn existing_empty_profiles_dir_stays_no_generations() {
    // The profiles dir exists but holds no system-N-link entries — the
    // classification must keep NoGenerations even though the system_root
    // tempdir is not a real mount point.
    let tmp = TempDir::new().expect("temp dir");
    let profiles = tmp.path().join("nix/var/nix/profiles");
    std::fs::create_dir_all(&profiles).expect("profiles");
    let err = with_reporter(|r| scan_generations(&config_for(&profiles, tmp.path()), r))
        .expect_err("must error");
    match err {
        NmblError::NoGenerations { searched } => assert_eq!(searched, profiles),
        other => panic!("expected NoGenerations, got {other:?}"),
    }
}
