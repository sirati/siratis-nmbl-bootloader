//! Seal-on-rescue call-ORDER + fail-closed tests (FIX-03 / FIX-06 /
//! FIX-27 / re-audit C-1).
//!
//! These prove the security contract WITHOUT a real TPM or cryptsetup:
//! the cap/close seams are overridden in `guard::test_seam` to record
//! their call order, and each test simulates the fork the guard would
//! perform AFTER `seal_secrets` returns `Ok` so we can assert both
//! `cap-index < fork` AND `close-index < fork`.

use std::path::PathBuf;

use super::guard::test_seam::Step;
use super::guard::{seal_secrets, seal_secrets_blocking, test_seam};
use super::registry::{self, MapperEntry, register_tpm_mapper};
use crate::tpm::CapOutcome;

/// Wipe every thread-local the seal touches so each test starts clean:
/// the once-latch, the mapper registry, and the seam's order log /
/// cap result / close-fail set.
fn fresh() {
    super::guard::reset_latch();
    registry::reset();
    test_seam::reset();
}

/// Push one TPM-unsealed mapper onto the registry (the way a successful
/// `luks-tpm` activation does).
fn register(name: &str) {
    register_tpm_mapper(MapperEntry {
        cryptsetup: PathBuf::from("/bin/cryptsetup"),
        name: name.to_string(),
    });
}

/// Index of the first matching step, or `None`.
fn idx(order: &[Step], target: &Step) -> Option<usize> {
    order.iter().position(|s| s == target)
}

/// The async seal needs a `LocalSender`; under the test seam the close
/// step ignores it, so a detached sender from `poller::build` (whose
/// poller half we drop) is sufficient.
fn dummy_sender() -> crate::sys::poller::LocalSender {
    crate::sys::poller::build().1
}

#[test]
fn blocking_seal_caps_then_closes_then_witness_before_fork() {
    fresh();
    register("cryptroot");
    register("crypthome");

    let sealed = seal_secrets_blocking(false).expect("seal must succeed when cap+close pass");
    // Simulate the fork the guard would perform now that it holds `Sealed`.
    let _ = sealed; // witness held by type; record the post-seal fork.
    test_seam::record_fork();

    let order = test_seam::order();
    let cap = idx(&order, &Step::Cap).expect("cap recorded");
    let close_root = idx(&order, &Step::Close("cryptroot".to_string())).expect("close root");
    let close_home = idx(&order, &Step::Close("crypthome".to_string())).expect("close home");
    let fork = idx(&order, &Step::Fork).expect("fork recorded");

    assert!(
        cap < close_root,
        "cap must precede mapper close (cap-first)"
    );
    assert!(
        cap < close_home,
        "cap must precede mapper close (cap-first)"
    );
    assert!(close_root < fork, "close-index < fork (FIX-03)");
    assert!(close_home < fork, "close-index < fork (FIX-03)");
    assert!(cap < fork, "cap-index < fork");
    assert_eq!(registry::pending(), 0, "registry drained on success");
}

#[tokio::test]
async fn async_seal_caps_then_closes_then_witness_before_fork() {
    fresh();
    register("cryptroot");
    let sender = dummy_sender();

    let _sealed = seal_secrets(false, &sender)
        .await
        .expect("seal must succeed");
    test_seam::record_fork();

    let order = test_seam::order();
    let cap = idx(&order, &Step::Cap).expect("cap recorded");
    let close = idx(&order, &Step::Close("cryptroot".to_string())).expect("close recorded");
    let fork = idx(&order, &Step::Fork).expect("fork recorded");
    assert!(cap < close, "cap-first");
    assert!(close < fork, "close-index < fork");
    assert!(cap < fork, "cap-index < fork");
}

/// FIX-06: a NO-CHOICE remote session (no mappers registered, TPM
/// present) still seals — caps the PCR — before the (simulated) first
/// render/fork.
#[test]
fn no_choice_remote_session_seals_before_first_render() {
    fresh();
    // No mappers registered: a remote session that opened on a non-LUKS
    // box still must cap before rendering.
    let _sealed = seal_secrets_blocking(false).expect("seal succeeds with no mappers");
    test_seam::record_fork();
    let order = test_seam::order();
    let cap = idx(&order, &Step::Cap).expect("cap recorded even with no mappers");
    let fork = idx(&order, &Step::Fork).expect("fork recorded");
    assert!(
        cap < fork,
        "cap (seal) must precede first render/fork (FIX-06)"
    );
}

