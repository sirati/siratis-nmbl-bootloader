//! The SEAL primitive: cap the lock PCR, THEN close every TPM-unsealed
//! LUKS mapper, and only then mint the unforgeable [`Sealed`] witness
//! (R-2 / FIX-03 / FIX-10 / FIX-27 / re-audit C-1). ALWAYS-COMPILED.
//!
//! ORDER is load-bearing and machine-checked: the cap MUST precede the
//! mapper close (a still-unsealable secret is worse than a live mapper,
//! so we poison the PCR first), and BOTH MUST precede any fork/execve
//! into an interactive context. The `nmbl-init-must-seal` flake check
//! enforces the seal-before-spawn shape; the [`super::tests`] call-ORDER
//! tests assert `cap-index < fork` AND `close-index < fork`.
//!
//! [`Sealed`] is a zero-sized token whose only constructor lives in this
//! module. The shell-spawn helpers and the `dispatch_execve` backstop
//! take a `&Sealed` (or `Sealed`) by value, so by type a shell cannot be
//! spawned without one.

use std::cell::Cell;

use crate::error::NmblError;
use crate::sys::poller::LocalSender;
use crate::tpm::CapOutcome;

use super::registry::{self, MapperEntry};

/// Unforgeable proof that [`seal_secrets`] (or [`seal_secrets_blocking`])
/// ran to completion: the lock PCR was capped (or there is provably no
/// TPM secret to protect) AND every TPM-unsealed LUKS mapper was closed.
///
/// The only constructor is private to this module ([`Sealed::mint`]), so
/// holding a `Sealed` is a compile-time guarantee the seal happened.
/// Threaded into every fork/execve shell-spawn helper so a shell cannot
/// be spawned without one (re-audit C-1).
#[derive(Clone, Copy, Debug)]
pub struct Sealed(());

impl Sealed {
    /// Mint the witness. Private: callers must go through
    /// [`seal_secrets`] / [`seal_secrets_blocking`], which only reach
    /// this after BOTH the cap and the mapper-close succeed.
    fn mint() -> Self {
        Sealed(())
    }

    /// Fabricate a witness for tests of the fork/execve primitives that
    /// the seal gates. Test-only — production code can ONLY obtain a
    /// `Sealed` from a real [`seal_secrets`] call.
    #[cfg(test)]
    #[must_use]
    pub fn test_witness() -> Self {
        Sealed(())
    }

    /// Mint a witness for the `--validate-initrm` dry-run ONLY.
    ///
    /// The dry-run drives the GENUINE shell-spawn control flow against
    /// [`crate::sys::ops::dryrun::DryRunSys`], whose `spawn_shell` runs the
    /// presence preflight and returns `DryRunShellPreflight` WITHOUT ever
    /// reaching the real fork/execve waist. The `Sealed` type therefore gates
    /// no real syscall on this path, so minting one here does NOT weaken C-1
    /// (which protects the real fork). Running the real
    /// [`seal_secrets_blocking`] in the dry-run is wrong: it would attempt a
    /// `cryptsetup close` side effect (and fail on a box without cryptsetup),
    /// which `--validate-initrm` must never do. `pub` only because the
    /// dry-run driver lives in the `nmbl-init` bin (a separate crate from this
    /// lib); the lib has no external downstream consumers, and the boot spine
    /// never calls this (it always goes through the real seal).
    #[must_use]
    pub fn dry_run_witness() -> Self {
        Sealed(())
    }
}

/// The seal could not complete: either the lock PCR is present but
/// uncappable (FIX-27 — a `Failed` cap diverts to refuse, NEVER a shell),
/// a required TPM is absent under `requireTpm`, or a TPM-unsealed mapper
/// could not be closed. Every guard site that receives a `SealFailed`
/// MUST divert to a non-interactive refuse/halt — NEVER offer a shell.
#[derive(Debug)]
pub struct SealFailed {
    /// Why the seal failed, for the refuse banner / logs.
    cause: NmblError,
}

impl SealFailed {
    fn new(cause: NmblError) -> Self {
        SealFailed { cause }
    }

    /// Consume the failure into the [`NmblError`] a divert-to-refuse path
    /// surfaces as the rescue cause.
    #[must_use]
    pub fn into_cause(self) -> NmblError {
        self.cause
    }

