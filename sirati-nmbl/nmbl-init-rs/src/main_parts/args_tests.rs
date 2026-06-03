//! Parse tests for [`super::parse_args_from`] and the early-exit
//! mutual-exclusion rules. Split out of `args.rs` to keep that file
//! within the file-size budget; included via `#[path]` from args.rs.

use super::*;
use std::path::Path;

#[cfg(feature = "stateful")]
#[test]
fn init_state_with_path_parses() {
    let args = parse_args_from(["--init-state", "/some/path"]).expect("--init-state should parse");
    assert_eq!(
        args.init_state_dir.as_deref(),
        Some(Path::new("/some/path"))
    );
    assert!(args.boot_succeeded_dir.is_none());
    assert!(args.validate_config.is_none());
}

#[cfg(feature = "stateful")]
#[test]
fn init_state_equals_form_parses() {
    let args = parse_args_from(["--init-state=/some/path"]).expect("--init-state=… should parse");
    assert_eq!(
        args.init_state_dir.as_deref(),
        Some(Path::new("/some/path"))
    );
}

#[cfg(feature = "stateful")]
#[test]
fn boot_succeeded_with_path_parses() {
    let args =
        parse_args_from(["--boot-succeeded", "/some/path"]).expect("--boot-succeeded should parse");
    assert_eq!(
        args.boot_succeeded_dir.as_deref(),
        Some(Path::new("/some/path"))
    );
    assert!(args.init_state_dir.is_none());
    assert!(args.validate_config.is_none());
}

