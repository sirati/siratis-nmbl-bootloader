//! Seal-on-rescue call-ORDER + fail-closed tests (FIX-03 / FIX-06 /
//! FIX-27 / re-audit C-1).
//!
//! These prove the security contract WITHOUT a real TPM or cryptsetup:
//! the cap/close seams are overridden in `guard::test_seam` to record
//! their call order, and each test simulates the fork the guard would
//! perform AFTER `seal_secrets` returns `Ok` so we can assert both
//! `cap-index < fork` AND `close-index < fork`.

use std::io::Write as _;
use std::path::PathBuf;

use super::guard::test_seam::Step;
use super::guard::{seal_secrets, seal_secrets_blocking, test_seam};
use super::registry::{self, MapperEntry, register_tpm_mapper};
use crate::tpm::CapOutcome;

/// Point the on-disk mapper registry at a per-test temp file so the
/// re-exec survival tests don't touch `/run`. Returns the guard so the
/// temp dir lives for the whole test.
fn redirect_persist() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    registry::set_persist_path(dir.path().join("tpm-unsealed-mappers"));
    dir
}

/// Wipe every thread-local the seal touches so each test starts clean:
/// the cap-latch, the mapper registry, and the seam's order log /
/// cap result / close-fail set. Redirects the on-disk registry to a temp
/// file so no test pollutes `/run`.
fn fresh() -> tempfile::TempDir {
    let dir = redirect_persist();
    super::guard::reset_latch();
    registry::reset();
    test_seam::reset();
    dir
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
    let _persist = fresh();
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
    let _persist = fresh();
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
    let _persist = fresh();
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
    let _persist = fresh();
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
    let _persist = fresh();
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
    let _persist = fresh();
    test_seam::set_cap(CapOutcome::NoTpm);
    // require_tpm = false ⇒ degrade-open (Ok), even with no TPM.
    assert!(
        seal_secrets_blocking(false).is_ok(),
        "no-TPM degrades open when requireTpm is off"
    );

    let _persist = fresh();
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
    let _persist = fresh();
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

/// FIX-58: with the mapper already closed by the first seal, a second
/// seal records NOTHING new — the cap is latched (no re-cap) and the now
/// empty registry yields no further close — so re-entering the emergency
/// menu after a shell exits does not error or double-cap.
#[test]
fn cap_latch_makes_second_seal_no_op_when_drained() {
    let _persist = fresh();
    register("cryptroot");
    let _first = seal_secrets_blocking(false).expect("first seal");
    let after_first = test_seam::order().len();
    let _second = seal_secrets_blocking(false).expect("second seal short-circuits");
    let after_second = test_seam::order().len();
    assert_eq!(
        after_first, after_second,
        "latched seal must not re-record cap/close once the registry is drained"
    );
}

/// The cap-latch makes only the CAP idempotent — the CLOSE step ALWAYS
/// drains the current registry. A mapper registered AFTER the first seal
/// latched must still be closed by the next seal (the C-1 masking hole).
#[test]
fn second_seal_closes_a_mapper_registered_after_the_first() {
    let _persist = fresh();
    register("cryptroot");
    let _first = seal_secrets_blocking(false).expect("first seal closes cryptroot");
    assert_eq!(registry::pending(), 0, "first seal drains its mapper");

    // A NEW mapper opens after the first seal already latched the cap.
    register("crypthome");
    assert_eq!(registry::pending(), 1, "the late mapper is registered");

    let _second = seal_secrets_blocking(false).expect("second seal closes the late mapper");
    let order = test_seam::order();
    // Exactly ONE cap (the second seal skipped it) and a close for the
    // late mapper.
    assert_eq!(
        order.iter().filter(|s| **s == Step::Cap).count(),
        1,
        "cap is idempotent-skippable: only the first seal caps"
    );
    assert!(
        idx(&order, &Step::Close("crypthome".to_string())).is_some(),
        "the second seal must close the late-registered mapper (C-1)"
    );
    assert_eq!(
        registry::pending(),
        0,
        "late mapper is GONE after the second seal"
    );
}

/// MED-2 / FIX-03: a mapper opened in a PRIOR (pre-panic) process image
/// survives only on the on-disk registry file. A fresh process + empty
/// in-memory registry must MERGE that file and close the file-sourced
/// mapper on seal — proving the post-panic emergency shell sees no live
/// TPM-unsealed plaintext.
#[test]
fn seal_closes_a_mapper_sourced_only_from_the_persist_file() {
    let dir = fresh();
    // Simulate the re-exec: the pre-panic process wrote the mapper line;
    // the resumed process's in-memory registry is empty.
    let path = dir.path().join("tpm-unsealed-mappers");
    let mut f = std::fs::File::create(&path).expect("write persist file");
    writeln!(f, "/bin/cryptsetup\tcryptroot").expect("write line");
    drop(f);

    assert_eq!(
        registry::pending(),
        1,
        "the file-sourced mapper is visible to the fresh process"
    );

    let _sealed = seal_secrets_blocking(false).expect("seal closes the file-sourced mapper");
    test_seam::record_fork();

    let order = test_seam::order();
    let close =
        idx(&order, &Step::Close("cryptroot".to_string())).expect("file-sourced mapper closed");
    let fork = idx(&order, &Step::Fork).expect("fork recorded");
    assert!(
        close < fork,
        "file-sourced mapper closed before any shell (MED-2)"
    );
    assert_eq!(
        registry::pending(),
        0,
        "the persist file is cleared once its mapper is closed"
    );
    assert!(
        !path.exists(),
        "the persist file is deleted after the last close"
    );
}

/// MED-2 fail-closed: an unconfirmed close of a file-sourced mapper keeps
/// the persist-file line intact AND returns `Err` — the re-exec survival
/// path is fail-closed, never silently dropped.
#[test]
fn unconfirmed_close_keeps_the_persist_line_and_errs() {
    let dir = fresh();
    let path = dir.path().join("tpm-unsealed-mappers");
    let mut f = std::fs::File::create(&path).expect("write persist file");
    writeln!(f, "/bin/cryptsetup\tcryptroot").expect("write line");
    drop(f);

    test_seam::fail_close("cryptroot");
    let result = seal_secrets_blocking(false);
    assert!(
        result.is_err(),
        "an unconfirmed close fails the seal (MED-2)"
    );
    assert_eq!(
        registry::pending(),
        1,
        "the un-closable file-sourced mapper stays registered (fail-closed)"
    );
    let body = std::fs::read_to_string(&path).expect("persist file still present");
    assert!(
        body.contains("cryptroot"),
        "the persist-file line survives an unconfirmed close, got {body:?}"
    );
}
