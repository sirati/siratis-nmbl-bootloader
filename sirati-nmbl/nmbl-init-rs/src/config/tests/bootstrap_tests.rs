#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use std::path::{Path, PathBuf};

use crate::config::bootstrap::{
    BootstrapBootFs, BootstrapConfig, BootstrapKernelModules, BootstrapRescue, BootstrapSection,
    default_bootstrap_config_path, resolve_full_config_path,
};
use crate::error::NmblError;

#[test]
fn bootstrap_parses_full_schema() {
    let toml = r#"
[bootstrap]
config_path = "/nmbl/config.toml"

[bootstrap.boot_fs]
device     = "/dev/disk/by-partlabel/disk-main-ESP"
fstype     = "vfat"
options    = "ro"
mountpoint = "/mnt/boot"

[bootstrap.kernel_modules]
explicit = ["vfat", "nls_cp437", "nls_iso8859_1", "ahci", "nvme"]

[bootstrap.rescue]
default_url    = "https://example.invalid/rescue.cpio"
default_sha256 = "deadbeef"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("full schema must parse");
    assert_eq!(
        cfg.bootstrap.config_path,
        PathBuf::from("/nmbl/config.toml")
    );
    assert_eq!(
        cfg.bootstrap.boot_fs.device,
        "/dev/disk/by-partlabel/disk-main-ESP",
    );
    assert_eq!(cfg.bootstrap.boot_fs.fstype, "vfat");
    assert_eq!(cfg.bootstrap.boot_fs.options, "ro");
    assert_eq!(cfg.bootstrap.boot_fs.mountpoint, PathBuf::from("/mnt/boot"));
    assert_eq!(
        cfg.bootstrap.kernel_modules.explicit,
        vec!["vfat", "nls_cp437", "nls_iso8859_1", "ahci", "nvme"],
    );
    assert_eq!(
        cfg.bootstrap.rescue.default_url,
        "https://example.invalid/rescue.cpio",
    );
    assert_eq!(cfg.bootstrap.rescue.default_sha256, "deadbeef");
}

#[test]
fn bootstrap_parses_minimal_with_defaults() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("minimal schema must parse");
    assert_eq!(
        cfg.bootstrap.config_path,
        PathBuf::from("/nmbl/config.toml")
    );
    assert_eq!(cfg.bootstrap.boot_fs.options, "");
    assert!(cfg.bootstrap.kernel_modules.explicit.is_empty());
    // Default mirrors `KernelModules::modules_dir` so the bootstrap
    // and full-config stages agree on where `modules.dep` lives.
    assert_eq!(
        cfg.bootstrap.kernel_modules.modules_dir,
        PathBuf::from("/lib/modules"),
    );
    assert_eq!(cfg.bootstrap.rescue.default_url, "");
    assert_eq!(cfg.bootstrap.rescue.default_sha256, "");
}

#[test]
fn bootstrap_kernel_modules_dir_override_parses() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.kernel_modules]
explicit    = ["vfat"]
modules_dir = "/run/custom/modules"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("override schema must parse");
    assert_eq!(
        cfg.bootstrap.kernel_modules.modules_dir,
        PathBuf::from("/run/custom/modules"),
    );
    assert_eq!(cfg.bootstrap.kernel_modules.explicit, vec!["vfat"]);
}

#[test]
fn bootstrap_rejects_unknown_top_level_field() {
    let toml = r#"
[bootstrap]
config_path = "/nmbl/config.toml"
mystery     = "nope"

[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
    let err = toml::from_str::<BootstrapConfig>(toml)
        .expect_err("unknown field in [bootstrap] must be rejected");
    assert!(
        err.to_string().contains("mystery"),
        "error should mention the unknown field, got: {err}",
    );
}

#[test]
fn bootstrap_rejects_unknown_boot_fs_field() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
secret     = "boom"
"#;
    toml::from_str::<BootstrapConfig>(toml).expect_err("unknown field in boot_fs must be rejected");
}

#[test]
fn bootstrap_rejects_missing_device() {
    let toml = r#"
[bootstrap.boot_fs]
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
    let err = toml::from_str::<BootstrapConfig>(toml)
        .expect_err("missing boot_fs.device must be rejected");
    assert!(
        err.to_string().contains("device"),
        "error should mention the missing field, got: {err}",
    );
}

#[test]
fn bootstrap_rejects_missing_boot_fs_section() {
    let toml = r#"
[bootstrap]
config_path = "/nmbl/config.toml"
"#;
    toml::from_str::<BootstrapConfig>(toml).expect_err("missing boot_fs section must be rejected");
}

#[test]
fn bootstrap_rescue_section_optional() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("rescue must be optional");
    assert_eq!(cfg.bootstrap.rescue.default_url, "");
    assert_eq!(cfg.bootstrap.rescue.default_sha256, "");
}

