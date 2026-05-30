#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]

use std::path::PathBuf;

use crate::config::Config;
use crate::config::entries::{Activation, ActivationKind, FilesystemEntry};
use crate::error::NmblError;

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

fn activation(kind: ActivationKind, produces: &[&str]) -> Activation {
    Activation {
        kind,
        required_modules: Vec::new(),
        binary: PathBuf::from("/bin/true"),
        argv: Vec::new(),
        produces_devices: produces.iter().map(PathBuf::from).collect(),
        source_devices: Vec::new(),
        description: "test".to_string(),
        prompt_label: None,
        pass_to_stage1: None,
    }
}

#[test]
fn validate_rejects_unsatisfiable_mapper_fs() {
    // A /dev/mapper/* root with no activation at all can never appear —
    // NMBL would hang waiting for it. Must be rejected.
    let mut c = config_with(vec![fs_entry("/dev/mapper/vg-root", "/")]);
    c.activations = Vec::new();
    let err = c
        .validate()
        .expect_err("mapper fs without any activation must be rejected");
    match err {
        NmblError::ConfigInvalid { reason, .. } => {
            assert!(
                reason.contains("/dev/mapper/vg-root"),
                "rejection should name the offending device, got: {reason}",
            );
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }
}

#[test]
fn validate_accepts_mapper_fs_with_matching_luks() {
    // Same mapper fs is fine once a luks activation produces exactly it.
    let mut c = config_with(vec![fs_entry("/dev/mapper/cryptroot", "/")]);
    c.activations = vec![activation(
        ActivationKind::LuksPassword,
        &["/dev/mapper/cryptroot"],
    )];
    c.validate()
        .expect("mapper fs covered by matching luks produces_devices must validate");
}

#[test]
fn validate_accepts_mapper_fs_with_lvm_present() {
    // An lvm activation can produce arbitrary /dev/mapper/<vg>-<lv>,
    // so any mapper fs is plausibly satisfiable (LVM-on-LUKS layout).
    let mut c = config_with(vec![fs_entry("/dev/mapper/vg-root", "/")]);
    c.activations = vec![activation(ActivationKind::Lvm, &[])];
    c.validate()
        .expect("mapper fs with an lvm activation present must validate");
}

#[test]
fn validate_accepts_mapper_fs_with_mdraid_present() {
    let mut c = config_with(vec![fs_entry("/dev/mapper/vg-root", "/")]);
    c.activations = vec![activation(ActivationKind::Mdraid, &[])];
    c.validate()
        .expect("mapper fs with an mdraid activation present must validate");
}

#[test]
fn validate_accepts_plain_block_device_without_activation() {
    // A bare /dev/nvme* device is not device-mapper and must pass even
    // with no activation — the check only constrains /dev/mapper/* nodes.
    let mut c = config_with(vec![fs_entry("/dev/nvme0n1p1", "/")]);
    c.activations = Vec::new();
    c.validate()
        .expect("plain non-mapper device must validate with no activation");
}
