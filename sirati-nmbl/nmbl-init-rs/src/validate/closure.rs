//! `--validate-nix-filesystem-closure`: NixOS-only sandbox check that
//! the NMBL TOML MATCHES the NixOS `config.fileSystems` closure.
//!
//! Input is `builtins.toJSON` of the NixOS filesystem set plus the NMBL
//! TOML (passed via `--config-toml`). The JSON is a LIST of objects,
//! each:
//!
//! ```json
//! { "mountPoint": "/",
//!   "device": "/dev/disk/by-uuid/…",
//!   "fsType": "ext4",
//!   "options": ["noatime"],
//!   "neededForBoot": true,
//!   "depends": [] }
//! ```
//!
//! Rules enforced (the toml-vs-closure correspondence ONLY — the toml's
//! own internal validity is `--validate-config`'s job):
//!
//! * Every NixOS filesystem that is root (`mountPoint == "/"`) or
//!   `neededForBoot` MUST appear in the toml `[[filesystems]]` with the
//!   SAME device, fsType and mountpoint. Missing or mismatched ⇒ error
//!   naming the mountpoint and the differing field.
//! * Every toml `[[filesystems]]` entry MUST correspond to a NixOS
//!   filesystem with the same mountpoint — no stray toml entry.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::config::Config;
use crate::error::{NmblError, Result};

/// One NixOS filesystem as emitted by `builtins.toJSON`. Unknown extra
/// fields are tolerated so the Nix side can add keys without breaking
/// older binaries (this is consumed at build time, but forward-compat
/// keeps the wire contract loose where it can be).
#[derive(Debug, Deserialize)]
pub struct NixFilesystem {
    #[serde(rename = "mountPoint")]
    pub mount_point: String,
    pub device: String,
    #[serde(rename = "fsType")]
    pub fs_type: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(rename = "neededForBoot", default)]
    pub needed_for_boot: bool,
    #[serde(default)]
    pub depends: Vec<String>,
}

/// Load the closure JSON and the toml, then check they correspond.
/// `Ok(())` on a match; `Err` with a precise message otherwise.
pub fn validate_nix_filesystem_closure(closure_json: &Path, config_toml: &Path) -> Result<()> {
    let json_text = std::fs::read_to_string(closure_json).map_err(|source| NmblError::Io {
        source,
        context: format!("reading fs-closure JSON {}", closure_json.display()),
    })?;
    let nix_fs: Vec<NixFilesystem> =
        serde_json::from_str(&json_text).map_err(|e| NmblError::ConfigInvalid {
            reason: format!("fs-closure JSON did not parse: {e}"),
            context: format!("parsing {}", closure_json.display()),
        })?;

    let config = Config::load(config_toml)?;
    check_correspondence(&nix_fs, &config)
}