#[cfg(feature = "stateful")]
#[test]
fn init_state_and_boot_succeeded_are_mutually_exclusive() {
    let err = parse_args_from(["--init-state", "/a", "--boot-succeeded", "/b"])
        .expect_err("both flags at once must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[cfg(feature = "stateful")]
#[test]
fn validate_config_and_init_state_are_mutually_exclusive() {
    let err = parse_args_from(["--validate-config", "/c", "--init-state", "/a"])
        .expect_err("validate-config + init-state must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[cfg(feature = "stateful")]
#[test]
fn init_state_without_argument_errors() {
    let err = parse_args_from(["--init-state"]).expect_err("--init-state without dir must error");
    assert!(err.contains("requires a directory argument"), "{err}");
}

#[cfg(feature = "stateful")]
#[test]
fn boot_succeeded_without_argument_errors() {
    let err =
        parse_args_from(["--boot-succeeded"]).expect_err("--boot-succeeded without dir must error");
    assert!(err.contains("requires a directory argument"), "{err}");
}

#[cfg(not(feature = "stateful"))]
#[test]
fn init_state_without_feature_errors() {
    // The operator built nmbl-init without `stateful` but still
    // passed `--init-state`; we must not silently ignore — that
    // would leave state.bin uninitialised and bricked installers
    // would be invisible at build time.
    let err = parse_args_from(["--init-state", "/a"])
        .expect_err("--init-state without feature must error");
    assert!(err.contains("stateful"), "{err}");
}

#[cfg(not(feature = "stateful"))]
#[test]
fn boot_succeeded_without_feature_errors() {
    let err = parse_args_from(["--boot-succeeded", "/a"])
        .expect_err("--boot-succeeded without feature must error");
    assert!(err.contains("stateful"), "{err}");
}

#[cfg(not(feature = "stateful"))]
#[test]
fn init_state_equals_without_feature_errors() {
    let err = parse_args_from(["--init-state=/a"])
        .expect_err("--init-state=… without feature must error");
    assert!(err.contains("stateful"), "{err}");
}

#[test]
fn unknown_args_are_ignored() {
    // PID 1 has no "usage" target; unknown flags must not abort.
    let args = parse_args_from(["--no-such-flag", "garbage"])
        .expect("unknown flags should be silently dropped");
    assert_eq!(args.config_path, PathBuf::from(DEFAULT_CONFIG_PATH));
    assert!(args.errored_report.is_none());
    assert!(args.validate_config.is_none());
}

#[test]
fn validate_config_parses_in_default_build() {
    let args = parse_args_from(["--validate-config", "/etc/nmbl/config.toml"])
        .expect("--validate-config should parse without stateful feature");
    assert_eq!(
        args.validate_config.as_deref(),
        Some(Path::new("/etc/nmbl/config.toml"))
    );
}

#[test]
fn validate_hardware_parses_both_forms() {
    let a = parse_args_from(["--validate-hardware=/c.toml"]).expect("equals form");
    assert_eq!(a.validate_hardware.as_deref(), Some(Path::new("/c.toml")));
    let b = parse_args_from(["--validate-hardware", "/c.toml"]).expect("space form");
    assert_eq!(b.validate_hardware.as_deref(), Some(Path::new("/c.toml")));
}

#[test]
fn validate_hardware_collects_tool_paths() {
    let a = parse_args_from([
        "--validate-hardware=/c.toml",
        "--tool=cryptsetup:/store/bin/cryptsetup",
    ])
    .expect("tool path should parse");
    assert_eq!(
        a.tools.cryptsetup(),
        Some(PathBuf::from("/store/bin/cryptsetup"))
    );
}

#[test]
fn bad_tool_spec_errors() {
    let err = parse_args_from(["--tool=cryptsetup"]).expect_err("missing ':' must error");
    assert!(err.contains("<kind>:<path>"), "{err}");
}

#[test]
fn validate_closure_requires_config_toml() {
    let err = parse_args_from(["--validate-nix-filesystem-closure=/fs.json"])
        .expect_err("closure without --config-toml must error");
    assert!(err.contains("--config-toml"), "{err}");
}

#[test]
fn validate_closure_parses_with_config_toml() {
    let a = parse_args_from([
        "--validate-nix-filesystem-closure=/fs.json",
        "--config-toml=/c.toml",
    ])
    .expect("closure + config-toml should parse");
    assert_eq!(a.validate_closure.as_deref(), Some(Path::new("/fs.json")));
    assert_eq!(a.config_toml.as_deref(), Some(Path::new("/c.toml")));
}

#[test]
fn validate_initrm_parses_both_forms() {
    let a = parse_args_from(["--validate-initrm=/c.toml"]).expect("equals form");
    assert_eq!(a.validate_initrm.as_deref(), Some(Path::new("/c.toml")));
    assert!(a.uki.is_none());
    assert!(a.initrm_closure.is_none());
    let b = parse_args_from(["--validate-initrm", "/c.toml"]).expect("space form");
    assert_eq!(b.validate_initrm.as_deref(), Some(Path::new("/c.toml")));
}

#[test]
fn validate_initrm_with_uki_and_closure_parse_together() {
    let a = parse_args_from([
        "--validate-initrm=/c.toml",
        "--uki=/EFI/BOOT/BOOTX64.EFI",
        "--initrm-closure=/extracted/initrd",
    ])
    .expect("initrm + uki + closure should parse");
    assert_eq!(a.validate_initrm.as_deref(), Some(Path::new("/c.toml")));
    assert_eq!(a.uki.as_deref(), Some(Path::new("/EFI/BOOT/BOOTX64.EFI")));
    assert_eq!(
        a.initrm_closure.as_deref(),
        Some(Path::new("/extracted/initrd"))
    );
}

#[test]
fn validate_initrm_modifiers_alone_are_not_early_exit_modes() {
    // `--uki` / `--initrm-closure` are modifiers, not early-exit
    // modes, so combining them with `--validate-config` must NOT trip
    // mutual exclusion (only the initrm mode itself would).
    let a = parse_args_from([
        "--validate-config=/c.toml",
        "--uki=/u.efi",
        "--initrm-closure=/root",
    ])
    .expect("config + uki/closure modifiers should parse");
    assert_eq!(a.validate_config.as_deref(), Some(Path::new("/c.toml")));
    assert_eq!(a.uki.as_deref(), Some(Path::new("/u.efi")));
}

#[test]
fn validate_initrm_and_config_are_mutually_exclusive() {
    let err = parse_args_from(["--validate-initrm=/a", "--validate-config=/b"])
        .expect_err("initrm + config at once must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[test]
fn validate_initrm_and_hardware_are_mutually_exclusive() {
    let err = parse_args_from(["--validate-initrm=/a", "--validate-hardware=/b"])
        .expect_err("initrm + hardware at once must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[cfg(feature = "staged-boot")]
#[test]
fn validate_config_fragment_parses_both_forms() {
    let a =
        parse_args_from(["--validate-config-fragment=/f.toml"]).expect("equals form should parse");
    assert_eq!(a.validate_fragment.as_deref(), Some(Path::new("/f.toml")));
    let b = parse_args_from(["--validate-config-fragment", "/f.toml"])
        .expect("space form should parse");
    assert_eq!(b.validate_fragment.as_deref(), Some(Path::new("/f.toml")));
}

#[cfg(feature = "staged-boot")]
#[test]
fn validate_config_fragment_and_config_are_mutually_exclusive() {
    let err = parse_args_from(["--validate-config=/a", "--validate-config-fragment=/b"])
        .expect_err("config + fragment at once must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[cfg(not(feature = "staged-boot"))]
#[test]
fn validate_config_fragment_without_feature_errors() {
    let err = parse_args_from(["--validate-config-fragment=/a"])
        .expect_err("fragment flag without feature must error");
    assert!(err.contains("staged-boot"), "{err}");
}

#[test]
fn validate_hardware_and_config_are_mutually_exclusive() {
    let err = parse_args_from(["--validate-config=/a", "--validate-hardware=/b"])
        .expect_err("two validate modes at once must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[test]
fn print_gen_id_parses_both_forms() {
    let a = parse_args_from(["--print-gen-id=/nix/store/top"]).expect("equals form");
    assert_eq!(a.print_gen_id.as_deref(), Some(Path::new("/nix/store/top")));
    let b = parse_args_from(["--print-gen-id", "/nix/store/top"]).expect("space form");
    assert_eq!(b.print_gen_id.as_deref(), Some(Path::new("/nix/store/top")));
}

#[test]
fn print_gen_id_is_mutually_exclusive_with_validate_config() {
    let err = parse_args_from(["--print-gen-id=/a", "--validate-config=/b"])
        .expect_err("print-gen-id + validate-config at once must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}

#[test]
fn validate_hardware_and_closure_are_mutually_exclusive() {
    let err = parse_args_from([
        "--validate-hardware=/a",
        "--validate-nix-filesystem-closure=/b",
        "--config-toml=/c",
    ])
    .expect_err("hardware + closure at once must be rejected");
    assert!(err.contains("mutually exclusive"), "{err}");
}
