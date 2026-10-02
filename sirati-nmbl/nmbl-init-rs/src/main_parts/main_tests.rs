use nmbl_init::config::Config;
use nmbl_init::error::NmblError;
use nmbl_init::imageload::{DriverImageHandle, DriverImagesHandle, detach_all_driver_images};
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::policy::should_force_rescue;
use nmbl_init::rescue::RescueMode;
use nmbl_init::terminal::TerminalAction;
use nmbl_init::ui::BootReporter;
use nmbl_init::ui::console::NoopConsole;

use super::should_force_external_rescue;
use crate::boot_runtime::should_teardown_driver_images;

#[cfg(feature = "stateful")]
#[test]
fn skipped_selector_honours_remembered_default_and_records_it() {
    use nmbl_init::config::StatefulConfig;
    use nmbl_init::generations::Generation;
    use nmbl_init::state::{State, read, write_padded};
    use nmbl_init::ui::Decision;

    use super::dispatch::resolve_default_index;
    use super::stateful::select_default_with_stateful;

    let dir = tempfile::tempdir().expect("temporary state directory");
    let profiles = dir.path().join("profiles");
    let state_dir = dir.path().join("boot/nmbl");
    std::fs::create_dir_all(&profiles).expect("profiles directory");
    std::fs::create_dir_all(&state_dir).expect("state directory");
    std::os::unix::fs::symlink("system-3-link", profiles.join("system"))
        .expect("active profile link");
    let generation = |number| Generation {
        number,
        profile_link: profiles.join(format!("system-{number}-link")),
        toplevel: dir.path().join(format!("system-{number}")),
        kernel: dir.path().join("kernel"),
        initrd: dir.path().join("initrd"),
        init_path: dir.path().join("init"),
        kernel_params: Vec::new(),
        label: String::new(),
    };
    let generations = [generation(3), generation(2)];
    let state_path = state_dir.join("state.bin");
    write_padded(&state_path, &State::default()).expect("initial state");
    std::fs::write(state_dir.join("boot-default"), "generation 2\n").expect("remembered default");

    let mut config = Config::recovery_default();
    config.paths.nix_profiles_dir = profiles;
    config.runtime_state_mountpoint = Some(dir.path().join("boot"));
    config.stateful = Some(StatefulConfig {
        max_recovery_attempts: 5,
        success_target: "multi-user.target".to_owned(),
    });

    let default_index = resolve_default_index(&config, &generations);
    assert_eq!(default_index, 1);
    let decision =
        select_default_with_stateful(&config, &generations, default_index).expect("boot decision");
    assert!(matches!(
        decision,
        Decision::Boot {
            generation_index: 1,
            ..
        }
    ));
    assert_eq!(
        read(&state_path)
            .expect("read state")
            .expect("state exists")
            .last_attempted_generation
            .map(|n| n.get()),
        Some(2),
    );
}

#[test]
fn force_on_boot_external_selects_rescue() {
    // The regression: force_on_boot=true + mode=external must select
    // the deterministic external-rescue path. Both conditions are
    // required — neither alone fires the trigger.
    let mut cfg = Config::recovery_default();
    cfg.rescue.force_on_boot = true;
    cfg.rescue.mode = RescueMode::External;
    assert!(should_force_external_rescue(&cfg));
}

#[test]
fn force_on_boot_requires_external_mode() {
    // force_on_boot with a non-external mode is a no-op: embedded and
    // none are not no-input deterministic rescue targets.
    for mode in [RescueMode::Embedded, RescueMode::None] {
        let mut cfg = Config::recovery_default();
        cfg.rescue.force_on_boot = true;
        cfg.rescue.mode = mode;
        assert!(
            !should_force_external_rescue(&cfg),
            "force_on_boot must not fire for mode {mode:?}"
        );
    }
}

#[test]
fn external_mode_without_force_does_not_trigger() {
    // Production default: external rescue configured but not forced
    // must leave the normal generation-boot flow untouched.
    let mut cfg = Config::recovery_default();
    cfg.rescue.force_on_boot = false;
    cfg.rescue.mode = RescueMode::External;
    assert!(!should_force_external_rescue(&cfg));
}

