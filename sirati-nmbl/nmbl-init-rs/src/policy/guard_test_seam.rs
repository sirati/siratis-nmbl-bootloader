//! Overridable cap/close seams + a shared call-ORDER log. The tests
//! install a cap outcome and a per-mapper close result, then assert
//! the recorded order (cap, then each close) and that the SUBSEQUENT
//! fork the test performs lands after both.
//!
//! Split out of `guard.rs` (the `#[cfg(test)] mod test_seam`) to keep that
//! file under the size limit; included via `#[path]` so `super::` still
//! resolves to the `guard` module.

use std::cell::RefCell;

use super::{MapperEntry, SealFailed, close_argv, close_outcome};
use crate::error::NmblError;
use crate::sys::poller::LocalSender;
use crate::tpm::CapOutcome;

/// One recorded seam invocation, in call order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `seal_secrets` capped the lock PCR.
    Cap,
    /// `seal_secrets` closed mapper `<name>`.
    Close(String),
    /// The TEST simulated a fork/execve into a shell (recorded by the
    /// test after `seal_secrets` returned `Ok`).
    Fork,
    /// `relock_and_refuse` wrote the rescue sentinel (recorded by the
    /// relock ORDER test so it can assert sentinel-write < relock).
    Sentinel,
    /// `relock_and_refuse` ran the LUKS/LVM/mdraid relock loop.
    Relock,
}

thread_local! {
    static ORDER: RefCell<Vec<Step>> = const { RefCell::new(Vec::new()) };
    static CAP_RESULT: RefCell<CapOutcome> = const { RefCell::new(CapOutcome::Capped) };
    /// Names whose close should FAIL (simulate a stuck mapper).
    static CLOSE_FAILS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Reset the order log, cap outcome, and close-fail set. Paired with
/// `super::reset()` (latch) and `registry::reset()` in each test.
pub fn reset() {
    ORDER.with(|o| o.borrow_mut().clear());
    CAP_RESULT.with(|c| *c.borrow_mut() = CapOutcome::Capped);
    CLOSE_FAILS.with(|f| f.borrow_mut().clear());
}

/// Make the next cap return `outcome`.
pub fn set_cap(outcome: CapOutcome) {
    CAP_RESULT.with(|c| *c.borrow_mut() = outcome);
}

/// Make `cryptsetup close <name>` fail in the test seam.
pub fn fail_close(name: &str) {
    CLOSE_FAILS.with(|f| f.borrow_mut().push(name.to_string()));
}

/// Record that the test performed a fork/execve into a shell.
pub fn record_fork() {
    ORDER.with(|o| o.borrow_mut().push(Step::Fork));
}

/// Record the relock ORDER test's sentinel-write step.
pub fn record_sentinel() {
    ORDER.with(|o| o.borrow_mut().push(Step::Sentinel));
}

/// Record the relock ORDER test's relock-loop step.
pub fn record_relock() {
    ORDER.with(|o| o.borrow_mut().push(Step::Relock));
}

/// Snapshot the recorded call order.
pub fn order() -> Vec<Step> {
    ORDER.with(|o| o.borrow().clone())
}

pub(in crate::policy) fn cap_lock_pcr_seam() -> CapOutcome {
    ORDER.with(|o| o.borrow_mut().push(Step::Cap));
    CAP_RESULT.with(|c| match &*c.borrow() {
        CapOutcome::Capped => CapOutcome::Capped,
        CapOutcome::NoTpm => CapOutcome::NoTpm,
        CapOutcome::Failed(_) => CapOutcome::Failed(NmblError::TpmProto {
            context: "test".to_string(),
            reason: "simulated uncappable TPM".to_string(),
        }),
    })
}

fn record_and_resolve_close(entry: &MapperEntry) -> Result<(), SealFailed> {
    ORDER.with(|o| o.borrow_mut().push(Step::Close(entry.name.clone())));
    let fails = CLOSE_FAILS.with(|f| f.borrow().iter().any(|n| n == &entry.name));
    // exit 0 = success, exit 1 = failure (mirrors the real exit-code
    // mapping in `close_outcome`); also exercises `close_argv`.
    let _ = close_argv(entry);
    close_outcome(&entry.name, if fails { 1 } else { 0 })
}

pub(in crate::policy) async fn close_one_async(
    entry: &MapperEntry,
    _sender: &LocalSender,
) -> Result<(), SealFailed> {
    record_and_resolve_close(entry)
}

pub(in crate::policy) fn close_one_blocking(entry: &MapperEntry) -> Result<(), SealFailed> {
    record_and_resolve_close(entry)
}
