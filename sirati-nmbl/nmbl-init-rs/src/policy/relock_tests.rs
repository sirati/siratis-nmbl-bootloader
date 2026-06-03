//! Tests for the relock-and-refuse terminus: the kind-aware relock argv
//! shapes (FIX-47) and the relock ORDER (cap < close < sentinel-write <
//! relock — FIX-10/FIX-03/FIX-21).

use std::path::PathBuf;

use super::argv::relock_argv;
use super::{refuse_require_tpm, relock_and_refuse, relock_and_refuse_blocking};
use crate::config::{Activation, ActivationKind, Config};
use crate::error::NmblError;
use crate::policy::guard::test_seam::Step;
use crate::policy::seal_dryrun::{DryRunSealScope, real_terminus_ops, reset_real_terminus_ops};
use crate::policy::{guard, registry};
use crate::terminal::TerminalAction;

/// Build a bare activation of `kind` that "produces" `devices`.
fn act(kind: ActivationKind, binary: &str, devices: &[&str]) -> Activation {
    Activation {
        kind,
        required_modules: Vec::new(),
        binary: PathBuf::from(binary),
        argv: Vec::new(),
        produces_devices: devices.iter().map(PathBuf::from).collect(),
        source_devices: Vec::new(),
        description: format!("{kind:?} test activation"),
        prompt_label: None,
        pass_to_stage1: None,
    }
}

#[test]
fn luks_relock_uses_cryptsetup_close_on_the_mapper_name() {
    let a = act(
        ActivationKind::LuksTpm,
        "/run/current-system/sw/bin/cryptsetup",
        &["/dev/mapper/cryptroot"],
    );
    let cmd = relock_argv(&a).expect("luks /dev/mapper device yields a relock");
    // Reuses the activation's own cryptsetup binary, strips the
    // /dev/mapper/ prefix (NOT a generic basename), exits 4 == inactive.
    assert_eq!(
        cmd.binary,
        PathBuf::from("/run/current-system/sw/bin/cryptsetup")
    );
    assert_eq!(cmd.argv, vec!["close".to_string(), "cryptroot".to_string()]);
    assert_eq!(cmd.absent_exit_code, 4);
}

#[test]
fn luks_relock_rejects_a_non_mapper_device() {
    // A by-id path on a LUKS activation is a mis-emitted config: we must
    // NOT guess a mapper name (FIX-47), so no command is produced.
    let a = act(
        ActivationKind::LuksPassword,
        "/bin/cryptsetup",
        &["/dev/disk/by-id/nvme-XYZ-part2"],
    );
    assert!(
        relock_argv(&a).is_none(),
        "by-id LUKS device must not relock"
    );
}

#[test]
fn luks_relock_rejects_a_malformed_mapper_device() {
    let a = act(
        ActivationKind::LuksKeyfile,
        "/bin/cryptsetup",
        &["/dev/mapper/"],
    );
    assert!(
        relock_argv(&a).is_none(),
        "empty mapper name must not relock"
    );
}

#[test]
fn lvm_relock_deactivates_the_vg_from_the_dev_vg_lv_form() {
    let a = act(ActivationKind::Lvm, "/bin/vgchange", &["/dev/vg0/root"]);
    let cmd = relock_argv(&a).expect("lvm /dev/<vg>/<lv> yields a relock");
    assert_eq!(cmd.binary, PathBuf::from("vgchange"));
    assert_eq!(cmd.argv, vec!["-an".to_string(), "vg0".to_string()]);
    assert_eq!(cmd.absent_exit_code, 0);
}

#[test]
fn lvm_relock_deactivates_the_vg_from_the_mapper_form() {
    // /dev/mapper/<vg>-<lv> with LVM's `--` escaping of a literal dash in
    // the VG name: `my--vg-root` => VG `my-vg`.
    let a = act(
        ActivationKind::Lvm,
        "/bin/vgchange",
        &["/dev/mapper/my--vg-root"],
    );
    let cmd = relock_argv(&a).expect("lvm mapper form yields a relock");
    assert_eq!(cmd.argv, vec!["-an".to_string(), "my-vg".to_string()]);
}

#[test]
fn mdraid_relock_stops_the_md_node() {
    let a = act(ActivationKind::Mdraid, "/bin/mdadm", &["/dev/md0"]);
    let cmd = relock_argv(&a).expect("mdraid /dev/md* yields a relock");
    assert_eq!(cmd.binary, PathBuf::from("mdadm"));
    assert_eq!(cmd.argv, vec!["--stop".to_string(), "/dev/md0".to_string()]);
}

#[test]
fn lvm_relock_warns_and_returns_none_on_an_unparseable_shape() {
    // LOW-1: an LVM activation whose produced device yields no VG (e.g. a bare
    // /dev/mapper/ with no `-` separator) must take the loud-warn path and
    // return None — the same audible signal the LUKS arm gives, not a silent
    // no-relock.
    let a = act(
        ActivationKind::Lvm,
        "/bin/vgchange",
        &["/dev/mapper/noseparator"],
    );
    assert!(
        relock_argv(&a).is_none(),
        "an unparseable LVM device must not relock (and warns)"
    );
}

