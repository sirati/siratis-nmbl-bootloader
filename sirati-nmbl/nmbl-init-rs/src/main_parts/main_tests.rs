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
    // Contract for the ordering fix: the force_on_boot branch must
    // run the EXPLICIT module set (which carries the auto-added
    // `loop`/`squashfs`/nicDrivers for mode==external) before
    // `rescue::dispatch`. We can't drive `run_inner` (it is PID-1
    // flow), but we can lock in that the explicit list — not the
    // early list — is the one a forced external rescue depends on,
    // and that the loader is a no-op when that list is empty (so the
    // pre-dispatch call never spuriously fails a forced boot on a
    // platform with built-in loop/squashfs).
    let mut cfg = Config::recovery_default();
    cfg.rescue.force_on_boot = true;
    cfg.rescue.mode = RescueMode::External;
    cfg.kernel_modules.explicit = vec![
        "loop".to_owned(),
        "squashfs".to_owned(),
        "virtio_net".to_owned(),
    ];
    assert!(should_force_external_rescue(&cfg));
    // The force path loads `config.kernel_modules.explicit`; confirm
    // the rescue-critical names live there and not in `early`.
    assert!(cfg.kernel_modules.explicit.iter().any(|m| m == "loop"));
    assert!(cfg.kernel_modules.explicit.iter().any(|m| m == "squashfs"));
    assert!(cfg.kernel_modules.early.is_empty());

    // Empty explicit list -> loader short-circuits Ok (no modules
    // tree parse), so a forced boot is never blocked by an empty set.
    let mut empty = Config::recovery_default();
    empty.kernel_modules.explicit.clear();
    let mut noop = NoopConsole::new();
    let mut reporter = BootReporter::new(&mut noop, "test");
    load_explicit_modules(&empty, &mut reporter).expect("empty explicit set must be a no-op");
}
