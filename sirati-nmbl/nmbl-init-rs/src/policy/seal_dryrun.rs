//! Property-6 dry-run seal seam: route the `--validate-initrm` seal's
//! hardware effects through a side-effect-free path.
//!
//! `--validate-initrm`'s `ErrorToErrorScreen` scenario drives the GENUINE
//! [`drop_to_emergency`](crate::shell::drop_to_emergency) →
//! [`seal_secrets`](super::seal_secrets) control flow. Left unguarded, that
//! seal would cap the REAL lock PCR (an irreversible poison-extend) and run a
//! REAL `cryptsetup close` on a TPM host. Neither is acceptable for a
//! validation run.
//!
//! [`DryRunSealScope`] flips a thread-local so [`super::guard`]'s seam routes
//! the cap through the side-effect-free
//! [`TpmOps::cap_lock_pcr`](crate::sys::ops::TpmOps::cap_lock_pcr) on a
//! [`DryRunSys`](crate::sys::ops::dryrun::DryRunSys) (which NEVER opens a TPM
//! nor poisons a PCR) and no-ops the mapper close. The real boot never enters
//! the scope, so its path is byte-identical. A separate counter records every
//! REAL hardware seal op so the Property-6 test can assert a dry-run performs
//! ZERO of them.

use std::cell::Cell;

thread_local! {
    /// Property-6 dry-run seal mode. When set, [`super::guard`]'s cap routes
    /// through the side-effect-free `TpmOps::cap_lock_pcr` on a `DryRunSys`
    /// and the mapper close no-ops. The `--validate-initrm`
    /// `ErrorToErrorScreen` scenario sets this around its `drop_to_emergency`
    /// run. Real boot never sets it. `Cell<bool>` per FIX-58.
    static DRY_RUN_SEAL: Cell<bool> = const { Cell::new(false) };

    /// Count of REAL hardware seal ops attempted (a real `cap_lock_pcr` TPM
    /// touch, or a real `cryptsetup close` exec). Incremented ONLY on the
    /// non-dry-run seam path, so the Property-6 test can assert a
    /// `--validate-initrm` run performs ZERO of them. Pure observability,
    /// inert on the real boot path. `Cell<u32>` per FIX-58.
    static REAL_SEAL_OPS: Cell<u32> = const { Cell::new(0) };
}

/// RAII scope putting the seal into Property-6 dry-run mode (cap routes
/// through `TpmOps` on a `DryRunSys` no-op; mapper close no-ops). Held by the
/// `--validate-initrm` scenario driver around the genuine `drop_to_emergency`
/// run; the previous value is restored on drop so nested/sequential scenarios
/// compose. NEVER constructed on the real boot path.
#[must_use]
pub struct DryRunSealScope {
    prev: bool,
}

impl DryRunSealScope {
    /// Enter dry-run seal mode, remembering the prior flag for restore.
    pub fn enter() -> Self {
        let prev = DRY_RUN_SEAL.with(Cell::get);
        DRY_RUN_SEAL.with(|f| f.set(true));
        Self { prev }
    }
}

impl Drop for DryRunSealScope {
    fn drop(&mut self) {
        DRY_RUN_SEAL.with(|f| f.set(self.prev));
    }
}

/// `true` when the seal is running in Property-6 dry-run mode. Read only by
/// the production seam in [`super::guard`]; under `cfg(test)` the recording
/// `test_seam` replaces that seam, so this reader is then unused.
#[cfg(not(test))]
pub(super) fn dry_run_seal_active() -> bool {
    DRY_RUN_SEAL.with(Cell::get)
}

/// Number of REAL hardware seal ops (TPM cap + `cryptsetup close`) attempted
/// on this thread since the last [`reset_real_seal_ops`]. The Property-6 test
/// asserts a dry-run `--validate-initrm` run leaves this at zero.
#[must_use]
pub fn real_seal_ops() -> u32 {
    REAL_SEAL_OPS.with(Cell::get)
}

/// Reset the real-seal-op counter (for a test to measure a single run).
pub fn reset_real_seal_ops() {
    REAL_SEAL_OPS.with(|c| c.set(0));
}

/// Record one real hardware seal op (cap or close). No-op accounting on the
/// real boot path; read back by the Property-6 dry-run-side-effect test.
#[cfg(not(test))]
pub(super) fn note_real_seal_op() {
    REAL_SEAL_OPS.with(|c| c.set(c.get().saturating_add(1)));
}
