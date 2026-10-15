#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha512};

use super::*;
use crate::config::{RescueConfig, RescueImage, RescueNetworkStage, RescueSystem, RescueTools};
use crate::sig::alg::AlgId;
use crate::sig::tests::TestSigner;

const RESCUE: &[u8] = b"stage two rescue";
const NETWORK: &[u8] = b"network stage";
const TOOLS: &[u8] = b"tools image";

fn hex(bytes: &[u8]) -> String {
    crate::util::hex::hex_lower(&Sha512::digest(bytes))
}

/// A full-system rescue config as config-toml.nix renders it, with every
/// image pinned to the bytes [`stage`] writes.
fn config(signing: bool) -> Config {
    let mut c = Config::recovery_default();
    c.rescue = RescueConfig {
        mode: RescueMode::External,
        sfs_path: Some(PathBuf::from("/nmbl/rescue.erofs")),
        image: RescueImage {
            format: RescueImageFormat::Erofs,
            sha512: Some(hex(RESCUE)),
        },
        network_stage: Some(RescueNetworkStage {
            path: PathBuf::from("nmbl/network.erofs"),
            sha512: Some(hex(NETWORK)),
        }),
        tools: Some(RescueTools {
            path: PathBuf::from("/nmbl/rescue-tools.erofs"),
            sha512: hex(TOOLS),
        }),
        system: Some(RescueSystem {
            sshd_port: 2222,
            authorized_keys: vec!["ssh-ed25519 AAAA operator".into()],
            host_key_path: None,
            modules: Vec::new(),
            network_profile: None,
        }),
        ..RescueConfig::default()
    };
    c.signing.enable = signing;
    c.signing.enforce = signing;
    c
}

/// Write the three images under `boot`, signed by `signer` when given.
fn stage(boot: &Path, signer: Option<&TestSigner>) {
    std::fs::create_dir_all(boot.join("nmbl")).expect("mkdir");
    for (name, bytes, domain) in [
        ("rescue.erofs", RESCUE, DOMAIN_RESCUE_SFS),
        ("network.erofs", NETWORK, DOMAIN_NETWORK_STAGE),
        ("rescue-tools.erofs", TOOLS, DOMAIN_RESCUE_TOOLS),
    ] {
        let path = boot.join("nmbl").join(name);
        std::fs::write(&path, bytes).expect("image");
        if let Some(signer) = signer {
            let digest: [u8; 64] = Sha512::digest(bytes).into();
            let sidecar = signer.sidecar_bytes(&digest, domain, 0);
            std::fs::write(boot.join("nmbl").join(format!("{name}.sig")), sidecar).expect("sig");
        }
    }
}

fn inspect(c: &Config, boot: &Path, keys: &[BakedKey]) -> RescueInspection {
    inspect_with_keys(c, boot, Ok(keys))
}

fn statuses(r: &RescueInspection) -> Vec<(&'static str, bool, PinStatus, SignatureStatus)> {
    r.images
        .iter()
        .map(|i| (i.role.as_str(), i.present, i.sha512, i.signature))
        .collect()
}

#[test]
fn good_unsigned_rescue_is_ok_and_signature_not_required() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), None);
    let r = inspect(&config(false), dir.path(), &[]);
    assert!(r.ok(), "{:?}", r.problems);
    assert_eq!(
        statuses(&r),
        vec![
            (
                "rescue-image",
                true,
                PinStatus::Matches,
                SignatureStatus::NotRequired
            ),
            (
                "network-stage",
                true,
                PinStatus::Matches,
                SignatureStatus::NotRequired
            ),
            (
                "tools",
                true,
                PinStatus::Matches,
                SignatureStatus::NotRequired
            ),
        ]
    );
    assert_eq!(r.images[0].path, dir.path().join("nmbl/rescue.erofs"));
    assert_eq!(r.images[2].path, dir.path().join("nmbl/rescue-tools.erofs"));
}

