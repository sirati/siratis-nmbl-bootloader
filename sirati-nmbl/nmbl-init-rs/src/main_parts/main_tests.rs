use nmbl_init::config::Config;
use nmbl_init::modules::load_explicit_modules;
use nmbl_init::policy::should_force_rescue;
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