#[test]
fn bootstrap_state_section_absent_decodes_to_none() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("state must be optional");
    assert!(cfg.bootstrap.state.is_none());
}

#[test]
fn bootstrap_state_section_present_parses_mountpoint() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.state]
mountpoint = "/mnt/boot-state"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("state must parse");
    let state = cfg.bootstrap.state.expect("state should be Some");
    assert_eq!(state.mountpoint, PathBuf::from("/mnt/boot-state"));
}

#[test]
fn bootstrap_state_rejects_unknown_field() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.state]
mountpoint  = "/mnt/boot-state"
extra_field = "x"
"#;
    let err = toml::from_str::<BootstrapConfig>(toml)
        .expect_err("unknown field in [bootstrap.state] must be rejected");
    assert!(
        err.to_string().contains("extra_field"),
        "error should mention the unknown field, got: {err}",
    );
}

#[test]
fn bootstrap_state_rejects_missing_mountpoint() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.state]
"#;
    let err = toml::from_str::<BootstrapConfig>(toml)
        .expect_err("missing bootstrap.state.mountpoint must be rejected");
    assert!(
        err.to_string().contains("mountpoint"),
        "error should mention the missing field, got: {err}",
    );
}

#[test]
fn bootstrap_load_reads_from_disk() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bootstrap.toml");
    std::fs::write(
        &path,
        r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#,
    )
    .expect("write bootstrap toml");

    let cfg = BootstrapConfig::load(&path).expect("load must succeed");
    assert_eq!(cfg.bootstrap.boot_fs.device, "/dev/sda1");
}

#[test]
fn bootstrap_load_missing_file_is_bootstrap_load_toml_error() {
    use std::error::Error;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("nope.toml");
    let err = BootstrapConfig::load(&path).expect_err("missing file must error");
    match &err {
        NmblError::Bootstrap { stage, source } => {
            assert_eq!(*stage, "load-toml", "stage should mark the failed step");
            assert!(
                matches!(source.as_ref(), NmblError::Io { .. }),
                "Bootstrap should wrap an Io error, got: {source:?}",
            );
        }
        other => panic!("expected Bootstrap variant, got: {other:?}"),
    }
    // The chained source must reach the inner Io variant so the
    // emergency-shell banner's chain walker keeps working.
    let inner = Error::source(&err).expect("Bootstrap must expose a source");
    assert!(
        inner.to_string().contains("reading bootstrap config"),
        "inner source should describe the read step, got: {inner}",
    );
}

#[test]
fn bootstrap_load_parse_failure_is_bootstrap_parse_toml_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bad.toml");
    std::fs::write(&path, "this is = not valid = toml").expect("write");
    let err = BootstrapConfig::load(&path).expect_err("bad toml must error");
    match &err {
        NmblError::Bootstrap { stage, source } => {
            assert_eq!(*stage, "parse-toml", "stage should mark the failed step");
            assert!(
                matches!(source.as_ref(), NmblError::Config { .. }),
                "Bootstrap should wrap a Config error, got: {source:?}",
            );
        }
        other => panic!("expected Bootstrap variant, got: {other:?}"),
    }
}

