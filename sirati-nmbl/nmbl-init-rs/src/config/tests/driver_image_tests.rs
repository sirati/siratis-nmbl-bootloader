#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use std::path::PathBuf;

use crate::config::Config;

#[test]
fn driver_images_default_when_absent() {
    // An empty config must leave the group disabled with no images, so a
    // build that never opted in keeps the legacy flow.
    let cfg: Config = toml::from_str("").expect("missing driver_images must default");
    assert!(!cfg.driver_images.enable);
    assert!(cfg.driver_images.images.is_empty());
}

#[test]
fn driver_images_round_trips_full_schema() {
    let toml = r#"
[driver_images]
enable = true

[[driver_images.images]]
path = "nmbl/driver-nvidia.sfs"
sig_path = "nmbl/driver-nvidia.sfs.sig"
modules = ["nvidia", "nvidia_modeset"]
blacklist = ["nouveau"]

[[driver_images.images]]
path = "nmbl/driver-zfs.sfs"
sig_path = "nmbl/driver-zfs.sfs.sig"
modules = ["zfs"]
"#;
    let cfg: Config = toml::from_str(toml).expect("full driver_images schema must parse");
    assert!(cfg.driver_images.enable);
    assert_eq!(cfg.driver_images.images.len(), 2);

    let nvidia = &cfg.driver_images.images[0];
    assert_eq!(nvidia.path, PathBuf::from("nmbl/driver-nvidia.sfs"));
    assert_eq!(nvidia.sig_path, PathBuf::from("nmbl/driver-nvidia.sfs.sig"));
    assert_eq!(nvidia.modules, vec!["nvidia", "nvidia_modeset"]);
    assert_eq!(nvidia.blacklist, vec!["nouveau"]);

    let zfs = &cfg.driver_images.images[1];
    assert_eq!(zfs.path, PathBuf::from("nmbl/driver-zfs.sfs"));
    assert_eq!(zfs.modules, vec!["zfs"]);
    // Absent blacklist defaults to empty (serde default).
    assert!(zfs.blacklist.is_empty());
}

#[test]
fn driver_images_enable_only_parses() {
    // `enable = true` with no images is a valid (if inert) config.
    let cfg: Config =
        toml::from_str("[driver_images]\nenable = true\n").expect("enable-only must parse");
    assert!(cfg.driver_images.enable);
    assert!(cfg.driver_images.images.is_empty());
}

#[test]
fn driver_images_rejects_unknown_group_field() {
    let toml = r#"
[driver_images]
enable = true
bogus = "nope"
"#;
    let err = toml::from_str::<Config>(toml)
        .expect_err("unknown field in [driver_images] must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("bogus") || msg.contains("unknown field"),
        "rejection should mention the unknown field, got: {msg}",
    );
}

#[test]
fn driver_images_rejects_unknown_image_field() {
    let toml = r#"
[driver_images]
enable = true

[[driver_images.images]]
path = "nmbl/driver-x.sfs"
firmware = ["should-not-be-here"]
"#;
    // `firmware` is a build-time-only Nix input baked into the squashfs; it
    // is NOT part of the runtime struct, so `deny_unknown_fields` must reject
    // it if it ever leaks into the emitted TOML.
    toml::from_str::<Config>(toml).expect_err("unknown field in image table must be rejected");
}
