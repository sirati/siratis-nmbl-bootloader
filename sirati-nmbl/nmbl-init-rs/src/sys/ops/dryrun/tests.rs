//! Hermetic unit tests for [`DryRunSys`].
//!
//! Each test builds a synthetic closure (a temp dir populated with a few
//! fake `.ko` / binary / kernel / initrd files) and drives `DryRunSys`
//! over it, asserting that a present file records NO finding and an
//! absent required file records a [`MissingFile`] with the right op/path.
//! No real device, fork, mount, or kexec is touched.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests assert on contract failures"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::sys::ops::{BlockOps, ExecOps, FsOps, KexecOps, KexecTarget, ModuleOps};

/// Make a unique temp dir for one test, populated by `setup`.
fn temp_closure(tag: &str, setup: impl FnOnce(&Path)) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "nmbl-dryrun-{}-{}-{tag}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).expect("mkdir temp closure");
    setup(&dir);
    dir
}

fn sys(root: PathBuf, scenario: DryRunScenario) -> DryRunSys {
    DryRunSys::new(ClosureView::new(root), scenario)
}

/// Drive a dry-run async method to completion on a current-thread tokio
/// runtime. The dry-run methods never yield (no real device wait), so
/// this returns immediately — but going through the genuine runtime
/// avoids a hand-rolled `unsafe` waker.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build current-thread runtime")
        .block_on(fut)
}

