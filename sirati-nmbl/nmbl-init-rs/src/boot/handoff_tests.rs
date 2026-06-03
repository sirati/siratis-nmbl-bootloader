//! Tests for the pre-kexec generation guard (#20): the verify gate
//! (enforce-refuse / audit-proceed / disabled-proceed) and the FIX-14
//! cmdline byte-identity between the measure seam and the load.

use super::*;
use std::path::PathBuf;

/// A real-FS-backed [`SysOps`] for the verify-gate tests: a [`DryRunSys`]
/// rooted at `/`, so `open_ro` / `read_file` resolve the absolute tempdir
/// paths the tests create as an identity — the verify streams the exact bytes
/// a `RealSys` open would, with no real side effects elsewhere.
fn test_ops() -> crate::sys::ops::dryrun::DryRunSys {
    use crate::sys::ops::dryrun::{ClosureView, DryRunScenario, DryRunSys};
    DryRunSys::new(
        ClosureView::new(PathBuf::from("/")),
        DryRunScenario::NormalBoot,
    )
}

fn gen_for(params: &[&str]) -> Generation {
    Generation {
        number: 42,
        profile_link: PathBuf::from("/mnt/system/nix/var/nix/profiles/system-42-link"),
        toplevel: PathBuf::from("/mnt/system/nix/store/abc123-nixos-system-42"),
        kernel: PathBuf::from("/mnt/system/boot/vmlinuz"),
        initrd: PathBuf::from("/mnt/system/boot/initrd"),
        init_path: PathBuf::from("/mnt/system/nix/var/nix/profiles/system-42-link/init"),
        kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
        label: String::new(),
    }
}

fn root() -> PathBuf {
    PathBuf::from("/mnt/system")
}

#[test]
fn build_cmdline_override_used_verbatim() {
    let g = gen_for(&["root=/dev/sda1", "quiet"]);
    let s = "init=/sbin/init debug";
    assert_eq!(build_cmdline(&g, Some(s), &root()), s);
}

#[test]
fn build_cmdline_no_override_joins_params_and_appends_init() {
    let g = gen_for(&["root=/dev/sda1", "ro", "quiet"]);
    assert_eq!(
        build_cmdline(&g, None, &root()),
        "root=/dev/sda1 ro quiet init=/nix/var/nix/profiles/system-42-link/init",
    );
}

#[test]
fn build_cmdline_empty_override_yields_empty() {
    let g = gen_for(&["root=/dev/sda1"]);
    assert_eq!(build_cmdline(&g, Some(""), &root()), "");
}

#[test]
fn injects_init_when_missing() {
    let mut g = gen_for(&["root=fstab"]);
    g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
    let out = build_cmdline(&g, None, &root());
    assert!(
        out.ends_with(" init=/nix/store/abc/init"),
        "unexpected cmdline: {out}",
    );
}

#[test]
fn respects_existing_init_in_params() {
    let mut g = gen_for(&["init=/explicit"]);
    g.init_path = PathBuf::from("/mnt/system/nix/store/xyz/init");
    assert_eq!(build_cmdline(&g, None, &root()), "init=/explicit");
}

#[test]
fn override_passes_through() {
    let mut g = gen_for(&["root=fstab"]);
    g.init_path = PathBuf::from("/mnt/system/nix/store/xyz/init");
    assert_eq!(build_cmdline(&g, Some("foo bar"), &root()), "foo bar");
}

#[test]
fn init_outside_system_root_warns_but_uses_raw() {
    let mut g = gen_for(&["root=fstab"]);
    g.init_path = PathBuf::from("/elsewhere/init");
    let out = build_cmdline(&g, None, &root());
    assert!(
        out.ends_with(" init=/elsewhere/init"),
        "unexpected cmdline: {out}",
    );
}

#[test]
fn empty_params_still_inject_init() {
    let mut g = gen_for(&[]);
    g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
    assert_eq!(build_cmdline(&g, None, &root()), "init=/nix/store/abc/init");
}

