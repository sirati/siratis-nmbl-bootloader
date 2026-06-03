#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use std::path::PathBuf;

use crate::config::Config;
#[cfg(feature = "image-splash")]
use crate::config::SplashBackgroundLocation;
use crate::config::entries::FilesystemEntry;
use crate::config::rescue_cfg::RescueConfig;
use crate::error::NmblError;
use crate::rescue::RescueMode;

fn fs_entry(device: &str, mountpoint: &str) -> FilesystemEntry {
    FilesystemEntry {
        device: device.to_string(),
        mountpoint: PathBuf::from(mountpoint),
        fstype: "ext4".to_string(),
        options: String::new(),
        is_root: false,
    }
}

fn config_with(entries: Vec<FilesystemEntry>) -> Config {
    let mut c = Config::recovery_default();
    c.filesystems = entries;
    c
}

#[test]
fn validate_accepts_dev_disk_by_label_paths() {
    // After the udev-less symlink populator landed, by-* paths
    // are valid `fileSystems[].device` strings.
    let c = config_with(vec![
        fs_entry("/dev/disk/by-label/boot", "/boot"),
        fs_entry("/dev/disk/by-partlabel/disk-main-ESP", "/boot"),
        fs_entry("/dev/disk/by-uuid/1234-ABCD", "/"),
        fs_entry("/dev/disk/by-partuuid/abcdef01-1234", "/data"),
    ]);
    c.validate().expect("by-* paths must validate");
}

#[test]
fn validate_accepts_raw_dev_paths() {
    let c = config_with(vec![fs_entry("/dev/vda1", "/boot")]);
    c.validate().expect("raw /dev/* paths must validate");
}

#[test]
fn validate_still_rejects_label_short_form() {
    let c = config_with(vec![fs_entry("LABEL=boot", "/boot")]);
    let err = c
        .validate()
        .expect_err("LABEL= short form must be rejected");
    match err {
        NmblError::ConfigInvalid { reason, .. } => {
            assert!(
                reason.contains("LABEL=") || reason.contains("short form"),
                "rejection message should mention LABEL=/short form, got: {reason}",
            );
            assert!(
                reason.contains("/dev/disk/by-"),
                "rejection should point at the by-* symlink form, got: {reason}",
            );
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }
}

#[test]
fn validate_still_rejects_uuid_short_form() {
    let c = config_with(vec![fs_entry("UUID=1234-ABCD", "/")]);
    c.validate().expect_err("UUID= short form must be rejected");
}

#[test]
fn validate_still_rejects_partuuid_short_form() {
    let c = config_with(vec![fs_entry("PARTUUID=abc-123", "/data")]);
    c.validate()
        .expect_err("PARTUUID= short form must be rejected");
}

#[test]
fn device_timeout_secs_defaults_to_thirty_when_absent() {
    // External TOMLs predating the knob must keep parsing cleanly
    // and observe the historic 30 s budget so the boot UX doesn't
    // silently regress on upgrade.
    let toml_text = "[general]\ntimeout_ms = 3000\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert_eq!(config.general.device_timeout_secs, 30);
}

#[test]
fn device_timeout_secs_is_honoured_when_present() {
    let toml_text = "[general]\ndevice_timeout_secs = 90\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert_eq!(config.general.device_timeout_secs, 90);
}

#[test]
fn timeout_ms_defaults_to_builtin_when_absent() {
    // Configs that omit the knob fall back to the built-in default.
    let toml_text = "[general]\nverbosity = \"info\"\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert_eq!(
        config.general.timeout_ms,
        crate::config::general::default_timeout_ms()
    );
}

#[test]
fn timeout_ms_is_honoured_when_present() {
    let toml_text = "[general]\ntimeout_ms = 500\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert_eq!(config.general.timeout_ms, 500);
}

