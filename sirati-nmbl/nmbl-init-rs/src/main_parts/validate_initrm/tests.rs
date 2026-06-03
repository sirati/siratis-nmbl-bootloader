//! Driver tests over a SYNTHETIC closure (temp dir).
//!
//! Driving the full boot core is heavy, so these focus on the
//! finding-collection layer through a deterministic, uname-independent
//! signal: the emergency-shell binary (`config.paths.shell`). The RawShell
//! scenario presence-checks it via `DryRunSys::spawn_shell`, so a closure
//! MISSING the shell yields a `spawn_shell` finding and a closure
//! CONTAINING it does not. The NormalBoot path is also exercised end-to-end
//! here (it runs against the same synthetic closure without panicking).
//!
//! Scenarios that need a real generations tree / modules.dep (the kexec and
//! load_module deep paths) are left to the nix gate + the orchestrator's
//! VM-less end-to-end test, which build a realistic closure.

use std::fs;
use std::path::{Path, PathBuf};

use nmbl_init::config::Config;

use super::validate_initrm;

/// Build a unique temp dir for one test, populated by `setup`.
fn temp_closure(tag: &str, setup: impl FnOnce(&Path)) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "nmbl-initrm-{}-{}-{tag}",
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

/// A config whose emergency shell is `/bin/validate-initrm-shell` — a path
/// we can stage (or not) in a synthetic closure to control the spawn_shell
/// presence check deterministically.
fn config_with_shell(shell: &str) -> Config {
    let mut c = Config::recovery_default();
    c.paths.shell = PathBuf::from(shell);
    c
}