    /// Borrow the underlying cause (for logging without consuming).
    #[must_use]
    pub fn cause(&self) -> &NmblError {
        &self.cause
    }
}

impl std::fmt::Display for SealFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "seal-on-rescue failed: {}", self.cause)
    }
}

thread_local! {
    /// Cap-only latch: flips to `true` ONLY after the lock PCR is capped
    /// (the irreversible PCR poison-extend is idempotent — re-extending
    /// the same value is pointless and a `Failed` re-cap would wrongly
    /// fail a later seal). The CLOSE step is NOT gated by this latch: it
    /// re-drains the registry on EVERY seal so a mapper registered AFTER
    /// the first seal is still closed before the next interactive context
    /// (the C-1 masking hole). `Cell<bool>` per FIX-58 — never an atomic
    /// / `OnceLock`.
    static CAP_LATCH: Cell<bool> = const { Cell::new(false) };
}

// The dry-run seam reader + the real-op counter are only referenced from the
// production (`#[cfg(not(test))]`) seam below; under `cfg(test)` the seam is
// the recording `test_seam`, so both would be unused.
#[cfg(not(test))]
use super::seal_dryrun::{dry_run_seal_active, note_real_seal_op};

/// ASYNC seal for sites already inside the interactive [`LocalRuntime`]
/// (the emergency menu, the remote session, the picker/pretty-shell
/// waist). Caps the lock PCR FIRST, then closes every registered
/// TPM-unsealed mapper through the async fork/exec runner, then mints
/// [`Sealed`]. `require_tpm` decides the no-TPM posture (degrade-open
/// vs. fail-closed); a present-but-uncappable TPM ALWAYS fails closed.
pub async fn seal_secrets(require_tpm: bool, sender: &LocalSender) -> Result<Sealed, SealFailed> {
    // The CAP is idempotent-skippable (PCR already poison-extended); the
    // CLOSE always drains the current merged registry so a mapper opened
    // after an earlier seal — or surfaced from the on-disk file after a
    // panic re-exec — is still closed (C-1 / FIX-03).
    if !CAP_LATCH.with(Cell::get) {
        cap_step(require_tpm)?;
        CAP_LATCH.with(|l| l.set(true));
    }
    close_all_async(sender).await?;
    Ok(Sealed::mint())
}

/// BLOCKING seal for the synchronous terminal sites that run AFTER the
/// runtime has unwound (`rescue::dispatch`, `run_force_rescue`, the
/// `dispatch_execve` backstop). Same cap-then-close-then-mint contract
/// as [`seal_secrets`] but drives the mapper close through the blocking
/// fork/exec runner because there is no live runtime to await on.
pub fn seal_secrets_blocking(require_tpm: bool) -> Result<Sealed, SealFailed> {
    // Same split-latch contract as [`seal_secrets`]: cap once, close on
    // every call so a later-registered (or post-panic file-sourced)
    // mapper is never masked by the latch (C-1 / FIX-03).
    if !CAP_LATCH.with(Cell::get) {
        cap_step(require_tpm)?;
        CAP_LATCH.with(|l| l.set(true));
    }
    close_all_blocking()?;
    Ok(Sealed::mint())
}

/// BEST-EFFORT seal for the REFUSE terminus (R-1 / R-7 / FIX-10). Unlike
/// [`seal_secrets`], this NEVER returns an error: the refuse path is the
/// safe fail-closed action and must proceed even when the cap or a mapper
/// close did not confirm (a present-but-uncappable TPM, FIX-27). It still
/// performs the cap FIRST and then closes every registered mapper — both
/// best-effort — so the common case really does lock the TPM and tear down
/// the plaintext devices before the refuse countdown renders. The real
/// security boundary is the `reboot(RB_AUTOBOOT)` that follows (a reset
/// re-initialises every PCR), so a failed best-effort cap degrades safely:
/// we are rebooting immediately regardless. Returns the [`Sealed`] witness
/// so [`super::relock::relock_and_refuse`] can mint the type-gated
/// [`crate::terminal::TerminalAction::RebootIntoRescue`].
pub(super) fn seal_for_refuse_blocking(require_tpm: bool) -> Sealed {
    if !CAP_LATCH.with(Cell::get) {
        // Best-effort: a `SealFailed` from the cap step is logged and
        // swallowed — the refuse proceeds (and the imminent reboot is the
        // real lock boundary). On success latch so a later real seal skips
        // the redundant re-cap.
        if cap_step(require_tpm).is_ok() {
            CAP_LATCH.with(|l| l.set(true));
        }
    }
    // Drain the registry best-effort; a stuck mapper is logged inside
    // `close_one_blocking`'s caller and the entry stays registered, but we
    // do NOT abort the refuse for it.
    close_all_best_effort_blocking();
    Sealed::mint()
}

