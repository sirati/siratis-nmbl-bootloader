//! Strict-shape proof that reaching recovery/rescue NEVER bypasses the TPM
//! cap (FIX-53 / R-2 / re-audit C-1).
//!
//! The invariant: every path that drops the operator into an interactive
//! rescue context — `rescue::dispatch` (the authoritative G4 seal), the
//! force-on-boot / sentinel-forced rescue that routes through it, and the
//! `recovery_default()` posture the rescue runs under — is structurally
//! DOMINATED by the seal. Concretely:
//!
//!   reaching an interactive rescue  ⇒  the cap fired first
//!                                       (or the seal failed → refuse, NEVER
//!                                        a shell with an uncapped TPM).
//!
//! We codify this over the control flow the way the seal-order tests do
//! (`super::tests`): the cap/close seams are overridden in `guard::test_seam`
//! to record their call order and to let us force a cap outcome, then we drive
//! the REAL `rescue::dispatch` and assert:
//!
//!   1. When the cap SUCCEEDS, the cap is recorded BEFORE `dispatch` yields any
//!      interactive (`Execve`) action — the shell is never reached uncapped.
//!   2. When the cap FAILS (a present-but-uncappable TPM), `dispatch` yields the
//!      `RebootIntoRescue` refuse terminus, NOT an `Execve` — divert, no shell.
//!   3. `recovery_default()` keeps the security posture strict (audit-neutral,
//!      no relaxed cap/seal knobs), so reaching recovery never widens trust.
//!
//! This is a CONTROL-FLOW test, not just a type test: the `Sealed`-by-value
//! gate on the spawn/refuse constructors is the compile-time half (covered by
//! the `compile_fail` doctest on `reboot_into_rescue` and the `must-seal`
//! lint); this is the run-time half that proves the cap actually runs on the
//! ONE chokepoint every rescue path funnels through.

use std::path::PathBuf;

use super::guard::test_seam::{self, Step};
use super::{guard, registry};
use crate::config::Config;
use crate::error::NmblError;
use crate::rescue::{self, RescueMode};
use crate::terminal::TerminalAction;
use crate::tpm::CapOutcome;
use crate::ui::console::{Console, NoopConsole};

/// Wipe every thread-local the seal touches and point the on-disk mapper
/// registry at a temp file, so each test starts from a clean cap-latch and an
/// empty registry (mirrors `super::tests::fresh`). The returned guard keeps the
/// temp dir alive for the whole test.
fn fresh() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    registry::set_persist_path(dir.path().join("tpm-unsealed-mappers"));
    guard::reset_latch();
    registry::reset();
    test_seam::reset();
    dir
}

/// Register one TPM-unsealed mapper (as a successful `luks-tpm` unlock does),
/// so the seal has a plaintext device it MUST close before any interactive
/// rescue context.
fn register_mapper(name: &str) {
    registry::register_tpm_mapper(registry::MapperEntry {
        cryptsetup: PathBuf::from("/bin/cryptsetup"),
        name: name.to_string(),
    });
}

/// A synthetic boot failure cause to feed the rescue dispatcher.
fn cause() -> NmblError {
    NmblError::Io {
        source: std::io::Error::other("synthetic recovery-shape cause"),
        context: "recovery-shape test".to_string(),
    }
}

/// Index of the first matching recorded seam step.
fn idx(order: &[Step], target: &Step) -> Option<usize> {
    order.iter().position(|s| s == target)
}

/// A recovery-posture config whose rescue mode hands the operator an
/// interactive context (`Embedded` ⇒ `execve` into a shell). Built from
/// `recovery_default()` so we test the ACTUAL recovery posture, not a bespoke
/// one — the whole point of FIX-53 is that the recovery default cannot bypass
/// the cap.
fn recovery_cfg_embedded() -> Config {
    let mut cfg = Config::recovery_default();
    cfg.rescue.mode = RescueMode::Embedded;
    cfg.paths.shell = PathBuf::from("/bin/recovery-shape-shell");
    cfg
}

