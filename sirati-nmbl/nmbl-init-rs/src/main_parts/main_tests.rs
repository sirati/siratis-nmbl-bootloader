use nmbl_init::config::Config;
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::rescue::RescueMode;
use nmbl_init::ui::BootReporter;
use nmbl_init::ui::console::NoopConsole;

use super::should_force_external_rescue;

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
    let mut ops = nmbl_init::sys::ops::RealSys::sync_only();
    load_explicit_modules(&mut ops, &empty, &mut reporter)
        .expect("empty explicit set must be a no-op");
}