#[test]
fn emergency_timeout_secs_defaults_to_none_when_absent() {
    // Absent → the Rust-side 30 s default applies; existing TOMLs
    // must not observe a behaviour change on upgrade.
    let toml_text = "[general]\ntimeout_ms = 3000\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert_eq!(config.general.emergency_timeout_secs, None);
}

#[test]
fn emergency_timeout_secs_is_honoured_when_present() {
    let toml_text = "[general]\nemergency_timeout_secs = 1\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert_eq!(config.general.emergency_timeout_secs, Some(1));
}

#[cfg(feature = "image-splash")]
#[test]
fn config_parses_without_splash_table() {
    // A config that doesn't mention [splash] at all must still parse,
    // because the feature defaults to off and existing on-disk configs
    // predate the new table.
    let toml_text = "[general]\ntimeout_ms = 3000\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert!(!config.splash.enable, "splash must default to disabled");
    assert_eq!(
        config.splash.background_image,
        PathBuf::from("/etc/splash/image.png"),
    );
    assert_eq!(
        config.splash.font_path,
        PathBuf::from("/etc/splash/font.ttf"),
    );
    assert_eq!(config.splash.dri_path, PathBuf::from("/dev/dri/card0"));
}

#[cfg(feature = "image-splash")]
#[test]
fn config_parses_with_splash_table() {
    let toml_text = "[splash]\nenable = true\nbackground_image = \"/foo.png\"\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert!(config.splash.enable, "enable = true must round-trip");
    assert_eq!(config.splash.background_image, PathBuf::from("/foo.png"));
    // Unset fields still pick up their defaults.
    assert_eq!(
        config.splash.font_path,
        PathBuf::from("/etc/splash/font.ttf"),
    );
    assert_eq!(config.splash.dri_path, PathBuf::from("/dev/dri/card0"));
}

#[cfg(feature = "image-splash")]
#[test]
fn splash_background_location_defaults_to_initrd_when_absent() {
    // Configs predating the sidecar knob must keep parsing and
    // observe the embedded (initrd) behaviour so the boot UX does
    // not silently change on upgrade.
    let toml_text = "[splash]\nenable = true\n";
    let config: Config = toml::from_str(toml_text).expect("config must parse");
    assert_eq!(
        config.splash.background_location,
        SplashBackgroundLocation::Initrd,
    );
}

#[cfg(feature = "image-splash")]
#[test]
fn splash_background_location_parses_both_modes() {
    for (raw, expected) in [
        ("initrd", SplashBackgroundLocation::Initrd),
        ("boot-partition", SplashBackgroundLocation::BootPartition),
    ] {
        let toml_text = format!("[splash]\nbackground_location = \"{raw}\"\n");
        let config: Config = toml::from_str(&toml_text).expect("mode value must parse");
        assert_eq!(config.splash.background_location, expected, "mode={raw}");
    }
}

#[cfg(feature = "image-splash")]
#[test]
fn splash_background_location_rejects_unknown_value() {
    let toml_text = "[splash]\nbackground_location = \"sd-card\"\n";
    toml::from_str::<Config>(toml_text).expect_err("unknown location value must reject");
}

#[cfg(feature = "image-splash")]
#[test]
fn config_rejects_unknown_splash_field() {
    let toml_text = "[splash]\nfoo = 1\n";
    let err =
        toml::from_str::<Config>(toml_text).expect_err("unknown splash field must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("foo") || msg.contains("unknown"),
        "rejection should mention the unknown field, got: {msg}",
    );
}

#[test]
fn rescue_section_defaults_when_absent() {
    // Empty config — every section must default. The rescue section
    // is `#[serde(default)]` so absence is the operator's signal
    // that they want the legacy embedded shell behaviour.
    let cfg: Config = toml::from_str("").expect("missing rescue section must default");
    assert_eq!(cfg.rescue.mode, RescueMode::default());
    assert!(cfg.rescue.sfs_path.is_none());
}