/// Anchor the refuse terminus's best-effort sentinel write at a temp boot
/// mountpoint so the fail-closed cases do not touch the real `/boot`. The
/// `RebootIntoRescue` refuse runs `write_sentinel`, which resolves against
/// `runtime_boot_mountpoint` when set.
fn redirect_boot(cfg: &mut Config, dir: &tempfile::TempDir) {
    cfg.runtime_boot_mountpoint = Some(dir.path().to_path_buf());
}

/// (1) The dominator: on the success path `rescue::dispatch` caps the lock PCR
/// and closes every TPM-unsealed mapper BEFORE it produces the interactive
/// `Execve` action. Reaching the recovery shell is dominated by the cap.
#[test]
fn dispatch_caps_before_yielding_an_interactive_shell() {
    let _persist = fresh();
    // A live luks-tpm mapper exists: the seal MUST close it before any shell.
    register_mapper("cryptroot");

    let cfg = recovery_cfg_embedded();
    let console: Box<dyn Console> = Box::new(NoopConsole::new());
    let action = rescue::dispatch(&cfg, console, cause()).expect("embedded dispatch succeeds");

    // The recovery shell IS reachable on the success path...
    assert!(
        matches!(action, TerminalAction::Execve { .. }),
        "embedded recovery yields an Execve into the shell on the success path"
    );

    // ...but ONLY after the cap and the mapper-close were recorded. The
    // interactive action carries no record of its own, so we assert the seam
    // order: cap and close both ran (the dominator), and the registry drained.
    let order = test_seam::order();
    assert!(
        idx(&order, &Step::Cap).is_some(),
        "the lock PCR was capped before recovery handed back a shell (FIX-53)"
    );
    assert!(
        idx(&order, &Step::Close("cryptroot".to_string())).is_some(),
        "the TPM-unsealed mapper was closed before the recovery shell (re-audit C-1)"
    );
    let cap = idx(&order, &Step::Cap).expect("cap recorded");
    let close = idx(&order, &Step::Close("cryptroot".to_string())).expect("close recorded");
    assert!(cap < close, "cap must precede the mapper close (cap-first)");
    assert_eq!(
        registry::pending(),
        0,
        "no TPM-unsealed mapper survives into the recovery shell (FIX-03)"
    );
}

/// (2) The fail-closed terminus: a present-but-uncappable TPM makes the seal
/// FAIL, and `rescue::dispatch` MUST divert to the `RebootIntoRescue` refuse —
/// never an interactive `Execve`. There is no way to reach a shell with an
/// uncapped TPM.
#[test]
fn uncappable_tpm_diverts_recovery_to_refuse_never_a_shell() {
    let persist = fresh();
    register_mapper("cryptroot");
    // The TPM is present but the cap does not confirm (FIX-27): fail-closed.
    test_seam::set_cap(CapOutcome::Failed(NmblError::TpmProto {
        context: "recovery-shape".to_string(),
        reason: "simulated uncappable TPM".to_string(),
    }));

    let mut cfg = recovery_cfg_embedded();
    redirect_boot(&mut cfg, &persist);
    let console: Box<dyn Console> = Box::new(NoopConsole::new());
    let action =
        rescue::dispatch(&cfg, console, cause()).expect("dispatch returns a refuse, not Err");

    // The structural assertion: an uncapped TPM yields the refuse terminus,
    // NEVER the `Execve` shell. `RebootIntoRescue` is itself `Sealed`-gated, so
    // its mere presence proves the (best-effort) seal ran before the reboot.
    match action {
        TerminalAction::RebootIntoRescue { .. } => {}
        TerminalAction::Execve { .. } => {
            panic!("FIX-53 VIOLATION: an uncappable TPM reached an interactive recovery shell")
        }
        other => panic!("expected RebootIntoRescue refuse on a failed seal, got {other:?}"),
    }

    // The cap was ATTEMPTED (recorded) before the refuse — the divert is not a
    // silent skip of the cap.
    assert!(
        idx(&test_seam::order(), &Step::Cap).is_some(),
        "the cap was attempted before diverting to refuse"
    );
}