/// Pure comparison core (testable without touching the filesystem).
pub fn check_correspondence(nix_fs: &[NixFilesystem], config: &Config) -> Result<()> {
    let toml_by_mp: BTreeMap<&str, &crate::config::FilesystemEntry> = config
        .filesystems
        .iter()
        .map(|fs| (mountpoint_str(fs), fs))
        .collect();
    let nix_mps: BTreeMap<&str, &NixFilesystem> =
        nix_fs.iter().map(|f| (f.mount_point.as_str(), f)).collect();

    let mut errors: Vec<String> = Vec::new();

    // 1. Every boot-critical NixOS fs must be in the toml and match.
    for f in nix_fs {
        if !(f.mount_point == "/" || f.needed_for_boot) {
            continue;
        }
        match toml_by_mp.get(f.mount_point.as_str()) {
            None => errors.push(format!(
                "NixOS filesystem {} ({}) is root/neededForBoot but is missing from the NMBL config",
                f.mount_point,
                boot_reason(f)
            )),
            Some(entry) => diff_entry(f, entry, &mut errors),
        }
    }

    // 2. No toml entry that the NixOS config does not declare.
    for fs in &config.filesystems {
        let mp = mountpoint_str(fs);
        if !nix_mps.contains_key(mp) {
            errors.push(format!(
                "NMBL config declares filesystem {mp} (device {}) which is not present in the \
                 NixOS filesystem configuration",
                fs.device
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(NmblError::ConfigInvalid {
            reason: errors.join("; "),
            context: "validating NMBL config against the NixOS filesystem closure".to_string(),
        })
    }
}

fn boot_reason(f: &NixFilesystem) -> &'static str {
    if f.mount_point == "/" {
        "root"
    } else {
        "neededForBoot"
    }
}

fn mountpoint_str(fs: &crate::config::FilesystemEntry) -> &str {
    fs.mountpoint.to_str().unwrap_or("")
}

/// Compare device + fsType of a matched pair, pushing a precise message
/// per differing field.
fn diff_entry(
    nix: &NixFilesystem,
    entry: &crate::config::FilesystemEntry,
    errors: &mut Vec<String>,
) {
    if nix.device != entry.device {
        errors.push(format!(
            "filesystem {}: device mismatch (NixOS {:?} vs NMBL {:?})",
            nix.mount_point, nix.device, entry.device
        ));
    }
    if nix.fs_type != entry.fstype {
        errors.push(format!(
            "filesystem {}: fsType mismatch (NixOS {:?} vs NMBL {:?})",
            nix.mount_point, nix.fs_type, entry.fstype
        ));
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> Config {
        toml::from_str(toml).expect("parse toml")
    }

    fn nix(json: &str) -> Vec<NixFilesystem> {
        serde_json::from_str(json).expect("parse json")
    }

    const MATCHING_TOML: &str = r#"
        [[filesystems]]
        device = "/dev/disk/by-uuid/root"
        mountpoint = "/"
        fstype = "ext4"
        is_root = true

        [[filesystems]]
        device = "/dev/disk/by-uuid/boot"
        mountpoint = "/boot"
        fstype = "vfat"
    "#;

    const MATCHING_JSON: &str = r#"[
        {"mountPoint":"/","device":"/dev/disk/by-uuid/root","fsType":"ext4","options":[],"neededForBoot":true,"depends":[]},
        {"mountPoint":"/boot","device":"/dev/disk/by-uuid/boot","fsType":"vfat","options":[],"neededForBoot":true,"depends":[]}
    ]"#;

    #[test]
    fn matching_closure_ok() {
        check_correspondence(&nix(MATCHING_JSON), &cfg(MATCHING_TOML)).expect("should match");
    }

    #[test]
    fn device_mismatch_errors() {
        let json = r#"[
            {"mountPoint":"/","device":"/dev/disk/by-uuid/OTHER","fsType":"ext4","neededForBoot":true},
            {"mountPoint":"/boot","device":"/dev/disk/by-uuid/boot","fsType":"vfat","neededForBoot":true}
        ]"#;
        let err = check_correspondence(&nix(json), &cfg(MATCHING_TOML)).expect_err("mismatch");
        let msg = format!("{err}");
        assert!(msg.contains("device mismatch"), "{msg}");
        assert!(msg.contains('/'), "{msg}");
    }

    #[test]
    fn fstype_mismatch_errors() {
        let json = r#"[
            {"mountPoint":"/","device":"/dev/disk/by-uuid/root","fsType":"btrfs","neededForBoot":true},
            {"mountPoint":"/boot","device":"/dev/disk/by-uuid/boot","fsType":"vfat","neededForBoot":true}
        ]"#;
        let err = check_correspondence(&nix(json), &cfg(MATCHING_TOML)).expect_err("mismatch");
        assert!(format!("{err}").contains("fsType mismatch"), "{err}");
    }

    #[test]
    fn missing_boot_fs_errors() {
        // NixOS declares a neededForBoot /boot the toml lacks.
        let toml = r#"
            [[filesystems]]
            device = "/dev/disk/by-uuid/root"
            mountpoint = "/"
            fstype = "ext4"
            is_root = true
        "#;
        let err = check_correspondence(&nix(MATCHING_JSON), &cfg(toml)).expect_err("missing");
        let msg = format!("{err}");
        assert!(msg.contains("/boot"), "{msg}");
        assert!(msg.contains("missing"), "{msg}");
    }

    #[test]
    fn extra_toml_fs_errors() {
        // The toml has an /extra fs the NixOS config never declares.
        let toml = r#"
            [[filesystems]]
            device = "/dev/disk/by-uuid/root"
            mountpoint = "/"
            fstype = "ext4"
            is_root = true

            [[filesystems]]
            device = "/dev/disk/by-uuid/boot"
            mountpoint = "/boot"
            fstype = "vfat"

            [[filesystems]]
            device = "/dev/disk/by-uuid/extra"
            mountpoint = "/extra"
            fstype = "ext4"
        "#;
        let err = check_correspondence(&nix(MATCHING_JSON), &cfg(toml)).expect_err("extra");
        let msg = format!("{err}");
        assert!(msg.contains("/extra"), "{msg}");
        assert!(msg.contains("not present"), "{msg}");
    }

    #[test]
    fn non_boot_nix_fs_not_required_in_toml() {
        // A NixOS fs that is neither root nor neededForBoot need not be
        // in the toml; but every toml entry must still be in the NixOS
        // set, so we include it there too.
        let json = r#"[
            {"mountPoint":"/","device":"/dev/disk/by-uuid/root","fsType":"ext4","neededForBoot":true},
            {"mountPoint":"/boot","device":"/dev/disk/by-uuid/boot","fsType":"vfat","neededForBoot":true},
            {"mountPoint":"/home","device":"/dev/disk/by-uuid/home","fsType":"ext4","neededForBoot":false}
        ]"#;
        check_correspondence(&nix(json), &cfg(MATCHING_TOML)).expect("non-boot fs optional");
    }
}
