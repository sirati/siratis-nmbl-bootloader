//! Tests for the signed priority-file boot gate (#31).
//!
//! Cover the three-way branch, the FULL-fingerprint narrowing (FIX-08), the
//! domain pinning to `priority-file:v1` (FIX-01), the AttestedVolume witness
//! shape (FIX-26), and the pre-console deferral that routes a refuse to the
//! shared `run_tui_session` Err arm rather than a shell (FIX-35).
//!
//! The crypto core (`verify_priority_against`) is exercised with a CRAFTED key
//! set because the real `BAKED_KEYS` static is empty in this build; the
//! file-level `run_priority_gate_at` branches that need no baked key (skip /
//! refuse-on-missing / audit-proceed) are driven through the public entry.

use std::path::PathBuf;

use fips204::traits::{KeyGen, SerDes, Signer};
use fips204::{Ph, ml_dsa_65};

use super::{GatePhase, run_priority_gate_at, verify_priority_against};
use crate::config::{Config, PriorityVolume};
use crate::error::NmblError;
use crate::sig::alg::{AlgId, HashId};
use crate::sig::wire::{self, Header};
use crate::sig::{BakedKey, DOMAIN_PRIORITY_FILE, fp};
use crate::sys::ops::RealSys;

/// A deterministic ML-DSA-65 signer that mints real priority-file sidecars the
/// gate's verify core accepts. Mirrors the host `nmbl-sign` triple: pre-hash the
/// SAME SHA-512 digest under `Ph::SHA512` with the per-role domain as the ctx.
struct Signer65 {
    sk: ml_dsa_65::PrivateKey,
    pk_bytes: Vec<u8>,
}

impl Signer65 {
    fn new(seed: u8) -> Self {
        let (pk, sk) = ml_dsa_65::KG::keygen_from_seed(&[seed; 32]);
        Self {
            sk,
            pk_bytes: pk.into_bytes().to_vec(),
        }
    }

    fn baked_key(&self) -> BakedKey {
        BakedKey::parse(&self.pk_bytes, AlgId::MlDsa65).expect("valid baked key")
    }

    fn full_fp(&self) -> [u8; 32] {
        fp(&self.pk_bytes)
    }

    /// Build a parseable sidecar over `digest` under `domain`.
    fn sidecar(&self, digest: &[u8; 64], domain: &[u8]) -> Vec<u8> {
        let signature = self
            .sk
            .try_hash_sign_with_seed(&[0x42u8; 32], digest, domain, &Ph::SHA512)
            .expect("sign");
        let header = Header {
            alg: AlgId::MlDsa65,
            hash: HashId::Sha512,
            key_id: 0,
            domain: wire::domain_tag(domain),
        };
        let mut buf = wire::encode(&header).to_vec();
        buf.extend_from_slice(&signature);
        buf
    }
}

fn digest(byte: u8) -> [u8; 64] {
    [byte; 64]
}

/// A config in enforcing secure-boot posture with the given allowed fingerprint
/// id strings.
fn enforcing_cfg(allowed: &[String]) -> Config {
    let mut cfg = Config::recovery_default();
    cfg.secure_boot.enable = true;
    cfg.secure_boot.enforce = true;
    cfg.secure_boot.allowed_key_ids = allowed.to_vec();
    cfg
}

// ---- (a) valid signature: the crypto core ACCEPTS -> Attested -------------

#[test]
fn valid_signature_verifies_then_yields_attested() {
    // The pure core returning Ok(()) is exactly what makes `evaluate` produce
    // GateDecision::Attested; we prove the crypto half here and the witness
    // shape below.
    let signer = Signer65::new(7);
    let cfg = enforcing_cfg(&[]); // empty allow-list = whole baked set
    let d = digest(0x11);
    let sc = signer.sidecar(&d, DOMAIN_PRIORITY_FILE);
    let baked = [signer.baked_key()];
    verify_priority_against(&cfg, &d, &sc, &baked)
        .expect("an honest priority-file signature must verify");
}

#[test]
fn attested_volume_exposes_its_mountpoint() {
    // The witness owns the mountpoint #33 resolves staged artifacts against.
    // A non-owned mount (PrePlainBoot reuse) drops without unmounting.
    let cfg = Config::recovery_default();
    let vol = priority_vol(PathBuf::from("/dev/none"), false);
    let mut fs = RealSys::sync_only();
    let attested =
        super::mount_priority_volume(&mut fs, GatePhase::PrePlainBoot, &with_boot_mp(cfg), &vol)
            .expect("pre-plain-boot reuses the boot FS, no mount");
    assert_eq!(
        attested.mountpoint(),
        std::path::Path::new("/run/boot-test")
    );
    drop(attested); // must not panic / must not try to unmount a non-owned FS
}

