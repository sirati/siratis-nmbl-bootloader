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
