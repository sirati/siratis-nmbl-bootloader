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
            Ok(()) => {
                registry::mark_closed(&entry.name);
                crate::nmbl_info!("seal: closed TPM-unsealed mapper {}", entry.name);
            }
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
            Ok(()) => {
                registry::mark_closed(&entry.name);
                crate::nmbl_info!("seal: closed TPM-unsealed mapper {}", entry.name);
            }
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
        CapOutcome::Capped => {
            crate::nmbl_info!("seal: lock PCR capped");
            Ok(())
        }
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
        #[cfg(not(test))]
        detach_mapper_mounts(&entry.name);
        close_one_async(&entry, sender).await?;
        registry::mark_closed(&entry.name);
        crate::nmbl_info!("seal: closed TPM-unsealed mapper {}", entry.name);
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
        #[cfg(not(test))]
        detach_mapper_mounts(&entry.name);
        close_one_blocking(&entry)?;
        registry::mark_closed(&entry.name);
        crate::nmbl_info!("seal: closed TPM-unsealed mapper {}", entry.name);
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

/// Lazily detach every mount backed by `/dev/mapper/<name>` so the close
/// that follows is not refused as busy. A rescue forced AFTER phase 3b (the
/// embedded-config sentinel re-check, a later boot failure) finds the system
/// filesystems still mounted on the TPM-unsealed mapper; without this the
/// seal fails and a rescue sentinel can never be honoured on such a box.
///
/// Only the STRICT seal (the path into an interactive rescue) detaches. A
/// lazy detach also drops every mount below the target, including an
/// embedded-config `/boot`, and the best-effort REFUSE seal still has to write
/// the rescue sentinel there; it reboots right after, which is its lock
/// boundary.
///
/// Mounts are matched by the kernel's view (`/proc/self/mountinfo`, major:minor
/// of the mapper node), not by the configured device string, and detached
/// deepest-first. Best-effort: a detach failure is logged and the close
/// reports the real outcome.
#[cfg(not(test))]
fn detach_mapper_mounts(name: &str) {
    use std::os::unix::fs::MetadataExt as _;

    let node = std::path::PathBuf::from(format!("/dev/mapper/{name}"));
    let Ok(meta) = std::fs::metadata(&node) else {
        return;
    };
    let rdev = meta.rdev();
    let dev_id = format!("{}:{}", libc::major(rdev), libc::minor(rdev));
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return;
    };
    let mut targets = mounts_on_device(&mountinfo, &dev_id);
    targets.sort_by_key(|t| std::cmp::Reverse(t.components().count()));
    for target in targets {
        match crate::sys::mount::umount(&target, nix::mount::MntFlags::MNT_DETACH) {
            Ok(()) => crate::nmbl_info!(
                "seal: detached {} (on TPM-unsealed mapper {name})",
                target.display()
            ),
            Err(e) => crate::nmbl_warn!("seal: could not detach {}: {e}", target.display()),
        }
    }
}

/// Mount points in `mountinfo` whose device (field 3, `major:minor`) is
/// `dev_id`. Octal escapes in the mount point (`\040` for a space) are
/// decoded.
pub(super) fn mounts_on_device(mountinfo: &str, dev_id: &str) -> Vec<std::path::PathBuf> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let dev = fields.nth(2)?;
            let mount_point = fields.nth(1)?;
            (dev == dev_id).then(|| std::path::PathBuf::from(unescape_mountinfo(mount_point)))
        })
        .collect()
}

/// Decode the `\ooo` octal escapes the kernel uses in mountinfo paths.
fn unescape_mountinfo(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'\\'
            && let Some(oct) = bytes.get(i + 1..i + 4)
            && let Ok(s) = std::str::from_utf8(oct)
            && let Ok(v) = u8::from_str_radix(s, 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// === Real-vs-test seams ===
//
// In production these call straight into the TPM cap and the activation
// runner. Under `#[cfg(test)]` they consult overridable hooks so the
// call-ORDER tests can drive the seal without a real TPM or cryptsetup
// while still observing that cap precedes close precedes fork.

#[cfg(not(test))]
fn cap_lock_pcr_seam() -> CapOutcome {
    crate::tpm::cap_lock_pcr()
}

#[cfg(not(test))]
async fn close_one_async(entry: &MapperEntry, sender: &LocalSender) -> Result<(), SealFailed> {
    let (outcome, _captured) =
        crate::sys::activation::run_capture(&entry.cryptsetup, &close_argv(entry), sender)
            .await
            .map_err(SealFailed::new)?;
    close_outcome(&entry.name, outcome.exit_code)
}

#[cfg(not(test))]
fn close_one_blocking(entry: &MapperEntry) -> Result<(), SealFailed> {
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
pub(super) mod test_seam {
    //! Overridable cap/close seams + a shared call-ORDER log. The tests
    //! install a cap outcome and a per-mapper close result, then assert
    //! the recorded order (cap, then each close) and that the SUBSEQUENT
    //! fork the test performs lands after both.

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
}