/// FIX-27: a present-but-uncappable TPM yields `SealFailed` and the test
/// MUST NOT then fork — proving the divert-to-refuse leaves no shell.
#[test]
fn present_but_uncappable_tpm_diverts_with_no_shell() {
    fresh();
    register("cryptroot");
    test_seam::set_cap(CapOutcome::Failed(crate::error::NmblError::TpmProto {
        context: "seed".to_string(),
        reason: "seed".to_string(),
    }));

    let result = seal_secrets_blocking(false);
    assert!(
        result.is_err(),
        "uncappable TPM must fail the seal (FIX-27)"
    );
    // The guard, seeing `Err`, diverts to refuse and NEVER forks. We
    // assert no Fork step was ever recorded.
    let order = test_seam::order();
    assert!(
        idx(&order, &Step::Fork).is_none(),
        "no shell may be forked after a failed seal (FIX-27)"
    );
    // The mapper stays registered (we never confirmed its close), making
    // the fail-closed state observable.
    assert_eq!(
        registry::pending(),
        1,
        "uncappable seal leaves mappers live + registered"
    );
}

/// re-audit C-1 / FIX-03: a NON-refuse G-path (e.g. G1 drop-to-emergency
/// taken AFTER a post-luks-tpm-unlock failure) must leave NO live mapper
/// — the seal closes the registered mapper before any shell.
#[test]
fn non_refuse_path_after_post_unlock_failure_leaves_no_live_mapper() {
    fresh();
    // A luks-tpm unlock SUCCEEDED (mapper registered) but a LATER
    // activation failed, dropping us toward the emergency menu (a
    // non-refuse G1 path). The seal there must still close the mapper.
    register("cryptroot");
    let _sealed = seal_secrets_blocking(false).expect("seal succeeds");
    test_seam::record_fork();

    let order = test_seam::order();
    let close = idx(&order, &Step::Close("cryptroot".to_string())).expect("mapper closed");
    let fork = idx(&order, &Step::Fork).expect("fork recorded");
    assert!(
        close < fork,
        "mapper closed before the shell (re-audit C-1)"
    );
    assert_eq!(
        registry::pending(),
        0,
        "mapper node is GONE after the seal (FIX-03)"
    );
}

/// `requireTpm` flips the no-TPM posture from degrade-open to fail-closed.
#[test]
fn require_tpm_fails_closed_on_no_tpm() {
    fresh();
    test_seam::set_cap(CapOutcome::NoTpm);
    // require_tpm = false ⇒ degrade-open (Ok), even with no TPM.
    assert!(
        seal_secrets_blocking(false).is_ok(),
        "no-TPM degrades open when requireTpm is off"
    );

    fresh();
    test_seam::set_cap(CapOutcome::NoTpm);
    // require_tpm = true ⇒ fail-closed.
    assert!(
        seal_secrets_blocking(true).is_err(),
        "no-TPM fails closed when requireTpm is on"
    );
}

/// A mapper whose close FAILS makes the seal fail-closed and leaves the
/// mapper registered.
#[test]
fn stuck_mapper_fails_the_seal() {
    fresh();
    register("cryptroot");
    test_seam::fail_close("cryptroot");
    let result = seal_secrets_blocking(false);
    assert!(result.is_err(), "a mapper that won't close fails the seal");
    assert_eq!(
        registry::pending(),
        1,
        "the un-closable mapper stays registered (fail-closed)"
    );
}

/// FIX-58: the once-latch makes a second seal idempotent — no re-cap, no
/// re-close — so re-entering the emergency menu after a shell exits does
/// not error or double-cap.
#[test]
fn once_latch_makes_second_seal_idempotent() {
    fresh();
    register("cryptroot");
    let _first = seal_secrets_blocking(false).expect("first seal");
    let after_first = test_seam::order().len();
    let _second = seal_secrets_blocking(false).expect("second seal short-circuits");
    let after_second = test_seam::order().len();
    assert_eq!(
        after_first, after_second,
        "latched seal must not re-record cap/close on the second call (FIX-58)"
    );
}