#[test]
fn bootstrap_validate_rejects_url_without_sha() {
    let cfg = BootstrapConfig {
        bootstrap: BootstrapSection {
            config_path: default_bootstrap_config_path(),
            boot_fs: BootstrapBootFs {
                device: "/dev/sda1".to_string(),
                fstype: "vfat".to_string(),
                options: String::new(),
                mountpoint: PathBuf::from("/mnt/boot"),
            },
            kernel_modules: BootstrapKernelModules::default(),
            rescue: BootstrapRescue {
                default_url: "https://example.invalid/rescue.cpio".to_string(),
                default_sha256: String::new(),
            },
            state: None,
            #[cfg(feature = "staged-boot")]
            staged: None,
        },
    };
    let err = cfg.validate().expect_err("url without sha must reject");
    match err {
        NmblError::ConfigInvalid { reason, .. } => {
            assert!(reason.contains("default_url"), "{reason}");
            assert!(reason.contains("default_sha256"), "{reason}");
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }
}

#[test]
fn bootstrap_validate_rejects_sha_without_url() {
    let cfg = BootstrapConfig {
        bootstrap: BootstrapSection {
            config_path: default_bootstrap_config_path(),
            boot_fs: BootstrapBootFs {
                device: "/dev/sda1".to_string(),
                fstype: "vfat".to_string(),
                options: String::new(),
                mountpoint: PathBuf::from("/mnt/boot"),
            },
            kernel_modules: BootstrapKernelModules::default(),
            rescue: BootstrapRescue {
                default_url: String::new(),
                default_sha256: "deadbeef".to_string(),
            },
            state: None,
            #[cfg(feature = "staged-boot")]
            staged: None,
        },
    };
    cfg.validate().expect_err("sha without url must reject");
}

#[test]
fn resolve_full_config_path_strips_leading_slash() {
    let mp = Path::new("/mnt/boot");
    let cp = Path::new("/nmbl/config.toml");
    assert_eq!(
        resolve_full_config_path(mp, cp),
        PathBuf::from("/mnt/boot/nmbl/config.toml"),
    );
}

#[test]
fn resolve_full_config_path_joins_relative_path() {
    let mp = Path::new("/mnt/boot");
    let cp = Path::new("nmbl/config.toml");
    assert_eq!(
        resolve_full_config_path(mp, cp),
        PathBuf::from("/mnt/boot/nmbl/config.toml"),
    );
}

#[test]
fn resolve_full_config_path_handles_nested_mountpoint() {
    let mp = Path::new("/run/nmbl/boot");
    let cp = Path::new("/nmbl/config.toml");
    assert_eq!(
        resolve_full_config_path(mp, cp),
        PathBuf::from("/run/nmbl/boot/nmbl/config.toml"),
    );
}

#[test]
fn bootstrap_validate_accepts_both_empty_and_both_set() {
    let mk = |url: &str, sha: &str| BootstrapConfig {
        bootstrap: BootstrapSection {
            config_path: default_bootstrap_config_path(),
            boot_fs: BootstrapBootFs {
                device: "/dev/sda1".to_string(),
                fstype: "vfat".to_string(),
                options: String::new(),
                mountpoint: PathBuf::from("/mnt/boot"),
            },
            kernel_modules: BootstrapKernelModules::default(),
            rescue: BootstrapRescue {
                default_url: url.to_string(),
                default_sha256: sha.to_string(),
            },
            state: None,
            #[cfg(feature = "staged-boot")]
            staged: None,
        },
    };
    mk("", "").validate().expect("both empty must pass");
    mk("https://example.invalid/r.cpio", "deadbeef")
        .validate()
        .expect("both set must pass");
}

#[cfg(feature = "staged-boot")]
#[test]
fn bootstrap_staged_section_absent_decodes_to_none() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("staged must be optional");
    assert!(cfg.bootstrap.staged.is_none());
}

#[cfg(feature = "staged-boot")]
#[test]
fn bootstrap_staged_section_present_parses_fields() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.staged]
mountpoint = "/mnt/staged"
fragment   = "nmbl/fragment.toml"
sig        = "nmbl/fragment.toml.sig"
"#;
    let cfg: BootstrapConfig = toml::from_str(toml).expect("staged must parse");
    let staged = cfg.bootstrap.staged.expect("staged should be Some");
    assert_eq!(staged.mountpoint, PathBuf::from("/mnt/staged"));
    assert_eq!(staged.fragment, PathBuf::from("nmbl/fragment.toml"));
    assert_eq!(staged.sig, PathBuf::from("nmbl/fragment.toml.sig"));
}

#[cfg(feature = "staged-boot")]
#[test]
fn bootstrap_staged_rejects_unknown_field() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.staged]
mountpoint = "/mnt/staged"
fragment   = "nmbl/fragment.toml"
sig        = "nmbl/fragment.toml.sig"
extra      = "x"
"#;
    let err = toml::from_str::<BootstrapConfig>(toml)
        .expect_err("unknown field in [bootstrap.staged] must be rejected");
    assert!(err.to_string().contains("extra"), "{err}");
}

#[cfg(feature = "staged-boot")]
#[test]
fn bootstrap_staged_rejects_missing_required_field() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.staged]
mountpoint = "/mnt/staged"
"#;
    toml::from_str::<BootstrapConfig>(toml)
        .expect_err("missing bootstrap.staged.fragment/sig must reject");
}

// F1 NEGATIVE: a binary built WITHOUT `staged-boot` `#[cfg]`s the `staged`
// field off `BootstrapSection`, so a `[bootstrap.staged]` table is unknown
// and `deny_unknown_fields` must reject it (FIX-40).
#[cfg(not(feature = "staged-boot"))]
#[test]
fn bootstrap_staged_section_rejected_without_secure_boot_feature() {
    let toml = r#"
[bootstrap.boot_fs]
device     = "/dev/sda1"
fstype     = "vfat"
mountpoint = "/mnt/boot"

[bootstrap.staged]
mountpoint = "/mnt/staged"
fragment   = "nmbl/fragment.toml"
sig        = "nmbl/fragment.toml.sig"
"#;
    let err = toml::from_str::<BootstrapConfig>(toml)
        .expect_err("a non-staged-boot binary must reject [bootstrap.staged]");
    let msg = err.to_string();
    assert!(
        msg.contains("staged") || msg.contains("unknown"),
        "rejection should mention the unknown staged table, got: {msg}",
    );
}
