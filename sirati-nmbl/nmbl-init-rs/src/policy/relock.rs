//! The relock-and-refuse policy terminus (R-1 / R-7 / R-13 / FIX-10 /
//! FIX-03 / FIX-21 / FIX-47). ALWAYS-COMPILED.
//!
//! Like the seal guard it builds on (FIX-09), this is compiled in EVERY
//! build: the always-compiled seal-failure diverts (the emergency-menu
//! refuse, `rescue::dispatch`, `run_force_rescue`) route through here, so
//! the [`TerminalAction::RebootIntoRescue`] terminus must be constructible
//! without the `secure-boot` feature. The only `secure-boot`-specific input
//! (the priority-volume / countdown / sentinel-path config) is read through
//! cfg-aware accessors that fall back to the single-sourced defaults.
//!
//! [`relock_and_refuse`] (and its blocking sibling) is the SHARED fail-path
//! every untrusted-image / failed-gate / seal-failure case routes through.
//! It performs, IN THIS ORDER:
//!
//! 1. **Cap** the lock PCR — BEST-EFFORT. A present-but-uncappable TPM does
//!    NOT abort the refuse (FIX-27): the imminent `reboot(RB_AUTOBOOT)` is
//!    the real lock boundary, and a refuse-loop brick is worse than a
//!    best-effort cap. (cap FIRST so the irreversible poison lands before
//!    anything slow — FIX-10.)
//! 2. **Close** every TPM-unsealed LUKS mapper from the [`super::registry`]
//!    (FIX-03), so no decrypted `/dev/mapper/<x>` node survives into the
//!    refuse countdown.
//! 3. **Write the sentinel** — BEFORE the relock closes its backing device
//!    (FIX-21) and while the boot FS is still writable — so the next boot
//!    sees the force-rescue marker even on a single-LUKS layout where the
//!    boot FS lives behind the priority volume.
//! 4. **Relock LUKS** via an async [`run_capture`] of a kind-aware
//!    [`relock_argv`] (FIX-47): `cryptsetup close <name>` for LUKS (only a
//!    `/dev/mapper/<name>` shape), `vgchange -an <vg>` for LVM, `mdadm
//!    --stop <md>` for mdraid. A best-effort loop: a failed relock is
//!    logged, never fatal.
//!
//! Steps 1–2 are exactly the BEST-EFFORT seal (`guard::seal_for_refuse*`),
//! which ALSO mints the unforgeable [`Sealed`] witness so the resulting
//! [`TerminalAction::RebootIntoRescue`] is type-gated on a seal (R-2). The
//! refuse countdown itself is rendered LATER, by [`super::refuse_screen`],
//! at the `run_tui_session` Err arm — this module only does the
//! security-load-bearing teardown and returns the terminus.

use crate::config::Config;
use crate::error::NmblError;
use crate::policy::guard::{seal_for_refuse_async, seal_for_refuse_blocking};
use crate::sys::poller::LocalSender;
use crate::terminal::TerminalAction;

mod argv;
#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
#[path = "relock_tests.rs"]
mod tests;

pub use argv::{RelockCommand, relock_argv};

/// The public refuse entry (R-1): the SOLE untrusted-image / policy-fail
/// terminus. ASYNC — for the refuse sites that run inside the interactive
/// runtime (the priority gate, the in-menu seal-failure diverts), so the
/// LUKS relock drives through the async [`run_capture`] (FIX-47, never the
/// blocking runner inside the runtime). Delegates to [`relock_and_refuse`].
///
/// [`run_capture`]: crate::sys::activation::run_capture
pub async fn refuse_unsigned(
    config: &Config,
    cause: NmblError,
    sender: &LocalSender,
) -> TerminalAction {
    relock_and_refuse(config, cause, sender).await
}

/// Blocking refuse entry, for the post-runtime terminal sites
/// (`rescue::dispatch`, `run_force_rescue`, the `dispatch_execve`
/// seal-failure backstop) that run AFTER the interactive runtime has
/// unwound and therefore may use the blocking process runner.
pub fn refuse_unsigned_blocking(config: &Config, cause: NmblError) -> TerminalAction {
    relock_and_refuse_blocking(config, cause)
}