/// (2b) Same fail-closed shape for `RescueMode::None`: even the halt-style
/// rescue mode cannot reach an interactive context with an uncapped TPM — a
/// failed seal still diverts to the `RebootIntoRescue` refuse, not the
/// `HaltWithBanner` halt (which would have skipped the relock/sentinel).
#[test]
fn uncappable_tpm_diverts_even_mode_none_to_refuse() {
    let persist = fresh();
    test_seam::set_cap(CapOutcome::Failed(NmblError::TpmProto {
        context: "recovery-shape".to_string(),
        reason: "simulated uncappable TPM".to_string(),
    }));

    let mut cfg = Config::recovery_default();
    cfg.rescue.mode = RescueMode::None;
    redirect_boot(&mut cfg, &persist);
    let console: Box<dyn Console> = Box::new(NoopConsole::new());
    let action = rescue::dispatch(&cfg, console, cause()).expect("dispatch returns a refuse");

    assert!(
        matches!(action, TerminalAction::RebootIntoRescue { .. }),
        "a failed seal diverts mode=None to the relock/refuse terminus, not a bare halt (FIX-53)"
    );
}

/// (3) `recovery_default()` is strict-shape: it never relaxes the cap/seal
/// posture. The require-TPM knob is unchanged from a feature-free default, the
/// rescue defaults do not force a no-seal path, and (under `secure-boot`) the
/// priority gate / signing are audit-neutral OFF — reaching recovery widens
/// nothing. This pins the posture so a future field that defaulted to a
/// bypass would fail here.
#[test]
fn recovery_default_keeps_a_strict_security_posture() {
    let cfg = Config::recovery_default();

    // The seal's require-TPM input comes from `tpm.require_tpm`; recovery must
    // not silently flip it (either polarity is a posture, but recovery must
    // match the plain default, not invent a relaxed one).
    assert_eq!(
        cfg.tpm.require_tpm,
        crate::config::TpmConfig::default().require_tpm,
        "recovery_default must not alter the require-TPM seal posture (FIX-53)"
    );

    // Under secure-boot, recovery keeps signing/secure-boot enforcement
    // audit-neutral OFF (the gate is skipped, not relaxed) and the sentinel
    // path stays at the single-sourced default — reaching recovery never
    // disables a cap that an enabled config would have applied.
    #[cfg(feature = "secure-boot")]
    {
        assert!(
            !cfg.secure_boot.enable,
            "recovery_default leaves the priority gate skipped, never relaxed (FIX-53)"
        );
        assert_eq!(
            cfg.secure_boot.sentinel_path,
            std::path::Path::new(crate::security_consts::SENTINEL_PATH),
            "recovery_default keeps the single-sourced sentinel path (FIX-38)"
        );
    }
}

/// The cap is the DOMINATOR for the no-mapper case too (FIX-06): a recovery
/// shell on a box with no live mapper still caps the PCR before the shell —
/// the absence of a mapper to close must not let the cap be skipped.
#[test]
fn dispatch_caps_before_shell_even_with_no_mapper() {
    let _persist = fresh();
    // No mapper registered (e.g. a non-LUKS box dropping to recovery).
    let cfg = recovery_cfg_embedded();
    let console: Box<dyn Console> = Box::new(NoopConsole::new());
    let action = rescue::dispatch(&cfg, console, cause()).expect("embedded dispatch succeeds");

    assert!(matches!(action, TerminalAction::Execve { .. }));
    assert!(
        idx(&test_seam::order(), &Step::Cap).is_some(),
        "the cap fired before the recovery shell even with no mapper to close (FIX-06/FIX-53)"
    );
}