#[test]
fn mdraid_relock_rejects_a_non_md_device() {
    let a = act(ActivationKind::Mdraid, "/bin/mdadm", &["/dev/sda1"]);
    // LOW-1: this also takes the loud-warn path (no /dev/md* node found).
    assert!(relock_argv(&a).is_none(), "non-/dev/md* must not relock");
}

#[test]
fn lvm_and_mdraid_with_no_produced_devices_warn_and_return_none() {
    // No produced devices at all ⇒ the warn path fires and returns None for
    // both kinds (LOW-1: a mis-emitted activation is loud, not silent).
    let lvm = act(ActivationKind::Lvm, "/bin/vgchange", &[]);
    let md = act(ActivationKind::Mdraid, "/bin/mdadm", &[]);
    assert!(relock_argv(&lvm).is_none());
    assert!(relock_argv(&md).is_none());
}

#[test]
fn zfs_has_no_relock() {
    let a = act(ActivationKind::Zfs, "/bin/zpool", &["tank"]);
    assert!(
        relock_argv(&a).is_none(),
        "zfs is re-imported clean on reboot"
    );
}

#[test]
fn refuse_require_tpm_ors_the_two_tables() {
    let mut cfg = Config::recovery_default();
    assert!(!refuse_require_tpm(&cfg));
    cfg.tpm.require_tpm = true;
    assert!(refuse_require_tpm(&cfg));
}

/// Drive the FULL blocking refuse and assert the security ORDER:
/// cap (Cap) < close-mappers (Close) < sentinel-write (Sentinel) < relock
/// (Relock). The cap+close are observed through the guard test seam; the
/// sentinel + relock markers are pushed by `relock_and_refuse_blocking`
/// under `#[cfg(test)]`. No activations ⇒ the relock loop is a no-op but
/// still records its marker.
#[test]
fn relock_order_is_cap_then_close_then_sentinel_then_relock() {
    let dir = tempfile::tempdir().expect("tempdir");
    registry::set_persist_path(dir.path().join("mappers"));
    guard::reset_latch();
    registry::reset();
    guard::test_seam::reset();

    // One TPM-unsealed mapper so a Close step is recorded.
    registry::register_tpm_mapper(crate::policy::MapperEntry {
        cryptsetup: PathBuf::from("/bin/cryptsetup"),
        name: "cryptdata".to_string(),
    });

    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());

    let action = relock_and_refuse_blocking(
        &cfg,
        NmblError::Signature {
            stage: "gen-kernel",
            detail: "synthetic".to_string(),
        },
    );
    assert!(
        matches!(action, TerminalAction::RebootIntoRescue { .. }),
        "refuse must yield RebootIntoRescue"
    );

    let order = guard::test_seam::order();
    let idx = |s: &Step| order.iter().position(|x| x == s);
    let cap = idx(&Step::Cap).expect("cap recorded");
    let close = idx(&Step::Close("cryptdata".to_string())).expect("close recorded");
    let sentinel = idx(&Step::Sentinel).expect("sentinel recorded");
    let relock = idx(&Step::Relock).expect("relock recorded");
    assert!(cap < close, "cap must precede close ({order:?})");
    assert!(close < sentinel, "close must precede sentinel ({order:?})");
    assert!(
        sentinel < relock,
        "sentinel must precede relock ({order:?})"
    );

    // The sentinel file is on disk at the resolved path.
    assert!(
        crate::policy::sentinel_present(&cfg),
        "sentinel must be present after the refuse"
    );
}

/// Block on `fut` in a fresh single-thread local runtime (the async refuse
/// terminus needs a `LocalSender` and a reactor).
fn block<F: std::future::Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}