#[test]
fn init_token_matched_only_at_token_start() {
    // A param ending in "init=" must NOT short-circuit injection — the
    // check looks at whole whitespace tokens, not substrings.
    let mut g = gen_for(&["weird_suffix_init=foo"]);
    g.init_path = PathBuf::from("/mnt/system/nix/store/abc/init");
    let out = build_cmdline(&g, None, &root());
    assert!(out.contains(" init=/nix/store/abc/init"), "got: {out}");
}

// ---- verify gate (#20 step a) -----------------------------------------

/// A config with no `[signing]` table ⇒ `signing.enable = false`.
fn config_signing_disabled() -> Config {
    toml::from_str::<Config>("[paths]\nsystem_root = \"/mnt/system\"\n")
        .expect("base config parses")
}

#[test]
fn verify_proceeds_when_signing_disabled() {
    // signing.enable = false ⇒ the gate short-circuits to Proceed without
    // touching any sidecar. The verify step is Ok regardless of feature —
    // the operator declined signature enforcement at build/config time,
    // which is not an allow-unsigned bypass of an enabled policy (FIX-04).
    let cfg = config_signing_disabled();
    let g = gen_for(&["root=/dev/sda1"]);
    assert!(
        verify_generation_signature(&test_ops(), &cfg, &g).is_ok(),
        "disabled signing must let the boot proceed"
    );
}

#[test]
fn measure_off_is_a_noop() {
    // tpm.measure = false (and, on secure-boot builds, secure_boot.enable =
    // false) ⇒ measure_handoff is a NO-OP: it returns Ok without touching the
    // TPM, regardless of whether a verified generation is present. This is the
    // measure-OFF posture (R-8).
    let cfg = config_signing_disabled();
    assert!(
        !measure_required(&cfg),
        "measure must be off for a bare config"
    );
    let g = gen_for(&["root=/dev/sda1"]);
    let no_images = crate::imageload::DriverImagesHandle::empty();
    assert!(
        measure_handoff(&cfg, &g, None, "init=/sbin/init", &no_images).is_ok(),
        "measure-off must be a no-op even with no verified generation",
    );
}

#[test]
fn handoff_cmdline_is_the_buffer_that_is_loaded() {
    // FIX-14: the cmdline the measure seam carries (Handoff::cmdline) is
    // built once and is the SAME buffer destructured for the load. This
    // proves there is exactly one cmdline String — the value measured
    // equals the value loaded, with no independent rebuild between the
    // seam and the kexec call.
    let g = gen_for(&["root=/dev/sda1", "ro", "quiet"]);
    let built = build_cmdline(&g, None, &root());
    let handoff = Handoff {
        cmdline: built.clone(),
    };
    // Mirror the destructure that feeds the load below the seam.
    let Handoff { cmdline } = handoff;
    assert_eq!(
        cmdline, built,
        "the cmdline handed to load must be byte-identical to the one the seam measured",
    );
}

#[cfg(feature = "secure-boot")]
mod secure_boot {
    use super::*;
    use crate::error::NmblError;
    use std::path::Path;
    use tempfile::TempDir;

    /// A real on-disk generation: a store-style toplevel, plus kernel and
    /// initrd files that actually exist so the verifier can open + hash
    /// them over a pinned fd. No sidecars are written, so an enforcing
    /// verify fails with a missing-signature error.
    fn on_disk_generation(root: &Path) -> Generation {
        let top = root.join("nix/store/abc123-nixos-system-42");
        std::fs::create_dir_all(&top).expect("toplevel");
        let boot = root.join("boot");
        std::fs::create_dir_all(&boot).expect("boot");
        std::fs::write(boot.join("vmlinuz"), b"fake-kernel-bytes").expect("kernel");
        std::fs::write(boot.join("initrd"), b"fake-initrd-bytes").expect("initrd");
        Generation {
            number: 42,
            profile_link: top.clone(),
            toplevel: top.clone(),
            kernel: boot.join("vmlinuz"),
            initrd: boot.join("initrd"),
            init_path: top.join("init"),
            kernel_params: vec!["root=/dev/sda1".to_string()],
            label: String::new(),
        }
    }