#[test]
fn dry_run_post_unlock_mount_owns_no_real_mount() {
    // Under a dry-run `FsOps` the PostUnlock mount is a no-op, so the witness
    // must NOT carry an `owned_mount`: its Drop would otherwise issue a stray
    // real `umount(2)` on a path nothing actually mounted (Property-6). The
    // mount source `/dev/none` is skipped by the dry-run mount heuristic, so no
    // finding is forced.
    use crate::sys::ops::dryrun::{ClosureView, DryRunScenario, DryRunSys};

    let cfg = Config::recovery_default();
    let vol = priority_vol(PathBuf::from("/dev/none"), true);
    let mut fs = DryRunSys::new(
        ClosureView::new(PathBuf::from("/")),
        DryRunScenario::NormalBoot,
    );
    let attested =
        super::mount_priority_volume(&mut fs, GatePhase::PostUnlock, &with_boot_mp(cfg), &vol)
            .expect("dry-run post-unlock mount succeeds (no-op)");
    assert!(
        attested.owned_mount.is_none(),
        "a dry-run mount must leave no owned mount for Drop to tear down",
    );
    drop(attested); // must not attempt a real umount
}

// ---- (b) bad / missing signature: REFUSE ----------------------------------

#[test]
fn tampered_signature_is_rejected_by_the_core() {
    let signer = Signer65::new(8);
    let cfg = enforcing_cfg(&[]);
    let d = digest(0x22);
    let mut sc = signer.sidecar(&d, DOMAIN_PRIORITY_FILE);
    let last = sc.len() - 1;
    sc[last] ^= 0x01;
    let baked = [signer.baked_key()];
    let err = verify_priority_against(&cfg, &d, &sc, &baked).unwrap_err();
    assert!(matches!(err, NmblError::Signature { .. }));
}