/// Cap → close-mappers → sentinel → relock, then return the type-gated
/// [`TerminalAction::RebootIntoRescue`] (ASYNC). The cap+close is the
/// best-effort seal that mints the [`Sealed`] witness; the relock loop runs
/// after the sentinel write so the marker is durable even when the relock
/// tears down the device the sentinel lives on (FIX-21).
///
/// [`Sealed`]: crate::policy::Sealed
pub async fn relock_and_refuse(
    config: &Config,
    cause: NmblError,
    sender: &LocalSender,
) -> TerminalAction {
    // (a) cap the lock PCR FIRST + (b) close every tpm-unsealed mapper.
    // Best-effort: the refuse proceeds even on an uncappable TPM (FIX-27).
    // Minting the witness here is what makes RebootIntoRescue reachable.
    let sealed = seal_for_refuse_async(refuse_require_tpm(config), sender).await;
    // (c) sentinel BEFORE (d) relock, while the FS is still writable.
    super::sentinel::write_sentinel(config);
    // (d) best-effort relock of every configured LUKS/LVM/mdraid volume.
    relock_volumes_async(config, sender).await;
    TerminalAction::reboot_into_rescue(sealed, cause)
}

/// Blocking sibling of [`relock_and_refuse`]. Same order; the relock loop
/// uses the blocking process runner because the runtime has already
/// unwound (the only context this is reached from).
pub fn relock_and_refuse_blocking(config: &Config, cause: NmblError) -> TerminalAction {
    let sealed = seal_for_refuse_blocking(refuse_require_tpm(config));
    super::sentinel::write_sentinel(config);
    #[cfg(test)]
    super::guard::test_seam::record_sentinel();
    relock_volumes_blocking(config);
    #[cfg(test)]
    super::guard::test_seam::record_relock();
    TerminalAction::reboot_into_rescue(sealed, cause)
}

/// Run the kind-aware relock command for every activation that has one,
/// through the async [`run_capture`] (FIX-47). Best-effort: a present
/// mapper whose relock FAILS is loud-warned (a hard signal), an absent one
/// is benign. Never aborts the refuse.
///
/// [`run_capture`]: crate::sys::activation::run_capture
async fn relock_volumes_async(config: &Config, sender: &LocalSender) {
    for act in &config.activations {
        let Some(cmd) = relock_argv(act) else {
            continue;
        };
        match crate::sys::activation::run_capture(&cmd.binary, &cmd.argv, sender).await {
            Ok((outcome, _)) => report_relock(&cmd, outcome.exit_code),
            Err(e) => warn_relock_spawn(&cmd, &e),
        }
    }
}

/// Blocking sibling of [`relock_volumes_async`].
fn relock_volumes_blocking(config: &Config) {
    for act in &config.activations {
        let Some(cmd) = relock_argv(act) else {
            continue;
        };
        match crate::sys::activation::run_capture_blocking(&cmd.binary, &cmd.argv) {
            Ok((outcome, _)) => report_relock(&cmd, outcome.exit_code),
            Err(e) => warn_relock_spawn(&cmd, &e),
        }
    }
}

/// Distinguish "volume absent / already locked" (benign) from
/// "present-and-relock-FAILED" (a hard signal), per FIX-47. `cryptsetup
/// close` exits 4 when the mapper is not active; `vgchange`/`mdadm` exit
/// non-zero when there is nothing to deactivate. We treat 0 and the
/// "already inactive" code as success and loud-warn on anything else.
fn report_relock(cmd: &RelockCommand, exit_code: i32) {
    if exit_code == 0 || exit_code == cmd.absent_exit_code {
        crate::nmbl_info!("refuse: relocked {} ({})", cmd.label, cmd.argv.join(" "));
    } else {
        crate::nmbl_warn!(
            "refuse: relock of {} FAILED (exit {exit_code}); the volume may still be unlocked: {} {}",
            cmd.label,
            cmd.binary.display(),
            cmd.argv.join(" ")
        );
    }
}

/// Log a relock that could not even spawn (missing binary, fork failure).
/// Best-effort: the refuse still proceeds to the reboot, which is the real
/// lock boundary.
fn warn_relock_spawn(cmd: &RelockCommand, err: &NmblError) {
    crate::nmbl_warn!(
        "refuse: could not spawn relock for {} ({}): {err}; rebooting anyway",
        cmd.label,
        cmd.binary.display()
    );
}

/// The effective `require_tpm` posture for the refuse seal: the OR of the
/// `[tpm]` and (when compiled) `[secure_boot]` knobs, so either table can
/// demand a working TPM (FIX-28). A `require_tpm` box with no TPM seals
/// best-effort here regardless — the refuse must never brick — but the flag
/// still drives the cap-vs-degrade decision inside the seal.
fn refuse_require_tpm(config: &Config) -> bool {
    #[cfg(feature = "secure-boot")]
    {
        config.tpm.require_tpm || config.secure_boot.require_tpm
    }
    #[cfg(not(feature = "secure-boot"))]
    {
        config.tpm.require_tpm
    }
}
