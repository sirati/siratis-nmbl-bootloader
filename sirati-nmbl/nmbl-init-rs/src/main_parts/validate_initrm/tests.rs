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