#[test]
fn rescue_section_parses_all_three_modes() {
    for (raw, expected) in [
        ("embedded", RescueMode::Embedded),
        ("external", RescueMode::External),
        ("none", RescueMode::None),
    ] {
        let toml = format!(
            r#"
[rescue]
mode = "{raw}"
"#
        );
        let cfg: Config = toml::from_str(&toml).expect("mode value must parse");
        assert_eq!(cfg.rescue.mode, expected, "mode={raw}");
    }
}

#[test]
fn rescue_section_parses_sfs_path_override() {
    let toml = r#"
[rescue]
mode     = "external"
sfs_path = "/mnt/boot/nmbl-rescue.sfs"
"#;
    let cfg: Config = toml::from_str(toml).expect("override must parse");
    assert_eq!(cfg.rescue.mode, RescueMode::External);
    assert_eq!(
        cfg.rescue.sfs_path,
        Some(PathBuf::from("/mnt/boot/nmbl-rescue.sfs")),
    );
}

#[test]
fn rescue_section_rejects_unknown_field() {
    let toml = r#"
[rescue]
mode    = "external"
mystery = "boom"
"#;
    toml::from_str::<Config>(toml).expect_err("unknown field must reject");
}

#[test]
fn emergency_shell_defaults_to_empty_extra_consoles() {
    // The default — no opt-in — must pin the picker to /dev/console
    // only. Adding extra_consoles is an explicit operator action,
    // not a side effect of upgrading the config schema.
    let cfg: Config = toml::from_str("").expect("missing emergency_shell must default");
    assert!(cfg.emergency_shell.extra_consoles.is_empty());
}

#[test]
fn emergency_shell_parses_extra_consoles_list() {
    let toml = r#"
[emergency_shell]
extra_consoles = ["/dev/ttyS0", "/dev/tty1"]
"#;
    let cfg: Config = toml::from_str(toml).expect("extra_consoles list must parse");
    assert_eq!(
        cfg.emergency_shell.extra_consoles,
        vec!["/dev/ttyS0".to_string(), "/dev/tty1".to_string()],
    );
}

#[test]
fn emergency_shell_rejects_unknown_field() {
    let toml = r#"
[emergency_shell]
extra_consoles = []
mystery        = "boom"
"#;
    toml::from_str::<Config>(toml).expect_err("unknown emergency_shell field must be rejected");
}

#[test]
fn rescue_default_mode_is_embedded() {
    let cfg = RescueConfig::default();
    assert_eq!(cfg.mode, RescueMode::Embedded);
    assert!(cfg.sfs_path.is_none());
}

#[test]
fn rescue_entrypoint_defaults_to_bin_sh_when_absent() {
    // Flat busybox image (and every legacy config) leaves the
    // entrypoint unset; the loader must fall back to /bin/sh.
    let cfg: Config = toml::from_str("[rescue]\nmode = \"external\"\n")
        .expect("config without entrypoint must parse");
    assert_eq!(cfg.rescue.entrypoint, PathBuf::from("/bin/sh"));
    assert_eq!(RescueConfig::default().entrypoint, PathBuf::from("/bin/sh"));
}

#[test]
fn rescue_entrypoint_parses_init_override() {
    // The full recovery system pins /init; config-toml.nix emits it
    // only when fullSystem.enable is set.
    let toml = r#"
[rescue]
mode       = "external"
entrypoint = "/init"
"#;
    let cfg: Config = toml::from_str(toml).expect("entrypoint override must parse");
    assert_eq!(cfg.rescue.entrypoint, PathBuf::from("/init"));
}

#[test]
fn rescue_force_on_boot_defaults_false_and_parses() {
    // Absent → production-safe false; present → honoured. The test
    // harness flips this to drive a deterministic rescue boot.
    let cfg: Config = toml::from_str("[rescue]\nmode = \"external\"\n")
        .expect("config without force_on_boot must parse");
    assert!(!cfg.rescue.force_on_boot);
    assert!(!RescueConfig::default().force_on_boot);

    let toml = r#"
[rescue]
mode          = "external"
force_on_boot = true
"#;
    let cfg: Config = toml::from_str(toml).expect("force_on_boot override must parse");
    assert!(cfg.rescue.force_on_boot);
}