#[test]
fn present_kexec_images_record_no_finding() {
    let root = temp_closure("kexec-ok", |d| {
        fs::create_dir_all(d.join("boot")).expect("mkdir");
        fs::write(d.join("boot/vmlinuz"), b"kernel").expect("write");
        fs::write(d.join("boot/initrd"), b"initrd").expect("write");
    });
    let mut s = sys(root.clone(), DryRunScenario::NormalBoot);
    s.kexec_load(
        KexecTarget::MultiFile {
            kernel: PathBuf::from("/boot/vmlinuz"),
            initrd: PathBuf::from("/boot/initrd"),
        },
        None,
        &[],
        "ro",
        0,
    )
    .expect("kexec_load dry-run");
    assert!(s.findings().is_empty(), "{:?}", s.findings().items());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn absent_kexec_kernel_records_finding() {
    let root = temp_closure("kexec-missing", |d| {
        fs::create_dir_all(d.join("boot")).expect("mkdir");
        fs::write(d.join("boot/initrd"), b"initrd").expect("write");
        // kernel deliberately absent
    });
    let mut s = sys(root.clone(), DryRunScenario::NormalBoot);
    s.kexec_load(
        KexecTarget::MultiFile {
            kernel: PathBuf::from("/boot/vmlinuz"),
            initrd: PathBuf::from("/boot/initrd"),
        },
        None,
        &[],
        "ro",
        0,
    )
    .expect("kexec_load dry-run");
    let items = s.findings().items();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].op, "kexec_load");
    assert_eq!(items[0].path, PathBuf::from("/boot/vmlinuz"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_shell_records_finding_and_preflight_errs() {
    let root = temp_closure("shell-missing", |_d| {});
    let mut s = sys(root.clone(), DryRunScenario::RawShell);
    // seal-exempt: DryRunSys::spawn_shell never reaches the real fork (it
    // returns DryRunShellPreflight), so the seal gates no syscall here.
    let r = s.spawn_shell(
        crate::policy::Sealed::test_witness(),
        Path::new("/bin/sh"),
        80,
        24,
    );
    // The contract: the typed preflight signal, NO fork performed.
    // (`PtyChild` is not `Debug`, so match instead of `expect_err`.)
    match r {
        Err(err) => assert!(
            matches!(err, NmblError::DryRunShellPreflight),
            "expected DryRunShellPreflight, got {err}"
        ),
        Ok(_) => panic!("spawn_shell must Err in dry-run, never fork"),
    }
    let items = s.findings().items();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].op, "spawn_shell");
    assert_eq!(items[0].path, PathBuf::from("/bin/sh"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn present_shell_records_no_finding_but_still_errs() {
    let root = temp_closure("shell-ok", |d| {
        fs::create_dir_all(d.join("bin")).expect("mkdir");
        fs::write(d.join("bin/sh"), b"#!/x").expect("write");
    });
    let mut s = sys(root.clone(), DryRunScenario::PrettyShell);
    // seal-exempt: DryRunSys::spawn_shell never reaches the real fork (it
    // returns DryRunShellPreflight), so the seal gates no syscall here.
    let r = s.spawn_shell(
        crate::policy::Sealed::test_witness(),
        Path::new("/bin/sh"),
        80,
        24,
    );
    assert!(r.is_err(), "dry-run never forks, always Errs");
    assert!(s.findings().is_empty(), "{:?}", s.findings().items());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn missing_run_binary_records_finding() {
    let root = temp_closure("run-missing", |_d| {});
    let mut s = sys(root.clone(), DryRunScenario::NormalBoot);
    let out = block_on(s.run(Path::new("/bin/cryptsetup"), &[], None)).expect("run dry-run");
    assert_eq!(out.exit_code, 0, "NormalBoot activation succeeds");
    let items = s.findings().items();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].op, "run");
    assert_eq!(items[0].path, PathBuf::from("/bin/cryptsetup"));
    fs::remove_dir_all(&root).ok();
}

#[test]
fn wait_for_device_passes_for_dev_path_and_seeded_synthetic() {
    let root = temp_closure("waitdev", |_d| {});
    let mut s = sys(root.clone(), DryRunScenario::NormalBoot);
    // A /dev/* path is kernel-created → passes with no finding.
    block_on(s.wait_for_device(Path::new("/dev/sda1"), Duration::ZERO, "root", None))
        .expect("wait");
    assert!(s.findings().is_empty(), "{:?}", s.findings().items());

    // Seed a synthetic mapper node via ensure_dev_node, then await it.
    fs::create_dir_all(root.join("sys/block/dm-0")).expect("mkdir");
    fs::write(root.join("sys/block/dm-0/dev"), b"253:0").expect("write");
    s.ensure_dev_node(Path::new("/sys/block/dm-0"), Path::new("/dev/mapper/root"))
        .expect("ensure_dev_node");
    block_on(s.wait_for_device(Path::new("/dev/mapper/root"), Duration::ZERO, "luks", None))
        .expect("wait seeded");
    assert!(s.findings().is_empty(), "{:?}", s.findings().items());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn scenario_outcome_differs_normal_vs_error() {
    let root = temp_closure("scenario", |d| {
        fs::create_dir_all(d.join("bin")).expect("mkdir");
        fs::write(d.join("bin/cryptsetup"), b"x").expect("write");
    });
    let mut normal = sys(root.clone(), DryRunScenario::NormalBoot);
    let n = block_on(normal.run(Path::new("/bin/cryptsetup"), &[], None)).expect("run");
    let mut error = sys(root.clone(), DryRunScenario::ErrorToErrorScreen);
    let e = block_on(error.run(Path::new("/bin/cryptsetup"), &[], None)).expect("run");
    assert_eq!(n.exit_code, 0);
    assert_ne!(e.exit_code, 0);
    // Both binaries were present → no findings either way.
    assert!(normal.findings().is_empty());
    assert!(error.findings().is_empty());
    fs::remove_dir_all(&root).ok();
}

#[test]
fn load_modules_records_only_absent_ko() {
    // Build a modules tree with a modules.dep listing two modules; ship
    // one .ko, omit the other; assert exactly one finding for the absent.
    let release = crate::sys::uname::kernel_release().expect("kernel release");
    // The closure stages the modules tree under the RUNTIME path
    // `/lib/modules/<release>/…`; `load_modules` is called with the
    // runtime modules_dir `/lib/modules`, so each `.ko` path grafts back
    // under the closure root. This mirrors how `--validate-initrm` will
    // run: config modules_dir is `/lib/modules`, closure root is the
    // extracted initrd.
    let root = temp_closure("modules", |d| {
        let mods = d.join(format!("lib/modules/{release}"));
        fs::create_dir_all(mods.join("kernel/fs")).expect("mkdir");
        // present.ko has no deps; absent.ko has no deps.
        fs::write(
            mods.join("modules.dep"),
            "kernel/fs/present.ko:\nkernel/fs/absent.ko:\n",
        )
        .expect("write dep");
        fs::write(mods.join("kernel/fs/present.ko"), b"ko").expect("write ko");
        // absent.ko deliberately omitted.
    });
    let mut s = sys(root.clone(), DryRunScenario::NormalBoot);
    s.load_modules(
        Path::new("/lib/modules"),
        &["present".to_owned(), "absent".to_owned()],
        &[],
    )
    .expect("load_modules dry-run");
    let items = s.findings().items();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0].op, "load_module");
    assert!(items[0].path.ends_with("absent.ko"), "{:?}", items[0].path);
    fs::remove_dir_all(&root).ok();
}

#[test]
fn no_op_methods_never_fail() {
    let root = temp_closure("noop", |_d| {});
    let mut s = sys(root.clone(), DryRunScenario::NormalBoot);
    s.ensure_dir(Path::new("/dev/pts")).expect("ensure_dir");
    s.mount(None, Path::new("/proc"), "proc", "")
        .expect("mount pseudo");
    s.umount(Path::new("/proc"), nix::mount::MntFlags::empty())
        .expect("umount");
    s.btrfs_scan(&[]).expect("btrfs_scan");
    assert!(s.findings().is_empty(), "{:?}", s.findings().items());
    fs::remove_dir_all(&root).ok();
}
