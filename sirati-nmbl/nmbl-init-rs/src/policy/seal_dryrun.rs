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

    /// Count of REAL refuse-TERMINUS ops attempted (a real `cryptsetup
    /// close` / `vgchange` / `mdadm` relock fork, or a real `/boot/nmbl`
    /// sentinel `write`/`create_dir_all`). Distinct from `REAL_SEAL_OPS`
    /// because the terminus's relock + sentinel are NOT seal ops and never
    /// touched that counter — which is exactly why the dry-run's destructive
    /// terminus leak went unobserved. Incremented ONLY when the async refuse
    /// terminus runs against the REAL ops (not the dry-run seam), so the
    /// Property-6 test can assert a `--validate-initrm` run performs ZERO of
    /// them. `Cell<u32>` per FIX-58.
    static REAL_TERMINUS_OPS: Cell<u32> = const { Cell::new(0) };
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

/// `true` when the seal is running in Property-6 dry-run mode. Read by the
/// production cap/close seam in [`super::guard`] AND by the async refuse
/// TERMINUS in [`super::relock`] (which routes its sentinel write + relock
/// forks through the side-effect-free `DryRunSys` ops when this is set). Under
/// `cfg(test)` the guard's recording `test_seam` replaces the guard seam; the
/// relock module reads this directly on both paths.
pub(super) fn dry_run_seal_active() -> bool {
    DRY_RUN_SEAL.with(Cell::get)
}

/// `true` when a `--validate-initrm` run holds a [`DryRunSealScope`]. The
/// public sibling of [`dry_run_seal_active`] for callers OUTSIDE the `policy`
/// module (the LUKS wrong-password recovery seam in
/// [`crate::activation`]), which must skip its real seal + real shell fork on
/// the dry-run path the same way the refuse terminus and the cap/close seam
/// do. Real boot never enters the scope, so this is always `false` there.
#[must_use]
pub fn validate_initrm_active() -> bool {
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

/// Number of REAL refuse-terminus ops (relock fork + sentinel write)
/// attempted on this thread since the last [`reset_real_terminus_ops`]. The
/// Property-6 test asserts a dry-run `--validate-initrm` run leaves this at
/// zero across ALL four scenarios — closing the hole the seal-op counter
/// (which the terminus never touched) left open.
#[must_use]
pub fn real_terminus_ops() -> u32 {
    REAL_TERMINUS_OPS.with(Cell::get)
}

/// Reset the real-terminus-op counter (for a test to measure a single run).
pub fn reset_real_terminus_ops() {
    REAL_TERMINUS_OPS.with(|c| c.set(0));
}

/// Record one real refuse-terminus op (a relock fork or a sentinel write).
/// Incremented only on the REAL terminus path (not the dry-run seam), so the
/// Property-6 test can assert a `--validate-initrm` run performs ZERO of them.
pub(super) fn note_real_terminus_op() {
    REAL_TERMINUS_OPS.with(|c| c.set(c.get().saturating_add(1)));
}