#[test]
fn recovery_default_has_no_runtime_boot_mountpoint() {
    // Legacy embedded-config mode never mounts a boot partition, so
    // the recovery-default Config must report None for the runtime
    // mountpoint. `rescue::locate_sfs` keys off this to surface a
    // clear diagnostic instead of fabricating a path.
    let cfg = Config::recovery_default();
    assert!(cfg.runtime_boot_mountpoint.is_none());
}

#[test]
fn runtime_boot_mountpoint_is_not_parsed_from_toml() {
    // The field is `#[serde(skip)]` so even if the operator's TOML
    // contains a stray top-level `runtime_boot_mountpoint = "…"` it
    // must be rejected as an unknown field by `deny_unknown_fields`.
    let toml = r#"runtime_boot_mountpoint = "/mnt/boot""#;
    toml::from_str::<Config>(toml).expect_err("runtime_boot_mountpoint is runtime-only");
}

#[cfg(feature = "stateful")]
#[test]
fn stateful_section_absent_decodes_to_none() {
    // Configs that predate the stateful knob must still parse and
    // produce `stateful = None`; the rollback flow only engages when
    // the operator opts in.
    let toml = "[general]\ntimeout_ms = 3000\n";
    let cfg: Config = toml::from_str(toml).expect("config must parse");
    assert!(cfg.stateful.is_none());
}

#[cfg(feature = "stateful")]
#[test]
fn stateful_section_present_parses_required_fields() {
    let toml = r#"
[stateful]
max_recovery_attempts = 5
success_target        = "multi-user.target"
"#;
    let cfg: Config = toml::from_str(toml).expect("[stateful] must parse");
    let s = cfg.stateful.expect("stateful should be Some");
    assert_eq!(s.max_recovery_attempts, 5);
    assert_eq!(s.success_target, "multi-user.target");
}

#[cfg(feature = "stateful")]
#[test]
fn stateful_section_rejects_unknown_field() {
    let toml = r#"
[stateful]
max_recovery_attempts = 5
success_target        = "multi-user.target"
mystery               = "boom"
"#;
    let err =
        toml::from_str::<Config>(toml).expect_err("unknown field in [stateful] must be rejected");
    assert!(err.to_string().contains("mystery"), "{err}");
}

#[cfg(feature = "stateful")]
#[test]
fn stateful_section_requires_max_recovery_attempts() {
    let toml = r#"
[stateful]
success_target = "multi-user.target"
"#;
    toml::from_str::<Config>(toml).expect_err("missing max_recovery_attempts must reject");
}

#[test]
fn tpm_section_absent_uses_defaults() {
    // Configs predating the [tpm] knob must parse and observe the
    // permissive default posture (no measure, no require) and the
    // single-sourced lock PCR.
    let cfg: Config = toml::from_str("[general]\ntimeout_ms = 3000\n").expect("config must parse");
    assert!(!cfg.tpm.measure);
    assert!(!cfg.tpm.require_tpm);
    assert_eq!(cfg.tpm.pcr_index, crate::security_consts::LOCK_PCR);
    assert_eq!(cfg.tpm.device, PathBuf::from("/dev/tpmrm0"));
    assert!(cfg.tpm.sealed_secrets.is_empty());
}

