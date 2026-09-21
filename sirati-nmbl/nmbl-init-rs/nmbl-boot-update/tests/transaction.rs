#![allow(clippy::unwrap_used, clippy::expect_used)]
use nmbl_boot_update::transaction::{self, Limits, Outcome};
use nmbl_host_tools::{cli, domain, sign};
use sha2::{Digest, Sha512};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
struct Fixture {
    _temp: TempDir,
    key: PathBuf,
    public: PathBuf,
    spool: PathBuf,
    boot: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let key = temp.path().join("operator.key");
        let public = temp.path().join("operator.pub");
        let args = vec![
            "keygen".into(),
            "--alg".into(),
            "ml-dsa-65".into(),
            "--out-priv".into(),
            key.display().to_string(),
            "--out-pub".into(),
            public.display().to_string(),
        ];
        nmbl_host_tools::run(cli::parse(&args).expect("keygen args")).expect("keygen");
        let spool = temp.path().join("spool");
        let boot = temp.path().join("boot");
        fs::create_dir_all(&spool).expect("spool");
        fs::create_dir_all(&boot).expect("boot");
        Self {
            _temp: temp,
            key,
            public,
            spool,
            boot,
        }
    }
    fn bundle(&self, name: &str, slot: char, fill: u8, extra: usize) -> PathBuf {
        self.bundle_signed(name, slot, fill, extra, &self.key)
    }
    fn bundle_signed(&self, name: &str, slot: char, fill: u8, extra: usize, key: &Path) -> PathBuf {
        let bundle = self.spool.join(name);
        fs::create_dir(&bundle).expect("bundle");
        let roles = [
            ("bootloader", "bootloader"),
            ("kernel", "kernel"),
            ("initrd", "initrd"),
            ("rescue", "rescue"),
            ("config", "config"),
        ];
        let artifact_domain = domain::domain_for("boot-set-artifact").expect("artifact domain");
        let entries = roles.iter().enumerate().map(|(index, (role, file))| {
            let mut bytes = vec![fill.wrapping_add(index as u8); 8192];
            if *role == "rescue" { bytes.resize(bytes.len() + extra, fill); }
            let path = bundle.join(file);
            fs::write(&path, &bytes).expect("payload");
            sign::run(&path, key, artifact_domain, Some(&bundle.join(format!("{file}.sig"))))
                .expect("sign artifact");
            let digest = format!("{:x}", Sha512::digest(&bytes));
            format!(
                r#"{{"role":"{role}","destination":"{file}","payload":"{file}","signature":"{file}.sig","domain":"boot-set-artifact","sha512":"{digest}"}}"#
            )
        }).collect::<Vec<_>>().join(",");
        let set_id = format!("{:x}", Sha512::digest(name.as_bytes()));
        let manifest = format!(
            r#"{{"version":1,"set_id":"{set_id}","target_slot":"{slot}","files":[{entries}]}}"#
        );
        fs::write(bundle.join("manifest.json"), manifest).expect("manifest");
        sign::run(
            &bundle.join("manifest.json"),
            key,
            domain::domain_for("boot-set-manifest").expect("manifest domain"),
            Some(&bundle.join("manifest.json.sig")),
        )
        .expect("sign manifest");
        bundle
    }
    fn install(&self, bundle: &Path, limits: Limits) -> nmbl_boot_update::Result<Outcome> {
        transaction::install(bundle, &self.public, &self.boot, &limits)
    }
    fn active(&self) -> String {
        fs::read_to_string(self.boot.join("nmbl-boot-sets/active")).expect("active")
    }
}
fn generate_key(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    let key = root.join(format!("{name}.key"));
    let public = root.join(format!("{name}.pub"));
    let args = vec![
        "keygen".into(),
        "--alg".into(),
        "ml-dsa-65".into(),
        "--out-priv".into(),
        key.display().to_string(),
        "--out-pub".into(),
        public.display().to_string(),
    ];
    nmbl_host_tools::run(cli::parse(&args).expect("keygen args")).expect("keygen");
    (key, public)
}
#[test]
fn atomic_switch_unchanged_and_rejections_preserve_active() {
    let fixture = Fixture::new();
    let first = fixture.bundle("first", 'A', 1, 0);
    assert_eq!(
        fixture.install(&first, Limits::default()).expect("first"),
        Outcome::Activated('A')
    );
    let kernel = fixture.boot.join("nmbl-boot-sets/A/kernel");
    let selector = fixture.boot.join("nmbl-boot-sets/active");
    let before = fs::metadata(&kernel).expect("kernel");
    let selector_before = fs::metadata(&selector).expect("selector");
    assert_eq!(
        fixture.install(&first, Limits::default()).expect("same"),
        Outcome::Unchanged('A')
    );
    let after = fs::metadata(&kernel).expect("kernel");
    let selector_after = fs::metadata(&selector).expect("selector");
    assert_eq!((before.ino(), before.mtime()), (after.ino(), after.mtime()));
    assert_eq!(
        (selector_before.ino(), selector_before.mtime()),
        (selector_after.ino(), selector_after.mtime())
    );

    let broken = fixture.bundle("broken", 'B', 2, 0);
    fs::write(broken.join("kernel"), b"tampered").expect("tamper");
    assert!(fixture.install(&broken, Limits::default()).is_err());
    assert_eq!(fixture.active(), "set nmbl_slot=A\n");

    let bad_signature = fixture.bundle("bad-signature", 'B', 6, 0);
    fs::write(bad_signature.join("kernel.sig"), b"bad signature").expect("tamper sig");
    assert!(fixture.install(&bad_signature, Limits::default()).is_err());
    let bad_manifest = fixture.bundle("bad-manifest", 'B', 7, 0);
    fs::write(bad_manifest.join("manifest.json.sig"), b"bad signature").expect("tamper manifest");
    assert!(fixture.install(&bad_manifest, Limits::default()).is_err());
    assert_eq!(fixture.active(), "set nmbl_slot=A\n");

    let second = fixture.bundle("second", 'B', 3, 0);
    assert_eq!(
        fixture.install(&second, Limits::default()).expect("second"),
        Outcome::Activated('B')
    );
    assert_eq!(fixture.active(), "set nmbl_slot=B\n");
}