#[test]
fn a_missing_priority_file_under_enforce_defers_a_refuse() {
    // FIX-35: the pre-console gate does NOT relock/refuse inline — it returns
    // Err(PolicyRefused) so the shared run_tui_session Err arm renders the
    // countdown. A missing signed file is a hard refuse under enforcement.
    let dir = temp_dir("missing");
    let mut cfg = enforcing_cfg(&[]);
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    cfg.secure_boot.signed_file_path = PathBuf::from("nmbl/priority.bin");
    // A non-inside-LUKS volume so the PrePlainBoot phase applies.
    cfg.secure_boot.priority_volume = Some(priority_vol(PathBuf::from("/dev/none"), false));

    let mut fs = RealSys::sync_only();
    let err = run_priority_gate_at(&mut fs, GatePhase::PrePlainBoot, &cfg).unwrap_err();
    assert!(
        matches!(err, NmblError::PolicyRefused { .. }),
        "a missing priority file must DEFER a PolicyRefused, not refuse inline"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- (FIX-08) FULL-fingerprint narrowing -----------------------------------

#[test]
fn a_key_matching_only_a_short_prefix_is_rejected() {
    // FIX-08: narrowing is on the FULL 32-byte fingerprint. An allowed id equal
    // to a short PREFIX of the signing key's fingerprint (padded to 32 bytes)
    // does NOT match, so the key is narrowed OUT and the verify fails.
    let signer = Signer65::new(9);
    let real_fp = signer.full_fp();
    // Build an allowed id that shares the first 4 bytes (the key_id hint width)
    // but differs in the rest — i.e. matches only a short prefix.
    let mut prefix_only = [0u8; 32];
    prefix_only[..4].copy_from_slice(&real_fp[..4]);
    assert_ne!(prefix_only, real_fp, "prefix id differs in the tail");

    let cfg = enforcing_cfg(&[crate::util::hex::hex_lower(&prefix_only)]);
    let d = digest(0x33);
    let sc = signer.sidecar(&d, DOMAIN_PRIORITY_FILE);
    let baked = [signer.baked_key()];
    let err = verify_priority_against(&cfg, &d, &sc, &baked).unwrap_err();
    assert!(
        matches!(
            err,
            NmblError::Signature {
                stage: "no-valid-key",
                ..
            }
        ),
        "a prefix-only fingerprint must narrow the key OUT: {err:?}"
    );

    // Sanity: the FULL fingerprint in the allow-list DOES verify.
    let cfg_full = enforcing_cfg(&[crate::util::hex::hex_lower(&real_fp)]);
    verify_priority_against(&cfg_full, &d, &sc, &baked)
        .expect("the full fingerprint narrows the key IN");
}

#[test]
fn a_malformed_allowed_key_id_is_a_hard_error() {
    // A non-hex / wrong-length allowed_key_id must NOT silently widen trust.
    let signer = Signer65::new(10);
    let cfg = enforcing_cfg(&["not-hex".to_string()]);
    let d = digest(0x44);
    let sc = signer.sidecar(&d, DOMAIN_PRIORITY_FILE);
    let baked = [signer.baked_key()];
    let err = verify_priority_against(&cfg, &d, &sc, &baked).unwrap_err();
    assert!(matches!(
        err,
        NmblError::Signature {
            stage: "priority-allowed-key-id",
            ..
        }
    ));
}

// ---- (FIX-01) domain pinned to priority-file:v1 ----------------------------

#[test]
fn a_cross_domain_signature_is_rejected() {
    // A signature minted under another role's domain must NOT verify under the
    // priority-file domain, even with the right key + digest.
    use crate::sig::DOMAIN_STAGED_FRAGMENT;
    let signer = Signer65::new(11);
    let cfg = enforcing_cfg(&[]);
    let d = digest(0x55);
    // Signed under the STAGED-FRAGMENT domain.
    let sc = signer.sidecar(&d, DOMAIN_STAGED_FRAGMENT);
    let baked = [signer.baked_key()];
    let err = verify_priority_against(&cfg, &d, &sc, &baked).unwrap_err();
    assert!(
        matches!(
            err,
            NmblError::Signature {
                stage: "domain-mismatch",
                ..
            }
        ),
        "a staged-fragment signature must not pass the priority-file gate: {err:?}"
    );

    // Sanity: under its own priority-file domain the same key verifies.
    let sc_ok = signer.sidecar(&d, DOMAIN_PRIORITY_FILE);
    verify_priority_against(&cfg, &d, &sc_ok, &baked).expect("the priority-file domain verifies");
}

// ---- posture: off skips; audit proceeds ------------------------------------

#[test]
fn disabled_secure_boot_skips_the_gate() {
    let cfg = Config::recovery_default(); // secure_boot.enable = false
    let mut fs = RealSys::sync_only();
    assert!(
        run_priority_gate_at(&mut fs, GatePhase::PrePlainBoot, &cfg)
            .unwrap()
            .is_none(),
        "an off posture skips the gate entirely"
    );
}

#[test]
fn the_phase_only_runs_for_its_own_volume_kind() {
    // An inside-LUKS volume is NOT handled by the PrePlainBoot phase (and vice
    // versa) — each hook owns one phase (FIX-34).
    let mut cfg = enforcing_cfg(&[]);
    cfg.secure_boot.priority_volume = Some(priority_vol(PathBuf::from("/dev/none"), true));
    let mut fs = RealSys::sync_only();
    assert!(
        run_priority_gate_at(&mut fs, GatePhase::PrePlainBoot, &cfg)
            .unwrap()
            .is_none(),
        "the pre-plain-boot phase skips an inside-LUKS volume"
    );
}

#[test]
fn audit_mode_proceeds_past_a_missing_file() {
    // enable && !enforce = audit: a missing file WARNs but proceeds (INSECURE),
    // returning a witness rather than refusing.
    let dir = temp_dir("audit");
    let mut cfg = Config::recovery_default();
    cfg.secure_boot.enable = true;
    cfg.secure_boot.enforce = false; // audit
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    cfg.secure_boot.signed_file_path = PathBuf::from("nmbl/priority.bin");
    cfg.secure_boot.priority_volume = Some(priority_vol(PathBuf::from("/dev/none"), false));

    let mut fs = RealSys::sync_only();
    let attested = run_priority_gate_at(&mut fs, GatePhase::PrePlainBoot, &cfg)
        .expect("audit mode must not refuse")
        .expect("audit mode proceeds with a witness");
    drop(attested);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- helpers ---------------------------------------------------------------

fn priority_vol(device: PathBuf, inside_luks: bool) -> PriorityVolume {
    PriorityVolume {
        device,
        mountpoint: PathBuf::from("/run/priority-test"),
        fstype: "ext4".to_string(),
        options: "ro,nosuid,nodev,noexec".to_string(),
        inside_luks,
    }
}

fn with_boot_mp(mut cfg: Config) -> Config {
    cfg.runtime_boot_mountpoint = Some(PathBuf::from("/run/boot-test"));
    cfg
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nmbl-gate-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}