#[test]
fn tpm_section_round_trips_all_fields() {
    let toml = r#"
[tpm]
measure     = true
pcr_index   = 7
require_tpm = true
device      = "/dev/tpm0"

[[tpm.sealed_secrets]]
name        = "luks-key"
sealed_path = "nmbl/luks.sealed"
unseal_to   = "/run/nmbl/luks.key"
"#;
    let cfg: Config = toml::from_str(toml).expect("[tpm] must parse");
    assert!(cfg.tpm.measure);
    assert!(cfg.tpm.require_tpm);
    assert_eq!(cfg.tpm.pcr_index, 7);
    assert_eq!(cfg.tpm.device, PathBuf::from("/dev/tpm0"));
    assert_eq!(cfg.tpm.sealed_secrets.len(), 1);
    let s = cfg
        .tpm
        .sealed_secrets
        .first()
        .expect("one sealed secret was parsed");
    assert_eq!(s.name, "luks-key");
    assert_eq!(s.sealed_path, PathBuf::from("nmbl/luks.sealed"));
    assert_eq!(s.unseal_to, PathBuf::from("/run/nmbl/luks.key"));
}

#[test]
fn tpm_section_rejects_unknown_field() {
    let toml = r#"
[tpm]
measure = true
mystery = "boom"
"#;
    let err = toml::from_str::<Config>(toml).expect_err("unknown field in [tpm] must be rejected");
    assert!(err.to_string().contains("mystery"), "{err}");
}

#[test]
fn tpm_sealed_secret_rejects_unknown_field() {
    let toml = r#"
[[tpm.sealed_secrets]]
name        = "k"
sealed_path = "a"
unseal_to   = "b"
mystery     = "boom"
"#;
    let err = toml::from_str::<Config>(toml)
        .expect_err("unknown field in [[tpm.sealed_secrets]] must be rejected");
    assert!(err.to_string().contains("mystery"), "{err}");
}

#[cfg(feature = "staged-boot")]
#[test]
fn staged_section_absent_decodes_to_none() {
    // Configs that predate the staged knob (and staged-feature builds
    // whose Nix config did not enable staging) must still parse and
    // produce `staged = None` — the staged path engages only on opt-in.
    let toml = "[general]\ntimeout_ms = 3000\n";
    let cfg: Config = toml::from_str(toml).expect("config must parse");
    assert!(cfg.staged.is_none());
}

#[cfg(feature = "staged-boot")]
#[test]
fn staged_section_present_parses_required_fields() {
    let toml = r#"
[staged]
enable   = true
image    = "nmbl-staged.img"
fragment = "nmbl/fragment.toml"
sig      = "nmbl/fragment.toml.sig"
"#;
    let cfg: Config = toml::from_str(toml).expect("[staged] must parse");
    let s = cfg.staged.expect("staged should be Some");
    assert!(s.enable);
    assert_eq!(s.image, PathBuf::from("nmbl-staged.img"));
    assert_eq!(s.fragment, PathBuf::from("nmbl/fragment.toml"));
    assert_eq!(s.sig, PathBuf::from("nmbl/fragment.toml.sig"));
}

#[cfg(feature = "staged-boot")]
#[test]
fn staged_section_rejects_unknown_field() {
    let toml = r#"
[staged]
image    = "nmbl-staged.img"
fragment = "nmbl/fragment.toml"
sig      = "nmbl/fragment.toml.sig"
mystery  = "boom"
"#;
    let err =
        toml::from_str::<Config>(toml).expect_err("unknown field in [staged] must be rejected");
    assert!(err.to_string().contains("mystery"), "{err}");
}

#[cfg(feature = "staged-boot")]
#[test]
fn staged_section_requires_image_fragment_and_sig() {
    // `image`/`fragment`/`sig` have no serde default — a partial table is
    // a hard parse error, not a silent fill-in.
    let toml = "[staged]\nenable = true\n";
    toml::from_str::<Config>(toml).expect_err("missing required staged fields must reject");
}