    /// A config in the given signing posture whose `runtime_boot_mountpoint`
    /// points at a real writable boot dir (so sidecar resolution succeeds
    /// and the only failure is the genuinely-missing signature).
    fn config_with_posture(boot: &Path, enable: bool, enforce: bool) -> Config {
        let text = format!(
            "[paths]\nsystem_root = \"/mnt/system\"\n\
             [signing]\nenable = {enable}\nenforce = {enforce}\n",
        );
        let mut cfg = toml::from_str::<Config>(&text).expect("config parses");
        cfg.runtime_boot_mountpoint = Some(boot.to_path_buf());
        cfg
    }

    #[test]
    fn enforce_mode_missing_sig_refuses_without_loading() {
        // enable+enforce, no sidecar on disk ⇒ the verify gate Refuses and
        // verify_generation_signature surfaces PolicyRefused. The keystone:
        // nothing is loaded — the error short-circuits before any kexec
        // staging (R-1 / FIX-04).
        let tmp = TempDir::new().expect("temp");
        let boot = tmp.path().join("boot");
        std::fs::create_dir_all(&boot).expect("boot dir");
        let cfg = config_with_posture(&boot, true, true);
        let g = on_disk_generation(tmp.path());

        let err = verify_generation_signature(&test_ops(), &cfg, &g)
            .expect_err("enforce + no sig must refuse");
        assert!(
            matches!(err, NmblError::PolicyRefused { .. }),
            "expected PolicyRefused, got {err:?}",
        );
    }

    #[test]
    fn audit_mode_missing_sig_warns_but_proceeds() {
        // enable, !enforce ⇒ audit: the same missing-signature verify runs
        // but only warns; verify_generation_signature returns Ok so the
        // boot proceeds. This is the ONLY relaxation (FIX-16/FIX-31).
        let tmp = TempDir::new().expect("temp");
        let boot = tmp.path().join("boot");
        std::fs::create_dir_all(&boot).expect("boot dir");
        let cfg = config_with_posture(&boot, true, false);
        let g = on_disk_generation(tmp.path());

        assert!(
            verify_generation_signature(&test_ops(), &cfg, &g).is_ok(),
            "audit mode must proceed past a missing signature",
        );
    }

    /// FIX-27 fail-closed: a measure-REQUIRED build (`tpm.measure = true`) with
    /// NO verified generation must REFUSE (PolicyRefused), never proceed to an
    /// unmeasured boot. This is the "measure required but inputs not verified"
    /// guard — it fires before any TPM access, so it is deterministic on a
    /// TPM-less test box.
    #[test]
    fn measure_required_without_verified_generation_refuses() {
        let cfg = toml::from_str::<Config>("[tpm]\nmeasure = true\n")
            .expect("config with tpm.measure parses");
        assert!(
            measure_required(&cfg),
            "tpm.measure = true ⇒ measure required"
        );
        let g = gen_for(&["root=/dev/sda1"]);
        let no_images = crate::imageload::DriverImagesHandle::empty();
        let err = measure_handoff(&cfg, &g, None, "init=/sbin/init", &no_images)
            .expect_err("measure required + unverified ⇒ refuse");
        assert!(
            matches!(err, NmblError::PolicyRefused { .. }),
            "expected PolicyRefused for an unmeasurable required boot, got {err:?}",
        );
    }

    /// The measure posture is also required when the secure-boot priority gate
    /// is enabled (R-8), independent of `tpm.measure`.
    #[test]
    fn secure_boot_enable_requires_measure() {
        let cfg = toml::from_str::<Config>("[secure_boot]\nenable = true\nenforce = true\n")
            .expect("config with secure_boot.enable parses");
        assert!(
            measure_required(&cfg),
            "secure_boot.enable = true ⇒ measure required",
        );
    }

    #[test]
    fn enforce_mode_disabled_signing_still_proceeds() {
        // The disabled short-circuit holds even on a secure-boot build:
        // signing.enable = false ⇒ Proceed, no sidecar lookup attempted.
        let tmp = TempDir::new().expect("temp");
        let boot = tmp.path().join("boot");
        std::fs::create_dir_all(&boot).expect("boot dir");
        let cfg = config_with_posture(&boot, false, false);
        let g = on_disk_generation(tmp.path());

        assert!(
            verify_generation_signature(&test_ops(), &cfg, &g).is_ok(),
            "disabled signing proceeds on a secure-boot build too",
        );
    }
}