#[test]
fn signed_rescue_verifies_under_each_images_own_domain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let signer = TestSigner::new(AlgId::MlDsa65, 3);
    stage(dir.path(), Some(&signer));
    let keys = [signer.baked_key()];
    let r = inspect(&config(true), dir.path(), &keys);
    assert!(r.ok(), "{:?}", r.problems);
    assert!(
        r.images
            .iter()
            .all(|i| i.signature == SignatureStatus::Verified)
    );

    // A tools signature replayed from the networking-stage role is refused.
    let digest: [u8; 64] = Sha512::digest(TOOLS).into();
    std::fs::write(
        dir.path().join("nmbl/rescue-tools.erofs.sig"),
        signer.sidecar_bytes(&digest, DOMAIN_NETWORK_STAGE, 0),
    )
    .expect("sig");
    let r = inspect(&config(true), dir.path(), &keys);
    assert_eq!(r.images[2].signature, SignatureStatus::Invalid);
    assert!(!r.ok());

    // An unknown key does not verify either.
    let stranger = [TestSigner::new(AlgId::MlDsa65, 4).baked_key()];
    let r = inspect(&config(true), dir.path(), &stranger);
    assert_eq!(r.images[0].signature, SignatureStatus::Invalid);
}

#[test]
fn signing_enabled_without_sidecars_reports_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), None);
    let r = inspect(&config(true), dir.path(), &[]);
    assert!(
        r.images
            .iter()
            .all(|i| i.signature == SignatureStatus::Missing)
    );
    assert_eq!(r.problems.len(), 3, "{:?}", r.problems);
}

#[test]
fn missing_image_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), None);
    std::fs::remove_file(dir.path().join("nmbl/network.erofs")).expect("rm");
    let r = inspect(&config(false), dir.path(), &[]);
    assert!(!r.ok());
    assert_eq!(
        statuses(&r)[1],
        (
            "network-stage",
            false,
            PinStatus::Unreadable,
            SignatureStatus::NotRequired
        )
    );
    assert!(r.problems[0].contains("network-stage"), "{:?}", r.problems);
}

#[test]
fn a_directory_is_not_an_image() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), None);
    std::fs::remove_file(dir.path().join("nmbl/rescue.erofs")).expect("rm");
    std::fs::create_dir(dir.path().join("nmbl/rescue.erofs")).expect("dir");
    let r = inspect(&config(false), dir.path(), &[]);
    assert!(!r.images[0].present);
    assert!(!r.ok());
}

#[test]
fn wrong_hash_is_a_mismatch() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), None);
    std::fs::write(dir.path().join("nmbl/rescue.erofs"), b"older rescue").expect("swap");
    let r = inspect(&config(false), dir.path(), &[]);
    assert_eq!(r.images[0].sha512, PinStatus::Mismatch);
    assert!(!r.ok());

    let mut c = config(false);
    c.rescue.image.sha512 = Some("abc".into());
    stage(dir.path(), None);
    let r = inspect(&c, dir.path(), &[]);
    assert_eq!(r.images[0].sha512, PinStatus::Mismatch);
}

#[test]
fn unpinned_tools_is_a_problem_but_unpinned_stages_are_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), None);
    let mut c = config(false);
    c.rescue.image.sha512 = None;
    if let Some(stage) = c.rescue.network_stage.as_mut() {
        stage.sha512 = None;
    }
    let r = inspect(&c, dir.path(), &[]);
    assert!(r.ok(), "{:?}", r.problems);
    assert_eq!(r.images[0].sha512, PinStatus::Unpinned);

    if let Some(tools) = c.rescue.tools.as_mut() {
        tools.sha512 = String::new();
    }
    let r = inspect(&c, dir.path(), &[]);
    assert_eq!(r.images[2].sha512, PinStatus::Unpinned);
    assert!(!r.ok());
    assert!(r.problems[0].contains("unpinned"), "{:?}", r.problems);
}

#[test]
fn unsafe_stage_path_is_reported_without_io() {
    let dir = tempfile::tempdir().expect("tempdir");
    stage(dir.path(), None);
    let mut c = config(false);
    if let Some(tools) = c.rescue.tools.as_mut() {
        tools.path = PathBuf::from("../escape.erofs");
    }
    let r = inspect(&c, dir.path(), &[]);
    assert_eq!(r.images[2].path, PathBuf::from("../escape.erofs"));
    assert!(!r.images[2].present);
    assert!(!r.ok());
}

#[test]
fn config_without_full_system_rescue_is_not_ok() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut c = Config::recovery_default();
    c.rescue.mode = RescueMode::External;
    let r = inspect(&c, dir.path(), &[]);
    assert!(r.images.is_empty());
    assert!(!r.ok());
    assert!(r.problems[0].contains("full-system"), "{:?}", r.problems);
    assert!(is_external_full_system(&config(false)));
}