// F1 NEGATIVE: the staged slice built WITHOUT secure-boot. A binary
// compiled without `staged-boot` (so without `secure-boot`) `#[cfg]`s the
// `staged` field off `Config`, so a `[staged]` table is an UNKNOWN table
// that `deny_unknown_fields` must reject. This guards FIX-40: a feature-
// free binary can never silently ignore a staged table the Nix side would
// only emit for a staged build.
#[cfg(not(feature = "staged-boot"))]
#[test]
fn staged_section_rejected_without_secure_boot_feature() {
    let toml = r#"
[staged]
enable   = true
image    = "nmbl-staged.img"
fragment = "nmbl/fragment.toml"
sig      = "nmbl/fragment.toml.sig"
"#;
    let err = toml::from_str::<Config>(toml)
        .expect_err("a non-staged-boot binary must reject the [staged] table");
    let msg = err.to_string();
    assert!(
        msg.contains("staged") || msg.contains("unknown"),
        "rejection should mention the unknown staged table, got: {msg}",
    );
}

#[cfg(feature = "secure-boot")]
#[test]
fn secure_boot_section_absent_uses_defaults() {
    // Configs predating the [secure_boot] knob must parse and observe the
    // disabled, audit-neutral default posture plus the single-sourced
    // refuse countdown and sentinel path.
    let cfg: Config = toml::from_str("[general]\ntimeout_ms = 3000\n").expect("config must parse");
    assert!(!cfg.secure_boot.enable);
    assert!(!cfg.secure_boot.enforce);
    assert!(cfg.secure_boot.priority_volume.is_none());
    assert!(cfg.secure_boot.allowed_key_ids.is_empty());
    assert_eq!(
        cfg.secure_boot.refuse_countdown_seconds,
        crate::security_consts::REFUSE_COUNTDOWN_SECONDS
    );
    assert_eq!(
        cfg.secure_boot.sentinel_path,
        PathBuf::from(crate::security_consts::SENTINEL_PATH)
    );
}

#[cfg(feature = "secure-boot")]
#[test]
fn secure_boot_section_round_trips_all_fields() {
    let toml = r#"
[secure_boot]
enable                    = true
signed_file_path          = "nmbl/priority.signed"
allowed_key_ids           = ["abcd", "ef01"]
sentinel_path             = "/boot/nmbl/rescue"
enforce                   = true
require_tpm               = true
refuse_countdown_seconds  = 45
allow_audit_mode_insecure = false

[secure_boot.priority_volume]
device      = "/dev/mapper/cryptpriority"
mountpoint  = "/mnt/nmbl-priority"
fstype      = "ext4"
options     = "ro,nosuid,nodev,noexec"
inside_luks = true
"#;
    let cfg: Config = toml::from_str(toml).expect("[secure_boot] must parse");
    let sb = &cfg.secure_boot;
    assert!(sb.enable);
    assert!(sb.enforce);
    assert!(sb.require_tpm);
    assert_eq!(sb.refuse_countdown_seconds, 45);
    assert_eq!(sb.signed_file_path, PathBuf::from("nmbl/priority.signed"));
    assert_eq!(
        sb.allowed_key_ids,
        vec!["abcd".to_owned(), "ef01".to_owned()]
    );
    assert_eq!(sb.sentinel_path, PathBuf::from("/boot/nmbl/rescue"));
    let pv = sb
        .priority_volume
        .as_ref()
        .expect("priority_volume should be Some");
    assert_eq!(pv.device, PathBuf::from("/dev/mapper/cryptpriority"));
    assert_eq!(pv.mountpoint, PathBuf::from("/mnt/nmbl-priority"));
    assert_eq!(pv.fstype, "ext4");
    assert_eq!(pv.options, "ro,nosuid,nodev,noexec");
    assert!(pv.inside_luks);
}

#[cfg(feature = "secure-boot")]
#[test]
fn secure_boot_priority_volume_options_default() {
    // `options` has a serde default — an omitted key fills the hardened
    // read-only set rather than erroring.
    let toml = r#"
[secure_boot.priority_volume]
device     = "/dev/sda2"
mountpoint = "/mnt/p"
fstype     = "ext4"
"#;
    let cfg: Config = toml::from_str(toml).expect("[secure_boot.priority_volume] must parse");
    let pv = cfg
        .secure_boot
        .priority_volume
        .as_ref()
        .expect("priority_volume should be Some");
    assert_eq!(pv.options, "ro,nosuid,nodev,noexec");
    assert!(!pv.inside_luks);
}