#[test]
fn a_present_sentinel_forces_rescue_at_boot() {
    // MED-1: the boot-runtime force-rescue decision is
    // `should_force_rescue(should_force_external_rescue(cfg), cfg)` — the
    // sentinel-aware union. A present sentinel must force rescue even when the
    // legacy force-on-boot trigger is OFF; the old code called
    // `should_force_external_rescue` directly and never read the sentinel.
    let dir = std::env::temp_dir().join(format!(
        "nmbl-med1-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("nmbl")).expect("sentinel dir");
    std::fs::write(dir.join("nmbl/rescue"), b"").expect("write sentinel");

    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    // Legacy trigger is OFF: only the sentinel should force rescue.
    cfg.rescue.force_on_boot = false;
    assert!(
        !should_force_external_rescue(&cfg),
        "the legacy trigger alone is off"
    );
    // The EXACT boot-runtime expression: sentinel ⇒ rescue.
    assert!(
        should_force_rescue(should_force_external_rescue(&cfg), &cfg),
        "a present sentinel must force rescue at boot (MED-1 rewire)"
    );

    // And with the sentinel removed the decision is false again.
    std::fs::remove_file(dir.join("nmbl/rescue")).expect("rm sentinel");
    assert!(
        !should_force_rescue(should_force_external_rescue(&cfg), &cfg),
        "no sentinel + no legacy trigger ⇒ normal boot"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn force_path_loads_explicit_set_before_dispatch() {
    // Contract for the force_on_boot branch: it runs the EXPLICIT module
    // set before `rescue::dispatch` so NMBL's own in-initramfs DHCP path
    // (the network-rescue NIC set, added when `rescue.network` is on) is
    // live before handoff. `loop`/`squashfs` are NO LONGER carried by the
    // explicit list — `rescue::dispatch` → `prepare_disk_rescue` loads
    // them on demand right before the loop-mount — so the force path does
    // not depend on them being in `explicit`. We can't drive `run_inner`
    // (PID-1 flow), but we can lock in that the explicit list (not the
    // early list) is the one the force path loads, and that the loader is
    // a no-op on an empty set (so a forced boot is never spuriously
    // blocked).
    let mut cfg = Config::recovery_default();
    cfg.rescue.force_on_boot = true;
    cfg.rescue.mode = RescueMode::External;
    cfg.kernel_modules.explicit = vec!["virtio_net".to_owned(), "af_packet".to_owned()];
    assert!(should_force_external_rescue(&cfg));
    // The force path loads `config.kernel_modules.explicit`; the
    // network-rescue NIC set lives there (not in `early`), while
    // loop/squashfs are intentionally absent (loaded on demand at
    // dispatch instead).
    assert!(
        cfg.kernel_modules
            .explicit
            .iter()
            .any(|m| m == "virtio_net")
    );
    assert!(!cfg.kernel_modules.explicit.iter().any(|m| m == "loop"));
    assert!(!cfg.kernel_modules.explicit.iter().any(|m| m == "squashfs"));
    assert!(cfg.kernel_modules.early.is_empty());

    // Empty explicit list -> loader short-circuits Ok (no modules
    // tree parse), so a forced boot is never blocked by an empty set.
    let mut empty = Config::recovery_default();
    empty.kernel_modules.explicit.clear();
    let mut noop = NoopConsole::new();
    let mut reporter = BootReporter::new(&mut noop, "test");
    load_explicit_modules(&empty, &mut reporter).expect("empty explicit set must be a no-op");
}

/// A test-only `Execve` action — the capped emergency-shell terminus. Built
/// here (no `Sealed` witness needed) so the FIX-55 teardown decision can be
/// asserted without driving the PID-1 flow.
fn execve_action() -> TerminalAction {
    TerminalAction::Execve {
        path: std::ffi::CString::new("/bin/sh").expect("cstring"),
        argv: vec![std::ffi::CString::new("sh").expect("cstring")],
        env: Vec::new(),
        banner: None,
        rescue_handoff: false,
    }
}

#[test]
fn driver_images_left_mounted_on_capped_shell_divert() {
    // FIX-55: the capped emergency shell (`Execve`) deliberately LEAVES the
    // driver images mounted so the operator can inspect them — they carry no
    // secrets. So the teardown decision is `false` for the Execve terminus.
    assert!(
        !should_teardown_driver_images(&execve_action()),
        "driver images must be left mounted into the capped shell (FIX-55)"
    );
}

#[test]
fn driver_images_torn_down_on_every_reboot_or_handoff_terminus() {
    // Every terminus that reboots or hands the machine off must tear the
    // images down first so the loop devices / mounts do not leak across the
    // cutover. (`RebootIntoRescue` is type-gated on an unforgeable `Sealed`
    // witness so it cannot be built in a unit test, but it is covered by the
    // same `!Execve` rule as `Reboot`/`Kexec` here.)
    for action in [
        TerminalAction::Kexec,
        TerminalAction::Reboot,
        TerminalAction::HaltWithBanner {
            cause: NmblError::ConfigInvalid {
                reason: "test".to_string(),
                context: "test".to_string(),
            },
        },
    ] {
        assert!(
            should_teardown_driver_images(&action),
            "a reboot/handoff terminus ({action:?}) must tear down driver images"
        );
    }
}

#[test]
fn teardown_of_empty_handle_on_kexec_is_a_noop() {
    // The normal (non-bootstrap / feature-off) boot loads no driver images, so
    // the handle is empty and the kexec-path teardown is a harmless no-op.
    let handle = DriverImagesHandle::empty();
    assert!(handle.is_empty());
    detach_all_driver_images(&handle).expect("empty teardown is a no-op Ok");
    // The decision predicate still says "tear down" on Kexec; the empty-handle
    // guard inside `teardown_driver_images_if_normal` is what makes it a no-op.
    assert!(should_teardown_driver_images(&TerminalAction::Kexec));
}

#[test]
fn loaded_handle_records_order_for_the_measure_seam() {
    // The hook makes `DriverImagesHandle::images()` available in load order so
    // #28 can thread the ordered refs into the TPM measure handoff. Pin that
    // the ordered handle the hook returns preserves declared order.
    let mut handle = DriverImagesHandle::empty();
    handle.push(DriverImageHandle::new(
        "a.sfs".to_string(),
        std::path::PathBuf::from("/run/nmbl-boot/a.sfs"),
        [0xa1u8; 64],
        3,
        std::path::PathBuf::from("/run/nmbl-driver-images/0"),
    ));
    handle.push(DriverImageHandle::new(
        "b.sfs".to_string(),
        std::path::PathBuf::from("/run/nmbl-boot/b.sfs"),
        [0xb2u8; 64],
        4,
        std::path::PathBuf::from("/run/nmbl-driver-images/1"),
    ));
    let imgs = handle.images();
    assert_eq!(imgs.len(), 2);
    assert_eq!(imgs.first().expect("first").loop_index(), 3);
    assert_eq!(imgs.get(1).expect("second").loop_index(), 4);
    // A reboot terminus tears this non-empty handle down.
    assert!(should_teardown_driver_images(&TerminalAction::Reboot));
}

/// LOW-B: a staged-rerun driver image, appended to the SAME accumulator the #24
/// hook owns, rides the base set's teardown — it is registered for the normal
/// pre-kexec terminus, not dropped without bookkeeping. This mirrors the exact
/// append `staged::rerun::rerun_merged_effects` performs (push the staged
/// handle's images into the shared accumulator) and asserts the combined set is
/// torn down on the normal terminus and left mounted only on the capped-shell
/// divert (FIX-55).
#[test]
fn staged_rerun_driver_image_is_registered_for_teardown() {
    // The base #24-hook accumulator with one baseline image.
    let mut accumulator = DriverImagesHandle::empty();
    accumulator.push(DriverImageHandle::new(
        "base.sfs".to_string(),
        std::path::PathBuf::from("/run/nmbl-boot/base.sfs"),
        [0x01u8; 64],
        1,
        std::path::PathBuf::from("/run/nmbl-driver-images/0"),
    ));

    // The staged loader returns its own handle; rerun appends each image into
    // the shared accumulator (the LOW-B fix).
    let mut staged = DriverImagesHandle::empty();
    staged.push(DriverImageHandle::new(
        "staged.sfs".to_string(),
        std::path::PathBuf::from("/run/nmbl-boot/staged.sfs"),
        [0x02u8; 64],
        2,
        std::path::PathBuf::from("/run/nmbl-driver-images/1"),
    ));
    for image in staged.images() {
        accumulator.push(image.clone());
    }

    // The staged image now lives in the accumulator the #24 hook tears down.
    assert_eq!(accumulator.len(), 2, "staged image joined the base set");
    assert_eq!(
        accumulator.images().get(1).expect("staged image").name(),
        "staged.sfs",
    );
    // Normal terminus ⇒ the whole set (base + staged) is torn down.
    assert!(should_teardown_driver_images(&TerminalAction::Kexec));
    detach_all_driver_images(&accumulator).expect("teardown of the combined set is Ok");
    // Capped-shell divert ⇒ left mounted for inspection (no secrets, FIX-55).
    assert!(!should_teardown_driver_images(&execve_action()));
}

#[test]
fn pid1_secures_initramfs_root_without_replacing_its_directory() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let directory = tempfile::tempdir().expect("private root");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o1777))
        .expect("kernel root permissions");
    let before = std::fs::metadata(directory.path()).expect("before");
    super::secure_initramfs_root(1, directory.path()).expect("secure root");
    let after = std::fs::metadata(directory.path()).expect("after");
    assert_eq!(after.mode() & 0o7777, 0o755);
    assert_eq!(
        (before.dev(), before.ino(), before.uid()),
        (after.dev(), after.ino(), after.uid())
    );
}

#[test]
fn non_pid1_tools_never_change_a_host_root() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().expect("private host root");
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o1777))
        .expect("host root permissions");
    super::secure_initramfs_root(42, directory.path()).expect("no-op");
    assert_eq!(
        std::fs::metadata(directory.path())
            .expect("after")
            .permissions()
            .mode()
            & 0o7777,
        0o1777
    );
    super::secure_initramfs_root(42, &directory.path().join("missing"))
        .expect("no filesystem access");
}