/// PROPERTY-6 (the destructive-terminus regression): driving the ASYNC refuse
/// terminus under a `DryRunSealScope` (the `--validate-initrm` mode) must
/// perform ZERO real terminus ops — NO real `/boot/nmbl` sentinel write and NO
/// real `cryptsetup close` / `vgchange -an` relock fork — even with a LUKS +
/// LVM `require_tpm` config that has BOTH relock shapes. It must STILL mint the
/// `RebootIntoRescue` terminus (the seam records "would relock/sentinel" and
/// returns the witness, exactly as the dry-run seal already does for the cap).
///
/// This is the non-vacuous seam proof: after the cap fix the four
/// `validate_initrm` scenarios no longer DIVERT to refuse, so this test drives
/// the terminus directly to confirm the seam no-ops it.
#[test]
fn dry_run_refuse_terminus_performs_no_real_sentinel_or_relock_ops() {
    let dir = tempfile::tempdir().expect("tempdir");
    registry::set_persist_path(dir.path().join("mappers"));
    guard::reset_latch();
    registry::reset();
    guard::test_seam::reset();

    // A relockable LUKS mapper AND an LVM volume group, so BOTH relock argv
    // shapes are produced — a real run would fork `cryptsetup close cryptroot`
    // and `vgchange -an vg0`.
    let mut cfg = Config::recovery_default();
    cfg.tpm.require_tpm = true;
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());
    cfg.activations.push(act(
        ActivationKind::LuksTpm,
        "/bin/cryptsetup",
        &["/dev/mapper/cryptroot"],
    ));
    cfg.activations.push(act(
        ActivationKind::Lvm,
        "/bin/vgchange",
        &["/dev/vg0/root"],
    ));

    reset_real_terminus_ops();
    // Enter the dry-run seal scope (the `--validate-initrm` mode), drive the
    // genuine async refuse terminus, and drop the scope.
    let action = {
        let _scope = DryRunSealScope::enter();
        block(relock_and_refuse(
            &cfg,
            NmblError::Signature {
                stage: "gen-kernel",
                detail: "dry-run terminus".to_string(),
            },
            &crate::sys::poller::build().1,
        ))
    };

    // The terminus still yields the type-gated RebootIntoRescue (the witness is
    // minted by the best-effort seal exactly as on the real path).
    assert!(
        matches!(action, TerminalAction::RebootIntoRescue { .. }),
        "the dry-run refuse must still mint RebootIntoRescue"
    );
    // ZERO real terminus ops: no sentinel write, no relock fork.
    assert_eq!(
        real_terminus_ops(),
        0,
        "the dry-run refuse terminus must perform NO real sentinel write and NO \
         real relock fork (routed through DryRunSys FsOps/ExecOps no-ops)"
    );
    // And NO sentinel file landed on disk under the resolved mountpoint.
    assert!(
        !crate::policy::sentinel_present(&cfg),
        "the dry-run refuse must NOT write a real /boot/nmbl sentinel"
    );
}

/// The real-boot-inert proof (invariant #1): with NO dry-run scope active, the
/// async terminus seam takes the REAL branch and bumps the real-terminus
/// counter (≥1 for the sentinel write). Pairs with the dry-run test above so a
/// regression that wires the seam backwards — inert on real boots — is caught.
/// Uses a config with NO activations so the real branch reaches only the
/// sentinel write (no real `cryptsetup`/`vgchange` fork happens in the test).
#[test]
fn real_refuse_terminus_runs_the_real_sentinel_op() {
    let dir = tempfile::tempdir().expect("tempdir");
    registry::set_persist_path(dir.path().join("mappers"));
    guard::reset_latch();
    registry::reset();
    guard::test_seam::reset();

    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());

    reset_real_terminus_ops();
    // NO DryRunSealScope: the seam must take the real branch.
    let action = block(relock_and_refuse(
        &cfg,
        NmblError::Signature {
            stage: "gen-kernel",
            detail: "real terminus".to_string(),
        },
        &crate::sys::poller::build().1,
    ));

    assert!(
        matches!(action, TerminalAction::RebootIntoRescue { .. }),
        "the real refuse must yield RebootIntoRescue"
    );
    assert!(
        real_terminus_ops() >= 1,
        "the real refuse terminus must perform the real sentinel op (counter \
         proves the seam is NOT inert on a real boot)"
    );
    assert!(
        crate::policy::sentinel_present(&cfg),
        "the real refuse must write the real sentinel"
    );
}

/// Even when the cap returns `Failed` (a present-but-uncappable TPM), the
/// refuse STILL proceeds: it closes the mapper, writes the sentinel, runs
/// the relock, and yields RebootIntoRescue (FIX-27). The refuse must never
/// abort on a cap failure.
#[test]
fn relock_proceeds_even_when_cap_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    registry::set_persist_path(dir.path().join("mappers"));
    guard::reset_latch();
    registry::reset();
    guard::test_seam::reset();
    guard::test_seam::set_cap(crate::tpm::CapOutcome::Failed(NmblError::TpmProto {
        context: "test".to_string(),
        reason: "uncappable".to_string(),
    }));

    registry::register_tpm_mapper(crate::policy::MapperEntry {
        cryptsetup: PathBuf::from("/bin/cryptsetup"),
        name: "cryptdata".to_string(),
    });

    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());

    let action = relock_and_refuse_blocking(
        &cfg,
        NmblError::Signature {
            stage: "gen-kernel",
            detail: "cap-fail".to_string(),
        },
    );
    assert!(
        matches!(action, TerminalAction::RebootIntoRescue { .. }),
        "a cap failure must NOT abort the refuse"
    );
    let order = guard::test_seam::order();
    assert!(order.contains(&Step::Cap), "cap was still attempted");
    assert!(
        order.contains(&Step::Close("cryptdata".to_string())),
        "the mapper is still closed after a cap failure"
    );
    assert!(
        crate::policy::sentinel_present(&cfg),
        "the sentinel is still written after a cap failure"
    );
}