#[cfg(feature = "secure-boot")]
#[test]
fn secure_boot_section_rejects_unknown_field() {
    let toml = r#"
[secure_boot]
enable  = true
mystery = "boom"
"#;
    let err = toml::from_str::<Config>(toml)
        .expect_err("unknown field in [secure_boot] must be rejected");
    assert!(err.to_string().contains("mystery"), "{err}");
}

#[cfg(feature = "secure-boot")]
#[test]
fn secure_boot_priority_volume_rejects_unknown_field() {
    let toml = r#"
[secure_boot.priority_volume]
device     = "/dev/sda2"
mountpoint = "/mnt/p"
fstype     = "ext4"
mystery    = "boom"
"#;
    let err = toml::from_str::<Config>(toml)
        .expect_err("unknown field in [secure_boot.priority_volume] must be rejected");
    assert!(err.to_string().contains("mystery"), "{err}");
}

// F1 NEGATIVE: a binary built WITHOUT secure-boot `#[cfg]`s the
// `secure_boot` field off `Config`, so a `[secure_boot]` table is an
// UNKNOWN table `deny_unknown_fields` must reject — a feature-free binary
// never silently accepts a secure-boot table the Nix side only emits for a
// secure-boot build (FIX-16).
#[cfg(not(feature = "secure-boot"))]
#[test]
fn secure_boot_section_rejected_without_secure_boot_feature() {
    let toml = r#"
[secure_boot]
enable = true
"#;
    let err = toml::from_str::<Config>(toml)
        .expect_err("a non-secure-boot binary must reject the [secure_boot] table");
    let msg = err.to_string();
    assert!(
        msg.contains("secure_boot") || msg.contains("unknown"),
        "rejection should mention the unknown secure_boot table, got: {msg}",
    );
}

#[test]
fn parse_toml_round_trips_a_full_config_unchanged() {
    // The factored `parse_toml` must produce exactly what the old inline
    // `toml::from_str::<Config>` produced: a non-trivial config decodes
    // through both paths to the same observable fields. Guards that the
    // factoring did not change the existing load path's parse behaviour.
    let toml = r#"
[general]
timeout_ms = 7000

[[filesystems]]
device     = "/dev/disk/by-label/root"
mountpoint = "/"
fstype     = "ext4"
is_root    = true
"#;
    let direct: Config = toml::from_str(toml).expect("direct parse must succeed");
    let viaapi = Config::parse_toml(toml, std::path::Path::new("/etc/nmbl/config.toml"))
        .expect("parse_toml");
    assert_eq!(viaapi.general.timeout_ms, direct.general.timeout_ms);
    assert_eq!(viaapi.filesystems.len(), direct.filesystems.len());
    let fs = viaapi.filesystems.first().expect("one filesystem entry");
    assert_eq!(fs.device, "/dev/disk/by-label/root");
    assert!(fs.is_root);
}

#[test]
fn parse_toml_surfaces_a_config_error_on_garbage() {
    // Malformed TOML must come back as NmblError::Config carrying the path
    // (the same diagnostic the old inline parse produced).
    let err = Config::parse_toml("this = = not toml", std::path::Path::new("/x/bad.toml"))
        .expect_err("garbage TOML must error");
    assert!(matches!(err, NmblError::Config { .. }), "{err}");
}

#[cfg(feature = "staged-boot")]
#[test]
fn load_fragment_accepts_a_partial_overlay() {
    // A fragment that sets only a subset of fields must parse: unlike a
    // full Config it does not require any mandatory section.
    use crate::config::ConfigFragment;
    let toml = "[general]\ntimeout_ms = 4200\n";
    let frag =
        ConfigFragment::parse_toml(toml, std::path::Path::new("/frag.toml")).expect("partial frag");
    let general = frag.general.expect("general overlay present");
    assert_eq!(general.timeout_ms, 4200);
    // Unmentioned tables stay absent so the merge leaves the base alone.
    assert!(frag.filesystems.is_none());
    assert!(frag.tpm.is_none());
}