/// Async sibling of [`seal_for_refuse_blocking`] for the refuse paths that
/// run inside the interactive runtime (the priority-gate refuse, the
/// seal-failure diverts in the emergency menu). Same best-effort,
/// always-`Sealed` contract.
pub(super) async fn seal_for_refuse_async(require_tpm: bool, sender: &LocalSender) -> Sealed {
    if !CAP_LATCH.with(Cell::get) && cap_step(require_tpm).is_ok() {
        CAP_LATCH.with(|l| l.set(true));
    }
    close_all_best_effort_async(sender).await;
    Sealed::mint()
}

/// Close every registered mapper, swallowing per-mapper failures. A mapper
/// whose close fails stays registered (so a later real seal still fails
/// closed on it), but the refuse is never blocked.
fn close_all_best_effort_blocking() {
    for entry in registry::snapshot() {
        match close_one_blocking(&entry) {
            Ok(()) => registry::mark_closed(&entry.name),
            Err(e) => crate::nmbl_warn!(
                "refuse: best-effort close of mapper {} failed: {}; rebooting anyway",
                entry.name,
                e.cause()
            ),
        }
    }
}

/// Async sibling of [`close_all_best_effort_blocking`].
async fn close_all_best_effort_async(sender: &LocalSender) {
    for entry in registry::snapshot() {
        match close_one_async(&entry, sender).await {
            Ok(()) => registry::mark_closed(&entry.name),
            Err(e) => crate::nmbl_warn!(
                "refuse: best-effort close of mapper {} failed: {}; rebooting anyway",
                entry.name,
                e.cause()
            ),
        }
    }
}

/// Step 1 — cap the lock PCR (shared by both seal shapes). Maps the rich
/// [`CapOutcome`] onto the seal policy (R-7 / FIX-27):
/// * `Capped` ⇒ proceed.
/// * `NoTpm` ⇒ proceed IFF `!require_tpm` (degrade-open), else fail closed.
/// * `Failed` ⇒ ALWAYS fail closed (present-but-uncappable diverts to refuse).
fn cap_step(require_tpm: bool) -> Result<(), SealFailed> {
    match cap_lock_pcr_seam() {
        CapOutcome::Capped => Ok(()),
        // cap-exempt: NO TPM is present, so there is no lock PCR to cap and no
        // TPM-sealed secret to poison — the cap is vacuous, not skipped. The
        // posture is the operator's `requireTpm`: degrade-open when unset
        // (luks-tpm box with no TPM), fail-closed when set (FIX-28). A
        // present-but-uncappable TPM is `Failed`, never `NoTpm`, and ALWAYS
        // fails closed below — this arm can only widen on a provably TPM-less box.
        CapOutcome::NoTpm => {
            if require_tpm {
                Err(SealFailed::new(NmblError::TpmProto {
                    context: "seal_secrets".to_string(),
                    reason: "requireTpm is set but no TPM is present to cap the lock PCR"
                        .to_string(),
                }))
            } else {
                Ok(())
            }
        }
        CapOutcome::Failed(e) => Err(SealFailed::new(e)),
    }
}

/// Step 2 (async) — close every registered TPM-unsealed mapper. A close
/// that does not confirm leaves its mapper registered and the seal
/// `Err` (fail-closed). Only after the registry is empty does the seal
/// succeed.
async fn close_all_async(sender: &LocalSender) -> Result<(), SealFailed> {
    for entry in registry::snapshot() {
        close_one_async(&entry, sender).await?;
        registry::mark_closed(&entry.name);
    }
    debug_assert_eq!(
        registry::pending(),
        0,
        "seal must drain the mapper registry"
    );
    Ok(())
}