#[test]
fn missing_shell_binary_is_reported() {
    // Closure with NOTHING staged → the RawShell/PrettyShell spawn_shell
    // preflight must record the absent shell binary.
    let root = temp_closure("missing-shell", |_d| {});
    let config = config_with_shell("/bin/validate-initrm-shell");

    let report = validate_initrm(&config, None, &root);
    assert!(!report.is_clean(), "missing shell must surface a finding");
    let rendered = report.render();
    assert!(
        rendered.contains("/bin/validate-initrm-shell"),
        "report should name the absent shell binary:\n{rendered}"
    );
    assert!(
        rendered.contains("spawn_shell"),
        "report should attribute the finding to spawn_shell:\n{rendered}"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn present_shell_binary_is_not_reported() {
    // Stage the shell binary so the spawn_shell preflight is satisfied.
    let root = temp_closure("present-shell", |d| {
        fs::create_dir_all(d.join("bin")).expect("mkdir bin");
        fs::write(d.join("bin/validate-initrm-shell"), b"#!/bin/sh\n").expect("write shell");
    });
    let config = config_with_shell("/bin/validate-initrm-shell");

    let report = validate_initrm(&config, None, &root);
    let rendered = report.render();
    // The shell binary must not appear as a missing-file finding. (Other
    // best-effort findings from the NormalBoot path — e.g. a missing
    // generations tree surfaced as wait_for_device/scan info — may still
    // appear; we only assert the shell binary is satisfied.)
    assert!(
        !rendered.contains("validate-initrm-shell"),
        "present shell binary must not be reported:\n{rendered}"
    );
    fs::remove_dir_all(&root).ok();
}

/// A config carrying one `luks-password` activation whose backing
/// `cryptsetup` lives at `binary`. The dry-run drives the genuine
/// activation control flow — including the passphrase prompt — so this is
/// the regression fixture for the infinite-spin bug: before the fix the
/// NormalBoot scenario hot-spun forever on the NoopConsole passphrase
/// prompt and this test would hang.
fn config_with_luks(binary: &str) -> Config {
    let mut c = config_with_shell("/bin/sh");
    let activation: nmbl_init::config::Activation = toml::from_str(&format!(
        r#"
            kind = "luks-password"
            binary = "{binary}"
            argv = ["luksOpen", "/dev/sda1", "cryptroot", "--key-file=-"]
            produces_devices = ["/dev/mapper/cryptroot"]
            description = "unlock root"
        "#
    ))
    .expect("parse luks activation");
    c.activations.push(activation);
    c
}

/// The hardest Property-6 fixture: `require_tpm = true` (the Nix default for
/// any measure/secure-boot config — the common install target) PLUS a
/// relockable LUKS `/dev/mapper/cryptroot` and an LVM `/dev/vg0/root`. Under
/// `require_tpm`, a `NoTpm` cap would FAIL the dry-run seal and divert to the
/// refuse terminus — whose `write_sentinel` (real `/boot/nmbl` write) and
/// `relock_volumes` (real `cryptsetup close` / `vgchange -an`) would run on a
/// LIVE host. This fixture makes BOTH the seal's close-mapper path and the
/// refuse terminus's relock+sentinel path reachable so the assertion that the
/// dry-run performs ZERO real ops is non-vacuous.
fn config_require_tpm_with_luks_and_lvm(cryptsetup: &str) -> Config {
    let mut c = config_with_luks(cryptsetup);
    // The Nix-default posture for the common install target: demand a TPM.
    c.tpm.require_tpm = true;
    let lvm: nmbl_init::config::Activation = toml::from_str(
        r#"
            kind = "lvm"
            binary = "/bin/vgchange"
            argv = ["-ay", "vg0"]
            produces_devices = ["/dev/vg0/root"]
            description = "activate LVM"
        "#,
    )
    .expect("parse lvm activation");
    c.activations.push(lvm);
    c
}

#[test]
fn luks_activation_dry_run_terminates_and_reports_cryptsetup() {
    // THE regression: an empty closure (no /bin/cryptsetup) must make the
    // dry-run TERMINATE and surface cryptsetup as a missing file — it must
    // NOT busy-spin on the NoopConsole passphrase prompt.
    let root = temp_closure("luks-missing-cryptsetup", |_d| {});
    let config = config_with_luks("/bin/cryptsetup");

    let report = validate_initrm(&config, None, &root);
    assert!(
        !report.is_clean(),
        "missing cryptsetup must surface a finding"
    );
    let rendered = report.render();
    assert!(
        rendered.contains("/bin/cryptsetup"),
        "report should name the absent cryptsetup binary:\n{rendered}"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn luks_activation_dry_run_with_cryptsetup_present_does_not_report_it() {
    // Stage /bin/cryptsetup so the run_with_tick presence-check is
    // satisfied; the dry-run must still terminate and not fork it.
    let root = temp_closure("luks-present-cryptsetup", |d| {
        fs::create_dir_all(d.join("bin")).expect("mkdir bin");
        fs::write(d.join("bin/cryptsetup"), b"#!/bin/sh\n").expect("write cryptsetup");
        fs::write(d.join("bin/sh"), b"#!/bin/sh\n").expect("write sh");
    });
    let config = config_with_luks("/bin/cryptsetup");

    let report = validate_initrm(&config, None, &root);
    let rendered = report.render();
    assert!(
        !rendered.contains("/bin/cryptsetup"),
        "present cryptsetup must not be reported as missing:\n{rendered}"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn validate_initrm_performs_no_real_tpm_or_cryptsetup_seal_ops() {
    // Property-6: the `--validate-initrm` dry-run drives the GENUINE
    // `drop_to_emergency` → `policy::seal_secrets` control flow (the
    // ErrorToErrorScreen scenario fails an activation and routes there). It
    // MUST NOT cap the real lock PCR (an irreversible TPM poison-extend) nor
    // run a real `cryptsetup close`. We stage a closure WITH cryptsetup +
    // shell present and a luks-tpm-style activation so the seal's
    // close-mapper path is reachable, run all four scenarios, and assert the
    // real-hardware-seal-op counter stayed at zero across every scenario.
    let root = temp_closure("no-real-seal", |d| {
        fs::create_dir_all(d.join("bin")).expect("mkdir bin");
        fs::write(d.join("bin/cryptsetup"), b"#!/bin/sh\n").expect("write cryptsetup");
        fs::write(d.join("bin/sh"), b"#!/bin/sh\n").expect("write sh");
    });
    let config = config_with_luks("/bin/cryptsetup");

    nmbl_init::policy::reset_real_seal_ops();
    let _report = validate_initrm(&config, None, &root);
    assert_eq!(
        nmbl_init::policy::real_seal_ops(),
        0,
        "validate-initrm must perform NO real TPM cap or cryptsetup close: \
         the dry-run seal routes the cap through TpmOps (no-op) and suppresses \
         the mapper close"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn validate_initrm_require_tpm_performs_no_real_seal_or_terminus_ops() {
    // Property-6, strengthened (the destructive-terminus regression): the
    // previous test ran with `require_tpm` UNSET, so the seal degraded open and
    // never approached the refuse terminus. With `require_tpm = true` (the Nix
    // default for any measure/secure-boot config — the COMMON install target) a
    // `NoTpm` dry-run cap used to FAIL the seal and divert to the refuse
    // terminus, whose `write_sentinel` (real `std::fs::write("/boot/nmbl/...")`)
    // and `relock_volumes` (real `cryptsetup close` / `vgchange -an` forks)
    // execute DIRECTLY — destroying the live root mapper on a secure-boot
    // install host. Neither effect ever touched `real_seal_ops`, which is why
    // the leak was missed twice.
    //
    // After the fix the dry-run cap returns `Capped` (so the seal succeeds and
    // the scenario reaches the emergency console as intended), AND every
    // terminus op is routed through the `FsOps`/`ExecOps` seam (no-op'd by
    // `DryRunSys` on the dry-run path). We stage a closure with cryptsetup + the
    // LVM/shell tools present and a `require_tpm` LUKS+LVM config so BOTH the
    // seal's close-mapper path AND the refuse terminus's relock+sentinel path
    // are reachable, then assert BOTH the real-seal-op AND the new
    // real-terminus-op counters stayed at zero across ALL FOUR scenarios.
    let root = temp_closure("require-tpm-no-real-ops", |d| {
        fs::create_dir_all(d.join("bin")).expect("mkdir bin");
        fs::write(d.join("bin/cryptsetup"), b"#!/bin/sh\n").expect("write cryptsetup");
        fs::write(d.join("bin/vgchange"), b"#!/bin/sh\n").expect("write vgchange");
        fs::write(d.join("bin/sh"), b"#!/bin/sh\n").expect("write sh");
    });
    let config = config_require_tpm_with_luks_and_lvm("/bin/cryptsetup");

    nmbl_init::policy::reset_real_seal_ops();
    nmbl_init::policy::reset_real_terminus_ops();
    let _report = validate_initrm(&config, None, &root);
    assert_eq!(
        nmbl_init::policy::real_seal_ops(),
        0,
        "require_tpm validate-initrm must perform NO real TPM cap or cryptsetup \
         close across any scenario (the dry-run seal no-ops both)"
    );
    assert_eq!(
        nmbl_init::policy::real_terminus_ops(),
        0,
        "require_tpm validate-initrm must perform NO real refuse-terminus op \
         (NO /boot/nmbl sentinel write, NO cryptsetup close / vgchange relock \
         fork) across any scenario: the terminus routes sentinel through FsOps \
         and relock through ExecOps, both no-op'd by DryRunSys on the dry-run"
    );
    fs::remove_dir_all(&root).ok();
}

#[test]
fn uki_missing_file_is_reported_as_parse_error() {
    // A `--uki` pointing at a nonexistent file must surface a UKI finding
    // (read error → ParseError note) and break cleanliness.
    let root = temp_closure("uki-missing", |d| {
        fs::create_dir_all(d.join("bin")).expect("mkdir bin");
        fs::write(d.join("bin/sh"), b"#!/bin/sh\n").expect("write shell");
    });
    let config = config_with_shell("/bin/sh");
    let bogus_uki = root.join("does-not-exist.efi");

    let report = validate_initrm(&config, Some(&bogus_uki), &root);
    assert!(!report.is_clean(), "unreadable UKI must break cleanliness");
    let rendered = report.render();
    assert!(
        rendered.contains("UKI validation FAILED"),
        "UKI read failure should be reported:\n{rendered}"
    );
    fs::remove_dir_all(&root).ok();
}