#[cfg(feature = "staged-boot")]
#[test]
fn load_fragment_accepts_an_empty_overlay() {
    // The empty fragment is valid (sets nothing); every field is None.
    use crate::config::ConfigFragment;
    let frag =
        ConfigFragment::parse_toml("", std::path::Path::new("/frag.toml")).expect("empty frag");
    assert!(frag.general.is_none());
    assert!(frag.activations.is_none());
}

#[cfg(feature = "staged-boot")]
#[test]
fn load_fragment_rejects_unknown_keys() {
    // deny_unknown_fields must carry over to the fragment: an unknown
    // table is a hard parse error, never a silent no-op.
    use crate::config::ConfigFragment;
    let toml = "[general]\ntimeout_ms = 1000\n\n[mystery]\nfoo = 1\n";
    let err = ConfigFragment::parse_toml(toml, std::path::Path::new("/frag.toml"))
        .expect_err("unknown table must be rejected");
    assert!(err.to_string().contains("mystery"), "{err}");
}

#[cfg(feature = "staged-boot")]
#[test]
fn load_fragment_reads_from_disk() {
    // The on-disk loader round-trips through the file system and the parse.
    use crate::config::load_fragment;
    let dir = std::env::temp_dir().join(format!("nmbl-frag-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mk tmp dir");
    let path = dir.join("fragment.toml");
    std::fs::write(&path, "[general]\ntimeout_ms = 9000\n").expect("write frag");
    let frag = load_fragment(&path).expect("load_fragment must succeed");
    assert_eq!(frag.general.expect("general").timeout_ms, 9000);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A non-root `/boot` filesystem entry whose runtime mount lives under
/// `system_root`.
fn boot_fs_entry() -> FilesystemEntry {
    FilesystemEntry {
        mountpoint: PathBuf::from("/boot"),
        fstype: "vfat".to_string(),
        ..fs_entry("/dev/disk/by-partlabel/disk-main-ESP", "/boot")
    }
}

/// The bootstrap `runtime_boot_mountpoint` is authoritative: when Phase 0.5
/// recorded it, `resolve_boot_mountpoint` returns it verbatim and never falls
/// back to deriving one from the filesystem table.
#[test]
fn resolve_boot_mountpoint_prefers_runtime_field() {
    let mut c = config_with(vec![boot_fs_entry()]);
    c.runtime_boot_mountpoint = Some(PathBuf::from("/run/nmbl-boot"));
    assert_eq!(
        c.resolve_boot_mountpoint(),
        Some(PathBuf::from("/run/nmbl-boot")),
    );
}

/// Embedded-config (UKI) mode: no Phase 0.5 ran, so the runtime field is
/// unset, but the `/boot` filesystem entry pins the boot partition to
/// `<system_root>/boot`. This is the signed-gen-happy fix — the verify must be
/// able to locate the sidecars on the boot partition without bootstrap mode.
#[test]
fn resolve_boot_mountpoint_derives_from_boot_entry_in_embedded_mode() {
    let mut c = config_with(vec![boot_fs_entry()]);
    c.paths.system_root = PathBuf::from("/mnt/system");
    assert!(c.runtime_boot_mountpoint.is_none());
    assert_eq!(
        c.resolve_boot_mountpoint(),
        Some(PathBuf::from("/mnt/system/boot")),
    );
}

/// No runtime field and no `/boot` entry ⇒ `None`, so the verify locator
/// surfaces its hard "cannot locate sidecars" error rather than guessing.
#[test]
fn resolve_boot_mountpoint_is_none_without_runtime_field_or_boot_entry() {
    let c = config_with(vec![fs_entry("/dev/sda1", "/data")]);
    assert_eq!(c.resolve_boot_mountpoint(), None);
}