/// Step 2 (blocking) — synchronous sibling of [`close_all_async`].
fn close_all_blocking() -> Result<(), SealFailed> {
    for entry in registry::snapshot() {
        close_one_blocking(&entry)?;
        registry::mark_closed(&entry.name);
    }
    debug_assert_eq!(
        registry::pending(),
        0,
        "seal must drain the mapper registry"
    );
    Ok(())
}

/// `cryptsetup close <name>` argv. `close` releases the device-mapper
/// node and wipes the volume key from kernel memory, so the unsealed
/// plaintext device is gone before the shell can read it.
fn close_argv(entry: &MapperEntry) -> Vec<String> {
    vec!["close".to_string(), entry.name.clone()]
}

/// Turn a non-zero `cryptsetup close` exit into a `SealFailed`. Exit 0
/// is success; exit 4 ("device <name> is not active") also means the
/// mapper is gone, which is the post-condition we want, so it is treated
/// as success too.
fn close_outcome(name: &str, exit_code: i32) -> Result<(), SealFailed> {
    if exit_code == 0 || exit_code == 4 {
        Ok(())
    } else {
        Err(SealFailed::new(NmblError::Activation {
            kind: format!("luks-tpm seal-close {name} (exit {exit_code})"),
            source: Box::new(NmblError::Io {
                source: std::io::Error::other("cryptsetup close failed"),
                context: format!("seal close {name}"),
            }),
        }))
    }
}

// === Real-vs-test seams ===
//
// In production these call straight into the TPM cap and the activation
// runner. Under `#[cfg(test)]` they consult overridable hooks so the
// call-ORDER tests can drive the seal without a real TPM or cryptsetup
// while still observing that cap precedes close precedes fork.

#[cfg(not(test))]
fn cap_lock_pcr_seam() -> CapOutcome {
    // Property-6: a `--validate-initrm` seal MUST NOT poison the real lock
    // PCR. Route the cap through the side-effect-free `TpmOps` on a
    // `DryRunSys` so the irreversible extend is replaced by a recorded
    // no-op; only the real boot reaches `crate::tpm::cap_lock_pcr()`.
    if dry_run_seal_active() {
        use crate::sys::ops::TpmOps;
        use crate::sys::ops::dryrun::{ClosureView, DryRunScenario, DryRunSys};
        let mut dry = DryRunSys::new(
            ClosureView::new(std::path::PathBuf::from("/")),
            DryRunScenario::ErrorToErrorScreen,
        );
        return dry.cap_lock_pcr();
    }
    note_real_seal_op();
    crate::tpm::cap_lock_pcr()
}

#[cfg(not(test))]
async fn close_one_async(entry: &MapperEntry, sender: &LocalSender) -> Result<(), SealFailed> {
    // Property-6: the dry-run seal must run NO real `cryptsetup close`.
    if dry_run_seal_active() {
        return Ok(());
    }
    note_real_seal_op();
    let (outcome, _captured) =
        crate::sys::activation::run_capture(&entry.cryptsetup, &close_argv(entry), sender)
            .await
            .map_err(SealFailed::new)?;
    close_outcome(&entry.name, outcome.exit_code)
}

#[cfg(not(test))]
fn close_one_blocking(entry: &MapperEntry) -> Result<(), SealFailed> {
    // Property-6: the dry-run seal must run NO real `cryptsetup close`.
    if dry_run_seal_active() {
        return Ok(());
    }
    note_real_seal_op();
    let (outcome, _captured) =
        crate::sys::activation::run_capture_blocking(&entry.cryptsetup, &close_argv(entry))
            .map_err(SealFailed::new)?;
    close_outcome(&entry.name, outcome.exit_code)
}

#[cfg(test)]
pub(super) use test_seam::{cap_lock_pcr_seam, close_one_async, close_one_blocking};

/// Reset the cap-latch. Test-only. Declared BEFORE the test seam module
/// so clippy's `items_after_test_module` lint stays happy.
#[cfg(test)]
pub(super) fn reset_latch() {
    CAP_LATCH.with(|l| l.set(false));
}

#[cfg(test)]
#[path = "guard_test_seam.rs"]
pub(super) mod test_seam;