#[test]
fn privileged_recheck_catches_mutation_after_unprivileged_validation() {
    let fixture = Fixture::new();
    let bundle = fixture.bundle("changed-after-client-check", 'A', 9, 0);
    nmbl_boot_update::validate::bundle(&bundle, &fixture.public).expect("unprivileged validation");
    fs::write(bundle.join("config"), b"changed after first check").expect("mutate input");
    assert!(fixture.install(&bundle, Limits::default()).is_err());
    assert!(!fixture.boot.join("nmbl-boot-sets/active").exists());
}

#[test]
fn low_space_reclaims_only_inactive_and_true_enospc_is_untouched() {
    let fixture = Fixture::new();
    let first = fixture.bundle("first", 'A', 1, 0);
    let second = fixture.bundle("second", 'B', 2, 0);
    fixture.install(&first, Limits::default()).expect("first");
    fixture.install(&second, Limits::default()).expect("second");
    let old_a = fs::read(fixture.boot.join("nmbl-boot-sets/A/kernel")).expect("old A");
    let third = fixture.bundle("third", 'A', 3, 0);
    let need = nmbl_boot_update::validate::bundle(&third, &fixture.public)
        .expect("validate third")
        .allocated_bytes;
    assert_eq!(
        fixture
            .install(
                &third,
                Limits {
                    available_bytes: Some(need),
                    fail_after_files: None
                }
            )
            .expect("normal"),
        Outcome::Activated('A')
    );

    let fourth = fixture.bundle("fourth", 'B', 4, 0);
    let need = nmbl_boot_update::validate::bundle(&fourth, &fixture.public)
        .expect("validate fourth")
        .allocated_bytes;
    assert_eq!(
        fixture
            .install(
                &fourth,
                Limits {
                    available_bytes: Some(need / 2),
                    fail_after_files: None
                }
            )
            .expect("low space"),
        Outcome::Activated('B')
    );

    let huge = fixture.bundle("huge", 'A', 5, 2 * 1024 * 1024);
    let active_before = fixture.active();
    let inactive_before = fs::read(fixture.boot.join("nmbl-boot-sets/A/kernel")).expect("inactive");
    assert!(
        fixture
            .install(
                &huge,
                Limits {
                    available_bytes: Some(0),
                    fail_after_files: None
                }
            )
            .is_err()
    );
    assert_eq!(fixture.active(), active_before);
    assert_eq!(
        fs::read(fixture.boot.join("nmbl-boot-sets/A/kernel")).expect("inactive"),
        inactive_before
    );
    assert_ne!(old_a, inactive_before);
}

#[test]
fn interrupted_staging_never_moves_selector() {
    let fixture = Fixture::new();
    let first = fixture.bundle("first", 'A', 1, 0);
    fixture.install(&first, Limits::default()).expect("first");
    let second = fixture.bundle("second", 'B', 2, 0);
    let result = fixture.install(
        &second,
        Limits {
            available_bytes: None,
            fail_after_files: Some(2),
        },
    );
    assert!(result.is_err());
    assert_eq!(fixture.active(), "set nmbl_slot=A\n");
    assert!(!fixture.boot.join("nmbl-boot-sets/B").exists());
}

#[test]
fn trusted_boot_set_replacement_rotates_the_external_trust_root() {
    let fixture = Fixture::new();
    let first = fixture.bundle("key-a-first", 'A', 1, 0);
    fixture
        .install(&first, Limits::default())
        .expect("key A generation");
    let (key_b, public_b) = generate_key(fixture._temp.path(), "operator-b");

    // The existing A trust root authorizes the complete transition set whose
    // bootloader artifact embeds B. The generation receiver itself never
    // changes the configured public key.
    let transition = fixture.bundle("key-a-transition-to-b", 'B', 2, 0);
    let bootloader = transition.join("bootloader");
    let old_digest = format!(
        "{:x}",
        Sha512::digest(fs::read(&bootloader).expect("old bootloader"))
    );
    let replacement = fs::read(&public_b).expect("public B");
    let new_digest = format!("{:x}", Sha512::digest(&replacement));
    fs::write(&bootloader, replacement).expect("replace trust artifact");
    sign::run(
        &bootloader,
        &fixture.key,
        domain::domain_for("boot-set-artifact").expect("artifact domain"),
        Some(&transition.join("bootloader.sig")),
    )
    .expect("sign replacement boot artifact");
    let manifest_path = transition.join("manifest.json");
    let manifest = fs::read_to_string(&manifest_path)
        .expect("transition manifest")
        .replace(&old_digest, &new_digest);
    fs::write(&manifest_path, manifest).expect("update transition manifest");
    sign::run(
        &manifest_path,
        &fixture.key,
        domain::domain_for("boot-set-manifest").expect("manifest domain"),
        Some(&transition.join("manifest.json.sig")),
    )
    .expect("sign transition manifest");
    fixture
        .install(&transition, Limits::default())
        .expect("A-authorized transition");
    let key_b_generation = fixture.bundle_signed("key-b-generation", 'A', 3, 0, &key_b);
    transaction::install(
        &key_b_generation,
        &public_b,
        &fixture.boot,
        &Limits::default(),
    )
    .expect("B generation");

    let stale_a = fixture.bundle("stale-key-a", 'B', 4, 0);
    assert!(transaction::install(&stale_a, &public_b, &fixture.boot, &Limits::default(),).is_err());
    assert_eq!(fixture.active(), "set nmbl_slot=A\n");
}
